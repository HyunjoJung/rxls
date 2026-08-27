use std::fs;
use std::sync::atomic::Ordering;

use super::cell_edit::sml_set_cell_value;
use super::save::SAVE_TEMP_COUNTER;
use super::sheet_lifecycle::{
    apply_sheet_reference_rewrites, collect_sheet_reference_rewrites,
    rewrite_deleted_sheet_qualifiers, rewrite_sheet_qualifiers,
};
use super::worksheet_features::{
    data_validation_nodes, data_validation_wrappers, inspect_table_part, sml_set_hyperlink,
    HyperlinkEdit,
};
use super::{worksheet_path, Spreadsheet};
use crate::xmltree::{
    reset_test_fail_commit, reset_test_node_budget, set_test_fail_commit_after,
    set_test_node_budget, XmlTree,
};
use crate::{
    Cell, Color, Comment, DataValidation, DocProperties, DvKind, DvOp, Error, Result, SheetVisible,
    Workbook,
};

/// A minimal worksheet part with exactly one row, one valued cell.
/// Shared by the narrow (`sml_set_cell_value`) and broad
/// (`Spreadsheet::set_cell_value`) node-budget regression tests below, so
/// the pinned budget and the fixture can never drift out of sync.
const MINIMAL_WORKSHEET_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#;
const EMPTY_WORKSHEET_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData></sheetData></worksheet>"#;
const MINIMAL_WORKBOOK_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
const MINIMAL_CONTENT_TYPES_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;
const UNTYPED_WORKSHEET_CONTENT_TYPES_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#;
const UNTYPED_WORKBOOK_CONTENT_TYPES_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;

/// Regression test for a `sml_set_cell_value` bug: it unconditionally
/// removed the cell's existing `<v>`/`<f>`/`<is>` child FIRST, and only
/// afterward performed the fallible write of the new value (`set_attr`
/// and/or `insert_fragment_at`, either of which can fail under node/attr
/// budget pressure). A failure at that point returned `Err` with the old
/// value already gone and the new value never written -- silent data
/// loss reported as "nothing happened."
///
/// This is the narrowest possible reproduction: it calls
/// `sml_set_cell_value` directly on a tree pinned to exactly its current
/// node count (zero room for even one more node), so the value-insert
/// step is guaranteed to fail.
#[test]
fn sml_set_cell_value_leaves_original_value_intact_when_node_budget_write_fails() {
    let mut tree = XmlTree::parse(MINIMAL_WORKSHEET_XML).expect("parse minimal worksheet");
    let budget = tree.node_count();
    let worksheet = tree.root_element().expect("root element");
    let sheet_data = tree
        .child_by_name(worksheet, b"sheetData")
        .expect("sheetData");
    let row = tree.child_by_name(sheet_data, b"row").expect("row");
    let cell = tree.child_by_name(row, b"c").expect("cell");

    set_test_node_budget(budget);
    let result = sml_set_cell_value(&mut tree, cell, &Cell::Number(999.0));
    reset_test_node_budget();

    assert!(
        result.is_err(),
        "overwriting the value must fail under a zero-room node budget"
    );
    let v = tree
        .child_by_name(cell, b"v")
        .expect("the ORIGINAL <v> child must survive a failed write");
    assert_eq!(tree.text_of(v), "1", "original value must be untouched");
    assert!(
        tree.child_by_name(cell, b"f").is_none(),
        "no half-written <f> child should appear"
    );
    assert!(
        tree.child_by_name(cell, b"is").is_none(),
        "no half-written <is> child should appear"
    );
    assert_eq!(
        tree.serialize(),
        XmlTree::parse(MINIMAL_WORKSHEET_XML)
            .expect("re-parse fixture")
            .serialize(),
        "tree must be byte-for-byte unchanged after a failed write"
    );
}

/// Builds a minimal single-sheet `.xlsx` ZIP whose `xl/worksheets/sheet1.xml`
/// is exactly [`MINIMAL_WORKSHEET_XML`], for the broader
/// `Spreadsheet::set_cell_value` end-to-end regression test below.
fn minimal_xlsx_with_worksheet(worksheet_xml: &[u8], content_types_xml: &[u8]) -> Vec<u8> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn add(
        zip: &mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>,
        opt: SimpleFileOptions,
        name: &str,
        bytes: &[u8],
    ) {
        zip.start_file(name, opt).unwrap();
        zip.write_all(bytes).unwrap();
    }

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    add(&mut zip, opt, "[Content_Types].xml", content_types_xml);
    add(
            &mut zip,
            opt,
            "_rels/.rels",
            br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        );
    add(&mut zip, opt, "xl/workbook.xml", MINIMAL_WORKBOOK_XML);
    add(
            &mut zip,
            opt,
            "xl/_rels/workbook.xml.rels",
            br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        );
    add(&mut zip, opt, "xl/worksheets/sheet1.xml", worksheet_xml);
    zip.finish().unwrap().into_inner()
}

fn minimal_xlsx_with_one_valued_cell() -> Vec<u8> {
    minimal_xlsx_with_worksheet(MINIMAL_WORKSHEET_XML, MINIMAL_CONTENT_TYPES_XML)
}

/// Broader end-to-end confirmation through the public API: the same
/// budget-failure scenario, driven through `Spreadsheet::set_cell_value`,
/// must report `Err` while leaving the cell's original value intact and
/// `edited_parts()` empty (the `?` in `set_cell_value` must short-circuit
/// before the edited-parts bookkeeping runs).
#[test]
fn set_cell_value_leaves_cell_untouched_when_node_budget_write_fails() {
    let input = minimal_xlsx_with_one_valued_cell();
    let budget = XmlTree::parse(MINIMAL_WORKSHEET_XML)
        .expect("parse fixture")
        .node_count();

    set_test_node_budget(budget);
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let result = spreadsheet.set_cell_value("Data", 0, 0, Cell::Number(999.0));
    reset_test_node_budget();

    assert!(
        result.is_err(),
        "write must fail under a zero-room node budget"
    );
    assert!(
        spreadsheet.edited_parts().is_empty(),
        "a failed edit must not be recorded as an edited part"
    );

    let saved = spreadsheet.save().expect("save must still succeed");
    let reopened = Workbook::open(&saved).expect("reopen saved package");
    assert_eq!(
        reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Number(1.0)),
        "original cell value must survive a failed edit"
    );
}

#[test]
fn set_cell_value_rolls_back_created_nodes_when_value_write_fails() {
    let input = minimal_xlsx_with_worksheet(EMPTY_WORKSHEET_XML, MINIMAL_CONTENT_TYPES_XML);
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable empty xlsx");
    let before = spreadsheet.save().expect("serialize original package");
    let budget = XmlTree::parse(EMPTY_WORKSHEET_XML)
        .expect("parse empty worksheet")
        .node_count()
        + 2;

    // Leave room for the new row and cell nodes, but not the value node.
    // The in-place edit therefore fails only after it has created both
    // containers; the public operation must discard that candidate.
    set_test_node_budget(budget);
    let result = spreadsheet.set_cell_value("Data", 0, 0, Cell::Number(999.0));
    reset_test_node_budget();

    assert!(
        result.is_err(),
        "the value write must exceed the node budget"
    );
    assert_rejected_edit_is_unchanged(&spreadsheet, &before);
}

#[test]
fn append_row_rolls_back_earlier_cells_when_later_cell_fails() {
    let input = minimal_xlsx_with_one_valued_cell();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let before = spreadsheet.save().expect("serialize original package");
    let budget = XmlTree::parse(MINIMAL_WORKSHEET_XML)
        .expect("parse populated worksheet")
        .node_count()
        + 4;

    // The budget admits the appended row, its first complete cell, and
    // the second cell container. Writing the second value then fails,
    // exercising rollback after a genuinely partial in-place append.
    set_test_node_budget(budget);
    let result = spreadsheet.append_row("Data", [Cell::Number(2.0), Cell::Number(3.0)]);
    reset_test_node_budget();

    assert!(
        result.is_err(),
        "the second value must exceed the node budget"
    );
    assert_rejected_edit_is_unchanged(&spreadsheet, &before);
}

#[test]
fn clear_range_rolls_back_when_candidate_package_validation_fails() {
    // This source package is readable and metadata parses losslessly, but
    // the worksheet has no content type. An untouched passthrough save is
    // permitted; once the worksheet is edited, final package validation
    // must reject it. The public clear must not retain that failed edit.
    let input =
        minimal_xlsx_with_worksheet(MINIMAL_WORKSHEET_XML, UNTYPED_WORKSHEET_CONTENT_TYPES_XML);
    let mut spreadsheet = Spreadsheet::open(&input).expect("open readable xlsx");
    let before = spreadsheet.save().expect("serialize untouched package");

    let result = spreadsheet.clear_range("Data", 0, 0, 0, 0);

    assert!(
        result.is_err(),
        "edited untyped worksheet must fail validation"
    );
    assert_rejected_edit_is_unchanged(&spreadsheet, &before);
}

#[test]
fn set_active_sheet_rolls_back_book_views_created_before_late_failure() {
    let input = minimal_xlsx_with_one_valued_cell();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let before = spreadsheet.save().expect("serialize original package");
    let budget = XmlTree::parse(MINIMAL_WORKBOOK_XML)
        .expect("parse minimal workbook")
        .node_count()
        + 1;

    // The missing <bookViews> fits, but the nested <workbookView> does
    // not. The public operation must discard the candidate containing
    // that first insertion when the second insertion is rejected.
    set_test_node_budget(budget);
    let result = spreadsheet.set_active_sheet("Data");
    reset_test_node_budget();

    assert!(
        result.is_err(),
        "the nested workbook view must exceed the node budget"
    );
    assert_rejected_edit_is_unchanged(&spreadsheet, &before);
}

#[test]
fn metadata_mutators_roll_back_final_package_validation_failures() {
    fn assert_rolls_back(content_types: &[u8], edit: impl FnOnce(&mut Spreadsheet) -> Result<()>) {
        let input = minimal_xlsx_with_worksheet(MINIMAL_WORKSHEET_XML, content_types);
        let mut spreadsheet = Spreadsheet::open(&input).expect("open readable xlsx");
        let before = spreadsheet.save().expect("serialize untouched package");

        assert!(
            edit(&mut spreadsheet).is_err(),
            "the touched untyped part must fail final validation"
        );
        assert_rejected_edit_is_unchanged(&spreadsheet, &before);
    }

    assert_rolls_back(UNTYPED_WORKBOOK_CONTENT_TYPES_XML, |spreadsheet| {
        spreadsheet.set_defined_name("Rate", "Data!$A$1")
    });
    assert_rolls_back(UNTYPED_WORKBOOK_CONTENT_TYPES_XML, |spreadsheet| {
        spreadsheet.set_sheet_visibility("Data", SheetVisible::Visible)
    });
    assert_rolls_back(UNTYPED_WORKSHEET_CONTENT_TYPES_XML, |spreadsheet| {
        spreadsheet.set_sheet_tab_color("Data", Color::rgb(0x12, 0x34, 0x56))
    });
}

fn assert_rejected_edit_is_unchanged(spreadsheet: &Spreadsheet, before: &[u8]) {
    assert!(
        spreadsheet.edited_parts().is_empty(),
        "a rejected edit must not record an edited package part"
    );
    assert_eq!(
        spreadsheet.save().expect("serialize rejected edit"),
        before,
        "a rejected edit must preserve the exact package bytes"
    );
}

#[test]
fn cell_input_validation_rejects_without_mutating_the_package() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "original");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let before = spreadsheet.save().expect("serialize original package");
    let nested_formula = Cell::Formula {
        formula: "INNER()".to_string(),
        cached: Box::new(Cell::Number(1.0)),
    };
    let invalid_values = vec![
        Cell::Number(f64::NAN),
        Cell::Date(f64::INFINITY),
        Cell::Text("illegal\u{1}text".to_string()),
        Cell::Error("#BAD\u{1}!".to_string()),
        Cell::Text("😀".repeat(16_384)),
        Cell::Formula {
            formula: "SUM(\u{1})".to_string(),
            cached: Box::new(Cell::Number(1.0)),
        },
        Cell::Formula {
            formula: "TEXT()".to_string(),
            cached: Box::new(Cell::Text("illegal\u{1}cache".to_string())),
        },
        Cell::Formula {
            formula: "ERROR()".to_string(),
            cached: Box::new(Cell::Error("#BAD\u{1}!".to_string())),
        },
        Cell::Formula {
            formula: "LONG()".to_string(),
            cached: Box::new(Cell::Text("x".repeat(32_768))),
        },
        Cell::Formula {
            formula: "OUTER()".to_string(),
            cached: Box::new(nested_formula),
        },
    ];

    for value in invalid_values {
        assert!(spreadsheet.set_cell_value("Data", 0, 0, value).is_err());
        assert_rejected_edit_is_unchanged(&spreadsheet, &before);
    }

    assert!(spreadsheet
        .set_cell_formula("Data", 0, 0, "=SUM(\u{1})", Cell::Number(1.0))
        .is_err());
    assert_rejected_edit_is_unchanged(&spreadsheet, &before);

    assert!(spreadsheet
        .append_row(
            "Data",
            [
                Cell::Text("would be partial".into()),
                Cell::Number(f64::NAN)
            ],
        )
        .is_err());
    assert_rejected_edit_is_unchanged(&spreadsheet, &before);
}

#[test]
fn defined_name_validation_rejects_invalid_or_colliding_names_without_mutation() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, 1.0);
    workbook.define_name("Rate", "Data!$A$1");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let before = spreadsheet.save().expect("serialize original package");

    for name in [
        "",
        "A1",
        "R1C1",
        "2024",
        "bad name",
        "bad-name",
        "_xlnm.Print_Area",
        "_XLNM.Print_Area",
    ] {
        assert!(spreadsheet.set_defined_name(name, "Data!$A$1").is_err());
        assert_rejected_edit_is_unchanged(&spreadsheet, &before);
    }

    assert!(spreadsheet.set_defined_name("rate", "Data!$A$2").is_err());
    assert_rejected_edit_is_unchanged(&spreadsheet, &before);

    assert!(spreadsheet
        .set_defined_name("ValidName", "Data!$A$1\u{1}")
        .is_err());
    assert_rejected_edit_is_unchanged(&spreadsheet, &before);
}

#[test]
fn document_property_validation_rejects_without_removing_existing_timestamps() {
    let original_timestamp = "2024-01-01T00:00:00Z";
    let mut workbook = Workbook::new();
    workbook.set_properties(
        DocProperties::new()
            .with_title("Original title")
            .with_company("Original company")
            .with_created(original_timestamp),
    );
    workbook.add_sheet("Data").write(0, 0, "value");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let before = spreadsheet.save().expect("serialize original package");
    let invalid_xml = "illegal\u{1}property";
    let invalid_properties = vec![
        DocProperties::new().with_title(invalid_xml),
        DocProperties::new().with_subject(invalid_xml),
        DocProperties::new().with_creator(invalid_xml),
        DocProperties::new().with_keywords(invalid_xml),
        DocProperties::new().with_description(invalid_xml),
        DocProperties::new().with_last_modified_by(invalid_xml),
        DocProperties::new().with_company(invalid_xml),
        DocProperties::new().with_created(invalid_xml),
    ];

    for properties in invalid_properties {
        assert!(spreadsheet.set_document_properties(properties).is_err());
        assert_rejected_edit_is_unchanged(&spreadsheet, &before);
    }

    assert!(spreadsheet
        .set_document_properties(
            DocProperties::new()
                .with_title("Candidate title")
                .with_created("2024-02-31T00:00:00Z"),
        )
        .is_err());
    assert_rejected_edit_is_unchanged(&spreadsheet, &before);

    let reopened = Workbook::open(&before).expect("reopen original package");
    assert_eq!(
        reopened.properties.created.as_deref(),
        Some(original_timestamp),
        "an invalid timestamp edit must not remove the existing timestamp"
    );
}

#[test]
fn transaction_rolls_back_every_edit_when_the_closure_fails() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "original");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let before = spreadsheet.save().expect("serialize original package");
    let before_parts = spreadsheet.edited_parts().to_vec();

    let result: Result<()> = spreadsheet.transaction(|draft| {
        draft.set_cell_value("Data", 0, 0, Cell::Text("candidate".into()))?;
        draft.set_sheet_tab_color("Data", Color::rgb(0x12, 0x34, 0x56))?;
        Err(Error::Zip("abort test transaction"))
    });

    assert!(matches!(result, Err(Error::Zip("abort test transaction"))));
    assert_eq!(spreadsheet.edited_parts(), before_parts);
    assert_eq!(
        spreadsheet.save().expect("serialize rolled-back package"),
        before,
        "a failed transaction must preserve the exact pre-transaction package bytes"
    );

    let reopened = Workbook::open(&before).expect("reopen original package");
    let sheet = reopened.sheet_by_name("Data").expect("Data sheet");
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("original".into())));
    assert_eq!(sheet.tab_color(), None);
}

#[test]
fn transaction_commits_a_successful_batch_and_returns_its_value() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "original");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    let value = spreadsheet
        .transaction(|draft| {
            draft.set_cell_value("Data", 0, 0, Cell::Text("committed".into()))?;
            draft.set_defined_name("Answer", "Data!$A$1")?;
            Ok(42_u8)
        })
        .expect("commit transaction");

    assert_eq!(value, 42);
    assert_eq!(
        spreadsheet.edited_parts(),
        &["xl/workbook.xml", "xl/worksheets/sheet1.xml"]
    );
    let saved = spreadsheet.save().expect("save committed package");
    let reopened = Workbook::open(&saved).expect("reopen committed package");
    assert_eq!(
        reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Text("committed".into()))
    );
    assert_eq!(
        reopened.defined_names(),
        &[("Answer".to_string(), "Data!$A$1".to_string())]
    );
}

#[test]
fn sheet_qualifier_rewriter_handles_quotes_3d_strings_and_external_books() {
    let formula =
        r#"Old!A1+'Old'!B2+'Old:Other'!C3+"Old!D4"+'[Book.xlsx]Old'!E5+[Book.xlsx]Old!F6"#;
    assert_eq!(
        rewrite_sheet_qualifiers(formula, "Old", "New Data"),
        r#"'New Data'!A1+'New Data'!B2+'New Data:Other'!C3+"Old!D4"+'[Book.xlsx]Old'!E5+[Book.xlsx]Old!F6"#
    );
    assert_eq!(
        rewrite_sheet_qualifiers("'O''Brien'!A1", "O'Brien", "Renamed"),
        "'Renamed'!A1"
    );
    assert_eq!(
        rewrite_deleted_sheet_qualifiers(formula, "Old"),
        r#"#REF!A1+#REF!B2+#REF!C3+"Old!D4"+'[Book.xlsx]Old'!E5+[Book.xlsx]Old!F6"#
    );

    let mut tree = XmlTree::parse(
        br#"<root><hyperlink location="'Old'!A1"/><worksheetSource sheet="Old"/></root>"#,
    )
    .expect("parse reference attributes");
    let rewrites = collect_sheet_reference_rewrites(&tree, "Old", "New Data");
    assert_eq!(rewrites.len(), 2);
    apply_sheet_reference_rewrites(&mut tree, &rewrites).expect("rewrite attributes");
    let xml = String::from_utf8(tree.serialize()).expect("serialized XML");
    assert!(xml.contains(r#"location="'New Data'!A1""#));
    assert!(xml.contains(r#"sheet="New Data""#));
}

fn zip_member(bytes: &[u8], name: &str) -> Vec<u8> {
    use std::io::Read;

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("open zip");
    let mut part = zip.by_name(name).expect("zip member");
    let mut out = Vec::new();
    part.read_to_end(&mut out).expect("read zip member");
    out
}

fn replace_zip_member(bytes: &[u8], name: &str, replacement: &[u8]) -> Vec<u8> {
    use std::io::{Read, Write};
    use zip::write::SimpleFileOptions;

    let mut input = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("open input zip");
    let mut output = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for index in 0..input.len() {
        let mut part = input.by_index(index).expect("read input zip member");
        let part_name = part.name().to_string();
        output
            .start_file(&part_name, SimpleFileOptions::default())
            .expect("start replacement zip member");
        if part_name == name {
            output
                .write_all(replacement)
                .expect("write replacement zip member");
        } else {
            let mut contents = Vec::new();
            part.read_to_end(&mut contents)
                .expect("read original zip member");
            output
                .write_all(&contents)
                .expect("copy original zip member");
        }
    }
    output
        .finish()
        .expect("finish replacement zip")
        .into_inner()
}

#[test]
fn edits_xlsx_with_above_root_office_document_target() {
    let input = replace_zip_member(
            &minimal_xlsx_with_one_valued_cell(),
            "_rels/.rels",
            br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="../../xl/workbook.xml"/></Relationships>"#,
        );

    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    spreadsheet
        .set_cell_value("Data", 0, 0, Cell::Number(2.0))
        .expect("edit through normalized workbook target");
    let saved = spreadsheet.save().expect("save edited package");
    let reopened = Workbook::open(&saved).expect("reopen saved package");
    assert_eq!(
        reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Number(2.0))
    );
}

#[test]
fn workbook_edit_selection_is_exact_ordered_and_backslash_compatible() {
    let base = minimal_xlsx_with_one_valued_cell();
    let custom = replace_zip_member(
            &base,
            "_rels/.rels",
            br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="custom" Type="https://attacker.invalid/officeDocument" Target="evil/workbook.xml"/><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        );
    let mut spreadsheet = Spreadsheet::open(&custom).expect("open exact root relationship");
    spreadsheet
        .set_cell_value("Data", 0, 0, Cell::Number(2.0))
        .expect("custom suffix relationship must not affect dispatch");

    let backslash = replace_zip_member(
            &base,
            "_rels/.rels",
            br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl\workbook.xml"/></Relationships>"#,
        );
    let mut spreadsheet = Spreadsheet::open(&backslash).expect("open backslash root target");
    spreadsheet
        .set_cell_value("Data", 0, 0, Cell::Number(3.0))
        .expect("edit through backslash root target");
    let reopened = Workbook::open(&spreadsheet.save().unwrap()).unwrap();
    assert_eq!(
        reopened
            .sheet_by_name("Data")
            .and_then(|sheet| sheet.cell(0, 0)),
        Some(&Cell::Number(3.0))
    );
}

#[test]
fn cell_mutation_requires_the_exact_worksheet_relationship_type() {
    for relationship_type in [
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet",
        "https://attacker.invalid/relationships/worksheet",
    ] {
        let relationships = format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{relationship_type}" Target="worksheets/sheet1.xml"/></Relationships>"#
        );
        let input = replace_zip_member(
            &minimal_xlsx_with_one_valued_cell(),
            "xl/_rels/workbook.xml.rels",
            relationships.as_bytes(),
        );
        let mut spreadsheet = Spreadsheet::open(&input).expect("open non-worksheet fixture");
        assert!(spreadsheet
            .set_cell_value("Data", 0, 0, Cell::Number(9.0))
            .is_err());
        assert!(spreadsheet.edited_parts().is_empty());
    }
}

#[test]
fn unrelated_hyperlink_edit_preserves_an_internal_fragment_relationship() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    sheet.write(0, 0, "fragment");
    sheet.write(0, 1, "external");
    let mut seed = Spreadsheet::open(&workbook.to_xlsx()).unwrap();
    seed.set_external_hyperlink("Data", 0, 0, "https://example.invalid")
        .unwrap();
    let with_relationship = seed.save().unwrap();
    let rels = String::from_utf8(zip_member(
        &with_relationship,
        "xl/worksheets/_rels/sheet1.xml.rels",
    ))
    .unwrap()
    .replace("https://example.invalid", "#Sheet2!A1")
    .replace(r#" TargetMode="External""#, "");
    let fragment_fixture = replace_zip_member(
        &with_relationship,
        "xl/worksheets/_rels/sheet1.xml.rels",
        rels.as_bytes(),
    );

    let mut spreadsheet = Spreadsheet::open(&fragment_fixture).unwrap();
    spreadsheet
        .set_external_hyperlink("Data", 0, 1, "https://example.com/new")
        .unwrap();
    let saved = spreadsheet
        .save()
        .expect("fragment target resolves to source part");
    let saved_rels =
        String::from_utf8(zip_member(&saved, "xl/worksheets/_rels/sheet1.xml.rels")).unwrap();
    assert!(saved_rels.contains(r##"Target="#Sheet2!A1""##));
    assert!(saved_rels.contains("https://example.com/new"));
}

fn zip_has_member(bytes: &[u8], name: &str) -> bool {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("open zip");
    let exists = zip.by_name(name).is_ok();
    exists
}

fn sheet_delete_dependency_fixture() -> Vec<u8> {
    use crate::Table;

    let mut workbook = Workbook::new();
    {
        let deleted = workbook.add_sheet("Delete");
        deleted.write(0, 0, "Value");
        deleted.write(1, 0, 2.0);
        deleted.add_comment(1, 0, "remove me", Some("author"));
        deleted.add_table(Table {
            range: (0, 0, 1, 0),
            name: "DeletedTable".into(),
            columns: vec!["Value".into()],
            style: None,
        });
    }
    workbook
        .add_sheet("Keep")
        .write_formula(0, 0, "Delete!A2+1", 3.0);
    workbook.define_name("DeletedGlobal", "Delete!$A$2");
    workbook.define_name("SafeGlobal", "Keep!$A$1");
    workbook.define_local_name("Delete", "DeletedLocal", "Delete!$A$2");
    workbook.define_local_name("Keep", "CrossLocal", "Delete!$A$2");
    workbook.define_local_name("Keep", "SafeLocal", "Keep!$A$1");

    let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open delete fixture");
    let package = seed.package.as_mut().expect("editable package");
    package
            .replace_part(
                "docProps/app.xml",
                br#"<?xml version="1.0" encoding="UTF-8"?><Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes" custom="preserve"><HeadingPairs keep="yes"><vt:vector size="4" baseType="variant"><vt:variant><vt:lpstr>Worksheets</vt:lpstr></vt:variant><vt:variant><vt:i4>2</vt:i4></vt:variant><vt:variant><vt:lpstr>Named Ranges</vt:lpstr></vt:variant><vt:variant><vt:i4>1</vt:i4></vt:variant></vt:vector></HeadingPairs><TitlesOfParts keep="yes"><vt:vector size="3" baseType="lpstr"><vt:lpstr>Delete</vt:lpstr><vt:lpstr>Keep</vt:lpstr><vt:lpstr>Unrelated title</vt:lpstr></vt:vector></TitlesOfParts><Extension keep="untouched"/></Properties>"#.to_vec(),
            )
            .expect("replace app metadata");
    package.set_part(
        "xl/custom/keep.bin",
        b"unknown worksheet extension payload".to_vec(),
        Some("application/octet-stream"),
    );
    package
        .add_relationship(
            "xl/worksheets/sheet1.xml",
            "http://example.com/relationships/customWorksheetExtension",
            "../custom/keep.bin",
            false,
        )
        .unwrap();
    seed.save().expect("save delete dependency fixture")
}

#[test]
fn add_sheet_wires_a_deterministic_part_and_preserves_existing_parts() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "original");
    let input = workbook.to_xlsx();
    let original_sheet = zip_member(&input, "xl/worksheets/sheet1.xml");
    let original_styles = zip_member(&input, "xl/styles.xml");
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet.add_sheet("Added").expect("add sheet");

    assert_eq!(
        spreadsheet.edited_parts(),
        &[
            "[Content_Types].xml",
            "xl/_rels/workbook.xml.rels",
            "xl/workbook.xml",
            "xl/worksheets/sheet2.xml",
        ]
    );
    let saved = spreadsheet.save().expect("save added sheet");
    assert!(zip_has_member(&saved, "xl/worksheets/sheet2.xml"));
    assert_eq!(
        zip_member(&saved, "xl/worksheets/sheet1.xml"),
        original_sheet
    );
    assert_eq!(zip_member(&saved, "xl/styles.xml"), original_styles);
    let rels =
        String::from_utf8(zip_member(&saved, "xl/_rels/workbook.xml.rels")).expect("UTF-8 rels");
    assert!(rels.contains(r#"Id="rId4""#));
    assert!(rels.contains(r#"Target="worksheets/sheet2.xml""#));

    let reopened = Workbook::open(&saved).expect("reopen added sheet");
    assert_eq!(reopened.sheet_names(), vec!["Data", "Added"]);
    assert_eq!(reopened.active_sheet_name(), Some("Data"));
}

#[test]
fn delete_active_sheet_repairs_local_names_and_preserves_surviving_parts() {
    use crate::PageSetup;

    let mut workbook = Workbook::new();
    workbook.add_sheet("First").write(0, 0, "first");
    workbook.add_sheet("Middle").write(0, 0, "middle");
    workbook
        .add_sheet("Last")
        .set_page_setup(PageSetup::new().with_print_area((0, 0, 1, 1)));
    workbook.set_active_sheet(1);
    let input = workbook.to_xlsx();
    let original_first = zip_member(&input, "xl/worksheets/sheet1.xml");
    let original_last = zip_member(&input, "xl/worksheets/sheet3.xml");
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .delete_sheet("Middle")
        .expect("delete active sheet");

    assert_eq!(
        spreadsheet.edited_parts(),
        &[
            "[Content_Types].xml",
            "xl/_rels/workbook.xml.rels",
            "xl/workbook.xml",
            "xl/worksheets/sheet2.xml",
        ]
    );
    let saved = spreadsheet.save().expect("save deleted sheet");
    assert!(!zip_has_member(&saved, "xl/worksheets/sheet2.xml"));
    assert_eq!(
        zip_member(&saved, "xl/worksheets/sheet1.xml"),
        original_first
    );
    assert_eq!(
        zip_member(&saved, "xl/worksheets/sheet3.xml"),
        original_last
    );

    let reopened = Workbook::open(&saved).expect("reopen deleted sheet");
    assert_eq!(reopened.sheet_names(), vec!["First", "Last"]);
    assert_eq!(reopened.active_sheet_name(), Some("Last"));
    assert_eq!(
        reopened
            .sheet_by_name("Last")
            .and_then(|sheet| sheet.page_setup())
            .and_then(|setup| setup.print_area),
        Some((0, 0, 1, 1))
    );
}

#[test]
fn delete_sheet_repairs_references_app_titles_and_owned_orphans() {
    let input = sheet_delete_dependency_fixture();
    let original_styles = zip_member(&input, "xl/styles.xml");
    let unknown = zip_member(&input, "xl/custom/keep.bin");
    let mut spreadsheet = Spreadsheet::open(&input).expect("open dependency fixture");

    spreadsheet
        .delete_sheet("Delete")
        .expect("delete sheet with repairable dependencies");
    let saved = spreadsheet.save().expect("save repaired deletion");

    for removed in [
        "xl/worksheets/sheet1.xml",
        "xl/worksheets/_rels/sheet1.xml.rels",
        "xl/comments1.xml",
        "xl/drawings/vmlDrawing1.vml",
        "xl/tables/table1.xml",
    ] {
        assert!(
            !zip_has_member(&saved, removed),
            "orphan survived: {removed}"
        );
    }
    assert_eq!(zip_member(&saved, "xl/custom/keep.bin"), unknown);
    assert_eq!(zip_member(&saved, "xl/styles.xml"), original_styles);

    let workbook_xml =
        String::from_utf8(zip_member(&saved, "xl/workbook.xml")).expect("workbook UTF-8");
    assert!(workbook_xml.contains("#REF!$A$2"));
    assert!(!workbook_xml.contains("DeletedLocal"));
    assert!(workbook_xml.contains(r#"name="CrossLocal" localSheetId="0">#REF!$A$2"#));
    assert!(workbook_xml.contains(r#"name="SafeLocal" localSheetId="0">Keep!$A$1"#));
    let keep_sheet =
        String::from_utf8(zip_member(&saved, "xl/worksheets/sheet2.xml")).expect("worksheet UTF-8");
    assert!(keep_sheet.contains("<f>#REF!A2+1</f>"));

    let app =
        String::from_utf8(zip_member(&saved, "docProps/app.xml")).expect("app properties UTF-8");
    assert!(app.contains(r#"custom="preserve""#));
    assert!(app.contains(r#"keep="untouched""#));
    assert!(app.contains(r#"<vt:i4>1</vt:i4>"#));
    assert!(app.contains(r#"<vt:vector size="2" baseType="lpstr">"#));
    assert!(!app.contains("<vt:lpstr>Delete</vt:lpstr>"));
    assert!(app.contains("<vt:lpstr>Keep</vt:lpstr>"));
    assert!(app.contains("<vt:lpstr>Unrelated title</vt:lpstr>"));

    let reopened = Workbook::open(&saved).expect("reopen repaired deletion");
    assert_eq!(reopened.sheet_names(), vec!["Keep"]);
    assert_eq!(
        reopened
            .sheet_by_name("Keep")
            .and_then(|sheet| sheet.cell(0, 0)),
        Some(&Cell::Formula {
            formula: "#REF!A2+1".into(),
            cached: Box::new(Cell::Number(3.0)),
        })
    );
    assert!(reopened
        .defined_names()
        .iter()
        .any(|(name, value)| name == "DeletedGlobal" && value == "#REF!$A$2"));
}

#[test]
fn delete_sheet_dependency_repair_rolls_back_after_an_earlier_rewrite() {
    let input = sheet_delete_dependency_fixture();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open rollback fixture");
    let before = spreadsheet.save().expect("serialize rollback fixture");

    set_test_fail_commit_after(1);
    let result = spreadsheet.delete_sheet("Delete");
    reset_test_fail_commit();

    assert!(result.is_err(), "injected later tree edit must fail");
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        spreadsheet.save().expect("save rolled-back fixture"),
        before
    );
}

#[test]
fn delete_sheet_rejects_ambiguous_and_unsafe_dependency_graphs() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("First");
    workbook.add_sheet("Second");
    let seed = workbook.to_xlsx();
    let relationships = String::from_utf8(zip_member(&seed, "xl/_rels/workbook.xml.rels"))
        .expect("workbook relationships UTF-8");
    let relationships = relationships.replacen(
            "</Relationships>",
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            1,
        );
    let ambiguous = replace_zip_member(
        &seed,
        "xl/_rels/workbook.xml.rels",
        relationships.as_bytes(),
    );
    assert!(
        Spreadsheet::open(&ambiguous).is_err(),
        "a duplicate relationship ID must fail closed before mutation"
    );

    let mut workbook = Workbook::new();
    workbook.add_sheet("First");
    workbook.add_sheet("Second");
    let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open pivot fixture");
    let package = seed.package.as_mut().expect("editable package");
    package.set_part(
        "xl/pivotTables/pivotTable1.xml",
        b"<pivotTableDefinition/>".to_vec(),
        Some("application/vnd.openxmlformats-officedocument.spreadsheetml.pivotTable+xml"),
    );
    package
        .add_relationship(
            "xl/worksheets/sheet1.xml",
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotTable",
            "../pivotTables/pivotTable1.xml",
            false,
        )
        .unwrap();
    let unsafe_graph = seed.save().expect("save pivot dependency fixture");
    let mut spreadsheet = Spreadsheet::open(&unsafe_graph).expect("reopen pivot fixture");
    let before = spreadsheet.save().expect("serialize pivot fixture");
    assert!(spreadsheet.delete_sheet("First").is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        spreadsheet.save().expect("save rejected pivot deletion"),
        before
    );
}

#[test]
fn add_delete_rejections_and_late_delete_failure_roll_back_exactly() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data");
    let mut spreadsheet = Spreadsheet::open(&workbook.to_xlsx()).expect("open one sheet");
    let before = spreadsheet.save().expect("save one sheet");
    assert!(spreadsheet.add_sheet("data").is_err());
    assert!(spreadsheet.delete_sheet("Data").is_err());
    assert_eq!(spreadsheet.save().expect("save rejected edits"), before);
    assert!(spreadsheet.edited_parts().is_empty());

    let mut workbook = Workbook::new();
    workbook.add_sheet("First");
    workbook.add_sheet("Second");
    let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open two sheets");
    let package = seed.package.as_mut().expect("editable package");
    let tree = package
        .part_tree_mut("xl/workbook.xml")
        .expect("promote workbook");
    let root = tree.root_element().expect("workbook root");
    let views = tree.child_by_name(root, b"bookViews").expect("book views");
    tree.remove_child(root, views).expect("remove book views");
    let input = seed.save().expect("save workbook without book views");
    let mut spreadsheet = Spreadsheet::open(&input).expect("reopen custom workbook");
    let before = spreadsheet.save().expect("serialize custom workbook");

    set_test_fail_commit_after(0);
    let result = spreadsheet.delete_sheet("First");
    reset_test_fail_commit();

    assert!(result.is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(spreadsheet.save().expect("save rolled back delete"), before);
    assert_eq!(
        Workbook::open(&before)
            .expect("reopen original")
            .sheet_names(),
        vec!["First", "Second"]
    );
}

#[test]
fn merge_and_common_layout_edits_round_trip_and_clear() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "anchor");
    let input = workbook.to_xlsx();
    let original_styles = zip_member(&input, "xl/styles.xml");
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .merge_cells("Data", 0, 0, 1, 1)
        .expect("merge cells");
    spreadsheet
        .set_row_height("Data", 2, 24.5)
        .expect("set row height");
    spreadsheet
        .set_row_hidden("Data", 2, true)
        .expect("hide row");
    spreadsheet
        .set_column_width("Data", 2, 18.25)
        .expect("set column width");
    spreadsheet
        .set_column_hidden("Data", 2, true)
        .expect("hide column");
    spreadsheet
        .set_freeze_panes("Data", 1, 2)
        .expect("freeze panes");
    spreadsheet
        .set_print_area("Data", Some((0, 0, 9, 3)))
        .expect("set print area");

    assert_eq!(
        spreadsheet.edited_parts(),
        &["xl/workbook.xml", "xl/worksheets/sheet1.xml"]
    );
    let saved = spreadsheet.save().expect("save layout edits");
    assert_eq!(zip_member(&saved, "xl/styles.xml"), original_styles);
    let reopened = Workbook::open(&saved).expect("reopen layout edits");
    let sheet = reopened.sheet_by_name("Data").expect("Data sheet");
    assert_eq!(sheet.merged_ranges(), &[(0, 0, 1, 1)]);
    assert_eq!(sheet.row_heights().get(&2), Some(&24.5));
    assert!(sheet.hidden_rows().contains(&2));
    assert_eq!(sheet.column_widths().get(&2), Some(&18.25));
    assert!(sheet.hidden_columns().contains(&2));
    assert_eq!(sheet.sheet_view().freeze, Some((1, 2)));
    assert_eq!(
        sheet.page_setup().and_then(|setup| setup.print_area),
        Some((0, 0, 9, 3))
    );

    spreadsheet
        .unmerge_cells("Data", 0, 0, 1, 1)
        .expect("unmerge cells");
    spreadsheet
        .set_row_hidden("Data", 2, false)
        .expect("unhide row");
    spreadsheet
        .set_column_hidden("Data", 2, false)
        .expect("unhide column");
    spreadsheet
        .clear_freeze_panes("Data")
        .expect("clear freeze panes");
    spreadsheet
        .set_print_area("Data", None)
        .expect("clear print area");
    let cleared = Workbook::open(&spreadsheet.save().expect("save cleared layout"))
        .expect("reopen cleared layout");
    let sheet = cleared.sheet_by_name("Data").expect("Data sheet");
    assert!(sheet.merged_ranges().is_empty());
    assert!(!sheet.hidden_rows().contains(&2));
    assert!(!sheet.hidden_columns().contains(&2));
    assert_eq!(sheet.sheet_view().freeze, None);
    assert_eq!(sheet.page_setup().and_then(|setup| setup.print_area), None);
}

#[test]
fn column_range_split_preserves_neighbor_layout_attributes() {
    let input = minimal_xlsx_with_one_valued_cell();
    let mut seed = Spreadsheet::open(&input).expect("open minimal xlsx");
    seed.package
            .as_mut()
            .expect("editable package")
            .replace_part(
                "xl/worksheets/sheet1.xml",
                br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><cols><col min="1" max="3" width="12" customWidth="1" hidden="1" outlineLevel="2" bestFit="1"/></cols><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#.to_vec(),
            )
            .expect("replace worksheet fixture");
    let custom = seed.save().expect("serialize custom fixture");
    let mut spreadsheet = Spreadsheet::open(&custom).expect("reopen custom fixture");

    spreadsheet
        .set_column_hidden("Data", 1, false)
        .expect("unhide middle column");
    spreadsheet
        .set_column_width("Data", 1, 20.0)
        .expect("resize middle column");

    let saved = spreadsheet.save().expect("save split columns");
    let xml =
        String::from_utf8(zip_member(&saved, "xl/worksheets/sheet1.xml")).expect("worksheet UTF-8");
    assert!(xml.contains(
        r#"min="1" max="1" width="12" customWidth="1" hidden="1" outlineLevel="2" bestFit="1""#
    ));
    assert!(
        xml.contains(r#"min="2" max="2" width="20" customWidth="1" outlineLevel="2" bestFit="1""#)
    );
    assert!(xml.contains(
        r#"min="3" max="3" width="12" customWidth="1" hidden="1" outlineLevel="2" bestFit="1""#
    ));
    let reopened = Workbook::open(&saved).expect("reopen split columns");
    let sheet = reopened.sheet_by_name("Data").expect("Data sheet");
    assert_eq!(sheet.column_widths().get(&0), Some(&12.0));
    assert_eq!(sheet.column_widths().get(&1), Some(&20.0));
    assert_eq!(sheet.column_widths().get(&2), Some(&12.0));
    assert!(sheet.hidden_columns().contains(&0));
    assert!(!sheet.hidden_columns().contains(&1));
    assert!(sheet.hidden_columns().contains(&2));
    assert_eq!(sheet.col_outline_levels().get(&1), Some(&2));
}

#[test]
fn merge_overlap_validation_and_late_failure_roll_back_exactly() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").merge(0, 0, 1, 1);
    let mut spreadsheet = Spreadsheet::open(&workbook.to_xlsx()).expect("open merged xlsx");
    let before = spreadsheet.save().expect("serialize original merge");

    assert!(spreadsheet.merge_cells("Data", 1, 1, 2, 2).is_err());
    assert!(spreadsheet.unmerge_cells("Data", 3, 3, 4, 4).is_err());
    assert!(spreadsheet.set_row_height("Data", 0, f32::NAN).is_err());
    assert!(spreadsheet
        .set_column_width("Data", u16::MAX, 10.0)
        .is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(spreadsheet.save().expect("save rejected edits"), before);

    set_test_fail_commit_after(0);
    let result = spreadsheet.merge_cells("Data", 0, 2, 0, 3);
    reset_test_fail_commit();
    assert!(result.is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(spreadsheet.save().expect("save rolled back merge"), before);
}

#[test]
fn save_to_path_atomically_replaces_and_cleans_failed_temporary_files() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "persisted");
    let spreadsheet = Spreadsheet::open(&workbook.to_xlsx()).expect("open editable xlsx");
    let unique = format!(
        "rxls-save-test-{}-{}",
        std::process::id(),
        SAVE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let root = std::env::temp_dir().join(unique);
    fs::create_dir(&root).expect("create test directory");
    let destination = root.join("book.xlsx");
    fs::write(&destination, b"old destination").expect("write old destination");

    spreadsheet
        .save_to_path(&destination)
        .expect("atomic save succeeds");
    let persisted = fs::read(&destination).expect("read atomic destination");
    assert_eq!(
        Workbook::open(&persisted)
            .expect("reopen atomic destination")
            .sheet_by_name("Data")
            .and_then(|sheet| sheet.cell(0, 0)),
        Some(&Cell::Text("persisted".into()))
    );

    let blocked = root.join("blocked.xlsx");
    fs::create_dir(&blocked).expect("create blocking destination directory");
    fs::write(blocked.join("marker"), b"unchanged").expect("write marker");
    assert!(spreadsheet.save_to_path(&blocked).is_err());
    assert_eq!(
        fs::read(blocked.join("marker")).expect("read marker"),
        b"unchanged"
    );
    let leftovers: Vec<_> = fs::read_dir(&root)
        .expect("list test directory")
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".rxls-tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files leaked: {leftovers:?}"
    );
    fs::remove_dir_all(&root).expect("clean test directory");
}

#[test]
fn legacy_comment_create_update_delete_round_trips_and_preserves_parts() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "anchor");
    let input = workbook.to_xlsx();
    let original_styles = zip_member(&input, "xl/styles.xml");
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .set_comment("Data", 0, 0, "first note", Some("Alice"))
        .expect("create comment");
    assert_eq!(
        spreadsheet.edited_parts(),
        &[
            "[Content_Types].xml",
            "xl/comments1.xml",
            "xl/drawings/vmlDrawing1.vml",
            "xl/worksheets/_rels/sheet1.xml.rels",
            "xl/worksheets/sheet1.xml",
        ]
    );
    let created = spreadsheet.save().expect("save created comment");
    let rels = String::from_utf8(zip_member(&created, "xl/worksheets/_rels/sheet1.xml.rels"))
        .expect("worksheet rels UTF-8");
    assert!(rels.contains(r#"Id="rId4""#) && rels.contains("/comments\""));
    assert!(rels.contains(r#"Id="rId5""#) && rels.contains("/vmlDrawing\""));
    let reopened = Workbook::open(&created).expect("reopen created comment");
    assert_eq!(
        reopened.sheet_by_name("Data").expect("Data").comments(),
        &[Comment {
            row: 0,
            col: 0,
            text: "first note".into(),
            author: Some("Alice".into()),
        }]
    );

    let original_sheet = zip_member(&created, "xl/worksheets/sheet1.xml");
    let original_vml = zip_member(&created, "xl/drawings/vmlDrawing1.vml");
    let mut spreadsheet = Spreadsheet::open(&created).expect("reopen for comment update");
    spreadsheet
        .set_comment("Data", 0, 0, "updated note", Some("Bob"))
        .expect("update comment");
    assert_eq!(spreadsheet.edited_parts(), &["xl/comments1.xml"]);
    let updated = spreadsheet.save().expect("save updated comment");
    assert_eq!(
        zip_member(&updated, "xl/worksheets/sheet1.xml"),
        original_sheet
    );
    assert_eq!(
        zip_member(&updated, "xl/drawings/vmlDrawing1.vml"),
        original_vml
    );
    let reopened = Workbook::open(&updated).expect("reopen updated comment");
    assert_eq!(
        reopened.sheet_by_name("Data").expect("Data").comments(),
        &[Comment {
            row: 0,
            col: 0,
            text: "updated note".into(),
            author: Some("Bob".into()),
        }]
    );

    let mut spreadsheet = Spreadsheet::open(&updated).expect("reopen for comment delete");
    spreadsheet
        .delete_comment("Data", 0, 0)
        .expect("delete comment");
    let deleted = spreadsheet.save().expect("save deleted comment");
    assert!(!zip_has_member(&deleted, "xl/comments1.xml"));
    assert!(!zip_has_member(&deleted, "xl/drawings/vmlDrawing1.vml"));
    assert!(!zip_has_member(
        &deleted,
        "xl/worksheets/_rels/sheet1.xml.rels"
    ));
    assert_eq!(zip_member(&deleted, "xl/styles.xml"), original_styles);
    assert!(Workbook::open(&deleted)
        .expect("reopen deleted comment")
        .sheet_by_name("Data")
        .expect("Data")
        .comments()
        .is_empty());
}

#[test]
fn comment_delete_preserves_other_vml_shapes_and_malformed_vml_rolls_back() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data");
    let mut spreadsheet = Spreadsheet::open(&workbook.to_xlsx()).expect("open xlsx");
    spreadsheet
        .set_comment("Data", 0, 0, "note", Some("Alice"))
        .expect("create comment");
    let created = spreadsheet.save().expect("save comment");

    let mut seed = Spreadsheet::open(&created).expect("open VML seed");
    let package = seed.package.as_mut().expect("editable package");
    let tree = package
        .part_tree_mut("xl/drawings/vmlDrawing1.vml")
        .expect("promote VML");
    let root = tree.root_element().expect("VML root");
    let index = tree.children_of(root).len();
    tree.insert_fragment_at(
        root,
        index,
        br##"<v:shape id="_x0000_s2048" type="#_x0000_t201"/>"##,
    )
    .expect("insert control-like shape");
    let with_control = seed.save().expect("save VML control fixture");
    let mut spreadsheet = Spreadsheet::open(&with_control).expect("reopen VML fixture");
    spreadsheet
        .delete_comment("Data", 0, 0)
        .expect("delete note but preserve control VML");
    let preserved = spreadsheet.save().expect("save preserved VML");
    assert!(!zip_has_member(&preserved, "xl/comments1.xml"));
    assert!(zip_has_member(&preserved, "xl/drawings/vmlDrawing1.vml"));
    let vml = String::from_utf8(zip_member(&preserved, "xl/drawings/vmlDrawing1.vml"))
        .expect("VML UTF-8");
    assert!(vml.contains("_x0000_s2048"));

    let mut seed = Spreadsheet::open(&created).expect("open malformed VML seed");
    seed.package
        .as_mut()
        .expect("editable package")
        .replace_part(
            "xl/drawings/vmlDrawing1.vml",
            b"not well-formed VML".to_vec(),
        )
        .expect("replace VML");
    let malformed = seed.save().expect("save malformed VML fixture");
    let mut spreadsheet = Spreadsheet::open(&malformed).expect("open malformed VML fixture");
    let before = spreadsheet.save().expect("serialize malformed fixture");
    assert!(spreadsheet.delete_comment("Data", 0, 0).is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(spreadsheet.save().expect("save rolled back delete"), before);
}

#[test]
fn external_and_internal_hyperlink_crud_reuses_relationship_ids() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    sheet.write(0, 0, "external");
    sheet.write(0, 1, "internal");
    let input = workbook.to_xlsx();
    let original_styles = zip_member(&input, "xl/styles.xml");
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    spreadsheet
        .set_external_hyperlink("Data", 0, 0, "https://example.com/one")
        .expect("create external hyperlink");
    spreadsheet
        .set_internal_hyperlink("Data", 0, 1, "Data!A1")
        .expect("create internal hyperlink");
    let created = spreadsheet.save().expect("save hyperlinks");
    let created_sheet = zip_member(&created, "xl/worksheets/sheet1.xml");
    let rels = String::from_utf8(zip_member(&created, "xl/worksheets/_rels/sheet1.xml.rels"))
        .expect("worksheet rels UTF-8");
    assert!(rels.contains(r#"Id="rId4""#));
    assert!(rels.contains(r#"Target="https://example.com/one""#));
    let sheet_xml = String::from_utf8(created_sheet.clone()).expect("worksheet UTF-8");
    assert!(sheet_xml.contains(r#"ref="A1" r:id="rId4""#));
    assert!(sheet_xml.contains(r#"ref="B1" location="Data!A1""#));
    assert_eq!(
        Workbook::open(&created)
            .expect("reopen hyperlinks")
            .sheet_by_name("Data")
            .expect("Data")
            .hyperlinks(),
        &[(0, 0, "https://example.com/one".into())]
    );

    let mut spreadsheet = Spreadsheet::open(&created).expect("reopen external update");
    spreadsheet
        .set_external_hyperlink("Data", 0, 0, "https://example.com/two")
        .expect("update external hyperlink");
    assert_eq!(
        spreadsheet.edited_parts(),
        &["xl/worksheets/_rels/sheet1.xml.rels"]
    );
    let external_updated = spreadsheet.save().expect("save external update");
    assert_eq!(
        zip_member(&external_updated, "xl/worksheets/sheet1.xml"),
        created_sheet
    );
    let rels = String::from_utf8(zip_member(
        &external_updated,
        "xl/worksheets/_rels/sheet1.xml.rels",
    ))
    .expect("updated rels UTF-8");
    assert!(rels.contains(r#"Id="rId4""#));
    assert!(rels.contains(r#"Target="https://example.com/two""#));

    let mut spreadsheet = Spreadsheet::open(&external_updated).expect("reopen internal update");
    let original_rels = zip_member(&external_updated, "xl/worksheets/_rels/sheet1.xml.rels");
    spreadsheet
        .set_internal_hyperlink("Data", 0, 1, "Data!B2")
        .expect("update internal hyperlink");
    assert_eq!(spreadsheet.edited_parts(), &["xl/worksheets/sheet1.xml"]);
    let internal_updated = spreadsheet.save().expect("save internal update");
    assert_eq!(
        zip_member(&internal_updated, "xl/worksheets/_rels/sheet1.xml.rels"),
        original_rels
    );

    let mut spreadsheet = Spreadsheet::open(&internal_updated).expect("reopen link deletes");
    spreadsheet
        .delete_hyperlink("Data", 0, 1)
        .expect("delete internal hyperlink");
    spreadsheet
        .delete_hyperlink("Data", 0, 0)
        .expect("delete external hyperlink");
    let deleted = spreadsheet.save().expect("save deleted hyperlinks");
    assert!(!zip_has_member(
        &deleted,
        "xl/worksheets/_rels/sheet1.xml.rels"
    ));
    let sheet_xml = String::from_utf8(zip_member(&deleted, "xl/worksheets/sheet1.xml"))
        .expect("deleted worksheet UTF-8");
    assert!(!sheet_xml.contains("<hyperlinks"));
    assert_eq!(zip_member(&deleted, "xl/styles.xml"), original_styles);
    let reopened = Workbook::open(&deleted).expect("reopen deleted hyperlinks");
    let sheet = reopened.sheet_by_name("Data").expect("Data");
    assert!(sheet.hyperlinks().is_empty());
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("external".into())));
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Text("internal".into())));
}

#[test]
fn retargeting_one_of_two_shared_external_links_splits_the_relationship() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    sheet.write(0, 0, "first");
    sheet.write(0, 1, "second");
    let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open xlsx");
    seed.set_external_hyperlink("Data", 0, 0, "https://example.com/shared")
        .expect("create external hyperlink");

    let worksheet_path =
        worksheet_path(seed.package.as_ref().expect("package"), "Data").expect("worksheet path");
    let tree = seed
        .package
        .as_mut()
        .expect("editable package")
        .part_tree_mut(&worksheet_path)
        .expect("promote worksheet");
    sml_set_hyperlink(
        tree,
        0,
        1,
        HyperlinkEdit::External("https://example.com/shared"),
        Some("rId4"),
    )
    .expect("share relationship with second cell");
    let shared = seed.save().expect("save shared relationship fixture");

    let mut spreadsheet = Spreadsheet::open(&shared).expect("reopen shared fixture");
    spreadsheet
        .set_external_hyperlink("Data", 0, 0, "https://example.com/first")
        .expect("retarget only the first hyperlink");
    let updated = spreadsheet.save().expect("save split relationships");
    let sheet_xml = String::from_utf8(zip_member(&updated, "xl/worksheets/sheet1.xml"))
        .expect("worksheet UTF-8");
    assert!(sheet_xml.contains(r#"ref="A1" r:id="rId5""#));
    assert!(sheet_xml.contains(r#"ref="B1" r:id="rId4""#));
    let rels = String::from_utf8(zip_member(&updated, "xl/worksheets/_rels/sheet1.xml.rels"))
        .expect("relationships UTF-8");
    assert!(rels.contains(r#"Id="rId4""#));
    assert!(rels.contains(r#"Target="https://example.com/shared""#));
    assert!(rels.contains(r#"Id="rId5""#));
    assert!(rels.contains(r#"Target="https://example.com/first""#));
    assert_eq!(
        Workbook::open(&updated)
            .expect("reopen split hyperlinks")
            .sheet_by_name("Data")
            .expect("Data")
            .hyperlinks(),
        &[
            (0, 0, "https://example.com/first".into()),
            (0, 1, "https://example.com/shared".into()),
        ]
    );
}

#[test]
fn comment_and_hyperlink_late_failures_roll_back_exactly() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let before_link = spreadsheet.save().expect("serialize link fixture");

    set_test_fail_commit_after(0);
    let result = spreadsheet.set_external_hyperlink("Data", 0, 0, "https://example.com");
    reset_test_fail_commit();
    assert!(result.is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        spreadsheet.save().expect("save rolled back link"),
        before_link
    );

    spreadsheet
        .set_comment("Data", 0, 0, "original", Some("Alice"))
        .expect("create rollback comment fixture");
    let with_comment = spreadsheet.save().expect("save rollback fixture");
    let mut spreadsheet = Spreadsheet::open(&with_comment).expect("reopen rollback fixture");
    let before = spreadsheet.save().expect("serialize rollback fixture");
    set_test_fail_commit_after(0);
    let result = spreadsheet.set_comment("Data", 0, 0, "candidate", Some("Alice"));
    reset_test_fail_commit();
    assert!(result.is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        spreadsheet.save().expect("save rolled back comment"),
        before
    );
}

#[test]
fn data_validation_create_update_delete_round_trips_and_preserves_unknown_xml() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "value");
    let input = workbook.to_xlsx();
    let original_styles = zip_member(&input, "xl/styles.xml");
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .set_data_validation(
            "Data",
            DataValidation::list((0, 0, 2, 0), "\"Yes,No\"").with_prompt("Pick", "Choose one"),
        )
        .expect("create data validation");
    assert_eq!(spreadsheet.edited_parts(), &["xl/worksheets/sheet1.xml"]);
    let created = spreadsheet.save().expect("save created validation");
    let reopened = Workbook::open(&created).expect("reopen created validation");
    let validations = reopened
        .sheet_by_name("Data")
        .expect("Data")
        .data_validations();
    assert_eq!(validations.len(), 1);
    assert_eq!(validations[0].sqref, (0, 0, 2, 0));
    assert_eq!(validations[0].kind, DvKind::List);

    let mut seed = Spreadsheet::open(&created).expect("open unknown XML seed");
    let tree = seed
        .package
        .as_mut()
        .expect("editable package")
        .part_tree_mut("xl/worksheets/sheet1.xml")
        .expect("promote worksheet");
    let root = tree.root_element().expect("worksheet root");
    let wrapper = data_validation_wrappers(tree, root)[0];
    tree.set_attr(wrapper, b"customWrapper", b"preserve")
        .expect("set wrapper extension attr");
    let validation = data_validation_nodes(tree, wrapper)[0];
    tree.set_attr(validation, b"errorStyle", b"warning")
        .expect("set unknown modeled-adjacent attr");
    tree.set_attr(validation, b"customRule", b"preserve")
        .expect("set custom rule attr");
    let index = tree.children_of(validation).len();
    tree.insert_fragment_at(
        validation,
        index,
        br#"<extLst><ext uri="custom"><custom keep="yes"/></ext></extLst>"#,
    )
    .expect("insert unknown child");
    let seeded = seed.save().expect("save unknown XML seed");

    let mut spreadsheet = Spreadsheet::open(&seeded).expect("reopen validation update");
    spreadsheet
        .set_data_validation(
            "Data",
            DataValidation::new((0, 0, 2, 0), DvKind::Whole, DvOp::Between, "1")
                .with_formula2("10")
                .with_error("Bounds", "Use 1 through 10"),
        )
        .expect("replace data validation");
    spreadsheet
        .set_data_validation(
            "Data",
            DataValidation::new((0, 2, 0, 2), DvKind::Custom, DvOp::Equal, "ISNUMBER(C1)"),
        )
        .expect("append second validation");
    let updated = spreadsheet.save().expect("save updated validations");
    let xml = String::from_utf8(zip_member(&updated, "xl/worksheets/sheet1.xml"))
        .expect("worksheet UTF-8");
    assert!(xml.contains(r#"<dataValidations count="2" customWrapper="preserve">"#));
    assert!(xml.contains(r#"errorStyle="warning" customRule="preserve""#));
    assert!(xml.contains(r#"<custom keep="yes"/>"#));
    assert!(xml.contains(r#"type="whole""#));
    assert!(xml.contains(r#"operator="between""#));
    assert!(xml.contains("<formula1>1</formula1><formula2>10</formula2>"));
    let reopened = Workbook::open(&updated).expect("reopen updated validations");
    assert_eq!(
        reopened
            .sheet_by_name("Data")
            .expect("Data")
            .data_validations()
            .len(),
        2
    );

    let mut spreadsheet = Spreadsheet::open(&updated).expect("reopen validation delete");
    spreadsheet
        .delete_data_validation("Data", (0, 0, 2, 0))
        .expect("delete first validation");
    let one_left = spreadsheet.save().expect("save one validation");
    let xml = String::from_utf8(zip_member(&one_left, "xl/worksheets/sheet1.xml"))
        .expect("worksheet UTF-8");
    assert!(xml.contains(r#"<dataValidations count="1" customWrapper="preserve">"#));
    let mut spreadsheet = Spreadsheet::open(&one_left).expect("reopen last validation delete");
    spreadsheet
        .delete_data_validation("Data", (0, 2, 0, 2))
        .expect("delete last validation");
    let deleted = spreadsheet.save().expect("save deleted validations");
    let xml = String::from_utf8(zip_member(&deleted, "xl/worksheets/sheet1.xml"))
        .expect("worksheet UTF-8");
    assert!(!xml.contains("<dataValidations"));
    assert_eq!(zip_member(&deleted, "xl/styles.xml"), original_styles);
}

#[test]
fn data_validation_rejections_and_late_failure_roll_back_exactly() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data");
    let mut spreadsheet = Spreadsheet::open(&workbook.to_xlsx()).expect("open xlsx");
    spreadsheet
        .set_data_validation("Data", DataValidation::list((0, 0, 2, 0), "\"A,B\""))
        .expect("seed validation");
    let seeded = spreadsheet.save().expect("save seeded validation");

    let mut spreadsheet = Spreadsheet::open(&seeded).expect("reopen rejection fixture");
    let before = spreadsheet.save().expect("serialize rejection fixture");
    assert!(spreadsheet
        .set_data_validation("Data", DataValidation::list((1, 0, 1, 0), "\"C,D\""))
        .is_err());
    assert!(spreadsheet
        .set_data_validation(
            "Data",
            DataValidation::new((4, 0, 4, 0), DvKind::Whole, DvOp::Between, ""),
        )
        .is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        spreadsheet.save().expect("save rejected validation"),
        before
    );

    set_test_fail_commit_after(0);
    let result = spreadsheet.set_data_validation(
        "Data",
        DataValidation::new((0, 0, 2, 0), DvKind::Whole, DvOp::Equal, "5"),
    );
    reset_test_fail_commit();
    assert!(result.is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        spreadsheet.save().expect("save rolled-back validation"),
        before
    );
}

#[test]
fn existing_table_bottom_resize_round_trips_and_preserves_unknown_xml() {
    use crate::Table;

    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    sheet.write(0, 0, "Name");
    sheet.write(0, 1, "Value");
    sheet.write(1, 0, "one");
    sheet.write(1, 1, 1.0);
    sheet.add_table(Table {
        range: (0, 0, 1, 1),
        name: "Sales".into(),
        columns: vec!["Name".into(), "Value".into()],
        style: None,
    });
    let input = workbook.to_xlsx();
    let original_sheet = zip_member(&input, "xl/worksheets/sheet1.xml");
    let original_styles = zip_member(&input, "xl/styles.xml");
    let mut seed = Spreadsheet::open(&input).expect("open table seed");
    let tree = seed
        .package
        .as_mut()
        .expect("editable package")
        .part_tree_mut("xl/tables/table1.xml")
        .expect("promote table");
    let plan = inspect_table_part(tree).expect("inspect table");
    tree.set_attr(plan.root, b"customTable", b"preserve")
        .expect("set custom table attr");
    tree.set_attr(
        plan.auto_filter.expect("autoFilter"),
        b"customFilter",
        b"preserve",
    )
    .expect("set custom filter attr");
    let index = tree.children_of(plan.root).len();
    tree.insert_fragment_at(
        plan.root,
        index,
        br#"<extLst><ext uri="custom"><custom keep="yes"/></ext></extLst>"#,
    )
    .expect("insert table extension");
    let seeded = seed.save().expect("save table seed");

    let mut spreadsheet = Spreadsheet::open(&seeded).expect("reopen table seed");
    spreadsheet
        .set_table_range("Data", "sales", (0, 0, 5, 1))
        .expect("resize table bottom row");
    assert_eq!(spreadsheet.edited_parts(), &["xl/tables/table1.xml"]);
    let resized = spreadsheet.save().expect("save resized table");
    assert_eq!(
        zip_member(&resized, "xl/worksheets/sheet1.xml"),
        original_sheet
    );
    assert_eq!(zip_member(&resized, "xl/styles.xml"), original_styles);
    let xml = String::from_utf8(zip_member(&resized, "xl/tables/table1.xml")).expect("table UTF-8");
    assert!(xml.contains(r#"ref="A1:B6""#));
    assert!(xml.contains(r#"customTable="preserve""#));
    assert!(xml.contains(r#"customFilter="preserve""#));
    assert!(xml.contains(r#"<custom keep="yes"/>"#));
    let reopened = Workbook::open(&resized).expect("reopen resized table");
    assert_eq!(
        reopened.sheet_by_name("Data").expect("Data").tables()[0].range,
        (0, 0, 5, 1)
    );

    let mut spreadsheet = Spreadsheet::open(&resized).expect("reopen table rejection");
    let before = spreadsheet.save().expect("serialize resized table");
    assert!(spreadsheet
        .set_table_range("Data", "Sales", (1, 0, 5, 1))
        .is_err());
    assert!(spreadsheet
        .set_table_range("Data", "Sales", (0, 0, 5, 2))
        .is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        spreadsheet.save().expect("save rejected table edits"),
        before
    );
}

#[test]
fn table_resize_inserts_missing_autofilter_and_rolls_back_late_failure() {
    use crate::Table;

    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    sheet.write(0, 0, "Header");
    sheet.add_table(Table {
        range: (0, 0, 1, 0),
        name: "Items".into(),
        columns: vec!["Header".into()],
        style: None,
    });
    let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open table fixture");
    let tree = seed
        .package
        .as_mut()
        .expect("editable package")
        .part_tree_mut("xl/tables/table1.xml")
        .expect("promote table");
    let plan = inspect_table_part(tree).expect("inspect table");
    tree.remove_child(plan.root, plan.auto_filter.expect("autoFilter"))
        .expect("remove autoFilter");
    let missing_filter = seed.save().expect("save missing autoFilter fixture");

    let mut spreadsheet = Spreadsheet::open(&missing_filter).expect("reopen rollback fixture");
    let before = spreadsheet.save().expect("serialize rollback fixture");
    set_test_fail_commit_after(0);
    let result = spreadsheet.set_table_range("Data", "Items", (0, 0, 3, 0));
    reset_test_fail_commit();
    assert!(result.is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(spreadsheet.save().expect("save rolled-back table"), before);

    spreadsheet
        .set_table_range("Data", "Items", (0, 0, 3, 0))
        .expect("resize and restore autoFilter");
    let saved = spreadsheet.save().expect("save restored autoFilter");
    let xml = String::from_utf8(zip_member(&saved, "xl/tables/table1.xml")).expect("table UTF-8");
    assert!(xml.contains(r#"<autoFilter ref="A1:A4"/>"#));
}

#[test]
fn rename_sheet_updates_formula_name_print_and_chart_references() {
    use crate::{Chart, ChartKind, PageSetup, Series};

    let mut workbook = Workbook::new();
    {
        let data = workbook.add_sheet("Old Data");
        data.write(0, 0, 10.0);
        data.write(1, 0, 20.0);
        data.set_page_setup(PageSetup::new().with_print_area((0, 0, 1, 0)));
    }
    {
        let other = workbook.add_sheet("Other");
        other.write_formula(0, 0, r#"'Old Data'!A1&"Old Data!A1""#, "10Old Data!A1");
        other.add_chart(Chart {
            kind: ChartKind::Line,
            title: None,
            series: vec![Series {
                name: Some("Values".into()),
                categories: None,
                values: "'Old Data'!$A$1:$A$2".into(),
                bubble_sizes: None,
            }],
            legend: false,
            data_labels: false,
            x_axis_title: None,
            y_axis_title: None,
            from: (2, 0),
            to: (12, 8),
        });
    }
    workbook.define_name("InputRange", "'Old Data'!$A$1:$A$2");
    let input = workbook.to_xlsx();
    let original_source_sheet = zip_member(&input, "xl/worksheets/sheet1.xml");
    let original_workbook_rels = zip_member(&input, "xl/_rels/workbook.xml.rels");
    let original_content_types = zip_member(&input, "[Content_Types].xml");
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .rename_sheet("Old Data", "Renamed Data")
        .expect("rename sheet atomically");

    assert_eq!(
        spreadsheet.edited_parts(),
        &[
            "xl/charts/chart1.xml",
            "xl/workbook.xml",
            "xl/worksheets/sheet2.xml"
        ]
    );
    let saved = spreadsheet.save().expect("save renamed package");
    assert_eq!(
        zip_member(&saved, "xl/worksheets/sheet1.xml"),
        original_source_sheet,
        "the renamed sheet's cell part is untouched when it has no references to itself"
    );
    assert_eq!(
        zip_member(&saved, "xl/_rels/workbook.xml.rels"),
        original_workbook_rels,
        "renaming does not churn relationship ids or targets"
    );
    assert_eq!(
        zip_member(&saved, "[Content_Types].xml"),
        original_content_types,
        "renaming does not churn package content types"
    );

    let reopened = Workbook::open(&saved).expect("reopen renamed package");
    assert_eq!(reopened.sheet_names(), vec!["Renamed Data", "Other"]);
    assert_eq!(
        reopened.defined_names(),
        &[(
            "InputRange".to_string(),
            "'Renamed Data'!$A$1:$A$2".to_string()
        )]
    );
    assert_eq!(
        reopened
            .sheet_by_name("Renamed Data")
            .and_then(|sheet| sheet.page_setup())
            .and_then(|setup| setup.print_area),
        Some((0, 0, 1, 0))
    );
    assert_eq!(
        reopened
            .sheet_by_name("Other")
            .and_then(|sheet| sheet.cell(0, 0)),
        Some(&Cell::Formula {
            formula: r#"'Renamed Data'!A1&"Old Data!A1""#.into(),
            cached: Box::new(Cell::Text("10Old Data!A1".into())),
        })
    );
    assert_eq!(
        reopened
            .sheet_by_name("Other")
            .and_then(|sheet| sheet.charts().first())
            .and_then(|chart| chart.series.first())
            .map(|series| series.values.as_str()),
        Some("'Renamed Data'!$A$1:$A$2")
    );
}

#[test]
fn rename_sheet_rolls_back_formula_and_name_updates_on_late_failure() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Old").write(0, 0, 1.0);
    workbook
        .add_sheet("Other")
        .write_formula(0, 0, "Old!A1+1", 2.0);
    workbook.define_name("Input", "Old!$A$1");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let before = spreadsheet.save().expect("serialize original package");

    // The workbook defined-name rewrite succeeds first; fail the later
    // worksheet formula rewrite and prove the outer rename transaction
    // discards both that first edit and all touched-part bookkeeping.
    set_test_fail_commit_after(1);
    let result = spreadsheet.rename_sheet("Old", "Renamed");
    reset_test_fail_commit();

    assert!(result.is_err(), "the injected worksheet rewrite must fail");
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        spreadsheet.save().expect("save rolled-back package"),
        before
    );
    let reopened = Workbook::open(&before).expect("reopen original package");
    assert_eq!(reopened.sheet_names(), vec!["Old", "Other"]);
    assert_eq!(
        reopened
            .sheet_by_name("Other")
            .and_then(|sheet| sheet.cell(0, 0)),
        Some(&Cell::Formula {
            formula: "Old!A1+1".into(),
            cached: Box::new(Cell::Number(2.0)),
        })
    );
    assert_eq!(
        reopened.defined_names(),
        &[("Input".to_string(), "Old!$A$1".to_string())]
    );
}

#[test]
fn rename_sheet_rejects_case_insensitive_duplicates_without_touching_parts() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, 1.0);
    workbook.add_sheet("Other").write(0, 0, 2.0);
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let before = spreadsheet.save().expect("save original package");

    assert!(spreadsheet.rename_sheet("Other", "data").is_err());
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(spreadsheet.save().expect("save unchanged package"), before);
}

#[test]
fn document_properties_roll_back_if_the_second_part_edit_fails() {
    let mut workbook = Workbook::new();
    workbook.set_properties(
        DocProperties::new()
            .with_title("Original title")
            .with_company("Original company"),
    );
    workbook.add_sheet("Data").write(0, 0, "value");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
    let before = spreadsheet.save().expect("serialize original package");

    // Updating core.xml's existing title consumes the first commit. Force
    // the following app.xml Company update to fail after core.xml has
    // already changed in the candidate, exercising clone-and-swap rollback.
    set_test_fail_commit_after(1);
    let result = spreadsheet.set_document_properties(
        DocProperties::new()
            .with_title("Candidate title")
            .with_company("Candidate company"),
    );
    reset_test_fail_commit();

    assert!(result.is_err(), "the injected app.xml edit must fail");
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        spreadsheet.save().expect("serialize rolled-back package"),
        before,
        "neither properties part may commit after the second part fails"
    );
    let reopened = Workbook::open(&before).expect("reopen original package");
    assert_eq!(reopened.properties.title.as_deref(), Some("Original title"));
    assert_eq!(
        reopened.properties.company.as_deref(),
        Some("Original company")
    );
}

#[test]
fn document_properties_commit_core_and_app_together() {
    let mut workbook = Workbook::new();
    workbook.set_properties(
        DocProperties::new()
            .with_title("Original title")
            .with_company("Original company"),
    );
    workbook.add_sheet("Data").write(0, 0, "value");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .set_document_properties(
            DocProperties::new()
                .with_title("Committed title")
                .with_company("Committed company"),
        )
        .expect("commit properties");

    assert_eq!(
        spreadsheet.edited_parts(),
        &["docProps/app.xml", "docProps/core.xml"]
    );
    let saved = spreadsheet.save().expect("save committed package");
    let reopened = Workbook::open(&saved).expect("reopen committed package");
    assert_eq!(
        reopened.properties.title.as_deref(),
        Some("Committed title")
    );
    assert_eq!(
        reopened.properties.company.as_deref(),
        Some("Committed company")
    );
}
