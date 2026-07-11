//! `Workbook` → `.xlsx` (SpreadsheetML / OOXML) — the inverse of the `xlsx`
//! reader, so `read → Workbook → write → read` round-trips the typed cells.
//!
//! Each cell maps to the exact form the reader recognizes: [`Cell::Text`] → a
//! shared string (`t="s"`), [`Cell::Number`] → a bare `<v>`, [`Cell::Date`] → a
//! `<v>` with style `s="1"` (a `numFmt 14` cell ⇒ the reader classifies it as a
//! date), [`Cell::Bool`] → `t="b"`, [`Cell::Error`] → `t="e"`. Reuses the `zip`
//! dependency already pulled in by the `xlsx` feature; no new deps.

mod cell;
mod comment;
mod drawing;
mod styles;
mod table;
mod validate;
mod workbook;
mod worksheet;
pub(crate) mod xml;

pub(crate) use validate::validate;
pub use validate::WriteError;
pub(crate) use workbook::is_w3cdtf;

use std::collections::HashMap;
use std::io::Write;

use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::write::cell::shared_strings_xml;
use crate::write::comment::{comments_xml_for_comments, vml_drawing_xml_for_comments};
use crate::write::drawing::build_drawings_with_budget;
use crate::write::styles::StyleTable;
use crate::write::table::{table_name, table_xml};
use crate::write::workbook::{
    app_xml_with_budget, content_types, core_xml_with_budget, root_rels, workbook_rels,
    workbook_xml_with_budget,
};
use crate::write::worksheet::{
    worksheet_xml, WorksheetXmlContext, WorksheetXmlOptions, WorksheetXmlRelationships,
};
use crate::write::xml::{
    esc_attr, NS_PKG_REL, REL_COMMENTS, REL_DRAWING, REL_HYPERLINK, REL_TABLE, REL_VML_DRAWING,
    XML_DECL,
};
use crate::write::xml::{CT_COMMENTS, CT_TABLE, CT_VML};
use crate::Workbook;

// Excel grid bounds (0-based): last column XFD = 16383, last row = 1_048_575.
const MAX_COL: u16 = 16_383;
const MAX_ROW: u32 = 1_048_575;
/// Portable sheet-count ceiling shared by checked validation and infallible
/// best-effort emission.
const MAX_SHEETS: usize = 255;
/// Upper bound on total emitted workbook payload bytes that scale with authored
/// data (cells, shared strings, styles, drawings, comments, and tables), so a
/// hostile/huge `Workbook` cannot drive an unbounded allocation.
const MAX_OUTPUT_BYTES: usize = 256 << 20;

struct BuiltTables {
    parts: Vec<(String, Vec<u8>)>,
    sheet_nums: Vec<Vec<usize>>,
    ct_overrides: Vec<(String, String)>,
}

struct BuiltComments {
    parts: Vec<(String, Vec<u8>)>,
    sheet_has_comments: Vec<bool>,
    ct_overrides: Vec<(String, String)>,
    need_vml: bool,
}

/// Serialize a [`Workbook`] to `.xlsx` bytes.
pub(crate) fn to_xlsx(wb: &Workbook) -> Vec<u8> {
    let sheet_count = wb.sheets.len().min(MAX_SHEETS);
    let mut sst: Vec<String> = Vec::new();
    let mut sst_idx: HashMap<String, usize> = HashMap::new();
    let mut sst_count = 0usize;
    let mut styles = StyleTable::new();
    let mut sheet_xmls: Vec<String> = Vec::with_capacity(sheet_count);
    let mut sheet_rels: Vec<WorksheetXmlRelationships> = Vec::with_capacity(sheet_count);

    let mut budget = MAX_OUTPUT_BYTES;
    // Drawings (images + charts) contribute their own parts + content-type
    // overrides + media-extension defaults.
    let mut drawings = build_drawings_with_budget(wb, sheet_count, &mut budget);

    let BuiltTables {
        parts: table_parts,
        sheet_nums: sheet_table_nums,
        ct_overrides: table_ct_overrides,
    } = build_table_parts_with_budget(wb, sheet_count, &mut budget);
    drawings.ct_overrides.extend(table_ct_overrides);

    let BuiltComments {
        parts: comment_parts,
        sheet_has_comments,
        ct_overrides: comment_ct_overrides,
        need_vml,
    } = build_comment_parts_with_budget(wb, sheet_count, &mut budget);
    drawings.need_vml |= need_vml;
    drawings.ct_overrides.extend(comment_ct_overrides);

    for (i, sheet) in wb.sheets.iter().take(sheet_count).enumerate() {
        let is_active = i == wb.active_sheet;
        let has_drawing = drawings.sheet_has_drawing.get(i).copied().unwrap_or(false);
        let has_comments = sheet_has_comments.get(i).copied().unwrap_or(false);
        let table_nums = sheet_table_nums.get(i).map_or(&[][..], Vec::as_slice);
        let opts = WorksheetXmlOptions {
            is_active,
            sheet_num: i + 1,
            has_drawing,
            has_comments,
            table_nums,
        };
        let mut ctx = WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (sx, rels) = worksheet_xml(sheet, opts, &mut ctx);
        sheet_xmls.push(sx);
        sheet_rels.push(rels);
    }
    // --- assemble the parts ---
    let mut parts: Vec<(String, Vec<u8>)> = vec![
        (
            "[Content_Types].xml".into(),
            content_types(sheet_count, &drawings).into_bytes(),
        ),
        ("_rels/.rels".into(), root_rels().into_bytes()),
        (
            "docProps/core.xml".into(),
            core_xml_with_budget(&wb.properties, &mut budget).into_bytes(),
        ),
        (
            "docProps/app.xml".into(),
            app_xml_with_budget(&wb.properties, &mut budget).into_bytes(),
        ),
        (
            "xl/workbook.xml".into(),
            workbook_xml_with_budget(wb, &mut budget).into_bytes(),
        ),
        (
            "xl/_rels/workbook.xml.rels".into(),
            workbook_rels(sheet_count).into_bytes(),
        ),
        ("xl/styles.xml".into(), styles.to_xml().into_bytes()),
        (
            "xl/sharedStrings.xml".into(),
            shared_strings_xml(&sst, sst_count).into_bytes(),
        ),
    ];
    for (i, sx) in sheet_xmls.into_iter().enumerate() {
        parts.push((format!("xl/worksheets/sheet{}.xml", i + 1), sx.into_bytes()));
    }
    // Per-sheet rels: hyperlinks (rId1..K), the drawing rel, the comments +
    // vmlDrawing rels, then a tablePart rel per table — the same order
    // `worksheet_xml` assumes when it emits `<drawing>`/`<legacyDrawing>`/
    // `<tablePart>` rel ids.
    for (i, rels) in sheet_rels.iter().enumerate() {
        if rels.is_empty() {
            continue;
        }
        let links = &rels.links;
        let has_drawing = rels.has_drawing;
        let has_comments = rels.has_comments;
        let table_count = rels.table_count.min(sheet_table_nums[i].len());
        let tables = &sheet_table_nums[i][..table_count];
        let n = i + 1;
        let mut r = String::new();
        r.push_str(XML_DECL);
        r.push_str(&format!(r#"<Relationships xmlns="{NS_PKG_REL}">"#));
        for (j, (_, url)) in links.iter().enumerate() {
            r.push_str(&format!(
                r#"<Relationship Id="rId{}" Type="{REL_HYPERLINK}" Target="{}" TargetMode="External"/>"#,
                j + 1,
                esc_attr(url)
            ));
        }
        let mut next = links.len();
        if has_drawing {
            next += 1;
            r.push_str(&format!(
                r#"<Relationship Id="rId{next}" Type="{REL_DRAWING}" Target="../drawings/drawing{n}.xml"/>"#
            ));
        }
        if has_comments {
            next += 1;
            r.push_str(&format!(
                r#"<Relationship Id="rId{next}" Type="{REL_COMMENTS}" Target="../comments{n}.xml"/>"#
            ));
            next += 1;
            r.push_str(&format!(
                r#"<Relationship Id="rId{next}" Type="{REL_VML_DRAWING}" Target="../drawings/vmlDrawing{n}.vml"/>"#
            ));
        }
        for &tn in tables {
            next += 1;
            r.push_str(&format!(
                r#"<Relationship Id="rId{next}" Type="{REL_TABLE}" Target="../tables/table{tn}.xml"/>"#
            ));
        }
        r.push_str("</Relationships>");
        parts.push((
            format!("xl/worksheets/_rels/sheet{}.xml.rels", i + 1),
            r.into_bytes(),
        ));
    }
    parts.extend(drawings.parts);
    parts.extend(table_parts);
    parts.extend(comment_parts);

    zip_parts(parts)
}

fn build_table_parts_with_budget(
    wb: &Workbook,
    sheet_count: usize,
    budget: &mut usize,
) -> BuiltTables {
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    let mut sheet_nums: Vec<Vec<usize>> = Vec::with_capacity(sheet_count);
    let mut ct_overrides: Vec<(String, String)> = Vec::new();
    let mut table_n = 0usize;
    // Table display names must be unique across emitted workbook tables (Excel
    // rejects duplicates).
    let mut seen_table_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for sheet in wb.sheets.iter().take(sheet_count) {
        let mut nums = Vec::with_capacity(sheet.tables.len());
        for t in &sheet.tables {
            if *budget == 0 {
                break;
            }
            let next_table_n = table_n + 1;
            let mut name = table_name(&t.name, next_table_n);
            while seen_table_names.contains(&name.to_lowercase()) {
                name = format!("{name}_{next_table_n}");
            }
            let path = format!("xl/tables/table{next_table_n}.xml");
            let part_name = format!("/xl/tables/table{next_table_n}.xml");
            let bytes = table_xml(t, next_table_n, &name).into_bytes();
            let cost = bytes
                .len()
                .saturating_add(content_type_override_cost(&part_name, CT_TABLE));
            if cost > *budget {
                if parts.is_empty() {
                    *budget = 0;
                }
                break;
            }
            *budget -= cost;
            table_n = next_table_n;
            seen_table_names.insert(name.to_lowercase());
            nums.push(next_table_n);
            parts.push((path, bytes));
            ct_overrides.push((part_name, CT_TABLE.to_string()));
        }
        sheet_nums.push(nums);
    }
    BuiltTables {
        parts,
        sheet_nums,
        ct_overrides,
    }
}

fn build_comment_parts_with_budget(
    wb: &Workbook,
    sheet_count: usize,
    budget: &mut usize,
) -> BuiltComments {
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    let mut sheet_has_comments = vec![false; sheet_count];
    let mut ct_overrides: Vec<(String, String)> = Vec::new();
    let mut need_vml = false;
    for (i, sheet) in wb.sheets.iter().take(sheet_count).enumerate() {
        if sheet.comments.is_empty() || *budget == 0 {
            continue;
        }
        let n = i + 1;
        let part_name = format!("/xl/comments{n}.xml");
        let fixed_part_cost =
            content_type_override_cost(&part_name, CT_COMMENTS).saturating_add(if need_vml {
                0
            } else {
                content_type_default_cost("vml", CT_VML)
            });
        let mut emitted_comments = Vec::new();
        let mut current_payload_cost = 0usize;
        for comment in &sheet.comments {
            let mut candidate = emitted_comments.clone();
            candidate.push(comment.clone());
            let comments = comments_xml_for_comments(&candidate);
            let vml = vml_drawing_xml_for_comments(&candidate);
            let candidate_payload_cost = comments.len().saturating_add(vml.len());
            let added_payload_cost = candidate_payload_cost.saturating_sub(current_payload_cost);
            let cost = added_payload_cost.saturating_add(if emitted_comments.is_empty() {
                fixed_part_cost
            } else {
                0
            });
            if !consume_budget(budget, cost) {
                break;
            }
            emitted_comments = candidate;
            current_payload_cost = candidate_payload_cost;
        }
        if emitted_comments.is_empty() {
            continue;
        }
        let comments = comments_xml_for_comments(&emitted_comments).into_bytes();
        let vml = vml_drawing_xml_for_comments(&emitted_comments).into_bytes();
        need_vml = true;
        sheet_has_comments[i] = true;
        parts.push((format!("xl/comments{n}.xml"), comments));
        parts.push((format!("xl/drawings/vmlDrawing{n}.vml"), vml));
        ct_overrides.push((part_name, CT_COMMENTS.to_string()));
    }
    BuiltComments {
        parts,
        sheet_has_comments,
        ct_overrides,
        need_vml,
    }
}

fn consume_budget(budget: &mut usize, cost: usize) -> bool {
    if cost > *budget {
        *budget = 0;
        return false;
    }
    *budget -= cost;
    true
}

fn content_type_override_cost(part_name: &str, content_type: &str) -> usize {
    format!(r#"<Override PartName="{part_name}" ContentType="{content_type}"/>"#).len()
}

fn content_type_default_cost(extension: &str, content_type: &str) -> usize {
    format!(r#"<Default Extension="{extension}" ContentType="{content_type}"/>"#).len()
}

fn zip_parts(parts: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    fn build(parts: &[(String, Vec<u8>)]) -> std::io::Result<Vec<u8>> {
        let mut zw = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, bytes) in parts {
            zw.start_file(path.as_str(), opt)?;
            zw.write_all(bytes)?;
        }
        Ok(zw.finish()?.into_inner())
    }
    build(&parts).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use crate::{
        Border, BorderStyle, Cell, CellEntry, CellStyle, Color, HAlign, Sheet, VAlign, Workbook,
    };
    use std::io::Read;

    fn part(bytes: &[u8], name: &str) -> String {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
        let mut s = String::new();
        z.by_name(name).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    fn entry(row: u32, col: u16, value: Cell) -> CellEntry {
        CellEntry {
            row,
            col,
            value,
            text: String::new(),
            style: None,
            hyperlink: None,
        }
    }

    fn worksheet_xml_with_budget<'a>(
        sheet: &Sheet,
        styles: &'a mut super::StyleTable,
        budget: &'a mut usize,
    ) -> (String, Vec<(String, String)>) {
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget,
        };
        let (xml, rels) = super::worksheet::worksheet_xml(sheet, opts, &mut ctx);
        (xml, rels.links)
    }

    #[test]
    fn shared_strings_count_total_references_not_unique_entries() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("sst");
        sheet.write(0, 0, "repeat");
        sheet.write(0, 1, "repeat");

        let sst = part(&wb.to_xlsx(), "xl/sharedStrings.xml");

        assert!(
            sst.contains(r#"count="2""#),
            "shared string count must track total cell references: {sst}"
        );
        assert!(
            sst.contains(r#"uniqueCount="1""#),
            "uniqueCount must track unique SST entries: {sst}"
        );
    }

    #[test]
    fn worksheet_budget_counts_new_shared_string_payload() {
        let mut sheet = Sheet::new("s");
        sheet.write(0, 0, "x".repeat(4096));

        let mut styles = super::StyleTable::new();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let mut budget = 64;
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, links) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(links.is_empty());
        assert!(
            sst.is_empty(),
            "new shared string payload must consume output budget"
        );
        assert!(
            !xml.contains(r#"<c r="A1""#),
            "cell pointing at an omitted shared string must not be emitted"
        );
        assert_eq!(budget, 0);
    }

    #[test]
    fn drawing_budget_omits_oversized_image_payload() {
        let mut wb = Workbook::new();
        wb.add_sheet("img").add_image(crate::Image {
            data: vec![7; 1024],
            format: crate::ImageFmt::Png,
            from: (0, 0),
            to: Some((1, 1)),
        });

        let mut budget = 64;
        let drawings =
            super::drawing::build_drawings_with_budget(&wb, wb.sheets.len(), &mut budget);

        assert_eq!(budget, 0);
        assert!(
            drawings.parts.is_empty(),
            "skipping the only oversized image should leave no dangling drawing parts"
        );
        assert!(!drawings.need_png);
    }

    #[test]
    fn table_budget_omits_oversized_table_payload() {
        let mut wb = Workbook::new();
        wb.add_sheet("tbl").add_table(crate::Table {
            range: (0, 0, 1, 0),
            name: "HugeTable".into(),
            columns: vec!["x".repeat(1024)],
            style: None,
        });

        let mut budget = 64;
        let built = super::build_table_parts_with_budget(&wb, wb.sheets.len(), &mut budget);

        assert_eq!(budget, 0);
        assert!(built.parts.is_empty());
        assert!(built.ct_overrides.is_empty());
        assert!(built.sheet_nums[0].is_empty());
    }

    #[test]
    fn table_budget_counts_content_type_override_payload() {
        let table = crate::Table {
            range: (0, 0, 1, 0),
            name: "BudgetTable".into(),
            columns: vec!["Header".into()],
            style: None,
        };
        let old_cost = super::table_xml(&table, 1, "BudgetTable")
            .len()
            .saturating_add("/xl/tables/table1.xml".len());
        let mut wb = Workbook::new();
        wb.add_sheet("tbl").add_table(table);

        let mut budget = old_cost;
        let built = super::build_table_parts_with_budget(&wb, wb.sheets.len(), &mut budget);

        assert_eq!(budget, 0);
        assert!(
            built.parts.is_empty(),
            "table content-type override XML must consume output budget"
        );
        assert!(built.ct_overrides.is_empty());
        assert!(built.sheet_nums[0].is_empty());
    }

    #[test]
    fn table_budget_preserves_budget_for_accepted_references() {
        let small = crate::Table {
            range: (0, 0, 1, 0),
            name: "FirstTable".into(),
            columns: vec!["Header".into()],
            style: None,
        };
        let first_name = super::table_name(&small.name, 1);
        let first_part_cost = super::table_xml(&small, 1, &first_name)
            .len()
            .saturating_add(super::content_type_override_cost(
                "/xl/tables/table1.xml",
                super::CT_TABLE,
            ));

        let ref_sheet = Sheet::new("tbl");
        let mut ref_styles = super::StyleTable::new();
        let mut ref_sst = Vec::new();
        let mut ref_sst_idx = std::collections::HashMap::new();
        let mut ref_sst_count = 0usize;
        let mut ref_budget = usize::MAX;
        let ref_opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[1],
        };
        let mut ref_ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut ref_styles,
            sst: &mut ref_sst,
            sst_idx: &mut ref_sst_idx,
            sst_count: &mut ref_sst_count,
            budget: &mut ref_budget,
        };
        let (_ref_xml, ref_rels) =
            super::worksheet::worksheet_xml(&ref_sheet, ref_opts, &mut ref_ctx);
        assert_eq!(ref_rels.table_count, 1);
        let table_ref_budget = usize::MAX - ref_budget;

        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("tbl");
        sheet.add_table(small);
        sheet.add_table(crate::Table {
            range: (3, 0, 4, 0),
            name: "HugeTable".into(),
            columns: vec!["x".repeat(4096)],
            style: None,
        });

        let mut budget = first_part_cost.saturating_add(table_ref_budget);
        let built = super::build_table_parts_with_budget(&wb, wb.sheets.len(), &mut budget);

        assert_eq!(budget, table_ref_budget);
        assert_eq!(built.parts.len(), 1);
        assert_eq!(built.ct_overrides.len(), 1);
        assert_eq!(built.sheet_nums[0], vec![1]);
    }

    #[test]
    fn unchecked_table_writer_sanitizes_cell_reference_names() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("tbl");
        s.write(0, 0, "h");
        s.write(1, 0, "v");
        s.add_table(crate::Table {
            range: (0, 0, 1, 0),
            name: "A1".into(),
            columns: vec!["h".into()],
            style: None,
        });

        let table = part(&wb.to_xlsx(), "xl/tables/table1.xml");

        assert!(
            table.contains(r#"name="Table1" displayName="Table1""#),
            "cell-reference table names must be sanitized before emission: {table}"
        );
    }

    #[test]
    fn comment_budget_omits_oversized_comment_payload() {
        let mut wb = Workbook::new();
        let text = "x".repeat(1024);
        wb.add_sheet("notes")
            .add_comment(0, 0, &text, Some("reviewer"));

        let mut budget = 64;
        let built = super::build_comment_parts_with_budget(&wb, wb.sheets.len(), &mut budget);

        assert_eq!(budget, 0);
        assert!(built.parts.is_empty());
        assert!(built.ct_overrides.is_empty());
        assert!(!built.sheet_has_comments[0]);
        assert!(!built.need_vml);
    }

    #[test]
    fn comment_budget_counts_content_type_records() {
        let mut sheet = Sheet::new("notes");
        sheet.add_comment(0, 0, "review", Some("author"));
        let old_cost = super::comment::comments_xml(&sheet)
            .len()
            .saturating_add(super::comment::vml_drawing_xml(&sheet).len())
            .saturating_add("/xl/comments1.xml".len());
        let mut wb = Workbook::new();
        wb.sheets.push(sheet);

        let mut budget = old_cost;
        let built = super::build_comment_parts_with_budget(&wb, wb.sheets.len(), &mut budget);

        assert_eq!(budget, 0);
        assert!(
            built.parts.is_empty(),
            "comment content-type override/default XML must consume output budget"
        );
        assert!(built.ct_overrides.is_empty());
        assert!(!built.sheet_has_comments[0]);
        assert!(!built.need_vml);
    }

    #[test]
    fn comment_budget_counts_comment_records_incrementally() {
        let mut one = Sheet::new("notes");
        one.add_comment(0, 0, "keep", Some("author"));
        let one_cost = super::comment::comments_xml(&one)
            .len()
            .saturating_add(super::comment::vml_drawing_xml(&one).len())
            .saturating_add(super::content_type_override_cost(
                "/xl/comments1.xml",
                super::CT_COMMENTS,
            ))
            .saturating_add(super::content_type_default_cost("vml", super::CT_VML));

        let mut sheet = Sheet::new("notes");
        sheet.add_comment(0, 0, "keep", Some("author"));
        sheet.add_comment(1, 0, "drop".repeat(1024), Some("author"));
        let mut wb = Workbook::new();
        wb.sheets.push(sheet);

        let mut budget = one_cost;
        let built = super::build_comment_parts_with_budget(&wb, wb.sheets.len(), &mut budget);

        assert_eq!(budget, 0);
        assert!(built.sheet_has_comments[0]);
        assert!(built.need_vml);
        assert_eq!(built.parts.len(), 2);
        assert_eq!(built.ct_overrides.len(), 1);
        let comments = String::from_utf8(built.parts[0].1.clone()).expect("comments xml");
        let vml = String::from_utf8(built.parts[1].1.clone()).expect("vml xml");
        assert!(
            comments.contains(">keep<"),
            "the first budgeted comment should remain"
        );
        assert!(
            !comments.contains("dropdrop"),
            "the over-budget comment text should be omitted"
        );
        assert!(vml.contains("<x:Row>0</x:Row>"));
        assert!(
            !vml.contains("<x:Row>1</x:Row>"),
            "VML shape for the omitted comment must not be emitted"
        );
    }

    #[test]
    fn worksheet_budget_counts_new_style_payload() {
        let mut sheet = Sheet::new("style");
        let fmt_code = "0".repeat(4096);
        sheet.write_styled(0, 0, 1.0, &CellStyle::new().num_fmt(&fmt_code));

        let mut styles = super::StyleTable::new();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let mut budget = 64;
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, links) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(links.is_empty());
        assert!(
            !styles.to_xml().contains(&fmt_code),
            "new style payload must consume output budget"
        );
        assert!(
            !xml.contains(r#"<c r="A1""#),
            "cell pointing at an omitted style must not be emitted"
        );
        assert_eq!(budget, 0);
    }

    #[test]
    fn worksheet_budget_counts_default_row_and_column_format_xml() {
        let style = CellStyle::new().bold();
        let format = crate::Format::from_cell_style(style.clone());

        let mut styles = super::StyleTable::new();
        let mut setup_budget = usize::MAX;
        let xf = styles
            .intern_with_budget(Some(&style), false, &mut setup_budget)
            .expect("pre-intern style");
        assert!(xf > 0);

        let mut default_sheet = Sheet::new("default");
        default_sheet.set_default_format(&format);
        let mut default_budget = 8;
        let mut default_styles = styles.clone();
        let (default_xml, default_links) =
            worksheet_xml_with_budget(&default_sheet, &mut default_styles, &mut default_budget);
        assert!(default_links.is_empty());
        assert_eq!(default_budget, 0);
        assert!(
            !default_xml.contains("<cols>"),
            "default worksheet format column XML must consume budget"
        );

        let mut column_sheet = Sheet::new("column");
        column_sheet.set_col_format(0, &format);
        let mut column_budget = 8;
        let mut column_styles = styles.clone();
        let (column_xml, column_links) =
            worksheet_xml_with_budget(&column_sheet, &mut column_styles, &mut column_budget);
        assert!(column_links.is_empty());
        assert_eq!(column_budget, 0);
        assert!(
            !column_xml.contains("<cols>"),
            "column default format XML must consume budget"
        );

        let mut row_sheet = Sheet::new("row");
        row_sheet.set_row_format(0, &format);
        let mut row_budget = 8;
        let (row_xml, row_links) =
            worksheet_xml_with_budget(&row_sheet, &mut styles, &mut row_budget);
        assert!(row_links.is_empty());
        assert_eq!(row_budget, 0);
        assert!(
            !row_xml.contains("<row"),
            "row default format XML must consume budget"
        );

        let mut table_sheet = Sheet::new("table");
        table_sheet.add_table(crate::Table {
            range: (0, 0, 0, 0),
            name: "BudgetTable".into(),
            columns: vec!["Header".into()],
            style: None,
        });
        table_sheet.set_table_header_format("BudgetTable", &format);
        let mut table_budget = 24;
        let mut table_styles = styles.clone();
        let (table_xml, table_links) =
            worksheet_xml_with_budget(&table_sheet, &mut table_styles, &mut table_budget);
        assert!(table_links.is_empty());
        assert_eq!(table_budget, 0);
        assert!(
            table_xml.contains(r#"<row r="1">"#),
            "budget should allow the table header row wrapper"
        );
        assert!(
            !table_xml.contains(r#"<c r="A1""#),
            "table header format-only cell XML must consume budget"
        );
    }

    #[test]
    fn worksheet_budget_counts_column_records_incrementally() {
        let mut one = Sheet::new("cols");
        one.set_col_width(0, 20.0);
        let mut one_styles = super::StyleTable::new();
        let mut one_budget = usize::MAX;
        let (one_xml, one_links) =
            worksheet_xml_with_budget(&one, &mut one_styles, &mut one_budget);
        assert!(one_links.is_empty());
        let start = one_xml.find("<cols>").expect("cols start");
        let end = one_xml[start..]
            .find("</cols>")
            .map(|idx| start + idx + "</cols>".len())
            .expect("cols end");
        let one_col_budget = end - start;

        let mut sheet = Sheet::new("cols");
        sheet.set_col_width(0, 20.0);
        sheet.set_col_width(1, 30.0);

        let mut styles = super::StyleTable::new();
        let mut budget = one_col_budget;
        let (xml, links) = worksheet_xml_with_budget(&sheet, &mut styles, &mut budget);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(
            xml.contains(r#"<cols><col min="1" max="1" width="20" customWidth="1"/></cols>"#),
            "the first budgeted column record should remain"
        );
        assert!(
            !xml.contains(r#"min="2" max="2""#),
            "the over-budget second column record must be omitted"
        );
    }

    #[test]
    fn worksheet_budget_counts_data_validation_payload() {
        let mut sheet = Sheet::new("dv");
        sheet.add_data_validation(crate::DataValidation {
            sqref: (0, 0, 9, 0),
            kind: crate::DvKind::Custom,
            operator: crate::DvOp::Between,
            formula1: format!("LEN(A1)<{}", "9".repeat(1024)),
            formula2: None,
            allow_blank: true,
            show_input_message: true,
            show_error_message: true,
            prompt: None,
            error: None,
        });

        let mut styles = super::StyleTable::new();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let mut budget = 64;
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, links) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(links.is_empty());
        assert!(
            !xml.contains("<dataValidations"),
            "data validation XML must consume output budget"
        );
        assert_eq!(budget, 0);
    }

    #[test]
    fn worksheet_budget_preserves_budget_after_accepted_data_validation() {
        let mut first_dv_sheet = Sheet::new("dv");
        first_dv_sheet.add_data_validation(crate::DataValidation::list((0, 0, 0, 0), "\"Yes,No\""));
        let mut first_styles = super::StyleTable::new();
        let mut first_budget = usize::MAX;
        let (first_xml, first_links) =
            worksheet_xml_with_budget(&first_dv_sheet, &mut first_styles, &mut first_budget);
        assert!(first_links.is_empty());
        assert!(first_xml.contains("<dataValidations"));
        let first_dv_budget = usize::MAX - first_budget;

        let mut print_sheet = Sheet::new("print");
        print_sheet.set_print_gridlines();
        let mut print_styles = super::StyleTable::new();
        let mut print_budget = usize::MAX;
        let (print_xml, print_links) =
            worksheet_xml_with_budget(&print_sheet, &mut print_styles, &mut print_budget);
        assert!(print_links.is_empty());
        assert!(print_xml.contains("<printOptions"));
        let print_options_budget = usize::MAX - print_budget;

        let huge_formula = format!("\"{}\"", "x".repeat(4096));
        let mut sheet = Sheet::new("dv");
        sheet.add_data_validation(crate::DataValidation::list((0, 0, 0, 0), "\"Yes,No\""));
        sheet.add_data_validation(crate::DataValidation::list((1, 0, 1, 0), &huge_formula));
        sheet.set_print_gridlines();

        let mut styles = super::StyleTable::new();
        let mut budget = first_dv_budget.saturating_add(print_options_budget);
        let (xml, links) = worksheet_xml_with_budget(&sheet, &mut styles, &mut budget);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(xml.contains(r#"<dataValidations count="1">"#));
        assert!(xml.contains("<printOptions"));
        assert!(
            !xml.contains(&huge_formula),
            "over-budget later data validation must be omitted"
        );
    }

    #[test]
    fn worksheet_budget_preserves_budget_after_accepted_conditional_format() {
        let fill = crate::Color::rgb(0xFF, 0xC7, 0xCE);

        let mut first_cf_sheet = Sheet::new("cf");
        first_cf_sheet.add_conditional_format(crate::CondFormat::new(
            (0, 0, 0, 0),
            crate::CfRule::expression("A1>0", fill),
        ));
        let mut first_styles = super::StyleTable::new();
        let mut first_budget = usize::MAX;
        let (first_xml, first_links) =
            worksheet_xml_with_budget(&first_cf_sheet, &mut first_styles, &mut first_budget);
        assert!(first_links.is_empty());
        assert!(first_xml.contains("<conditionalFormatting"));
        let first_cf_budget = usize::MAX - first_budget;

        let mut dv_sheet = Sheet::new("dv");
        dv_sheet.add_data_validation(crate::DataValidation::list((0, 0, 0, 0), "\"Yes,No\""));
        let mut dv_styles = super::StyleTable::new();
        let mut dv_budget = usize::MAX;
        let (dv_xml, dv_links) =
            worksheet_xml_with_budget(&dv_sheet, &mut dv_styles, &mut dv_budget);
        assert!(dv_links.is_empty());
        assert!(dv_xml.contains("<dataValidations"));
        let dv_budget_cost = usize::MAX - dv_budget;

        let huge_formula = format!("A2>{}", "9".repeat(4096));
        let mut sheet = Sheet::new("cf");
        sheet.add_conditional_format(crate::CondFormat::new(
            (0, 0, 0, 0),
            crate::CfRule::expression("A1>0", fill),
        ));
        sheet.add_conditional_format(crate::CondFormat::new(
            (1, 0, 1, 0),
            crate::CfRule::expression(&huge_formula, fill),
        ));
        sheet.add_data_validation(crate::DataValidation::list((0, 1, 0, 1), "\"Yes,No\""));

        let mut styles = super::StyleTable::new();
        let mut budget = first_cf_budget.saturating_add(dv_budget_cost);
        let (xml, links) = worksheet_xml_with_budget(&sheet, &mut styles, &mut budget);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(xml.contains(r#"<conditionalFormatting sqref="A1:A1">"#));
        assert!(xml.contains(r#"<dataValidations count="1">"#));
        assert!(
            !xml.contains(&huge_formula),
            "over-budget later conditional format must be omitted"
        );
    }

    #[test]
    fn worksheet_budget_counts_hyperlink_payload() {
        let mut sheet = Sheet::new("links");
        sheet.write_url(
            0,
            0,
            format!("https://example.test/{}", "x".repeat(1024)),
            "x",
        );

        let mut styles = super::StyleTable::new();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let mut budget = 96;
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, links) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(
            links.is_empty(),
            "hyperlink relationship target must consume output budget"
        );
        assert!(
            !xml.contains("<hyperlinks>"),
            "hyperlink XML must not point at an omitted relationship"
        );
        assert_eq!(budget, 0);
    }

    #[test]
    fn worksheet_budget_counts_escaped_hyperlink_relationship_payload() {
        let mut sheet = Sheet::new("links");
        let url = format!("https://example.test/?q={}", "&".repeat(256));
        sheet.write_url(0, 0, &url, "x");

        let row_xml = r#"<row r="1"></row>"#.len();
        let cell_xml = r#"<c r="A1" t="s"><v>0</v></c>"#.len();
        let shared_xml = r#"<si><t xml:space="preserve">x</t></si>"#.len();
        let old_relationship_estimate = url.len().saturating_add(128);
        let hyperlink_xml = r#"<hyperlinks><hyperlink ref="A1" r:id="rId1"/></hyperlinks>"#.len();
        let mut budget = row_xml
            .saturating_add(cell_xml)
            .saturating_add(shared_xml)
            .saturating_add(old_relationship_estimate)
            .saturating_add(hyperlink_xml);
        let mut styles = super::StyleTable::new();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, links) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert_eq!(budget, 0);
        assert!(
            links.is_empty(),
            "escaped hyperlink relationship XML must consume output budget"
        );
        assert!(
            !xml.contains("<hyperlinks>"),
            "hyperlink XML must not point at an omitted escaped relationship"
        );
    }

    #[test]
    fn worksheet_budget_counts_hyperlink_records_incrementally() {
        let mut full = Sheet::new("links");
        full.write_url(0, 0, "https://example.test/a", "a");
        full.write_url(0, 1, "https://example.test/b", "b");
        let mut full_styles = super::StyleTable::new();
        let mut full_budget = usize::MAX;
        let (_full_xml, full_links) =
            worksheet_xml_with_budget(&full, &mut full_styles, &mut full_budget);
        assert_eq!(full_links.len(), 2);
        let full_cost = usize::MAX - full_budget;

        let mut sheet = Sheet::new("links");
        sheet.write_url(0, 0, "https://example.test/a", "a");
        sheet.write_url(0, 1, "https://example.test/b", "b");
        let second_hyperlink_xml = r#"<hyperlink ref="B1" r:id="rId2"/>"#.len();

        let mut styles = super::StyleTable::new();
        let mut budget = full_cost.saturating_sub(second_hyperlink_xml);
        let (xml, links) = worksheet_xml_with_budget(&sheet, &mut styles, &mut budget);

        assert_eq!(budget, 0);
        assert_eq!(
            links,
            vec![("A1".to_string(), "https://example.test/a".to_string())]
        );
        assert!(
            xml.contains(r#"<hyperlinks><hyperlink ref="A1" r:id="rId1"/></hyperlinks>"#),
            "the first budgeted hyperlink XML record should remain"
        );
        assert!(
            !xml.contains(r#"<hyperlink ref="B1""#),
            "the over-budget second hyperlink XML record must be omitted"
        );
    }

    #[test]
    fn worksheet_budget_counts_structural_metadata_payload() {
        let mut sheet = Sheet::new("structure");
        sheet.protect();
        sheet.autofilter(0, 0, 99, 12);
        sheet.merge(0, 0, 0, 12);

        let mut styles = super::StyleTable::new();
        let mut budget = 32;
        let (xml, links) = worksheet_xml_with_budget(&sheet, &mut styles, &mut budget);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(
            !xml.contains("<sheetProtection"),
            "sheet protection XML must consume output budget"
        );
        assert!(
            !xml.contains("<autoFilter"),
            "autofilter XML must consume output budget"
        );
        assert!(
            !xml.contains("<mergeCells"),
            "merge-cell XML must consume output budget"
        );
    }

    #[test]
    fn worksheet_budget_counts_merge_records_incrementally() {
        let mut one = Sheet::new("merge");
        one.merge(0, 0, 0, 1);
        let mut one_styles = super::StyleTable::new();
        let mut one_budget = usize::MAX;
        let (one_xml, one_links) =
            worksheet_xml_with_budget(&one, &mut one_styles, &mut one_budget);
        assert!(one_links.is_empty());
        let start = one_xml.find("<mergeCells").expect("mergeCells start");
        let end = one_xml[start..]
            .find("</mergeCells>")
            .map(|idx| start + idx + "</mergeCells>".len())
            .expect("mergeCells end");
        let one_merge_budget = end - start;

        let mut sheet = Sheet::new("merge");
        sheet.merge(0, 0, 0, 1);
        sheet.merge(1, 0, 1, 1);

        let mut styles = super::StyleTable::new();
        let mut budget = one_merge_budget;
        let (xml, links) = worksheet_xml_with_budget(&sheet, &mut styles, &mut budget);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(
            xml.contains(r#"<mergeCells count="1">"#),
            "merge wrapper should reflect the accepted subset"
        );
        assert!(
            xml.contains(r#"<mergeCell ref="A1:B1"/>"#),
            "the first budgeted merge should remain"
        );
        assert!(
            !xml.contains(r#"<mergeCell ref="A2:B2"/>"#),
            "the over-budget second merge must be omitted"
        );
    }

    #[test]
    fn worksheet_budget_counts_drawing_sheet_relationship_payload() {
        let sheet = Sheet::new("drawing-rel");
        let mut styles = super::StyleTable::new();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let mut budget = r#"<drawing r:id="rId1"/>"#.len();
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: true,
            has_comments: false,
            table_nums: &[],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, links) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(
            !xml.contains("<drawing"),
            "drawing sheet relationship XML must consume output budget"
        );
    }

    #[test]
    fn worksheet_budget_counts_comment_sheet_relationship_payload() {
        let sheet = Sheet::new("comment-rel");
        let mut styles = super::StyleTable::new();
        let mut budget = r#"<legacyDrawing r:id="rId2"/>"#.len();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: true,
            table_nums: &[],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, links) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(
            !xml.contains("<legacyDrawing"),
            "comment sheet relationship XML must consume output budget"
        );
    }

    #[test]
    fn worksheet_budget_preserves_budget_after_accepted_drawing_reference() {
        let sheet_num = 1_000_000_000usize;
        let sheet = Sheet::new("drawing-rel");

        let mut drawing_styles = super::StyleTable::new();
        let mut drawing_sst = Vec::new();
        let mut drawing_sst_idx = std::collections::HashMap::new();
        let mut drawing_sst_count = 0usize;
        let mut drawing_budget = usize::MAX;
        let drawing_opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num,
            has_drawing: true,
            has_comments: false,
            table_nums: &[],
        };
        let mut drawing_ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut drawing_styles,
            sst: &mut drawing_sst,
            sst_idx: &mut drawing_sst_idx,
            sst_count: &mut drawing_sst_count,
            budget: &mut drawing_budget,
        };
        let (_drawing_xml, drawing_rels) =
            super::worksheet::worksheet_xml(&sheet, drawing_opts, &mut drawing_ctx);
        assert!(drawing_rels.has_drawing);
        let drawing_ref_budget = usize::MAX - drawing_budget;

        let mut drawing_table_styles = super::StyleTable::new();
        let mut drawing_table_sst = Vec::new();
        let mut drawing_table_sst_idx = std::collections::HashMap::new();
        let mut drawing_table_sst_count = 0usize;
        let mut drawing_table_budget = usize::MAX;
        let drawing_table_opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num,
            has_drawing: true,
            has_comments: false,
            table_nums: &[1],
        };
        let mut drawing_table_ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut drawing_table_styles,
            sst: &mut drawing_table_sst,
            sst_idx: &mut drawing_table_sst_idx,
            sst_count: &mut drawing_table_sst_count,
            budget: &mut drawing_table_budget,
        };
        let (_drawing_table_xml, drawing_table_rels) =
            super::worksheet::worksheet_xml(&sheet, drawing_table_opts, &mut drawing_table_ctx);
        assert!(drawing_table_rels.has_drawing);
        assert_eq!(drawing_table_rels.table_count, 1);
        let drawing_table_budget_cost = usize::MAX - drawing_table_budget;
        let table_ref_budget_after_drawing = drawing_table_budget_cost - drawing_ref_budget;

        let mut styles = super::StyleTable::new();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let mut budget = drawing_ref_budget.saturating_add(table_ref_budget_after_drawing);
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num,
            has_drawing: true,
            has_comments: true,
            table_nums: &[1],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, rels) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(rels.links.is_empty());
        assert_eq!(budget, 0);
        assert!(rels.has_drawing);
        assert!(
            !rels.has_comments,
            "over-budget comment/VML relationships must be omitted"
        );
        assert_eq!(rels.table_count, 1);
        assert!(xml.contains("<drawing"));
        assert!(!xml.contains("<legacyDrawing"));
        assert!(xml.contains(r#"<tableParts count="1">"#));
    }

    #[test]
    fn worksheet_budget_counts_table_sheet_relationship_payload() {
        let sheet = Sheet::new("table-rel");
        let mut styles = super::StyleTable::new();
        let mut budget = r#"<tableParts count="1"><tablePart r:id="rId1"/></tableParts>"#.len();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[1],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, links) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(
            !xml.contains("<tableParts"),
            "table sheet relationship XML must consume output budget"
        );
    }

    #[test]
    fn worksheet_budget_counts_table_part_records_incrementally() {
        let sheet = Sheet::new("table-rel");
        let mut one_styles = super::StyleTable::new();
        let mut one_sst = Vec::new();
        let mut one_sst_idx = std::collections::HashMap::new();
        let mut one_sst_count = 0usize;
        let mut one_budget = usize::MAX;
        let one_opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[1],
        };
        let mut one_ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut one_styles,
            sst: &mut one_sst,
            sst_idx: &mut one_sst_idx,
            sst_count: &mut one_sst_count,
            budget: &mut one_budget,
        };
        let (_one_xml, one_rels) = super::worksheet::worksheet_xml(&sheet, one_opts, &mut one_ctx);
        assert_eq!(one_rels.table_count, 1);
        let one_table_ref_budget = usize::MAX - one_budget;

        let mut styles = super::StyleTable::new();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let mut budget = one_table_ref_budget;
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[1, 2],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, rels) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(rels.links.is_empty());
        assert_eq!(budget, 0);
        assert_eq!(rels.table_count, 1);
        assert!(
            xml.contains(r#"<tableParts count="1"><tablePart r:id="rId1"/></tableParts>"#),
            "the first budgeted table reference should remain"
        );
        assert!(
            !xml.contains(r#"<tablePart r:id="rId2"/>"#),
            "the over-budget second table reference must be omitted"
        );
    }

    #[test]
    fn worksheet_budget_counts_page_setup_payload() {
        let mut sheet = Sheet::new("page");
        sheet.set_print_gridlines();
        sheet.set_print_headings();
        sheet.set_page_setup(crate::PageSetup {
            landscape: true,
            paper_size: Some(9),
            scale: Some(125),
            fit_to_width: Some(1),
            fit_to_height: Some(2),
            first_page_number: Some(7),
            margins: Some((0.1, 0.2, 0.3, 0.4, 0.5, 0.6)),
            header: Some(format!("Header {}", "x".repeat(256))),
            footer: Some(format!("Footer {}", "y".repeat(256))),
            center_horizontally: true,
            center_vertically: true,
            print_area: None,
            repeat_rows: None,
            repeat_cols: None,
        });

        let mut styles = super::StyleTable::new();
        let mut budget = 64;
        let (xml, links) = worksheet_xml_with_budget(&sheet, &mut styles, &mut budget);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(
            !xml.contains("<printOptions"),
            "print options XML must consume output budget"
        );
        assert!(
            !xml.contains("<pageMargins"),
            "page margins XML must consume output budget"
        );
        assert!(
            !xml.contains("<pageSetup"),
            "page setup XML must consume output budget"
        );
        assert!(
            !xml.contains("<headerFooter"),
            "header/footer XML must consume output budget"
        );
    }

    #[test]
    fn worksheet_budget_preserves_budget_after_accepted_page_setup_records() {
        let mut setup_only = Sheet::new("page");
        setup_only.set_page_setup(crate::PageSetup::new().with_paper_size(9));
        let mut setup_styles = super::StyleTable::new();
        let mut setup_budget = usize::MAX;
        let (setup_xml, setup_links) =
            worksheet_xml_with_budget(&setup_only, &mut setup_styles, &mut setup_budget);
        assert!(setup_links.is_empty());
        assert!(setup_xml.contains("<pageMargins"));
        assert!(setup_xml.contains("<pageSetup"));
        let accepted_page_setup_budget = usize::MAX - setup_budget;

        let table_ref_sheet = Sheet::new("table-ref");
        let mut table_styles = super::StyleTable::new();
        let mut table_sst = Vec::new();
        let mut table_sst_idx = std::collections::HashMap::new();
        let mut table_sst_count = 0usize;
        let mut table_budget = usize::MAX;
        let table_opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[1],
        };
        let mut table_ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut table_styles,
            sst: &mut table_sst,
            sst_idx: &mut table_sst_idx,
            sst_count: &mut table_sst_count,
            budget: &mut table_budget,
        };
        let (_table_xml, table_rels) =
            super::worksheet::worksheet_xml(&table_ref_sheet, table_opts, &mut table_ctx);
        assert_eq!(table_rels.table_count, 1);
        let table_ref_budget = usize::MAX - table_budget;

        let mut sheet = Sheet::new("page");
        sheet.set_page_setup(
            crate::PageSetup::new()
                .with_paper_size(9)
                .with_header(format!("Header {}", "x".repeat(4096))),
        );

        let mut styles = super::StyleTable::new();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let mut budget = accepted_page_setup_budget.saturating_add(table_ref_budget);
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[1],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, rels) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(rels.links.is_empty());
        assert_eq!(budget, 0);
        assert!(xml.contains("<pageMargins"));
        assert!(xml.contains("<pageSetup"));
        assert!(
            !xml.contains("<headerFooter"),
            "over-budget header/footer must be omitted"
        );
        assert_eq!(rels.table_count, 1);
        assert!(xml.contains(r#"<tableParts count="1">"#));
    }

    #[test]
    fn worksheet_budget_counts_sheet_pr_view_and_format_payload() {
        let mut sheet = Sheet::new("view");
        sheet.set_tab_color([0x12, 0x34, 0x56]);
        sheet.set_outline_summary(false, false);
        sheet.hide_gridlines();
        sheet.set_show_headers(false);
        sheet.set_right_to_left(true);
        sheet.set_zoom(125);
        sheet.freeze_panes(1, 1);
        sheet.set_default_row_height(24.0);
        sheet.set_default_col_width(18.0);
        sheet.set_page_setup(crate::PageSetup {
            fit_to_width: Some(1),
            fit_to_height: Some(1),
            ..Default::default()
        });

        let mut styles = super::StyleTable::new();
        let mut budget = 8;
        let (xml, links) = worksheet_xml_with_budget(&sheet, &mut styles, &mut budget);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(
            !xml.contains("<sheetPr>"),
            "sheet properties XML must consume output budget"
        );
        assert!(
            !xml.contains("<sheetViews>"),
            "sheet view XML must consume output budget"
        );
        assert!(
            !xml.contains("<sheetFormatPr"),
            "default sheet format XML must consume output budget"
        );
    }

    #[test]
    fn worksheet_budget_bounds_single_rich_string_run() {
        let mut sheet = Sheet::new("rich");
        sheet.write_rich(
            0,
            0,
            vec![crate::TextRun::new(
                "x".repeat(4096),
                crate::Font::default(),
            )],
        );

        let mut styles = super::StyleTable::new();
        let mut sst = Vec::new();
        let mut sst_idx = std::collections::HashMap::new();
        let mut sst_count = 0usize;
        let mut budget = 64;
        let opts = super::worksheet::WorksheetXmlOptions {
            is_active: false,
            sheet_num: 1,
            has_drawing: false,
            has_comments: false,
            table_nums: &[],
        };
        let mut ctx = super::worksheet::WorksheetXmlContext {
            styles: &mut styles,
            sst: &mut sst,
            sst_idx: &mut sst_idx,
            sst_count: &mut sst_count,
            budget: &mut budget,
        };
        let (xml, links) = super::worksheet::worksheet_xml(&sheet, opts, &mut ctx);

        assert!(links.is_empty());
        assert!(
            !xml.contains(r#"<c r="A1""#),
            "rich inline string payload must consume output budget"
        );
        assert_eq!(budget, 0);
    }

    #[test]
    fn workbook_budget_counts_doc_properties_and_defined_names() {
        let mut wb = Workbook::new();
        wb.add_sheet("Data");
        wb.properties.title = Some(format!("title-{}", "x".repeat(512)));
        wb.properties.company = Some(format!("company-{}", "y".repeat(512)));
        wb.define_name("HugeName", format!("Data!$A$1:{}", "Z".repeat(512)));

        let mut core_budget = 64;
        let core = super::workbook::core_xml_with_budget(&wb.properties, &mut core_budget);
        assert_eq!(core_budget, 0);
        assert!(
            !core.contains("<dc:title>"),
            "document property payload must consume output budget"
        );

        let mut app_budget = 64;
        let app = super::workbook::app_xml_with_budget(&wb.properties, &mut app_budget);
        assert_eq!(app_budget, 0);
        assert!(
            !app.contains("<Company>"),
            "extended document property payload must consume output budget"
        );

        let mut workbook_budget = 64;
        let xml = super::workbook::workbook_xml_with_budget(&wb, &mut workbook_budget);
        assert_eq!(workbook_budget, 0);
        assert!(
            !xml.contains(r#"<definedName name="HugeName">"#),
            "workbook defined-name payload must consume output budget"
        );
        assert!(
            xml.contains("<sheets>"),
            "core workbook sheet list must remain present"
        );
    }

    #[test]
    fn workbook_budget_counts_structure_and_view_metadata() {
        let mut wb = Workbook::new();
        wb.add_sheet("Data");
        wb.add_sheet("Summary");
        wb.protect_structure();
        wb.set_active_sheet(1);

        let mut budget = r#"<workbookProtection lockStructure="1"/>"#.len() - 1;
        let xml = super::workbook::workbook_xml_with_budget(&wb, &mut budget);

        assert_eq!(budget, 0);
        assert!(
            !xml.contains("<workbookProtection"),
            "workbook structure protection XML must consume output budget"
        );
        assert!(
            !xml.contains("<bookViews>"),
            "workbook active-sheet view XML must consume output budget"
        );
        assert!(
            xml.contains("<sheets>"),
            "core workbook sheet list must remain present"
        );
    }

    #[test]
    fn worksheet_budget_counts_sparkline_groups_incrementally() {
        let mut one = Sheet::new("spark");
        one.add_sparkline(crate::Sparkline::new((0, 0), "spark!$A$1:$A$2"));
        let mut one_budget = usize::MAX;
        let mut one_styles = super::StyleTable::new();
        let (one_xml, one_links) =
            worksheet_xml_with_budget(&one, &mut one_styles, &mut one_budget);
        assert!(one_links.is_empty());
        let start = one_xml.find("<extLst>").expect("sparkline extLst start");
        let end = one_xml[start..]
            .find("</extLst>")
            .map(|idx| start + idx + "</extLst>".len())
            .expect("sparkline extLst end");
        let one_sparkline_budget = end - start;

        let mut sheet = Sheet::new("spark");
        sheet.add_sparkline(crate::Sparkline::new((0, 0), "spark!$A$1:$A$2"));
        sheet.add_sparkline(crate::Sparkline::new((1, 0), "spark!$B$1:$B$2"));

        let mut styles = super::StyleTable::new();
        let mut budget = one_sparkline_budget;
        let (xml, links) = worksheet_xml_with_budget(&sheet, &mut styles, &mut budget);

        assert!(links.is_empty());
        assert_eq!(budget, 0);
        assert!(
            xml.contains("<x14:sparklineGroups"),
            "sparkline wrapper should be emitted when the first group fits"
        );
        assert!(
            xml.contains("<xm:f>spark!$A$1:$A$2</xm:f>"),
            "the first sparkline group must consume budget independently"
        );
        assert!(
            !xml.contains("<xm:f>spark!$B$1:$B$2</xm:f>"),
            "the over-budget second sparkline group must be omitted"
        );
    }

    #[test]
    fn round_trips_typed_cells_through_xlsx() {
        let sheet = Sheet {
            name: "가격표".to_string(),
            is_worksheet: true,
            cells: vec![
                entry(0, 0, Cell::Text("품목".to_string())),
                entry(0, 1, Cell::Text("수량".to_string())),
                entry(1, 0, Cell::Text("케이블".to_string())),
                entry(1, 1, Cell::Number(42.0)),
                entry(1, 2, Cell::Date(45366.0)),
                entry(1, 3, Cell::Bool(true)),
                entry(1, 4, Cell::Error("#N/A".to_string())),
                // A shared string reused, to exercise interning.
                entry(2, 0, Cell::Text("품목".to_string())),
            ],
            ..Default::default()
        };
        let wb = Workbook {
            sheets: vec![sheet],
            ..Default::default()
        };

        let bytes = super::to_xlsx(&wb);
        let back = Workbook::open(&bytes).expect("written .xlsx must reopen");
        assert_eq!(back.sheets.len(), 1);
        let s = &back.sheets[0];
        assert_eq!(s.name, "가격표");
        assert_eq!(s.cell(0, 0), Some(&Cell::Text("품목".to_string())));
        assert_eq!(s.cell(0, 1), Some(&Cell::Text("수량".to_string())));
        assert_eq!(s.cell(1, 0), Some(&Cell::Text("케이블".to_string())));
        assert_eq!(s.cell(1, 1), Some(&Cell::Number(42.0)));
        assert_eq!(s.cell(1, 2), Some(&Cell::Date(45366.0)));
        assert_eq!(s.cell(1, 3), Some(&Cell::Bool(true)));
        assert_eq!(s.cell(1, 4), Some(&Cell::Error("#N/A".to_string())));
        assert_eq!(s.cell(2, 0), Some(&Cell::Text("품목".to_string())));
        // The date renders through the shared format path.
        assert!(s.to_text().contains("2024-03-15"), "{}", s.to_text());
    }

    #[test]
    fn round_trips_multiple_sheets() {
        let wb = Workbook {
            sheets: vec![
                Sheet {
                    name: "Sheet1".to_string(),
                    is_worksheet: true,
                    cells: vec![entry(0, 0, Cell::Text("a".to_string()))],
                    ..Default::default()
                },
                Sheet {
                    name: "둘째".to_string(),
                    is_worksheet: true,
                    cells: vec![entry(0, 0, Cell::Number(7.0))],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let back = Workbook::open(&super::to_xlsx(&wb)).expect("reopen");
        assert_eq!(back.sheets.len(), 2);
        assert_eq!(back.sheets[0].name, "Sheet1");
        assert_eq!(back.sheets[1].name, "둘째");
        assert_eq!(back.sheets[1].cell(0, 0), Some(&Cell::Number(7.0)));
    }

    #[test]
    fn unchecked_writer_caps_sheet_count_to_checked_ceiling() {
        let mut wb = Workbook::new();
        for idx in 0..=255 {
            wb.add_sheet(format!("S{idx}")).write(0, 0, idx as f64);
        }

        let bytes = wb.to_xlsx();
        let back = Workbook::open(&bytes).expect("reopen capped workbook");
        assert_eq!(back.sheets.len(), 255);
        assert_eq!(back.sheets[0].name, "S0");
        assert_eq!(back.sheets[254].name, "S254");
        assert!(back.sheet_by_name("S255").is_none());

        let content_types = part(&bytes, "[Content_Types].xml");
        assert!(content_types.contains(r#"/xl/worksheets/sheet255.xml"#));
        assert!(!content_types.contains(r#"/xl/worksheets/sheet256.xml"#));

        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        assert!(zip.by_name("xl/worksheets/sheet255.xml").is_ok());
        assert!(zip.by_name("xl/worksheets/sheet256.xml").is_err());
    }

    #[test]
    fn authors_a_styled_report() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("입찰");
        sheet.write_styled(
            0,
            0,
            "제목",
            &CellStyle::new().bold().fill([0x11, 0x22, 0x33]),
        );
        sheet.merge(0, 0, 0, 2);
        sheet.write_styled(1, 0, 150_000_000.0, &CellStyle::new().num_fmt("₩#,##0"));
        sheet.write(1, 1, Cell::Date(46_000.0)); // no num_fmt → date xf (numFmt 14)
        sheet.write_url(2, 0, "https://example.test/x", "링크");
        sheet.set_col_width(0, 40.0);
        sheet.freeze_panes(1, 0);
        sheet.autofilter(0, 0, 2, 2);

        let bytes = wb.to_xlsx();

        // Values round-trip through our own reader.
        let back = Workbook::open(&bytes).expect("authored .xlsx must reopen");
        let s = &back.sheets[0];
        assert_eq!(s.cell(0, 0), Some(&Cell::Text("제목".to_string())));
        assert_eq!(s.cell(1, 0), Some(&Cell::Number(150_000_000.0)));
        assert_eq!(s.cell(1, 1), Some(&Cell::Date(46_000.0)));

        // Styles + layout are actually emitted (structural check, no openpyxl needed).
        let styles = part(&bytes, "xl/styles.xml");
        assert!(styles.contains("₩#,##0"), "custom numFmt missing");
        assert!(styles.contains("FF112233"), "fill color missing");
        assert!(styles.contains("<b/>"), "bold missing");
        let sheet1 = part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(
            sheet1.contains(r#"<mergeCell ref="A1:C1"/>"#),
            "merge missing"
        );
        assert!(sheet1.contains(r#"state="frozen""#), "freeze missing");
        assert!(sheet1.contains("<autoFilter"), "autofilter missing");
        assert!(sheet1.contains("<hyperlinks>"), "hyperlinks missing");
        assert!(
            sheet1.contains(r#"<col min="1" max="1""#),
            "col width missing"
        );
        let rels = part(&bytes, "xl/worksheets/_rels/sheet1.xml.rels");
        assert!(
            rels.contains("https://example.test/x"),
            "hyperlink rel missing"
        );
    }

    #[test]
    fn authoring_is_ooxml_conformant() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("입찰: 비교/표*[2026]"); // illegal chars → sanitized
        let hdr = CellStyle::new()
            .bold()
            .italic()
            .font_name("맑은 고딕")
            .size(12)
            .color([0x11, 0x22, 0x33])
            .fill([0xDD, 0xEE, 0xFF])
            .align(HAlign::Center)
            .valign(VAlign::Middle)
            .wrap()
            .border(Border {
                left: BorderStyle::Thin,
                right: BorderStyle::Thin,
                top: BorderStyle::Thin,
                bottom: BorderStyle::Thin,
                color: Some(Color([0, 0, 0])),
                ..Default::default()
            });
        sheet.write_styled(0, 0, "제목", &hdr);
        sheet.merge(0, 0, 0, 2);
        sheet.write(0, 1, "UNDER_MERGE_DROP"); // inside the merge → must be dropped
        sheet.write(1, 0, "first");
        sheet.write(1, 0, "last"); // duplicate (row,col) → last wins, one <c>
        sheet.set_row_height(1, 30.0);
        sheet.write_styled(2, 0, 1234.5, &CellStyle::new().num_fmt("₩#,##0"));
        sheet.write(2, 1, Cell::Date(46_000.0));
        sheet.write(
            2,
            2,
            Cell::Formula {
                formula: "SUM(A3:B3)".to_string(),
                cached: Box::new(Cell::Number(99.0)),
            },
        );
        sheet.write_url(3, 0, "https://example.test/x", "링크");
        sheet.freeze_panes(1, 0);
        sheet.autofilter(0, 0, 3, 2);
        sheet.write(0, u16::MAX, "OFF_GRID"); // past XFD → must be dropped

        let bytes = wb.to_xlsx();
        let sheet1 = part(&bytes, "xl/worksheets/sheet1.xml");
        let styles = part(&bytes, "xl/styles.xml");
        let wbxml = part(&bytes, "xl/workbook.xml");

        // worksheet element order: autoFilter < mergeCells < hyperlinks
        let af = sheet1.find("<autoFilter").expect("autoFilter present");
        let mc = sheet1.find("<mergeCells").expect("mergeCells present");
        let hl = sheet1.find("<hyperlinks").expect("hyperlinks present");
        assert!(
            af < mc && mc < hl,
            "element order wrong: af={af} mc={mc} hl={hl}"
        );
        // cell under a merge dropped; off-grid dropped
        assert!(
            !sheet1.contains("UNDER_MERGE_DROP"),
            "under-merge cell emitted"
        );
        assert!(!sheet1.contains("OFF_GRID"), "out-of-grid cell emitted");
        // duplicate (row,col) coalesced to one <c>, last-write-wins
        assert_eq!(
            sheet1.matches(r#"<c r="A2""#).count(),
            1,
            "duplicate cell ref"
        );
        // sheet name sanitized (no illegal chars)
        assert!(
            !wbxml.contains('*') && !wbxml.contains("비교/"),
            "sheet name not sanitized"
        );
        // _FilterDatabase emitted
        assert!(
            wbxml.contains("_xlnm._FilterDatabase"),
            "_FilterDatabase missing"
        );
        // row height, borders, font facets, valign, formula now exercised
        assert!(sheet1.contains(r#"ht="30""#), "row height missing");
        assert!(styles.contains(r#"style="thin""#), "border missing");
        assert!(styles.contains("맑은 고딕"), "font name missing");
        assert!(styles.contains(r#"<sz val="12"/>"#), "font size missing");
        assert!(styles.contains("FF112233"), "font color missing");
        assert!(styles.contains("<i/>"), "italic missing");
        assert!(styles.contains(r#"vertical="center""#), "valign missing");
        assert!(sheet1.contains("<f>SUM(A3:B3)</f>"), "formula missing");

        // Reopens through our reader; dedup kept the last write.
        let back = Workbook::open(&bytes).expect("conformant .xlsx reopens");
        assert_eq!(
            back.sheets[0].cell(1, 0),
            Some(&Cell::Text("last".to_string())),
            "dedup last-write-wins"
        );
    }

    /// Strict-consumer gate: open the authored report with openpyxl. Skips (does
    /// not fail) when python/openpyxl is unavailable, so default `cargo test` is
    /// green without the toolchain. Set `RXLS_REQUIRE_OPENPYXL=1` to turn a skip
    /// into a failure; the strict local gate sets it so the gate is genuinely
    /// enforced, never silently bypassed.
    #[test]
    fn authored_report_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("rpt");
        s.write_styled(
            0,
            0,
            "공고명",
            &CellStyle::new().bold().fill([0xDD, 0xEB, 0xF7]),
        );
        s.write_url(1, 0, "https://www.g2b.go.kr/x", "콘텐츠 제작");
        s.write_styled(1, 1, 150_000_000.0, &CellStyle::new().num_fmt("₩#,##0"));
        s.write(1, 2, Cell::Date(46_000.0));
        s.merge(2, 0, 2, 2);
        s.write_styled(2, 0, "합계", &CellStyle::new().bold().align(HAlign::Center));
        s.set_col_width(0, 30.0);
        s.freeze_panes(1, 0);
        s.autofilter(0, 0, 1, 2);
        let bytes = wb.to_xlsx();

        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws['A1'].font.bold\nassert (ws['A1'].fill.fgColor.rgb or '').endswith('DDEBF7')\nassert '\u{20a9}' in ws['B2'].number_format\nassert hasattr(ws['C2'].value,'year')\nassert 'A3:C3' in [str(r) for r in ws.merged_cells.ranges]\nassert ws.freeze_panes=='A2'\nassert ws.auto_filter.ref=='A1:C2'\nassert ws['A2'].hyperlink.target.startswith('https://')\nprint('OPENPYXL_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "OPENPYXL_OK");
    }

    #[test]
    fn read_xlsx_sheet_view_and_autofilter_roundtrip_openpyxl() {
        let bytes = super::zip_parts(vec![
            (
                "xl/workbook.xml".into(),
                br#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#
                    .to_vec(),
            ),
            (
                "xl/_rels/workbook.xml.rels".into(),
                br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#.to_vec(),
            ),
            (
                "xl/worksheets/sheet1.xml".into(),
                br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetViews><sheetView showGridLines="0" showRowColHeaders="0" rightToLeft="1" zoomScale="125" workbookViewId="0"><pane xSplit="2" ySplit="1" topLeftCell="C2" activePane="bottomRight" state="frozen"/></sheetView></sheetViews><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>item</t></is></c></row></sheetData><autoFilter ref="A1:C10"/></worksheet>"#.to_vec(),
            ),
        ]);
        let wb = Workbook::open(&bytes).expect("synthetic xlsx should read");
        let out = wb.to_xlsx();

        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb['Data']\nassert ws.freeze_panes=='C2', ws.freeze_panes\nassert ws.auto_filter.ref=='A1:C10', ws.auto_filter.ref\nassert ws.sheet_view.showGridLines==False, ws.sheet_view.showGridLines\nassert ws.sheet_view.showRowColHeaders==False, ws.sheet_view.showRowColHeaders\nassert ws.sheet_view.rightToLeft==True, ws.sheet_view.rightToLeft\nassert int(ws.sheet_view.zoomScale)==125, ws.sheet_view.zoomScale\nprint('READ_VIEW_OK')\n";
        assert_opens_in_openpyxl(&out, script, "READ_VIEW_OK");
    }

    #[test]
    fn formula_cached_date_opens_as_date_in_openpyxl_data_only() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("formula-date");
        s.write(
            0,
            0,
            Cell::Formula {
                formula: "TODAY()".into(),
                cached: Box::new(Cell::Date(45_366.0)),
            },
        );

        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1], data_only=True)\nws=wb.active\nv=ws['A1'].value\nassert hasattr(v,'year'), (v, type(v), ws['A1'].number_format)\nassert v.year==2024 and v.month==3 and v.day==15, v\nprint('FORMULA_DATE_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "FORMULA_DATE_OK");
    }

    /// Write `bytes` to a temp `.xlsx`, run `script` (which must `print(sentinel)`
    /// on success) under python+openpyxl, and assert. Skips gracefully without
    /// python/openpyxl unless `RXLS_REQUIRE_OPENPYXL` is set (the strict local
    /// gate sets it, so the gate is genuinely enforced).
    fn assert_opens_in_openpyxl(bytes: &[u8], script: &str, sentinel: &str) {
        use std::process::Command;
        let required = std::env::var_os("RXLS_REQUIRE_OPENPYXL").is_some();
        let path = std::env::temp_dir().join(format!("rxls_gate_{sentinel}.xlsx"));
        if std::fs::write(&path, bytes).is_err() {
            assert!(
                !required,
                "could not write temp .xlsx for the openpyxl gate"
            );
            return;
        }
        for py in ["python", "python3", "py"] {
            match Command::new(py)
                .args(["-c", script, path.to_str().unwrap_or("")])
                .output()
            {
                Ok(out) => {
                    let so = String::from_utf8_lossy(&out.stdout);
                    let se = String::from_utf8_lossy(&out.stderr);
                    if se.contains("No module named 'openpyxl'") {
                        assert!(
                            !required,
                            "RXLS_REQUIRE_OPENPYXL set but openpyxl is not installed"
                        );
                        eprintln!("openpyxl not installed — skipping strict gate");
                        return;
                    }
                    assert!(
                        out.status.success() && so.contains(sentinel),
                        "openpyxl rejected the authored .xlsx:\nstdout={so}\nstderr={se}"
                    );
                    return;
                }
                Err(_) => continue, // this python name not found; try next
            }
        }
        assert!(
            !required,
            "RXLS_REQUIRE_OPENPYXL set but no python interpreter found"
        );
        eprintln!("python not found — skipping openpyxl strict gate");
    }

    #[test]
    fn page_setup_protection_tabcolor_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("rpt");
        s.write(0, 0, "x");
        s.set_tab_color([0xFF, 0x00, 0x00]);
        s.protect();
        s.set_page_setup(crate::PageSetup {
            landscape: true,
            margins: Some((0.5, 0.5, 0.5, 0.5, 0.2, 0.2)),
            header: Some("&CTitle".to_string()),
            footer: Some("&CPage &P".to_string()),
            print_area: Some((0, 0, 9, 4)),
            repeat_rows: Some((0, 0)),
            repeat_cols: Some((0, 1)),
            paper_size: Some(9), // A4
            scale: Some(80),
            ..Default::default()
        });
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws.page_setup.orientation=='landscape', ws.page_setup.orientation\nassert int(ws.page_setup.paperSize)==9, ws.page_setup.paperSize\nassert int(ws.page_setup.scale)==80, ws.page_setup.scale\nassert ws.protection.sheet, 'not protected'\ntc=ws.sheet_properties.tabColor\nassert tc is not None and str(tc.rgb).endswith('FF0000'), tc\nassert '$A$1:$E$10' in str(ws.print_area), ws.print_area\nassert ws.print_title_rows=='$1:$1', ws.print_title_rows\nassert ws.print_title_cols=='$A:$B', ws.print_title_cols\nprint('PAGESETUP_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "PAGESETUP_OK");
    }

    #[test]
    fn page_setup_centering_first_page_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("rpt");
        s.write(0, 0, "x");
        // Combine with print gridlines/headings to assert the merged printOptions.
        s.set_print_gridlines();
        s.set_print_headings();
        s.set_page_setup(crate::PageSetup {
            center_horizontally: true,
            center_vertically: true,
            first_page_number: Some(3),
            ..Default::default()
        });
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws.print_options.horizontalCentered==True, ws.print_options.horizontalCentered\nassert ws.print_options.verticalCentered==True, ws.print_options.verticalCentered\nassert ws.print_options.gridLines==True, ws.print_options.gridLines\nassert ws.print_options.headings==True, ws.print_options.headings\nassert int(ws.page_setup.firstPageNumber)==3, ws.page_setup.firstPageNumber\nassert ws.page_setup.useFirstPageNumber==True, ws.page_setup.useFirstPageNumber\nprint('CENTER_FPN_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "CENTER_FPN_OK");
    }

    #[test]
    fn granular_protection_open_in_openpyxl() {
        let mut wb = Workbook::new();
        // Sheet 1: granular allowances — sorting + AutoFilter + cell formatting
        // are permitted, everything else stays locked.
        let s = wb.add_sheet("granular");
        s.write(0, 0, "x");
        s.protect_with(crate::ProtectionOptions {
            sort: true,
            auto_filter: true,
            format_cells: true,
            ..Default::default()
        });
        // Sheet 2: plain protect() — every action stays locked.
        let s2 = wb.add_sheet("locked");
        s2.write(0, 0, "y");
        s2.protect();
        let bytes = wb.to_xlsx();
        // openpyxl bool semantics: True = action is *locked*. So an allowed
        // action reads back False; a still-locked one reads back True.
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\ng=wb['granular']\nassert g.protection.sheet, 'granular not protected'\nassert g.protection.sort is False, g.protection.sort\nassert g.protection.autoFilter is False, g.protection.autoFilter\nassert g.protection.formatCells is False, g.protection.formatCells\nassert g.protection.insertRows is True, g.protection.insertRows\nassert g.protection.deleteRows is True, g.protection.deleteRows\nl=wb['locked']\nassert l.protection.sheet, 'locked not protected'\nassert l.protection.sort is True, l.protection.sort\nassert l.protection.formatCells is True, l.protection.formatCells\nprint('PROT_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "PROT_OK");
    }

    #[test]
    fn format_cell_protection_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("prot");
        s.protect();
        s.write_with_format(0, 0, "editable", &crate::Format::new().set_unlocked());
        s.write_with_format(1, 0, "hidden", &crate::Format::new().set_hidden());

        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws.protection.sheet, 'sheet not protected'\nassert ws['A1'].protection.locked is False, ws['A1'].protection\nassert ws['A1'].protection.hidden is False, ws['A1'].protection\nassert ws['A2'].protection.locked is True, ws['A2'].protection\nassert ws['A2'].protection.hidden is True, ws['A2'].protection\nprint('CELL_PROT_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "CELL_PROT_OK");
    }

    #[test]
    fn data_validation_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("dv");
        s.write(0, 0, "pick");
        s.add_data_validation(crate::DataValidation::list((1, 0, 5, 0), "\"가,나,다\""));
        s.add_data_validation(crate::DataValidation {
            sqref: (1, 1, 5, 1),
            kind: crate::DvKind::Whole,
            operator: crate::DvOp::Between,
            formula1: "1".into(),
            formula2: Some("100".into()),
            allow_blank: true,
            show_input_message: true,
            show_error_message: true,
            prompt: None,
            error: Some(("Bad".into(), "1-100 only".into())),
        });
        s.add_data_validation(crate::DataValidation {
            sqref: (1, 2, 5, 2),
            kind: crate::DvKind::Custom,
            operator: crate::DvOp::Between, // ignored for custom
            formula1: "ISNUMBER(C2)".into(),
            formula2: None,
            allow_blank: false,
            show_input_message: true,
            show_error_message: true,
            prompt: None,
            error: None,
        });
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\ndvs=list(ws.data_validations.dataValidation)\nassert len(dvs)==3, len(dvs)\ntypes={d.type for d in dvs}\nassert 'list' in types and 'whole' in types and 'custom' in types, types\nlst=[d for d in dvs if d.type=='list'][0]\nassert '가,나,다' in lst.formula1, lst.formula1\nwhole=[d for d in dvs if d.type=='whole'][0]\nassert whole.operator=='between' and whole.formula1=='1' and whole.formula2=='100'\ncust=[d for d in dvs if d.type=='custom'][0]\nassert 'ISNUMBER' in cust.formula1 and cust.operator is None, (cust.formula1, cust.operator)\nprint('DV_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "DV_OK");
    }

    #[test]
    fn read_xlsx_data_validations_roundtrip_openpyxl() {
        let bytes = super::zip_parts(vec![
            (
                "xl/workbook.xml".into(),
                br#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#
                    .to_vec(),
            ),
            (
                "xl/_rels/workbook.xml.rels".into(),
                br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#.to_vec(),
            ),
            (
                "xl/worksheets/sheet1.xml".into(),
                br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData/><dataValidations count="3"><dataValidation type="list" allowBlank="1" sqref="A1 A3:A4" promptTitle="Pick" prompt="Choose one"><formula1>&quot;Yes,No&quot;</formula1></dataValidation><dataValidation type="whole" operator="between" allowBlank="0" sqref="B1:B2" errorTitle="Bounds" error="1..9 only"><formula1>1</formula1><formula2>9</formula2></dataValidation><dataValidation type="custom" sqref="C1"><formula1>ISNUMBER(C1)</formula1></dataValidation></dataValidations></worksheet>"#.to_vec(),
            ),
        ]);
        let wb = Workbook::open(&bytes).expect("synthetic xlsx should read");
        let out = wb.to_xlsx();

        let script = r#"import sys
from openpyxl import load_workbook
wb=load_workbook(sys.argv[1])
ws=wb['Data']
dvs=list(ws.data_validations.dataValidation)
assert len(dvs)==4, [(d.type, d.sqref, d.formula1, d.formula2) for d in dvs]
lists=[d for d in dvs if d.type=='list']
assert len(lists)==2, [d.sqref for d in lists]
assert {str(d.sqref) for d in lists}=={'A1','A3:A4'}, [str(d.sqref) for d in lists]
assert all('Yes,No' in d.formula1 for d in lists), [d.formula1 for d in lists]
assert all(d.showInputMessage==False for d in lists), [d.showInputMessage for d in lists]
whole=[d for d in dvs if d.type=='whole'][0]
assert whole.operator=='between' and whole.formula1=='1' and whole.formula2=='9', (whole.operator, whole.formula1, whole.formula2)
assert whole.allow_blank==False, whole.allow_blank
assert whole.showErrorMessage==False, whole.showErrorMessage
assert whole.errorTitle=='Bounds' and whole.error=='1..9 only', (whole.errorTitle, whole.error)
custom=[d for d in dvs if d.type=='custom'][0]
assert custom.formula1=='ISNUMBER(C1)', custom.formula1
assert custom.allow_blank==False, custom.allow_blank
assert custom.showInputMessage==False and custom.showErrorMessage==False, (custom.showInputMessage, custom.showErrorMessage)
print('READ_DV_OK')
"#;
        assert_opens_in_openpyxl(&out, script, "READ_DV_OK");
    }

    #[test]
    fn rich_string_round_trips_as_text() {
        // A rich cell authored with multiple runs reads back (via rxls's own reader)
        // as the concatenated text — the inline `<is><r>` survives the round-trip.
        let mut wb = Workbook::new();
        let s = wb.add_sheet("r");
        s.write_rich(
            0,
            0,
            vec![
                crate::TextRun::new(
                    "a",
                    crate::Font {
                        bold: true,
                        ..Default::default()
                    },
                ),
                crate::TextRun::new("b", crate::Font::default()),
            ],
        );
        // A later plain write supersedes a rich cell.
        s.write_rich(1, 0, vec![crate::TextRun::new("x", crate::Font::default())]);
        s.write(1, 0, "plain");
        let bytes = wb.to_xlsx();
        let wb2 = Workbook::open(&bytes).unwrap();
        assert_eq!(
            wb2.sheets[0].cell(0, 0),
            Some(&crate::Cell::Text("ab".to_string()))
        );
        assert_eq!(
            wb2.sheets[0].cell(1, 0),
            Some(&crate::Cell::Text("plain".to_string()))
        );
    }

    #[test]
    fn rich_string_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("rich");
        s.write_rich(
            0,
            0,
            vec![
                crate::TextRun::new(
                    "Hello ",
                    crate::Font {
                        bold: true,
                        ..Default::default()
                    },
                ),
                crate::TextRun::new(
                    "World",
                    crate::Font {
                        italic: true,
                        color: Some(crate::Color([255, 0, 0])),
                        name: Some("Calibri".into()),
                        size_pt: Some(12),
                        ..Default::default()
                    },
                ),
            ],
        );
        s.write_rich_with_format(
            1,
            0,
            vec![crate::TextRun::new("Styled", crate::Font::default())],
            &crate::Format::new()
                .set_bg_color([0xDD, 0xEB, 0xF7])
                .set_align(crate::FormatAlign::Center),
        );
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nv=str(ws['A1'].value)\nassert 'Hello' in v and 'World' in v, repr(v)\nassert ws['A2'].value=='Styled', ws['A2'].value\nassert ws['A2'].alignment.horizontal=='center', ws['A2'].alignment\nassert str(ws['A2'].fill.fgColor.rgb).endswith('DDEBF7'), ws['A2'].fill.fgColor.rgb\nprint('RICH_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "RICH_OK");
    }

    #[test]
    fn format_underline_and_strikethrough_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("font");
        let fmt = crate::Format::new().set_underline().set_strikethrough();
        s.write_with_format(0, 0, "decorated", &fmt);

        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nfont=wb.active['A1'].font\nassert font.underline == 'single', font.underline\nassert font.strike is True, font.strike\nprint('FONT_DECOR_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "FONT_DECOR_OK");
    }

    #[test]
    fn format_font_script_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("script");
        s.write_with_format(
            0,
            0,
            "m2",
            &crate::Format::new().set_font_script(crate::FormatScript::Superscript),
        );
        s.write_with_format(
            1,
            0,
            "h2o",
            &crate::Format::new().set_font_script(crate::FormatScript::Subscript),
        );
        s.write_rich(
            2,
            0,
            vec![crate::TextRun::new(
                "run",
                crate::Font {
                    script: crate::FormatScript::Superscript,
                    ..Default::default()
                },
            )],
        );

        let bytes = wb.to_xlsx();
        let script = "import sys, zipfile\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws['A1'].font.vertAlign == 'superscript', ws['A1'].font.vertAlign\nassert ws['A2'].font.vertAlign == 'subscript', ws['A2'].font.vertAlign\nz=zipfile.ZipFile(sys.argv[1])\nbody=z.read('xl/worksheets/sheet1.xml').decode('utf-8')\nassert '<vertAlign val=\"superscript\"/>' in body, body\nprint('FONT_SCRIPT_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "FONT_SCRIPT_OK");
    }

    #[test]
    fn format_individual_borders_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("border");
        let fmt = crate::Format::new()
            .set_border_top(crate::FormatBorder::Thick)
            .set_border_bottom(crate::FormatBorder::Double)
            .set_border_left(crate::FormatBorder::Thin)
            .set_border_right(crate::FormatBorder::Medium)
            .set_border_color([0x11, 0x22, 0x33]);
        s.write_with_format(0, 0, "edge", &fmt);

        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nb=wb.active['A1'].border\nassert b.top.style == 'thick', b.top.style\nassert b.bottom.style == 'double', b.bottom.style\nassert b.left.style == 'thin', b.left.style\nassert b.right.style == 'medium', b.right.style\nfor side in [b.top,b.bottom,b.left,b.right]:\n    assert str(side.color.rgb).endswith('112233'), side.color.rgb\nprint('BORDER_SIDES_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "BORDER_SIDES_OK");
    }

    #[test]
    fn format_pattern_fill_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("fill");
        let solid = crate::Format::new().set_background_color([0xDD, 0xEB, 0xF7]);
        let pattern = crate::Format::new()
            .set_pattern(crate::FormatPattern::DarkVertical)
            .set_background_color([0xFF, 0xEE, 0x99])
            .set_foreground_color([0x22, 0x66, 0xAA]);
        s.write_with_format(0, 0, "solid", &solid);
        s.write_with_format(1, 0, "pattern", &pattern);

        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nsolid=ws['A1'].fill\nassert solid.patternType == 'solid', solid.patternType\nassert str(solid.fgColor.rgb).endswith('DDEBF7'), solid.fgColor.rgb\npat=ws['A2'].fill\nassert pat.patternType == 'darkVertical', pat.patternType\nassert str(pat.fgColor.rgb).endswith('2266AA'), pat.fgColor.rgb\nassert str(pat.bgColor.rgb).endswith('FFEE99'), pat.bgColor.rgb\nprint('PATTERN_FILL_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "PATTERN_FILL_OK");
    }

    #[test]
    fn format_merge_overrides_only_explicit_fields() {
        let base = crate::Format::new()
            .set_num_format("#,##0")
            .set_font_name("Calibri")
            .set_font_color([0x11, 0x22, 0x33])
            .set_align(crate::FormatAlign::Right);
        let overlay = crate::Format::new()
            .set_bold()
            .set_bg_color([0xDD, 0xEB, 0xF7]);

        let merged = base.merge(&overlay).into_cell_style();

        assert_eq!(merged.num_fmt.as_deref(), Some("#,##0"));
        let font = merged.font.as_ref().expect("font");
        assert_eq!(font.name.as_deref(), Some("Calibri"));
        assert_eq!(font.color, Some(crate::Color([0x11, 0x22, 0x33])));
        assert!(font.bold);
        assert_eq!(
            merged.align.as_ref().and_then(|a| a.horizontal),
            Some(crate::HAlign::Right)
        );
        assert_eq!(
            merged.effective_fill(),
            Some(crate::Fill::solid([0xDD, 0xEB, 0xF7]))
        );
    }

    #[test]
    fn blank_row_and_column_formats_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("styles");
        s.write_blank_with_format(0, 0, &crate::Format::new().set_bg_color([0xDD, 0xEB, 0xF7]));
        s.write_with_format(1, 0, 12.5, &crate::Format::new().set_num_format("0.00"));
        s.set_row_format(2, &crate::Format::new().set_bold());
        s.write(2, 0, "row");
        s.set_col_format(
            1,
            &crate::Format::new().set_align(crate::FormatAlign::Center),
        );
        s.write(0, 1, "col");

        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws['A1'].value is None, ws['A1'].value\nassert str(ws['A1'].fill.fgColor.rgb).endswith('DDEBF7'), ws['A1'].fill.fgColor.rgb\nassert ws['A2'].number_format == '0.00', ws['A2'].number_format\nassert ws['A3'].font.bold is True, ws['A3'].font\nassert ws['B1'].alignment.horizontal == 'center', ws['B1'].alignment\nprint('FORMAT_PATHS_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "FORMAT_PATHS_OK");
    }

    #[test]
    fn worksheet_default_format_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("defaults");
        let base = crate::Format::new()
            .set_bold()
            .set_bg_color([0xDD, 0xEB, 0xF7]);
        s.set_default_format(&base);
        s.write(0, 0, "base");
        s.set_row_format(1, &crate::Format::new().set_num_format("0.00"));
        s.write(1, 0, 12.5);
        s.write_with_format(0, 1, "explicit", &crate::Format::new().set_italic());
        s.write_blank_with_format(
            2,
            0,
            &crate::Format::new().set_border(crate::FormatBorder::Thin),
        );

        let bytes = wb.to_xlsx();
        let sheet1 = part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(
            sheet1.contains(r#"<cols><col min="1" max="16384" style=""#),
            "worksheet default format should emit a whole-sheet column style: {sheet1}"
        );

        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws['A1'].font.bold is True, ws['A1'].font\nassert str(ws['A1'].fill.fgColor.rgb).endswith('DDEBF7'), ws['A1'].fill.fgColor.rgb\nassert ws['A2'].font.bold is True, ws['A2'].font\nassert str(ws['A2'].fill.fgColor.rgb).endswith('DDEBF7'), ws['A2'].fill.fgColor.rgb\nassert ws['A2'].number_format == '0.00', ws['A2'].number_format\nassert ws['B1'].font.bold is True and ws['B1'].font.italic is True, ws['B1'].font\nassert str(ws['B1'].fill.fgColor.rgb).endswith('DDEBF7'), ws['B1'].fill.fgColor.rgb\nassert ws['A3'].value is None, ws['A3'].value\nassert ws['A3'].font.bold is True, ws['A3'].font\nassert str(ws['A3'].fill.fgColor.rgb).endswith('DDEBF7'), ws['A3'].fill.fgColor.rgb\nassert ws['A3'].border.left.style == 'thin', ws['A3'].border\nprint('DEFAULT_FORMAT_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "DEFAULT_FORMAT_OK");
    }

    #[test]
    fn rich_string_run_emits_underline_and_strikethrough() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("rich");
        s.write_rich(
            0,
            0,
            vec![crate::TextRun::new(
                "Decorated",
                crate::Font {
                    underline: true,
                    strikethrough: true,
                    ..Default::default()
                },
            )],
        );

        let bytes = wb.to_xlsx();
        let script = "import sys, zipfile\nfrom openpyxl import load_workbook\nload_workbook(sys.argv[1])\nz=zipfile.ZipFile(sys.argv[1])\nbody=z.read('xl/worksheets/sheet1.xml').decode('utf-8')\nstart=body.index('<rPr>')\nend=body.index('</rPr>', start)\nrpr=body[start:end]\nassert '<u/>' in rpr, rpr\nassert '<strike/>' in rpr, rpr\nprint('RICH_DECOR_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "RICH_DECOR_OK");
    }

    #[test]
    fn doc_properties_open_in_openpyxl() {
        let mut wb = Workbook::new();
        wb.add_sheet("s").write(0, 0, "x");
        wb.properties = crate::DocProperties {
            title: Some("Quarterly Report".into()),
            creator: Some("rxls author".into()),
            company: Some("ACME".into()),
            created: Some("2024-01-02T03:04:05Z".into()),
            ..Default::default()
        };
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\np=wb.properties\nassert p.title=='Quarterly Report', p.title\nassert p.creator=='rxls author', p.creator\nprint('PROPS_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "PROPS_OK");
    }

    #[test]
    fn defined_names_and_sheet_view_open_in_openpyxl() {
        let mut wb = Workbook::new();
        {
            let s = wb.add_sheet("s");
            s.write(0, 0, 10.0);
            s.hide_gridlines();
            s.set_zoom(150);
        }
        wb.define_name("TaxRate", "s!$A$1");
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws.sheet_view.showGridLines==False, ws.sheet_view.showGridLines\nassert ws.sheet_view.zoomScale==150, ws.sheet_view.zoomScale\ndn=wb.defined_names\nnames=list(dn.keys()) if hasattr(dn,'keys') else [d.name for d in dn.definedName]\nassert 'TaxRate' in names, names\nprint('NAMES_VIEW_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "NAMES_VIEW_OK");
    }

    #[test]
    fn sheet_view_headers_and_rtl_open_in_openpyxl() {
        let mut wb = Workbook::new();
        {
            let s = wb.add_sheet("s");
            s.write(0, 0, 1.0);
            s.set_show_headers(false);
            s.set_right_to_left(true);
        }
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws.sheet_view.showRowColHeaders==False, ws.sheet_view.showRowColHeaders\nassert ws.sheet_view.rightToLeft==True, ws.sheet_view.rightToLeft\nprint('HEADERS_RTL_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "HEADERS_RTL_OK");
    }

    #[test]
    fn autofit_and_hidden_sheet_open_in_openpyxl() {
        let mut wb = Workbook::new();
        {
            let s = wb.add_sheet("data");
            s.write(0, 0, "a fairly long header cell");
            s.set_autofit();
        }
        wb.add_sheet("secret").hide();
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb['data']\nw=ws.column_dimensions['A'].width\nassert w and w>=20, w\nassert wb['secret'].sheet_state=='hidden', wb['secret'].sheet_state\nprint('AUTOFIT_HIDDEN_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "AUTOFIT_HIDDEN_OK");
    }

    #[test]
    fn all_hidden_keeps_one_visible_in_openpyxl() {
        let mut wb = Workbook::new();
        wb.add_sheet("a").hide();
        wb.add_sheet("b").hide();
        let bytes = wb.to_xlsx();
        // openpyxl rejects/repairs a workbook with no visible sheet — confirm one stays visible.
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nstates=[ws.sheet_state for ws in wb.worksheets]\nassert 'visible' in states, states\nprint('ONE_VISIBLE_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "ONE_VISIBLE_OK");
    }

    #[test]
    fn very_hidden_sheet_writes_very_hidden() {
        let mut wb = Workbook::new();
        wb.add_sheet("visible").write(0, 0, "x");
        wb.add_sheet("secret").hide_very();
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nassert wb['secret'].sheet_state=='veryHidden', wb['secret'].sheet_state\nprint('VH_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "VH_OK");
    }

    #[test]
    fn outline_and_print_options_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("o");
        for r in 0..5u32 {
            s.write(r, 0, f64::from(r));
        }
        s.group_rows(1, 3, 1); // 0-based rows 1..=3 => openpyxl rows 2..=4
        s.group_cols(2, 3, 1); // 0-based cols 2..=3 => openpyxl C..=D
        s.set_print_gridlines();
        s.set_print_headings();
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws.row_dimensions[2].outline_level==1, ws.row_dimensions[2].outline_level\nassert ws.column_dimensions['C'].outline_level==1, ws.column_dimensions['C'].outline_level\nassert ws.print_options.gridLines==True, ws.print_options.gridLines\nassert ws.print_options.headings==True, ws.print_options.headings\nprint('OUTLINE_PRINT_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "OUTLINE_PRINT_OK");
    }

    #[test]
    fn outline_summary_direction_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("o");
        for r in 0..5u32 {
            s.write(r, 0, f64::from(r));
        }
        s.group_rows(1, 3, 1);
        s.set_outline_summary(false, false); // summaries above / left
        s.collapse_row(0); // 0-based row 0 => openpyxl row 1
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert ws.sheet_properties.outlinePr.summaryBelow==False, ws.sheet_properties.outlinePr.summaryBelow\nassert ws.sheet_properties.outlinePr.summaryRight==False, ws.sheet_properties.outlinePr.summaryRight\nassert ws.row_dimensions[1].collapsed==True, ws.row_dimensions[1].collapsed\nassert ws.row_dimensions[1].hidden==True, ws.row_dimensions[1].hidden\nprint('OUTLINE_SUMMARY_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "OUTLINE_SUMMARY_OK");
    }

    #[test]
    fn conditional_formatting_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("cf");
        for r in 0..5u32 {
            s.write(r, 0, f64::from(r) * 10.0);
            s.write(r, 1, f64::from(r));
        }
        s.add_conditional_format(crate::CondFormat {
            sqref: (0, 0, 4, 0),
            rule: crate::CfRule::CellIs {
                op: crate::DvOp::GreaterThan,
                formula1: "20".into(),
                formula2: None,
                fill: Color([0xFF, 0xC7, 0xCE]),
            },
        });
        s.add_conditional_format(crate::CondFormat {
            sqref: (0, 1, 4, 1),
            rule: crate::CfRule::ColorScale2 {
                min: Color([0xF8, 0x69, 0x6B]),
                max: Color([0x63, 0xBE, 0x7B]),
            },
        });
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nallrules=[]\nfor cf in ws.conditional_formatting:\n    allrules.extend(cf.rules)\ntypes={r.type for r in allrules}\nassert 'cellIs' in types, types\nassert 'colorScale' in types, types\ncellis=[r for r in allrules if r.type=='cellIs'][0]\nfill=cellis.dxf.fill\nassert fill.patternType=='solid', fill.patternType\nassert str(fill.fgColor.rgb).endswith('FFC7CE'), fill.fgColor.rgb\nprint('CF_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "CF_OK");
    }

    #[test]
    fn cf_rule_types_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("cf2");
        for r in 0..8u32 {
            s.write(r, 0, f64::from(r));
        }
        let fill = Color([0xFF, 0xC7, 0xCE]);
        for rule in [
            crate::CfRule::TopBottom {
                rank: 3,
                bottom: false,
                percent: false,
                fill,
            },
            crate::CfRule::AboveAverage { below: false, fill },
            crate::CfRule::DuplicateValues {
                unique: false,
                fill,
            },
            crate::CfRule::Expression {
                formula: "$A1>5".into(),
                fill,
            },
        ] {
            s.add_conditional_format(crate::CondFormat {
                sqref: (0, 0, 7, 0),
                rule,
            });
        }
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nallrules=[]\nfor cf in ws.conditional_formatting:\n    allrules.extend(cf.rules)\ntypes={r.type for r in allrules}\nfor t in ['top10','aboveAverage','duplicateValues','expression']:\n    assert t in types, (t, types)\nprint('CF2_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "CF2_OK");
    }

    /// A valid 1×1 PNG (header + IHDR + IDAT + IEND).
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0A, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0,
        0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn images_and_charts_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("viz");
        for r in 0..5u32 {
            s.write(r, 0, format!("c{r}"));
            s.write(r, 1, f64::from(r) * 2.0);
        }
        s.add_image(crate::Image {
            data: PNG_1X1.to_vec(),
            format: crate::ImageFmt::Png,
            from: (0, 3),
            to: Some((5, 6)),
        });
        s.add_chart(crate::Chart {
            kind: crate::ChartKind::Bar,
            title: Some("실적".into()),
            series: vec![crate::Series {
                name: Some("값".into()),
                categories: Some("viz!$A$1:$A$5".into()),
                values: "viz!$B$1:$B$5".into(),
                bubble_sizes: None,
            }],
            legend: true,
            data_labels: true,
            x_axis_title: Some("월".into()),
            y_axis_title: Some("금액".into()),
            from: (7, 0),
            to: (20, 8),
        });
        let bytes = wb.to_xlsx();
        let script = "import sys, zipfile\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert len(ws._charts)>=1, ('charts', len(ws._charts))\nassert ws._charts[0].legend is not None, 'legend missing'\nz=zipfile.ZipFile(sys.argv[1])\ncx=[n for n in z.namelist() if n.startswith('xl/charts/chart')][0]\nbody=z.read(cx).decode('utf-8')\nassert body.count('<c:title>')>=3, ('titles', body.count('<c:title>'))\nassert '<c:dLbls>' in body, 'data labels missing'\nnames=z.namelist()\nassert any(n.startswith('xl/media/image') for n in names), names\nassert any(n.startswith('xl/drawings/drawing') for n in names), names\nprint('VIZ_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "VIZ_OK");
    }

    #[test]
    fn doughnut_chart_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("d");
        for r in 0..3u32 {
            s.write(r, 0, f64::from(r) + 1.0);
        }
        s.add_chart(crate::Chart {
            kind: crate::ChartKind::Doughnut,
            title: None,
            series: vec![crate::Series {
                name: None,
                categories: None,
                values: "d!$A$1:$A$3".into(),
                bubble_sizes: None,
            }],
            legend: false,
            data_labels: false,
            x_axis_title: None,
            y_axis_title: None,
            from: (0, 2),
            to: (10, 8),
        });
        let bytes = wb.to_xlsx();
        let script = "import sys, zipfile\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert len(ws._charts)>=1\nz=zipfile.ZipFile(sys.argv[1])\ncx=[n for n in z.namelist() if n.startswith('xl/charts/chart')][0]\nassert 'doughnutChart' in z.read(cx).decode('utf-8')\nprint('DOUGHNUT_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "DOUGHNUT_OK");
    }

    #[test]
    fn radar_chart_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("r");
        for r in 0..4u32 {
            s.write(r, 0, format!("c{r}"));
            s.write(r, 1, f64::from(r) + 1.0);
        }
        s.add_chart(crate::Chart {
            kind: crate::ChartKind::Radar,
            title: Some("레이더".into()),
            series: vec![crate::Series {
                name: Some("값".into()),
                categories: Some("r!$A$1:$A$4".into()),
                values: "r!$B$1:$B$4".into(),
                bubble_sizes: None,
            }],
            legend: true,
            data_labels: false,
            x_axis_title: None,
            y_axis_title: None,
            from: (0, 3),
            to: (12, 10),
        });
        let bytes = wb.to_xlsx();
        let script = "import sys, zipfile\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert len(ws._charts)>=1\nz=zipfile.ZipFile(sys.argv[1])\ncx=[n for n in z.namelist() if n.startswith('xl/charts/chart')][0]\nassert 'radarChart' in z.read(cx).decode('utf-8')\nprint('RADAR_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "RADAR_OK");
    }

    #[test]
    fn bubble_chart_writes_explicit_size_range_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("b");
        for r in 0..4u32 {
            s.write(r, 0, f64::from(r) + 1.0);
            s.write(r, 1, f64::from(r) * 2.0 + 3.0);
            s.write(r, 2, f64::from(r) * 5.0 + 10.0);
        }
        s.add_chart(crate::Chart {
            kind: crate::ChartKind::Bubble,
            title: None,
            series: vec![crate::Series {
                name: None,
                categories: Some("b!$A$1:$A$4".into()),
                values: "b!$B$1:$B$4".into(),
                bubble_sizes: Some("b!$C$1:$C$4".into()),
            }],
            legend: false,
            data_labels: false,
            x_axis_title: None,
            y_axis_title: None,
            from: (0, 4),
            to: (12, 10),
        });
        let bytes = wb.to_xlsx();
        let script = "import sys, zipfile\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert len(ws._charts)>=1\nz=zipfile.ZipFile(sys.argv[1])\ncx=[n for n in z.namelist() if n.startswith('xl/charts/chart')][0]\nbody=z.read(cx).decode('utf-8')\nassert '<c:bubbleSize>' in body, body\nassert '<c:f>b!$C$1:$C$4</c:f>' in body, body\nprint('BUBBLE_SIZE_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "BUBBLE_SIZE_OK");
    }

    #[test]
    fn sparklines_open_in_openpyxl_and_emit_x14_extension() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("spark");
        for r in 0..5u32 {
            s.write(r, 0, f64::from(r + 1));
        }
        s.add_sparkline(crate::Sparkline {
            location: (0, 1),
            range: "spark!$A$1:$A$5".into(),
            kind: crate::SparklineKind::Column,
        });

        let bytes = wb.to_xlsx();
        let script = "import sys, zipfile\nfrom openpyxl import load_workbook\nload_workbook(sys.argv[1])\nz=zipfile.ZipFile(sys.argv[1])\nbody=z.read('xl/worksheets/sheet1.xml').decode('utf-8')\nassert '<extLst>' in body, body\nassert '<x14:sparklineGroups' in body, body\nassert '<x14:sparklineGroup type=\"column\"' in body, body\nassert '<xm:f>spark!$A$1:$A$5</xm:f>' in body, body\nassert '<xm:sqref>B1</xm:sqref>' in body, body\nprint('SPARKLINE_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "SPARKLINE_OK");
    }

    #[test]
    fn default_row_col_sizing_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("fmt");
        s.write(0, 0, "x");
        s.set_default_row_height(22.0);
        s.set_default_col_width(14.0);
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert abs(float(ws.sheet_format.defaultRowHeight)-22.0)<0.01, ws.sheet_format.defaultRowHeight\nassert abs(float(ws.sheet_format.defaultColWidth)-14.0)<0.01, ws.sheet_format.defaultColWidth\nprint('FMT_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "FMT_OK");
    }

    #[test]
    fn table_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("t");
        for (c, h) in ["공고명", "기관", "가격"].iter().enumerate() {
            s.write(0, c as u16, *h);
        }
        for r in 1..4u32 {
            s.write(r, 0, format!("공고{r}"));
            s.write(r, 1, "조달청");
            s.write(r, 2, f64::from(r) * 1000.0);
        }
        s.add_table(crate::Table {
            range: (0, 0, 3, 2),
            name: "Sales".into(),
            columns: vec!["공고명".into(), "기관".into(), "가격".into()],
            style: None,
        });
        s.set_table_header_format(
            "Sales",
            &crate::Format::new()
                .set_bold()
                .set_bg_color([0x1F, 0x4E, 0x79])
                .set_font_color([0xFF, 0xFF, 0xFF]),
        );
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nassert len(ws.tables)>=1, list(ws.tables)\nt=list(ws.tables.values())[0]\nassert t.ref=='A1:C4', t.ref\nassert len(t.tableColumns)==3, len(t.tableColumns)\nassert ws['A1'].font.bold is True, ws['A1'].font\nassert str(ws['A1'].fill.fgColor.rgb).endswith('1F4E79'), ws['A1'].fill.fgColor.rgb\nassert str(ws['A1'].font.color.rgb).endswith('FFFFFF'), ws['A1'].font.color.rgb\nprint('TABLE_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "TABLE_OK");
    }

    #[test]
    fn active_sheet_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        wb.add_sheet("first").write(0, 0, "a");
        wb.add_sheet("second").write(0, 0, "b");
        wb.add_sheet("third").write(0, 0, "c");
        wb.set_active_sheet(1); // "second"

        let bytes = wb.to_xlsx();

        // Structural: activeTab on the workbookView, tabSelected on sheet2 only.
        let wbxml = part(&bytes, "xl/workbook.xml");
        assert!(
            wbxml.contains(r#"<bookViews><workbookView activeTab="1"/></bookViews>"#),
            "activeTab missing: {wbxml}"
        );
        let pr = wbxml.find("<workbookPr").expect("workbookPr present");
        let bv = wbxml.find("<bookViews").expect("bookViews present");
        let sh = wbxml.find("<sheets").expect("sheets present");
        assert!(pr < bv && bv < sh, "order wrong: pr={pr} bv={bv} sh={sh}");
        assert!(
            part(&bytes, "xl/worksheets/sheet2.xml").contains(r#"tabSelected="1""#),
            "sheet2 not tabSelected"
        );
        assert!(
            !part(&bytes, "xl/worksheets/sheet1.xml").contains("tabSelected"),
            "sheet1 should not be tabSelected"
        );

        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nassert wb._active_sheet_index==1, wb._active_sheet_index\nassert wb.active.title=='second', wb.active.title\nprint('ACTIVE_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "ACTIVE_OK");
    }

    #[test]
    fn structure_protection_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        wb.add_sheet("s").write(0, 0, "x");
        wb.protect_structure();
        let bytes = wb.to_xlsx();

        // Structural: workbookProtection emitted after workbookPr, before bookViews.
        let wbxml = part(&bytes, "xl/workbook.xml");
        assert!(
            wbxml.contains(r#"<workbookProtection lockStructure="1"/>"#),
            "workbookProtection missing: {wbxml}"
        );
        let pr = wbxml.find("<workbookPr").expect("workbookPr present");
        let wp = wbxml
            .find("<workbookProtection")
            .expect("workbookProtection present");
        let sh = wbxml.find("<sheets").expect("sheets present");
        assert!(pr < wp && wp < sh, "order wrong: pr={pr} wp={wp} sh={sh}");

        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nassert wb.security is not None and wb.security.lockStructure, wb.security\nprint('WBPROT_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "WBPROT_OK");
    }

    #[test]
    fn comment_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("c");
        s.write(0, 0, "셀");
        s.add_comment(0, 0, "검토 필요", Some("조달청"));
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\nc=ws['A1'].comment\nassert c is not None, 'no comment on A1'\nassert '검토 필요' in c.text, repr(c.text)\nprint('COMMENT_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "COMMENT_OK");
    }

    #[test]
    fn indent_shrink_alignment_opens_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("a");
        s.write_styled(
            0,
            0,
            "들여쓰기",
            &CellStyle::new()
                .align(HAlign::Left)
                .indent(2)
                .shrink_to_fit(),
        );
        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\na=ws['A1'].alignment\nassert int(a.indent)==2, a.indent\nassert a.shrink_to_fit in (True,1), a.shrink_to_fit\nprint('INDENT_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "INDENT_OK");
    }

    #[test]
    fn format_facade_alignment_setters_open_in_openpyxl() {
        let mut wb = Workbook::new();
        let s = wb.add_sheet("fmt");
        let fmt = crate::Format::new()
            .set_text_wrap()
            .set_valign(crate::VAlign::Middle)
            .set_indent(2)
            .set_shrink_to_fit()
            .set_text_rotation(-45);
        s.write_with_format(0, 0, "wrapped rotated", &fmt);

        let bytes = wb.to_xlsx();
        let script = "import sys\nfrom openpyxl import load_workbook\nwb=load_workbook(sys.argv[1])\nws=wb.active\na=ws['A1'].alignment\nassert a.wrap_text in (True,1), a.wrap_text\nassert a.vertical == 'center', a.vertical\nassert int(a.indent) == 2, a.indent\nassert a.shrink_to_fit in (True,1), a.shrink_to_fit\nassert int(a.textRotation) == 135, a.textRotation\nprint('FORMAT_ALIGN_OK')\n";
        assert_opens_in_openpyxl(&bytes, script, "FORMAT_ALIGN_OK");
    }
}
