//! Public extended-property worksheet lifecycle regressions for #130.
#![cfg(feature = "xlsx")]

use std::io::{Cursor, Read, Write};

use rxls::{Spreadsheet, Workbook};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const APP: &str = "docProps/app.xml";
const EP: &str = "http://schemas.openxmlformats.org/officeDocument/2006/extended-properties";
const VT: &str = "http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes";

fn metadata() -> String {
    format!(
        r#"<Properties xmlns="{EP}" xmlns:vt="{VT}" custom="preserve"><Application>Independent fixture</Application><Company>Keep &amp; Co</Company><HeadingPairs keep="yes"><vt:vector size="6" baseType="variant"><vt:variant><vt:lpstr>Charts</vt:lpstr></vt:variant><vt:variant><vt:i4>1</vt:i4></vt:variant><vt:variant><vt:lpstr>Worksheets</vt:lpstr></vt:variant><vt:variant><vt:i4>2</vt:i4></vt:variant><vt:variant><vt:lpstr>Named Ranges</vt:lpstr></vt:variant><vt:variant><vt:i4>1</vt:i4></vt:variant></vt:vector></HeadingPairs><TitlesOfParts keep="yes"><vt:vector size="4" baseType="lpstr"><vt:lpstr>Earlier title</vt:lpstr><vt:lpstr>Data</vt:lpstr><vt:lpstr>Keep</vt:lpstr><vt:lpstr>Later title</vt:lpstr></vt:vector></TitlesOfParts><!--preserve-comment--><Extension keep="untouched"/></Properties>"#
    )
}

fn fixture(app: &str) -> Vec<u8> {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, 1.0);
    workbook.add_sheet("Keep").write(0, 0, "untouched");
    let bytes = workbook.to_xlsx_checked().unwrap();
    let mut source = ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut output = ZipWriter::new(Cursor::new(Vec::new()));
    let mut found = false;
    for index in 0..source.len() {
        let mut part = source.by_index(index).unwrap();
        let mut contents = Vec::new();
        part.read_to_end(&mut contents).unwrap();
        output
            .start_file(part.name(), SimpleFileOptions::default())
            .unwrap();
        if part.name() == APP {
            found = true;
            output.write_all(app.as_bytes()).unwrap();
        } else {
            output.write_all(&contents).unwrap();
        }
    }
    if !found {
        output
            .start_file(APP, SimpleFileOptions::default())
            .unwrap();
        output.write_all(app.as_bytes()).unwrap();
    }
    output.finish().unwrap().into_inner()
}

fn replace_part(bytes: &[u8], name: &str, replacement: &[u8]) -> Vec<u8> {
    let mut source = ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut output = ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..source.len() {
        let mut part = source.by_index(index).unwrap();
        let mut contents = Vec::new();
        part.read_to_end(&mut contents).unwrap();
        output
            .start_file(part.name(), SimpleFileOptions::default())
            .unwrap();
        output
            .write_all(if part.name() == name {
                replacement
            } else {
                &contents
            })
            .unwrap();
    }
    output.finish().unwrap().into_inner()
}

fn part(bytes: &[u8], name: &str) -> Vec<u8> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut output = Vec::new();
    archive
        .by_name(name)
        .unwrap()
        .read_to_end(&mut output)
        .unwrap();
    output
}

fn app(bytes: &[u8]) -> String {
    String::from_utf8(part(bytes, APP)).unwrap()
}

fn assert_metadata(bytes: &[u8], count: usize, names: &[&str]) {
    let xml = app(bytes);
    assert!(
        xml.contains(&format!(
            "<vt:lpstr>Worksheets</vt:lpstr></vt:variant><vt:variant><vt:i4>{count}</vt:i4>"
        )),
        "{xml}"
    );
    assert!(
        xml.contains(&format!(r#"size="{}" baseType="lpstr""#, names.len() + 2)),
        "{xml}"
    );
    let mut expected = "<vt:lpstr>Earlier title</vt:lpstr>".to_string();
    for name in names {
        expected.push_str(&format!("<vt:lpstr>{name}</vt:lpstr>"));
    }
    expected.push_str("<vt:lpstr>Later title</vt:lpstr>");
    assert!(xml.contains(&expected), "{xml}");
    for unchanged in [
        "<Company>Keep &amp; Co</Company>",
        "<!--preserve-comment-->",
        r#"<Extension keep="untouched"/>"#,
        r#"custom="preserve""#,
    ] {
        assert!(xml.contains(unchanged), "{xml}");
    }
}

#[test]
fn added_sheet_metadata_stays_coherent_after_save_reopen_and_delete() {
    let original = fixture(&metadata());
    let mut spreadsheet = Spreadsheet::open(&original).unwrap();
    spreadsheet.add_sheet("New").unwrap();
    let saved = spreadsheet.save().unwrap();
    assert_metadata(&saved, 3, &["Data", "Keep", "New"]);
    assert_eq!(
        part(&saved, "xl/worksheets/sheet2.xml"),
        part(&original, "xl/worksheets/sheet2.xml")
    );
    let mut reopened = Spreadsheet::open(&saved).unwrap();
    reopened.delete_sheet("New").unwrap();
    assert_metadata(&reopened.save().unwrap(), 2, &["Data", "Keep"]);
}

#[test]
fn renamed_sheet_metadata_stays_coherent_after_save_reopen_and_delete() {
    let original = fixture(&metadata());
    let mut spreadsheet = Spreadsheet::open(&original).unwrap();
    spreadsheet.rename_sheet("Data", "Renamed").unwrap();
    let saved = spreadsheet.save().unwrap();
    assert_metadata(&saved, 2, &["Renamed", "Keep"]);
    assert_eq!(
        part(&saved, "xl/worksheets/sheet2.xml"),
        part(&original, "xl/worksheets/sheet2.xml")
    );
    let mut reopened = Spreadsheet::open(&saved).unwrap();
    reopened.delete_sheet("Renamed").unwrap();
    assert_metadata(&reopened.save().unwrap(), 1, &["Keep"]);
}

#[test]
fn empty_heading_variant_returns_atomic_error_without_panicking() {
    for field in ["<vt:lpstr>Worksheets</vt:lpstr>", "<vt:i4>2</vt:i4>"] {
        let original = fixture(&metadata().replace(field, ""));
        let mut spreadsheet = Spreadsheet::open(&original).unwrap();
        let before = spreadsheet.save().unwrap();
        let edited = spreadsheet.edited_parts().to_vec();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            spreadsheet.delete_sheet("Data")
        }));
        assert!(result.is_ok(), "empty metadata variant panicked");
        assert!(result.unwrap().is_err());
        assert_eq!(spreadsheet.save().unwrap(), before);
        assert_eq!(spreadsheet.edited_parts(), edited);
    }
}

fn operate(spreadsheet: &mut Spreadsheet, operation: u8) -> rxls::Result<()> {
    match operation {
        0 => spreadsheet.add_sheet("New"),
        1 => spreadsheet.rename_sheet("Data", "Renamed"),
        2 => spreadsheet.delete_sheet("Data"),
        _ => unreachable!(),
    }
}

#[test]
fn malformed_recognized_metadata_rejects_every_lifecycle_operation_atomically() {
    let valid = metadata();
    let invalid = [
        valid.replace("<vt:i4>2</vt:i4>", "<vt:i4>3</vt:i4>"),
        valid.replace(
            r#"size="4" baseType="lpstr""#,
            r#"size="5" baseType="lpstr""#,
        ),
        valid.replace(r#"baseType="variant""#, r#"baseType="i4""#),
        valid.replace(r#"baseType="lpstr""#, r#"baseType="variant""#),
        valid.replace("<vt:i4>2</vt:i4>", "<vt:lpstr>2</vt:lpstr>"),
        valid.replace("<vt:i4>2</vt:i4>", "<vt:i4>2147483648</vt:i4>"),
        valid.replace(
            "<vt:lpstr>Named Ranges</vt:lpstr>",
            "<vt:lpstr>Worksheets</vt:lpstr>",
        ),
        valid.replace("<vt:lpstr>Keep</vt:lpstr>", "<vt:lpstr>Data</vt:lpstr>"),
        valid.replace("<vt:lpstr>Keep</vt:lpstr>", "<vt:lpstr>Gone</vt:lpstr>"),
        valid.replace(
            "</vt:vector></HeadingPairs>",
            "<vt:i4>7</vt:i4></vt:vector></HeadingPairs>",
        ),
        valid.replace(
            "</vt:vector></TitlesOfParts>",
            "<vt:i4>7</vt:i4></vt:vector></TitlesOfParts>",
        ),
        valid.replace("<vt:lpstr>Data</vt:lpstr>", "<vt:x:lpstr>Data</vt:x:lpstr>"),
        valid.replace(
            "<vt:variant><vt:i4>2</vt:i4></vt:variant>",
            "<vt:x:variant><vt:i4>2</vt:i4></vt:x:variant>",
        ),
        valid.replace("<vt:lpstr>Worksheets</vt:lpstr>", ""),
        valid.replace("<vt:i4>2</vt:i4>", ""),
    ];
    for (case, xml) in invalid.iter().enumerate() {
        for operation in 0..3 {
            let mut spreadsheet = Spreadsheet::open(&fixture(xml)).unwrap();
            let before = spreadsheet.save().unwrap();
            let edited = spreadsheet.edited_parts().to_vec();
            assert!(
                operate(&mut spreadsheet, operation).is_err(),
                "case {case}, op {operation}"
            );
            assert_eq!(
                spreadsheet.save().unwrap(),
                before,
                "case {case}, op {operation}"
            );
            assert_eq!(spreadsheet.edited_parts(), edited);
        }
    }
}

#[test]
fn absent_or_foreign_metadata_stays_byte_identical() {
    for xml in [
        format!(r#"<Properties xmlns="{EP}"><Company>Keep</Company></Properties>"#),
        metadata().replace(EP, "https://example.com/foreign-properties"),
        format!(
            r#"<Properties xmlns="{EP}"><HeadingPairs xmlns=""><vector/></HeadingPairs><TitlesOfParts xmlns=""><vector/></TitlesOfParts></Properties>"#
        ),
    ] {
        for operation in 0..3 {
            let original = fixture(&xml);
            let mut spreadsheet = Spreadsheet::open(&original).unwrap();
            operate(&mut spreadsheet, operation).unwrap();
            assert_eq!(
                part(&spreadsheet.save().unwrap(), APP),
                part(&original, APP)
            );
            assert!(!spreadsheet.edited_parts().iter().any(|name| name == APP));
        }
    }
}

#[test]
fn alternate_prefixes_strict_namespaces_and_local_bindings_remain_coherent() {
    let valid = metadata();
    let variants = [
        valid.replace("xmlns:vt=", "xmlns:v=").replace("vt:", "v:"),
        valid
            .replace(
                EP,
                "http://purl.oclc.org/ooxml/officeDocument/extendedProperties",
            )
            .replace(
                VT,
                "http://purl.oclc.org/ooxml/officeDocument/docPropsVTypes",
            ),
        valid
            .replace(&format!(r#" xmlns:vt="{VT}""#), "")
            .replace("<vt:vector ", &format!(r#"<vt:vector xmlns:vt="{VT}" "#)),
    ];
    for xml in variants {
        let mut spreadsheet = Spreadsheet::open(&fixture(&xml)).unwrap();
        spreadsheet.add_sheet("New").unwrap();
        spreadsheet.rename_sheet("New", "Renamed").unwrap();
        let saved = spreadsheet.save().unwrap();
        let normalized = app(&saved).replace("v:", "vt:");
        assert!(normalized.contains(
            "<vt:lpstr>Keep</vt:lpstr><vt:lpstr>Renamed</vt:lpstr><vt:lpstr>Later title</vt:lpstr>"
        ));
        let mut reopened = Spreadsheet::open(&saved).unwrap();
        reopened.delete_sheet("Renamed").unwrap();
        let normalized = app(&reopened.save().unwrap()).replace("v:", "vt:");
        assert!(normalized.contains(
            "<vt:lpstr>Data</vt:lpstr><vt:lpstr>Keep</vt:lpstr><vt:lpstr>Later title</vt:lpstr>"
        ));
    }
}

#[test]
fn composed_lifecycle_preserves_unicode_names_and_unrelated_groups() {
    let mut spreadsheet = Spreadsheet::open(&fixture(&metadata())).unwrap();
    spreadsheet.add_sheet("새 시트 & Co").unwrap();
    let saved = spreadsheet.save().unwrap();
    assert_metadata(&saved, 3, &["Data", "Keep", "새 시트 &amp; Co"]);
    let mut spreadsheet = Spreadsheet::open(&saved).unwrap();
    spreadsheet.rename_sheet("새 시트 & Co", "새 이름").unwrap();
    let saved = spreadsheet.save().unwrap();
    assert_metadata(&saved, 3, &["Data", "Keep", "새 이름"]);
    let mut spreadsheet = Spreadsheet::open(&saved).unwrap();
    spreadsheet.delete_sheet("Data").unwrap();
    assert_metadata(&spreadsheet.save().unwrap(), 2, &["Keep", "새 이름"]);
}

#[test]
fn noop_rename_retains_original_app_bytes_and_edit_tracking() {
    let original = fixture(&metadata());
    let mut spreadsheet = Spreadsheet::open(&original).unwrap();
    spreadsheet.rename_sheet("Data", "Data").unwrap();
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        part(&spreadsheet.save().unwrap(), APP),
        part(&original, APP)
    );
}

#[test]
fn worksheet_relationship_prefix_aliases_work_with_title_and_scalar_metadata() {
    for xml in [
        metadata(),
        format!(r#"<Properties xmlns="{EP}"><Company>Keep</Company></Properties>"#),
    ] {
        let original = fixture(&xml);
        let workbook = String::from_utf8(part(&original, "xl/workbook.xml")).unwrap();
        let alias = workbook
            .replace("xmlns:r=", "xmlns:q=")
            .replace("r:id=", "q:id=");
        let original = replace_part(&original, "xl/workbook.xml", alias.as_bytes());
        for operation in 0..3 {
            let mut spreadsheet = Spreadsheet::open(&original).unwrap();
            operate(&mut spreadsheet, operation).unwrap();
            let saved = spreadsheet.save().unwrap();
            let reopened = Workbook::open(&saved).unwrap();
            let expected: &[&str] = match operation {
                0 => &["Data", "Keep", "New"],
                1 => &["Renamed", "Keep"],
                2 => &["Keep"],
                _ => unreachable!(),
            };
            assert_eq!(reopened.sheet_names(), expected);
            if xml.contains("HeadingPairs") {
                assert_metadata(&saved, expected.len(), expected);
            } else {
                assert_eq!(part(&saved, APP), part(&original, APP));
            }
        }
    }
}

#[test]
fn added_worksheet_binds_relationship_ids_without_changing_foreign_prefixes() {
    let original = fixture(&metadata());
    let workbook = String::from_utf8(part(&original, "xl/workbook.xml")).unwrap();
    let alias = workbook
        .replace("xmlns:r=", "xmlns:q=")
        .replace("r:id=", "q:id=");
    for workbook in [
        alias.replace("<workbook ", r#"<workbook xmlns:r="urn:foreign-root" "#),
        alias.replace("<sheets>", r#"<sheets xmlns:r="urn:foreign-sheets">"#),
        alias
            .replace("<workbook ", r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" "#)
            .replace("<sheets>", r#"<sheets xmlns:r="urn:foreign-sheets">"#),
    ] {
        let input = replace_part(&original, "xl/workbook.xml", workbook.as_bytes());
        let mut spreadsheet = Spreadsheet::open(&input).unwrap();
        spreadsheet.add_sheet("New").unwrap();
        let saved = spreadsheet.save().unwrap();
        let saved_workbook = String::from_utf8(part(&saved, "xl/workbook.xml")).unwrap();
        for binding in [r#"xmlns:r="urn:foreign-root""#, r#"xmlns:r="urn:foreign-sheets""#] {
            if workbook.contains(binding) {
                assert!(saved_workbook.contains(binding));
            }
        }
        let mut reopened = Spreadsheet::open(&saved).unwrap();
        reopened.rename_sheet("New", "Renamed").unwrap();
        assert_metadata(&reopened.save().unwrap(), 3, &["Data", "Keep", "Renamed"]);
        let mut reopened = Spreadsheet::open(&reopened.save().unwrap()).unwrap();
        reopened.delete_sheet("Renamed").unwrap();
        assert_metadata(&reopened.save().unwrap(), 2, &["Data", "Keep"]);
    }
}

#[test]
fn foreign_shadowed_or_duplicate_expanded_relationship_ids_reject_atomically() {
    let original = fixture(&metadata());
    let workbook = String::from_utf8(part(&original, "xl/workbook.xml")).unwrap();
    let invalid = [
        workbook.replace("r:id=", r#"xmlns:r="https://example.com/foreign-relationships" r:id="#),
        workbook.replace("<sheets>", r#"<sheets xmlns:q="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#)
            .replace(r#"r:id="rId1""#, r#"r:id="rId1" q:id="rId1""#),
    ];
    for workbook in invalid {
        for operation in 0..3 {
            let input = replace_part(&original, "xl/workbook.xml", workbook.as_bytes());
            let mut spreadsheet = Spreadsheet::open(&input).unwrap();
            let before = spreadsheet.save().unwrap();
            assert!(operate(&mut spreadsheet, operation).is_err());
            assert!(spreadsheet.edited_parts().is_empty());
            assert_eq!(spreadsheet.save().unwrap(), before);
        }
    }
}

#[test]
fn updated_scalar_comments_and_processing_instructions_are_preserved() {
    let xml = metadata()
        .replace(
            "<vt:i4>2</vt:i4>",
            "<vt:i4><!--count-->2<?keep count?></vt:i4>",
        )
        .replace(
            "<vt:lpstr>Data</vt:lpstr>",
            "<vt:lpstr>Da<!--title-->ta<?keep title?></vt:lpstr>",
        );
    let mut spreadsheet = Spreadsheet::open(&fixture(&xml)).unwrap();
    spreadsheet.add_sheet("New").unwrap();
    spreadsheet.rename_sheet("Data", "Renamed").unwrap();
    let saved = spreadsheet.save().unwrap();
    let xml = app(&saved);
    assert!(xml.contains("<vt:i4><!--count-->3<?keep count?></vt:i4>"));
    assert!(xml.contains("<vt:lpstr>Renamed<!--title--><?keep title?></vt:lpstr>"));
    let mut spreadsheet = Spreadsheet::open(&saved).unwrap();
    spreadsheet.delete_sheet("New").unwrap();
    let xml = app(&spreadsheet.save().unwrap());
    assert!(xml.contains("<vt:i4><!--count-->2<?keep count?></vt:i4>"));
    assert!(xml.contains("<vt:lpstr>Renamed<!--title--><?keep title?></vt:lpstr>"));
}

// Append to the current tests/sheet_lifecycle_metadata.rs, which owns ZIP helpers.

fn replace_metadata_entry_type(
    xml: &str,
    opener: &str,
    marker: &str,
    old: &str,
    new: &str,
) -> String {
    assert_eq!(xml.matches(marker).count(), 1, "controlled fixture marker");
    let marker_index = xml.find(marker).unwrap();
    let start = xml[..marker_index].rfind(opener).unwrap();
    let end = marker_index + xml[marker_index..].find('>').unwrap() + 1;
    assert_eq!(
        xml[start..end].matches(old).count(),
        1,
        "controlled fixture type"
    );
    let entry = xml[start..end].replace(old, new);
    let mut changed = xml.to_string();
    changed.replace_range(start..end, &entry);
    changed
}

// A complete retained package with a minimal, controlled chartsheet target.
// This tests public reader/editor admission and relationship-level metadata;
// it makes no native chart-rendering or real-producer chartsheet claim.
fn with_metadata_chartsheet(bytes: &[u8], sheet_number: usize) -> Vec<u8> {
    let path = format!("xl/worksheets/sheet{sheet_number}.xml");
    let relationships = String::from_utf8(part(bytes, "xl/_rels/workbook.xml.rels")).unwrap();
    let relationships = replace_metadata_entry_type(
        &relationships,
        "<Relationship ",
        &format!(r#"Target="worksheets/sheet{sheet_number}.xml""#),
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet",
    );
    let bytes = replace_part(
        bytes,
        "xl/_rels/workbook.xml.rels",
        relationships.as_bytes(),
    );
    let types = String::from_utf8(part(&bytes, "[Content_Types].xml")).unwrap();
    let types = replace_metadata_entry_type(
        &types,
        "<Override ",
        &format!(r#"PartName="/{path}""#),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.chartsheet+xml",
    );
    let bytes = replace_part(&bytes, "[Content_Types].xml", types.as_bytes());
    replace_part(
        &bytes,
        &path,
        br#"<chartsheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#,
    )
}

fn assert_chart_and_worksheet_titles(bytes: &[u8], count: usize, names: &[&str]) {
    let xml = app(bytes);
    assert!(
        xml.contains("<vt:lpstr>Charts</vt:lpstr></vt:variant><vt:variant><vt:i4>1</vt:i4>"),
        "{xml}"
    );
    assert!(
        xml.contains(&format!(
            "<vt:lpstr>Worksheets</vt:lpstr></vt:variant><vt:variant><vt:i4>{count}</vt:i4>"
        )),
        "{xml}"
    );
    assert!(
        xml.contains(&format!(r#"size="{}" baseType="lpstr""#, names.len() + 2)),
        "{xml}"
    );
    let mut ordered = "<vt:lpstr>Keep</vt:lpstr>".to_string();
    for name in names {
        ordered.push_str(&format!("<vt:lpstr>{name}</vt:lpstr>"));
    }
    ordered.push_str("<vt:lpstr>Later title</vt:lpstr>");
    assert!(xml.contains(&ordered), "{xml}");
    assert!(xml.contains("<Company>Keep &amp; Co</Company>"));
    assert!(xml.contains("<!--preserve-comment-->"));
}

#[test]
fn worksheet_title_counts_exclude_chartsheets_across_lifecycle_edits() {
    let xml = metadata()
        .replace("<vt:i4>2</vt:i4>", "<vt:i4>1</vt:i4>")
        .replace("<vt:lpstr>Keep</vt:lpstr>", "")
        .replace(
            r#"size="4" baseType="lpstr""#,
            r#"size="3" baseType="lpstr""#,
        )
        .replace("Earlier title", "Keep");
    let original = with_metadata_chartsheet(&fixture(&xml), 2);
    let admitted = Workbook::open(&original).unwrap();
    assert_eq!(admitted.sheet_names(), vec!["Data", "Keep"]);
    assert_eq!(admitted.worksheets().len(), 1);
    assert_eq!(
        admitted.sheet_by_name("Keep").unwrap().sheet_type(),
        rxls::SheetType::ChartSheet
    );
    let mut spreadsheet = Spreadsheet::open(&original).unwrap();
    let before = spreadsheet.save().unwrap();
    assert!(
        spreadsheet.delete_sheet("Data").is_err(),
        "chartsheet must not satisfy the last-worksheet rule"
    );
    assert_eq!(spreadsheet.save().unwrap(), before);
    assert!(spreadsheet.edited_parts().is_empty());
    spreadsheet.add_sheet("New").unwrap();
    let saved = spreadsheet.save().unwrap();
    assert_chart_and_worksheet_titles(&saved, 2, &["Data", "New"]);
    assert_eq!(
        part(&saved, "xl/worksheets/sheet2.xml"),
        part(&original, "xl/worksheets/sheet2.xml")
    );
    let mut spreadsheet = Spreadsheet::open(&saved).unwrap();
    spreadsheet.rename_sheet("New", "Added").unwrap();
    assert_chart_and_worksheet_titles(&spreadsheet.save().unwrap(), 2, &["Data", "Added"]);
    let mut spreadsheet = Spreadsheet::open(&spreadsheet.save().unwrap()).unwrap();
    spreadsheet.delete_sheet("Data").unwrap();
    let saved = spreadsheet.save().unwrap();
    assert_chart_and_worksheet_titles(&saved, 1, &["Added"]);
    let reopened = Workbook::open(&saved).unwrap();
    assert_eq!(reopened.sheet_names(), vec!["Keep", "Added"]);
    assert_eq!(reopened.worksheets().len(), 1);
    assert_eq!(
        reopened.sheet_by_name("Keep").unwrap().sheet_type(),
        rxls::SheetType::ChartSheet
    );
    assert_eq!(
        part(&saved, "xl/worksheets/sheet2.xml"),
        part(&original, "xl/worksheets/sheet2.xml")
    );
    assert_eq!(
        part(&saved, "xl/styles.xml"),
        part(&original, "xl/styles.xml")
    );
}

#[test]
fn zero_worksheet_title_group_adds_a_title_with_the_vector_binding() {
    let xml = format!(
        r#"<Properties xmlns="{EP}" xmlns:h="{VT}" xmlns:z="urn:foreign"><HeadingPairs><h:vector size="2" baseType="variant"><h:variant><h:lpstr>Worksheets</h:lpstr></h:variant><h:variant><h:i4>0</h:i4></h:variant></h:vector></HeadingPairs><TitlesOfParts><z:vector xmlns:z="{VT}" size="0" baseType="lpstr"/></TitlesOfParts><!--preserve-comment--><Extension keep="untouched"/></Properties>"#
    );
    let original = with_metadata_chartsheet(&with_metadata_chartsheet(&fixture(&xml), 1), 2);
    let admitted = Workbook::open(&original).unwrap();
    assert!(admitted.worksheets().is_empty());
    assert_eq!(admitted.sheet_names(), vec!["Data", "Keep"]);
    assert!(admitted
        .sheets
        .iter()
        .all(|sheet| sheet.sheet_type() == rxls::SheetType::ChartSheet));
    let mut spreadsheet = Spreadsheet::open(&original).unwrap();
    spreadsheet.add_sheet("New").unwrap();
    let saved = spreadsheet.save().unwrap();
    let xml = app(&saved);
    assert!(xml.contains("<h:i4>1</h:i4>"));
    assert!(xml.contains(r#"size="1" baseType="lpstr""#));
    assert!(
        xml.contains("<z:lpstr>New</z:lpstr>"),
        "the vector's own z binding must override the foreign root binding: {xml}"
    );
    assert!(xml.contains(r#"xmlns:z="urn:foreign""#));
    assert!(xml.contains("<!--preserve-comment-->"));
    let reopened = Workbook::open(&saved).unwrap();
    assert_eq!(
        reopened
            .worksheets()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["New"]
    );
    for number in 1..=2 {
        let path = format!("xl/worksheets/sheet{number}.xml");
        assert_eq!(part(&saved, &path), part(&original, &path));
    }
    let mut spreadsheet = Spreadsheet::open(&saved).unwrap();
    spreadsheet.rename_sheet("New", "Renamed").unwrap();
    let saved = spreadsheet.save().unwrap();
    assert!(app(&saved).contains("<z:lpstr>Renamed</z:lpstr>"));
    let before = spreadsheet.save().unwrap();
    let edited = spreadsheet.edited_parts().to_vec();
    assert!(spreadsheet.delete_sheet("Renamed").is_err());
    assert_eq!(spreadsheet.save().unwrap(), before);
    assert_eq!(spreadsheet.edited_parts(), edited);
}

#[test]
fn reordered_worksheet_titles_preserve_order_and_match_exact_names() {
    let xml = metadata().replace(
        "<vt:lpstr>Data</vt:lpstr><vt:lpstr>Keep</vt:lpstr>",
        "<vt:lpstr>Keep</vt:lpstr><vt:lpstr>Data</vt:lpstr>",
    );
    let mut spreadsheet = Spreadsheet::open(&fixture(&xml)).unwrap();
    spreadsheet.add_sheet("New").unwrap();
    let saved = spreadsheet.save().unwrap();
    assert_metadata(&saved, 3, &["Keep", "Data", "New"]);
    assert_eq!(
        Workbook::open(&saved).unwrap().sheet_names(),
        vec!["Data", "Keep", "New"]
    );
    let mut spreadsheet = Spreadsheet::open(&saved).unwrap();
    spreadsheet.rename_sheet("Data", "Renamed").unwrap();
    let saved = spreadsheet.save().unwrap();
    assert_metadata(&saved, 3, &["Keep", "Renamed", "New"]);
    assert_eq!(
        Workbook::open(&saved).unwrap().sheet_names(),
        vec!["Renamed", "Keep", "New"]
    );
    let mut spreadsheet = Spreadsheet::open(&saved).unwrap();
    spreadsheet.delete_sheet("Keep").unwrap();
    let saved = spreadsheet.save().unwrap();
    assert_metadata(&saved, 2, &["Renamed", "New"]);
    assert_eq!(
        Workbook::open(&saved).unwrap().sheet_names(),
        vec!["Renamed", "New"]
    );
}
