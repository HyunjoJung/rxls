//! Black-box CLI tests over the public binary surface.

use std::process::Command;

#[cfg(feature = "xlsx")]
use rxls::{Cell, Workbook};

#[cfg(any(feature = "xlsx", feature = "ods"))]
fn json_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\")
}

#[test]
fn cli_version_reports_crate_identity() {
    let bin = std::env::var("CARGO_BIN_EXE_rxls").unwrap_or_else(|_| {
        if cfg!(windows) {
            "target\\debug\\rxls.exe".to_string()
        } else {
            "target/debug/rxls".to_string()
        }
    });
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .expect("run rxls --version");

    assert!(
        output.status.success(),
        "rxls --version failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert_eq!(stdout.trim(), format!("rxls {}", env!("CARGO_PKG_VERSION")));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_info_reports_workbook_and_sheet_structure() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xlsx/reader-structural.xlsx"
    );
    let bin = std::env::var("CARGO_BIN_EXE_rxls").unwrap_or_else(|_| {
        if cfg!(windows) {
            "target\\debug\\rxls.exe".to_string()
        } else {
            "target/debug/rxls".to_string()
        }
    });
    let output = Command::new(bin)
        .args(["info", fixture])
        .output()
        .expect("run rxls info");

    assert!(
        output.status.success(),
        "rxls info failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls info"));
    assert!(stdout.contains("format: xlsx"));
    assert!(stdout.contains("sheets: 2"));
    assert!(stdout.contains("defined_names: 1"));
    assert!(stdout.contains("title: rxls structural fixture"));
    assert!(stdout.contains("sheet[0]: Data type=worksheet visible=visible dimensions=A1:C5"));
    assert!(stdout.contains("sheet[1]: Hidden type=worksheet visible=hidden"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_dump_reports_bounded_sheet_cells() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xlsx/reader-structural.xlsx"
    );
    let bin = std::env::var("CARGO_BIN_EXE_rxls").unwrap_or_else(|_| {
        if cfg!(windows) {
            "target\\debug\\rxls.exe".to_string()
        } else {
            "target/debug/rxls".to_string()
        }
    });
    let output = Command::new(bin)
        .args(["dump", fixture, "--sheet", "0", "--limit", "4"])
        .output()
        .expect("run rxls dump");

    assert!(
        output.status.success(),
        "rxls dump failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls dump"));
    assert!(stdout.contains("sheet: 0 Data"));
    assert!(stdout.contains("limit: 4 cells"));
    assert!(stdout.contains("A1\ttext\titem"));
    assert!(stdout.contains("B1\ttext\tamount"));
    assert!(stdout.contains("C1\ttext\tok"));
    assert!(stdout.contains("A2\ttext\troad"));
    assert!(stdout.contains("truncated: true"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_dump_reports_formatted_display_text() {
    use rxls::Format;

    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("Display");
        sheet.write(0, 0, "completion");
        sheet.write_number_with_format(1, 0, 0.5, &Format::new().set_num_format("0%"));
    }

    let path = std::env::temp_dir().join(format!(
        "rxls_cli_dump_display_text_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, workbook.to_xlsx()).expect("write display fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "dump",
            path.to_str().expect("utf8 fixture path"),
            "--sheet",
            "0",
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls dump");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls dump failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains("A2\tnumber\t0.5\tdisplay=50%"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("truncated: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_csv_exports_sheet_with_delimiter() {
    use rxls::Format;

    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("CSV");
        sheet.write(0, 0, "name");
        sheet.write(0, 1, "note");
        sheet.write(0, 2, "percent");
        sheet.write(1, 0, "Road");
        sheet.write(1, 1, "quoted\t\"ok\"");
        sheet.write_number_with_format(1, 2, 0.5, &Format::new().set_num_format("0%"));
    }

    let path = std::env::temp_dir().join(format!(
        "rxls_cli_csv_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, workbook.to_xlsx()).expect("write csv fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "csv",
            path.to_str().expect("utf8 fixture path"),
            "--sheet",
            "0",
            "--delimiter",
            "\\t",
        ])
        .output()
        .expect("run rxls csv");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls csv failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert_eq!(
        stdout,
        "name\tnote\tpercent\nRoad\t\"quoted\t\"\"ok\"\"\"\t50%\n"
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_formula_reports_bounded_formula_cells() {
    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("Formulas");
        sheet.write(0, 0, 2.0);
        sheet.write(
            0,
            1,
            Cell::Formula {
                formula: "A1*2".into(),
                cached: Box::new(Cell::Number(4.0)),
            },
        );
        sheet.write(
            1,
            1,
            Cell::Formula {
                formula: "B1+1".into(),
                cached: Box::new(Cell::Number(5.0)),
            },
        );
    }
    let path = std::env::temp_dir().join(format!(
        "rxls_cli_formula_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, workbook.to_xlsx()).expect("write formula fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "formula",
            path.to_str().expect("utf8 fixture path"),
            "--sheet",
            "0",
            "--limit",
            "1",
        ])
        .output()
        .expect("run rxls formula");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls formula failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls formula"));
    assert!(stdout.contains("sheet: 0 Formulas"));
    assert!(stdout.contains("limit: 1 formulas"));
    assert!(stdout.contains("B1\tA1*2\tcached=number:4"));
    assert!(stdout.contains("truncated: true"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_diagnose_reports_machine_readable_counts() {
    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "value");
        sheet.write(
            1,
            0,
            Cell::Formula {
                formula: "A1".into(),
                cached: Box::new(Cell::Text("value".into())),
            },
        );
        sheet.freeze_panes(1, 1);
    }
    workbook.define_name("NamedValue", "Data!$A$1");

    let hidden_sheet = workbook.add_sheet("Hidden");
    hidden_sheet.hide();
    hidden_sheet.write(0, 0, "ignored");

    let path = std::env::temp_dir().join(format!(
        "rxls_cli_diagnose_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, workbook.to_xlsx()).expect("write diagnose fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").unwrap_or_else(|_| {
        if cfg!(windows) {
            "target\\debug\\rxls.exe".to_string()
        } else {
            "target/debug/rxls".to_string()
        }
    });
    let output = Command::new(bin)
        .args(["diagnose", path.to_str().expect("utf8 fixture path")])
        .output()
        .expect("run rxls diagnose");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls diagnose failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains(r#""format":"xlsx""#));
    assert!(
        stdout.contains(r#""stats":{"sheets":2,"cells":3,"formulas":1,"text_truncated":false}"#)
    );
    assert!(stdout.contains(r#""defined_names_count":1"#));
    assert!(stdout.contains(r#""features":{"comments":0,"data_validations":0,"tables":0,"merged_ranges":0,"hyperlinks":0,"images":0,"charts":0,"sparklines":0,"conditional_formatting":0,"hidden_sheets":1,"frozen_panes":1,"page_setup":0,"protection":0,"pivot_tables":0,"vba_project":false,"threaded_comments":0,"external_links":0,"custom_xml":0}"#));
    assert!(stdout.contains(r#""warnings":["FormulaCacheOnly"]"#));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_diagnose_reports_preserved_package_inventory() {
    let path = std::env::temp_dir().join(format!(
        "rxls_cli_diagnose_package_inventory_{}_{}.xlsm",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, synthetic_xlsm_with_inventory()).expect("write diagnose fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").unwrap_or_else(|_| {
        if cfg!(windows) {
            "target\\debug\\rxls.exe".to_string()
        } else {
            "target/debug/rxls".to_string()
        }
    });
    let output = Command::new(bin)
        .args(["diagnose", path.to_str().expect("utf8 fixture path")])
        .output()
        .expect("run rxls diagnose");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls diagnose failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains(r#""format":"xlsm""#));
    assert!(stdout.contains(r#""pivot_tables":1"#));
    assert!(stdout.contains(r#""vba_project":true"#));
    assert!(stdout.contains(r#""threaded_comments":1"#));
    assert!(stdout.contains(r#""external_links":1"#));
    assert!(stdout.contains(r#""custom_xml":1"#));
    assert!(stdout
        .contains(r#""warnings":["MacrosPresentNotExecuted","PivotTablesPreservedNotModeled"]"#));
}

#[cfg(feature = "xlsx")]
fn synthetic_xlsm_with_inventory() -> Vec<u8> {
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
    add(
        &mut zip,
        opt,
        "[Content_Types].xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="bin" ContentType="application/vnd.ms-office.vbaProject"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.ms-excel.sheet.macroEnabled.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
    );
    add(
        &mut zip,
        opt,
        "_rels/.rels",
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/workbook.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/_rels/workbook.xml.rels",
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.microsoft.com/office/2006/relationships/vbaProject" Target="vbaProject.bin"/></Relationships>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/worksheets/sheet1.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>inventory</t></is></c></row></sheetData></worksheet>"#,
    );
    add(&mut zip, opt, "xl/vbaProject.bin", b"macro payload");
    add(
        &mut zip,
        opt,
        "xl/pivotTables/pivotTable1.xml",
        b"<pivotTableDefinition/>",
    );
    add(
        &mut zip,
        opt,
        "xl/threadedComments/threadedComment1.xml",
        b"<ThreadedComments/>",
    );
    add(
        &mut zip,
        opt,
        "xl/externalLinks/externalLink1.xml",
        b"<externalLink/>",
    );
    add(&mut zip, opt, "customXml/item1.xml", b"<root/>");
    add(&mut zip, opt, "customXml/itemProps1.xml", b"<props/>");
    zip.finish().unwrap().into_inner()
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_metadata_reports_workbook_metadata() {
    let mut workbook = Workbook::new();
    workbook.properties.title = Some("CLI Metadata".into());
    workbook.define_name("NamedTotal", "Summary!$B$2");
    workbook.protect_structure();
    workbook.add_sheet("Data").write(0, 0, "item");
    workbook.add_sheet("Summary").write(0, 0, "total");
    workbook.set_active_sheet(1);

    let path = std::env::temp_dir().join(format!(
        "rxls_cli_metadata_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, workbook.to_xlsx()).expect("write metadata fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args(["metadata", path.to_str().expect("utf8 fixture path")])
        .output()
        .expect("run rxls metadata");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls metadata failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls metadata"));
    assert!(stdout.contains("date1904: false"));
    assert!(stdout.contains("partial: false"));
    assert!(stdout.contains("structure_protected: true"));
    assert!(stdout.contains("active_sheet: 1"));
    assert!(stdout.contains("active_sheet_name: Summary"));
    assert!(stdout.contains("title: CLI Metadata"));
    assert!(stdout.contains("defined_name: NamedTotal=Summary!$B$2"));
    assert!(stdout.contains("sheet[0]: Data type=worksheet visible=visible"));
    assert!(stdout.contains("sheet[1]: Summary type=worksheet visible=visible"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_metadata_reports_aggregate_r3_metadata_counts() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xlsx/reader-structural.xlsx"
    );
    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args(["metadata", fixture])
        .output()
        .expect("run rxls metadata");

    assert!(
        output.status.success(),
        "rxls metadata failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("metadata_merged_ranges: 1"));
    assert!(stdout.contains("metadata_hyperlinks: 1"));
    assert!(stdout.contains("metadata_comments: 1"));
    assert!(stdout.contains("metadata_tables: 1"));
    assert!(stdout.contains("metadata_data_validations: 0"));
    assert!(stdout.contains("metadata_conditional_formats: 0"));
    assert!(stdout.contains("metadata_autofilters: 1"));
    assert!(stdout.contains("metadata_page_setups: 1"));
    assert!(stdout.contains("metadata_images: 0"));
    assert!(stdout.contains("metadata_charts: 0"));
    assert!(stdout.contains("metadata_sheet_views: 1"));
    assert!(stdout.contains("metadata_tab_colors: 1"));
    assert!(stdout.contains("metadata_print_options: 1"));
    assert!(stdout.contains("metadata_sparklines: 0"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_metadata_reports_sheet_display_metadata_values() {
    use rxls::ProtectionOptions;

    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("Layout");
        sheet.write(0, 0, "status");
        sheet.freeze_panes(1, 1);
        sheet.hide_gridlines();
        sheet.set_zoom(125);
        sheet.set_show_headers(false);
        sheet.set_right_to_left(true);
        sheet.set_tab_color([0x12, 0x34, 0x56]);
        sheet.set_print_gridlines();
        sheet.set_print_headings();
        sheet.protect_with(ProtectionOptions {
            sort: true,
            auto_filter: true,
            ..Default::default()
        });
        sheet.group_rows(1, 3, 1);
        sheet.group_cols(1, 2, 1);
        sheet.collapse_row(4);
        sheet.set_outline_summary(false, false);
    }
    workbook.add_sheet("Plain").write(0, 0, "plain");

    let path = std::env::temp_dir().join(format!(
        "rxls_cli_metadata_sheet_display_metadata_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, workbook.to_xlsx()).expect("write metadata display fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args(["metadata", path.to_str().expect("utf8 fixture path")])
        .output()
        .expect("run rxls metadata");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls metadata failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("sheet[0]: Layout type=worksheet visible=visible"));
    assert!(stdout.contains("sheet[1]: Plain type=worksheet visible=visible"));
    assert!(stdout.contains(
        "sheet_detail[0].sheet_view: freeze=1,1 hide_gridlines=true zoom=125 show_headers=false right_to_left=true"
    ));
    assert!(stdout.contains("sheet_detail[0].tab_color: 123456"));
    assert!(stdout.contains("sheet_detail[0].print_options: gridlines=true headings=true"));
    assert!(stdout.contains("sheet_detail[0].protection: protected=true options=sort,auto_filter"));
    assert!(stdout.contains("sheet_detail[0].row_outline: 2:1,3:1,4:1"));
    assert!(stdout.contains("sheet_detail[0].col_outline: B:1,C:1"));
    assert!(stdout.contains("sheet_detail[0].collapsed_rows: 5"));
    assert!(stdout.contains("sheet_detail[0].outline_summary: below=false,right=false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_inspect_package_reports_bounded_zip_parts() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xlsx/reader-structural.xlsx"
    );
    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args(["inspect-package", fixture, "--limit", "3"])
        .output()
        .expect("run rxls inspect-package");

    assert!(
        output.status.success(),
        "rxls inspect-package failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls inspect-package"));
    assert!(stdout.contains("format: xlsx"));
    assert!(stdout.contains("parts: "));
    assert!(stdout.contains("compressed_bytes: "));
    assert!(stdout.contains("uncompressed_bytes: "));
    assert!(stdout.contains("part: [Content_Types].xml"));
    assert!(stdout.contains("part: _rels/.rels"));
    assert!(stdout.contains("truncated: true"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_inspect_output_reports_generated_writer_package() {
    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "name");
        sheet.write(0, 1, "amount");
        sheet.write(1, 0, "road");
        sheet.write(1, 1, 10.0);
    }
    workbook.define_name("NamedAmount", "Data!$B$2");

    let path = std::env::temp_dir().join(format!(
        "rxls_cli_inspect_output_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, workbook.to_xlsx()).expect("write workbook fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "inspect-output",
            path.to_str().expect("utf8 fixture path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls inspect-output");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls inspect-output failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls inspect-output"));
    assert!(stdout.contains("source_format: xlsx"));
    assert!(stdout.contains("output_format: xlsx"));
    assert!(stdout.contains("output_bytes: "));
    assert!(stdout.contains("worksheet_parts: 1"));
    assert!(stdout.contains("shared_strings_part: true"));
    assert!(stdout.contains("styles_part: true"));
    assert!(stdout.contains("relationships: "));
    assert!(stdout.contains("readback_sheets: 1"));
    assert!(stdout.contains("readback_cells: 4"));
    assert!(stdout.contains("readback_defined_names: 1"));
    assert!(stdout.contains("readback_partial: false"));
    assert!(stdout.contains("semantic_differences: 0"));
    assert!(stdout.contains("semantic_truncated: false"));
    assert!(stdout.contains("semantic_equal: true"));
    assert!(stdout.contains("part: [Content_Types].xml"));
    assert!(stdout.contains("truncated: true"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_inspect_output_reports_readback_metadata_counts() {
    use rxls::{
        CfRule, Color, CondFormat, DataValidation, DvOp, PageSetup, Sparkline, SparklineKind,
    };

    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("Meta");
        sheet.write(0, 0, "status");
        sheet.write(0, 1, "score");
        for row in 1..=3u32 {
            sheet.write(row, 0, if row % 2 == 0 { "Yes" } else { "No" });
            sheet.write(row, 1, f64::from(row) * 10.0);
        }
        sheet.add_data_validation(DataValidation::list((1, 0, 3, 0), "\"Yes,No\""));
        sheet.add_conditional_format(CondFormat {
            sqref: (1, 1, 3, 1),
            rule: CfRule::CellIs {
                op: DvOp::GreaterThan,
                formula1: "15".into(),
                formula2: None,
                fill: Color::rgb(0xFF, 0xC7, 0xCE),
            },
        });
        sheet.set_page_setup(PageSetup {
            landscape: true,
            print_area: Some((0, 0, 3, 1)),
            repeat_rows: Some((0, 0)),
            ..PageSetup::default()
        });
        sheet.autofilter(0, 0, 3, 1);
        sheet.add_sparkline(Sparkline {
            location: (4, 1),
            range: "Meta!$B$2:$B$4".into(),
            kind: SparklineKind::Column,
        });
    }

    let path = std::env::temp_dir().join(format!(
        "rxls_cli_inspect_output_metadata_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, workbook.to_xlsx()).expect("write workbook fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "inspect-output",
            path.to_str().expect("utf8 fixture path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls inspect-output");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls inspect-output failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("readback_data_validations: 1"));
    assert!(stdout.contains("readback_conditional_formats: 1"));
    assert!(stdout.contains("readback_autofilters: 1"));
    assert!(stdout.contains("readback_page_setups: 1"));
    assert!(stdout.contains("readback_sparklines: 1"));
    assert!(stdout.contains("semantic_equal: true"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_inspect_output_reports_bounded_semantic_difference_rows() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Data" r:id="rId1"/><sheet name="Chart" state="hidden" r:id="rId2"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>ok</t></is></c></row></sheetData></worksheet>"#,
        ),
        ("xl/chartsheets/sheet1.xml", r#"<chartsheet/>"#),
    ];
    for (name, body) in parts {
        zip.start_file(name, options).expect("start fixture part");
        zip.write_all(body.as_bytes()).expect("write fixture part");
    }
    let bytes = zip.finish().expect("finish fixture").into_inner();

    let path = std::env::temp_dir().join(format!(
        "rxls_cli_inspect_output_semantic_difference_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, bytes).expect("write semantic difference fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "inspect-output",
            path.to_str().expect("utf8 fixture path"),
            "--limit",
            "1",
        ])
        .output()
        .expect("run rxls inspect-output");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls inspect-output failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("semantic_difference: sheet[1].type left=chart right=worksheet"));
    assert!(stdout.contains("semantic_differences: 1"));
    assert!(stdout.contains("semantic_truncated: false"));
    assert!(stdout.contains("semantic_equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_bounded_workbook_differences() {
    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Data");
        sheet.write(0, 0, "name");
        sheet.write(1, 1, 10.0);
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Data");
        sheet.write(0, 0, "name");
        sheet.write(1, 1, 11.0);
        sheet.write(2, 2, "extra");
    }

    let base = format!(
        "rxls_cli_compare_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "1",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls compare"));
    assert!(stdout.contains("left_format: xlsx"));
    assert!(stdout.contains("right_format: xlsx"));
    assert!(stdout.contains("limit: 1 differences"));
    assert!(stdout.contains("difference: sheet[0].Data!B2 left=number:10 right=number:11"));
    assert!(stdout.contains("truncated: true"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_formula_cached_value_differences() {
    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Formulas");
        sheet.write(0, 0, 2.0);
        sheet.write(
            0,
            1,
            Cell::Formula {
                formula: "A1*2".into(),
                cached: Box::new(Cell::Number(4.0)),
            },
        );
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Formulas");
        sheet.write(0, 0, 2.0);
        sheet.write(
            0,
            1,
            Cell::Formula {
                formula: "A1*2".into(),
                cached: Box::new(Cell::Number(5.0)),
            },
        );
    }

    let base = format!(
        "rxls_cli_compare_formula_cache_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "2",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for formula cached-value differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains(
        "difference: sheet[0].Formulas!B1 left=formula:=A1*2 cached=number:4 right=formula:=A1*2 cached=number:5"
    ));
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_formatted_display_text_differences() {
    use rxls::Format;

    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Display");
        sheet.write(0, 0, "completion");
        sheet.write_number_with_format(1, 0, 0.5, &Format::new().set_num_format("0%"));
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Display");
        sheet.write(0, 0, "completion");
        sheet.write(1, 0, 0.5);
    }

    let base = format!(
        "rxls_cli_compare_display_text_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for formatted display text differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains("difference: sheet[0].Display!A2.display left=50% right=0.5"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_workbook_view_metadata_differences() {
    let mut left = Workbook::new();
    left.add_sheet("Data").write(0, 0, "item");
    left.add_sheet("Summary").write(0, 0, "total");

    let mut right = Workbook::new();
    right.add_sheet("Data").write(0, 0, "item");
    right.add_sheet("Summary").write(0, 0, "total");
    right.protect_structure();
    right.set_active_sheet(1);

    let base = format!(
        "rxls_cli_compare_workbook_view_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for workbook view metadata differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("difference: workbook.structure_protected left=false right=true"));
    assert!(stdout.contains("difference: workbook.active_sheet left=0 right=1"));
    assert!(stdout.contains("difference: workbook.active_sheet_name left=Data right=Summary"));
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_workbook_property_and_defined_name_differences() {
    let mut left = Workbook::new();
    left.properties.title = Some("Left Report".into());
    left.properties.creator = Some("Analyst A".into());
    left.define_name("NamedTotal", "Data!$A$1");
    left.add_sheet("Data").write(0, 0, 10.0);

    let mut right = Workbook::new();
    right.properties.title = Some("Right Report".into());
    right.properties.creator = Some("Analyst B".into());
    right.define_name("NamedTotal", "Data!$B$1");
    right.add_sheet("Data").write(0, 0, 10.0);

    let base = format!(
        "rxls_cli_compare_workbook_metadata_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for workbook metadata differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains("difference: workbook.property.title left=Left Report right=Right Report")
    );
    assert!(stdout.contains("difference: workbook.property.creator left=Analyst A right=Analyst B"));
    assert!(stdout.contains(
        "difference: workbook.defined_name[0] left=NamedTotal=Data!$A$1 right=NamedTotal=Data!$B$1"
    ));
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_sheet_metadata_differences() {
    use rxls::DataValidation;

    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Data");
        sheet.write(0, 0, "status");
        sheet.write(1, 0, "Yes");
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Data");
        sheet.write(0, 0, "status");
        sheet.write(1, 0, "Yes");
        sheet.add_data_validation(DataValidation::list((1, 0, 1, 0), "\"Yes,No\""));
    }

    let base = format!(
        "rxls_cli_compare_metadata_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "2",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for metadata differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("difference: sheet[0].data_validations left=0 right=1"));
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_detailed_sheet_metadata_differences() {
    use rxls::{DataValidation, Table};

    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Data");
        sheet.write(0, 0, "status");
        sheet.write(0, 1, "note");
        sheet.write(1, 0, "Yes");
        sheet.write(1, 1, "old");
        sheet.write_url_with_text(2, 0, "https://example.test/old", "link");
        sheet.add_comment(0, 1, "old note", Some("Analyst"));
        sheet.add_table(Table {
            range: (0, 0, 1, 1),
            name: "OldTable".into(),
            columns: vec!["status".into(), "note".into()],
            style: None,
        });
        sheet.add_data_validation(DataValidation::list((1, 0, 1, 0), "\"Yes,No\""));
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Data");
        sheet.write(0, 0, "status");
        sheet.write(0, 1, "note");
        sheet.write(1, 0, "Yes");
        sheet.write(1, 1, "old");
        sheet.write_url_with_text(2, 0, "https://example.test/new", "link");
        sheet.add_comment(0, 1, "new note", Some("Analyst"));
        sheet.add_table(Table {
            range: (0, 0, 1, 1),
            name: "NewTable".into(),
            columns: vec!["status".into(), "note".into()],
            style: None,
        });
        sheet.add_data_validation(DataValidation::list((1, 0, 1, 0), "\"Open,Closed\""));
    }

    let base = format!(
        "rxls_cli_compare_metadata_detail_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "8",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for detailed metadata differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains(
        "difference: sheet[0].hyperlink[0] left=A3:https://example.test/old right=A3:https://example.test/new"
    ));
    assert!(stdout.contains(
        "difference: sheet[0].comment[0] left=B1:Analyst:old note right=B1:Analyst:new note"
    ));
    assert!(stdout.contains(
        "difference: sheet[0].table[0] left=OldTable A1:B2 [status,note] style=TableStyleMedium2 right=NewTable A1:B2 [status,note] style=TableStyleMedium2"
    ));
    assert!(stdout.contains(
        "difference: sheet[0].data_validation[0] left=A2:A2 list between \"Yes,No\" allow_blank=true show_input=true show_error=true prompt= error= right=A2:A2 list between \"Open,Closed\" allow_blank=true show_input=true show_error=true prompt= error="
    ));
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_table_style_differences() {
    use rxls::Table;

    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Tables");
        sheet.write(0, 0, "status");
        sheet.write(0, 1, "note");
        sheet.write(1, 0, "Open");
        sheet.write(1, 1, "same");
        sheet.add_table(Table {
            range: (0, 0, 1, 1),
            name: "StatusTable".into(),
            columns: vec!["status".into(), "note".into()],
            style: Some("TableStyleMedium2".into()),
        });
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Tables");
        sheet.write(0, 0, "status");
        sheet.write(0, 1, "note");
        sheet.write(1, 0, "Open");
        sheet.write(1, 1, "same");
        sheet.add_table(Table {
            range: (0, 0, 1, 1),
            name: "StatusTable".into(),
            columns: vec!["status".into(), "note".into()],
            style: Some("TableStyleMedium9".into()),
        });
    }

    let base = format!(
        "rxls_cli_compare_table_style_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for table style differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains(
            "difference: sheet[0].table[0] left=StatusTable A1:B2 [status,note] style=TableStyleMedium2 right=StatusTable A1:B2 [status,note] style=TableStyleMedium9"
        ),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_data_validation_option_differences() {
    use rxls::DataValidation;

    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Rules");
        sheet.write(0, 0, "status");
        sheet.write(1, 0, "Yes");
        let mut rule = DataValidation::list((1, 0, 3, 0), "\"Yes,No\"");
        rule.allow_blank = false;
        rule.show_input_message = true;
        rule.show_error_message = true;
        rule.prompt = Some(("Input".into(), "Pick Yes or No".into()));
        rule.error = Some(("Invalid".into(), "Use Yes or No".into()));
        sheet.add_data_validation(rule);
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Rules");
        sheet.write(0, 0, "status");
        sheet.write(1, 0, "Yes");
        let mut rule = DataValidation::list((1, 0, 3, 0), "\"Yes,No\"");
        rule.allow_blank = true;
        rule.show_input_message = false;
        rule.show_error_message = false;
        rule.prompt = Some(("Prompt".into(), "Choose a value".into()));
        rule.error = Some(("Stop".into(), "Different message".into()));
        sheet.add_data_validation(rule);
    }

    let base = format!(
        "rxls_cli_compare_data_validation_options_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for data-validation option differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains(
            "difference: sheet[0].data_validation[0] left=A2:A4 list between \"Yes,No\" allow_blank=false show_input=true show_error=true prompt=Input:Pick Yes or No error=Invalid:Use Yes or No right=A2:A4 list between \"Yes,No\" allow_blank=true show_input=false show_error=false prompt=Prompt:Choose a value error=Stop:Different message"
        ),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_page_setup_margin_differences() {
    use rxls::PageSetup;

    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Print");
        sheet.write(0, 0, "status");
        sheet.write(1, 0, "same");
        sheet.set_page_setup(PageSetup {
            margins: Some((0.5, 0.6, 0.7, 0.8, 0.2, 0.3)),
            ..Default::default()
        });
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Print");
        sheet.write(0, 0, "status");
        sheet.write(1, 0, "same");
        sheet.set_page_setup(PageSetup {
            margins: Some((0.9, 1.0, 1.1, 1.2, 0.4, 0.5)),
            ..Default::default()
        });
    }

    let base = format!(
        "rxls_cli_compare_page_setup_margins_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for page setup margin differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains(
            "difference: sheet[0].page_setup[0] left=landscape=false margins=0.5,0.6,0.7,0.8,0.2,0.3 print_area= repeat_rows= repeat_cols= fit=x scale= first_page= centered=falsexfalse paper= header= footer= right=landscape=false margins=0.9,1,1.1,1.2,0.4,0.5 print_area= repeat_rows= repeat_cols= fit=x scale= first_page= centered=falsexfalse paper= header= footer="
        ),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_sheet_view_tab_color_and_print_option_differences() {
    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Display");
        sheet.write(0, 0, "status");
        sheet.write(1, 0, "same");
        sheet.freeze_panes(1, 1);
        sheet.hide_gridlines();
        sheet.set_show_headers(false);
        sheet.set_zoom(125);
        sheet.set_tab_color([0x12, 0x34, 0x56]);
        sheet.set_print_gridlines();
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Display");
        sheet.write(0, 0, "status");
        sheet.write(1, 0, "same");
        sheet.freeze_panes(2, 2);
        sheet.set_right_to_left(true);
        sheet.set_zoom(150);
        sheet.set_tab_color([0xAB, 0xCD, 0xEF]);
        sheet.set_print_headings();
    }

    let base = format!(
        "rxls_cli_compare_sheet_view_metadata_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for sheet view metadata differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains(
            "difference: sheet[0].sheet_view left=freeze=1,1 hide_gridlines=true zoom=125 show_headers=false right_to_left=false right=freeze=2,2 hide_gridlines=false zoom=150 show_headers=none right_to_left=true"
        ),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("difference: sheet[0].tab_color left=123456 right=ABCDEF"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains(
        "difference: sheet[0].print_options left=gridlines=true headings=false right=gridlines=false headings=true"
    ));
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_sheet_protection_and_outline_differences() {
    use rxls::ProtectionOptions;

    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Layout");
        sheet.write(0, 0, "status");
        sheet.write(1, 0, "same");
        sheet.protect_with(ProtectionOptions {
            sort: true,
            auto_filter: true,
            ..Default::default()
        });
        sheet.group_rows(1, 3, 1);
        sheet.group_cols(1, 2, 1);
        sheet.collapse_row(4);
        sheet.set_outline_summary(false, false);
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Layout");
        sheet.write(0, 0, "status");
        sheet.write(1, 0, "same");
        sheet.protect_with(ProtectionOptions {
            format_cells: true,
            ..Default::default()
        });
        sheet.group_rows(1, 3, 2);
        sheet.group_cols(1, 2, 2);
        sheet.collapse_row(5);
    }

    let base = format!(
        "rxls_cli_compare_sheet_outline_metadata_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "8",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for protection and outline differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains(
            "difference: sheet[0].protection left=protected=true options=sort,auto_filter right=protected=true options=format_cells"
        ),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("difference: sheet[0].row_outline left=2:1,3:1,4:1 right=2:2,3:2,4:2"));
    assert!(stdout.contains("difference: sheet[0].col_outline left=B:1,C:1 right=B:2,C:2"));
    assert!(stdout.contains("difference: sheet[0].collapsed_rows left=5 right=6"));
    assert!(stdout.contains(
        "difference: sheet[0].outline_summary left=below=false,right=false right=below=true,right=true"
    ));
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_chart_series_and_axis_differences() {
    use rxls::{Chart, ChartKind, Series};

    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("ChartData");
        for row in 0..3u32 {
            sheet.write(row, 0, format!("c{row}"));
            sheet.write(row, 1, f64::from(row + 1));
            sheet.write(row, 2, f64::from(row + 10));
        }
        sheet.add_chart(Chart {
            kind: ChartKind::Line,
            title: Some("Trend".into()),
            series: vec![Series {
                name: Some("Value".into()),
                categories: Some("ChartData!$A$1:$A$3".into()),
                values: "ChartData!$B$1:$B$3".into(),
                bubble_sizes: None,
            }],
            legend: true,
            data_labels: true,
            x_axis_title: Some("Category".into()),
            y_axis_title: Some("Amount".into()),
            from: (5, 1),
            to: (14, 7),
        });
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("ChartData");
        for row in 0..3u32 {
            sheet.write(row, 0, format!("c{row}"));
            sheet.write(row, 1, f64::from(row + 1));
            sheet.write(row, 2, f64::from(row + 10));
        }
        sheet.add_chart(Chart {
            kind: ChartKind::Line,
            title: Some("Trend".into()),
            series: vec![Series {
                name: Some("Value".into()),
                categories: Some("ChartData!$A$1:$A$3".into()),
                values: "ChartData!$C$1:$C$3".into(),
                bubble_sizes: None,
            }],
            legend: true,
            data_labels: true,
            x_axis_title: Some("Segment".into()),
            y_axis_title: Some("Revenue".into()),
            from: (5, 1),
            to: (14, 7),
        });
    }

    let base = format!(
        "rxls_cli_compare_chart_series_axis_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for chart series and axis differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains(
            "difference: sheet[0].chart[0] left=line title=Trend x_axis=Category y_axis=Amount series=[name=Value categories=ChartData!$A$1:$A$3 values=ChartData!$B$1:$B$3 bubble=] legend=true labels=true anchor=B6->H15 right=line title=Trend x_axis=Segment y_axis=Revenue series=[name=Value categories=ChartData!$A$1:$A$3 values=ChartData!$C$1:$C$3 bubble=] legend=true labels=true anchor=B6->H15"
        ),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_compare_reports_image_payload_differences() {
    use rxls::{Image, ImageFmt};

    let mut left = Workbook::new();
    {
        let sheet = left.add_sheet("Images");
        sheet.write(0, 0, "same");
        sheet.add_image(Image {
            data: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            format: ImageFmt::Png,
            from: (1, 1),
            to: Some((4, 4)),
        });
    }

    let mut right = Workbook::new();
    {
        let sheet = right.add_sheet("Images");
        sheet.write(0, 0, "same");
        sheet.add_image(Image {
            data: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12],
            format: ImageFmt::Png,
            from: (1, 1),
            to: Some((4, 4)),
        });
    }

    let base = format!(
        "rxls_cli_compare_image_payload_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let left_path = std::env::temp_dir().join(format!("{base}_left.xlsx"));
    let right_path = std::env::temp_dir().join(format!("{base}_right.xlsx"));
    std::fs::write(&left_path, left.to_xlsx()).expect("write left workbook");
    std::fs::write(&right_path, right.to_xlsx()).expect("write right workbook");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "compare",
            left_path.to_str().expect("utf8 left path"),
            right_path.to_str().expect("utf8 right path"),
            "--limit",
            "4",
        ])
        .output()
        .expect("run rxls compare");
    let _ = std::fs::remove_file(&left_path);
    let _ = std::fs::remove_file(&right_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "rxls compare should return 1 for same-size image payload differences: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(
        stdout.contains(
            "difference: sheet[0].image[0] left=png:B2->E5 bytes=12 digest=2d7d4819416d7fb9 right=png:B2->E5 bytes=12 digest=2d7d4919416d816c"
        ),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("truncated: false"));
    assert!(stdout.contains("equal: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_corpus_report_summarizes_manifest_parse_results() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "ok");

    let base = std::env::temp_dir().join(format!(
        "rxls_cli_corpus_report_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::create_dir_all(&base).expect("create temp corpus dir");
    let valid_path = base.join("valid.xlsx");
    let bad_path = base.join("bad.xlsx");
    let bad_zip_path = base.join("bad-zip.xlsx");
    let manifest_path = base.join("manifest.json");
    std::fs::write(&valid_path, workbook.to_xlsx()).expect("write valid workbook");
    std::fs::write(&bad_path, b"not a workbook").expect("write corrupt workbook");
    std::fs::write(&bad_zip_path, b"PK\x03\x04truncated").expect("write corrupt ZIP workbook");
    std::fs::write(
        &manifest_path,
        format!(
            r#"{{
  "files": [
    {{"source": "test", "path": "valid.xlsx", "local_path": "{}"}},
    {{"source": "test", "path": "bad.xlsx", "local_path": "{}"}},
    {{"source": "test", "path": "bad-zip.xlsx", "local_path": "{}"}}
  ]
}}"#,
            json_path(&valid_path),
            json_path(&bad_path),
            json_path(&bad_zip_path)
        ),
    )
    .expect("write manifest");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "corpus-report",
            manifest_path.to_str().expect("utf8 manifest path"),
            "--limit",
            "1",
        ])
        .output()
        .expect("run rxls corpus-report");
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        output.status.success(),
        "rxls corpus-report failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls corpus-report"));
    assert!(stdout.contains("manifest_files: 3"));
    assert!(stdout.contains("eligible_files: 3"));
    assert!(stdout.contains("opened: 1"));
    assert!(stdout.contains("failed: 2"));
    assert!(stdout.contains("expected_rejections: 2"));
    assert!(stdout.contains("unexpected_failures: 0"));
    assert!(stdout.contains("by_ext: .xlsx files=3 opened=1 failed=2"));
    assert!(stdout.contains("by_failure_kind: invalid_zip failed=1"));
    assert!(stdout.contains("by_failure_kind: not_ole2 failed=1"));
    assert!(stdout.contains("by_failure_decision: excluded_malformed_container failed=2"));
    assert!(stdout
        .contains("failure: .xlsx bad.xlsx kind=not_ole2 decision=excluded_malformed_container"));
    assert!(stdout.contains("truncated: true"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_corpus_report_fails_on_unexpected_io_errors() {
    let base = std::env::temp_dir().join(format!(
        "rxls_cli_corpus_report_io_error_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::create_dir_all(&base).expect("create temp corpus dir");
    let missing_path = base.join("missing.xlsx");
    let manifest_path = base.join("manifest.json");
    std::fs::write(
        &manifest_path,
        format!(
            r#"{{
  "files": [
    {{"source": "test", "path": "missing.xlsx", "local_path": "{}"}}
  ]
}}"#,
            json_path(&missing_path)
        ),
    )
    .expect("write manifest");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "corpus-report",
            manifest_path.to_str().expect("utf8 manifest path"),
            "--limit",
            "5",
        ])
        .output()
        .expect("run rxls corpus-report");
    let _ = std::fs::remove_dir_all(&base);

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("failed: 1"));
    assert!(stdout.contains("expected_rejections: 0"));
    assert!(stdout.contains("unexpected_failures: 1"));
    assert!(stdout.contains("by_failure_decision: needs_io_triage failed=1"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_corpus_report_tags_misleading_zip_signature_failures() {
    let base = std::env::temp_dir().join(format!(
        "rxls_cli_corpus_report_misleading_zip_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::create_dir_all(&base).expect("create temp corpus dir");
    let misleading_path = base.join("misleading.xls");
    let manifest_path = base.join("manifest.json");
    std::fs::write(&misleading_path, b"PK\x03\x04truncated")
        .expect("write corrupt ZIP workbook with misleading extension");
    std::fs::write(
        &manifest_path,
        format!(
            r#"{{
  "files": [
    {{"source": "test", "path": "misleading.xls", "local_path": "{}"}}
  ]
}}"#,
            json_path(&misleading_path)
        ),
    )
    .expect("write manifest");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "corpus-report",
            manifest_path.to_str().expect("utf8 manifest path"),
            "--limit",
            "5",
        ])
        .output()
        .expect("run rxls corpus-report");
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        output.status.success(),
        "rxls corpus-report failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls corpus-report"));
    assert!(stdout.contains("by_failure_kind: invalid_zip failed=1"));
    assert!(stdout.contains("by_failure_evidence: zip_signature_misleading_extension failed=1"));
    assert!(stdout.contains("failure: .xls misleading.xls kind=invalid_zip decision=excluded_malformed_container evidence=zip_signature_misleading_extension container=zip extension_mismatch=true"));
    assert!(stdout.contains("truncated: false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_corpus_report_counts_xlsm_as_ooxml_eligible() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("MacroLike").write(0, 0, "ok");

    let base = std::env::temp_dir().join(format!(
        "rxls_cli_corpus_report_xlsm_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::create_dir_all(&base).expect("create temp corpus dir");
    let xlsm_path = base.join("macro-enabled.xlsm");
    let manifest_path = base.join("manifest.json");
    std::fs::write(&xlsm_path, workbook.to_xlsx()).expect("write xlsm-labelled workbook");
    std::fs::write(
        &manifest_path,
        format!(
            r#"{{
  "files": [
    {{"source": "test", "path": "macro-enabled.xlsm", "local_path": "{}"}}
  ]
}}"#,
            json_path(&xlsm_path)
        ),
    )
    .expect("write manifest");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "corpus-report",
            manifest_path.to_str().expect("utf8 manifest path"),
            "--limit",
            "5",
        ])
        .output()
        .expect("run rxls corpus-report");
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        output.status.success(),
        "rxls corpus-report failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("manifest_files: 1"));
    assert!(stdout.contains("eligible_files: 1"));
    assert!(stdout.contains("opened: 1"));
    assert!(stdout.contains("failed: 0"));
    assert!(stdout.contains("expected_rejections: 0"));
    assert!(stdout.contains("unexpected_failures: 0"));
    assert!(stdout.contains("skipped: 0"));
    assert!(stdout.contains("by_ext: .xlsm files=1 opened=1 failed=0"));
}

#[cfg(feature = "ods")]
#[test]
fn cli_corpus_report_tags_encrypted_ods_evidence() {
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;

    let base = std::env::temp_dir().join(format!(
        "rxls_cli_corpus_report_encrypted_ods_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::create_dir_all(&base).expect("create temp corpus dir");
    let ods_path = base.join("protected.ods");
    let manifest_path = base.join("manifest.json");
    let odf_manifest = r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
  <manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet">
    <manifest:encryption-data manifest:checksum-type="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0#sha256-1k" manifest:checksum="abc"/>
  </manifest:file-entry>
  <manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml">
    <manifest:encryption-data manifest:checksum-type="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0#sha256-1k" manifest:checksum="abc"/>
  </manifest:file-entry>
</manifest:manifest>"#;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    zip.start_file("mimetype", options).unwrap();
    zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
        .unwrap();
    zip.start_file("content.xml", options).unwrap();
    zip.write_all(&[0xff, 0xfe, 0xfd, 0xfc]).unwrap();
    zip.start_file("META-INF/manifest.xml", options).unwrap();
    zip.write_all(odf_manifest.as_bytes()).unwrap();
    std::fs::write(&ods_path, zip.finish().unwrap().into_inner()).expect("write encrypted ods");
    std::fs::write(
        &manifest_path,
        format!(
            r#"{{
  "files": [
    {{"source": "test", "path": "protected.ods", "local_path": "{}"}}
  ]
}}"#,
            json_path(&ods_path)
        ),
    )
    .expect("write manifest");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "corpus-report",
            manifest_path.to_str().expect("utf8 manifest path"),
            "--limit",
            "5",
        ])
        .output()
        .expect("run rxls corpus-report");
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        output.status.success(),
        "rxls corpus-report failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("manifest_files: 1"));
    assert!(stdout.contains("eligible_files: 1"));
    assert!(stdout.contains("opened: 0"));
    assert!(stdout.contains("failed: 1"));
    assert!(stdout.contains("expected_rejections: 1"));
    assert!(stdout.contains("unexpected_failures: 0"));
    assert!(stdout.contains("by_ext: .ods files=1 opened=0 failed=1"));
    assert!(stdout.contains("by_failure_kind: unsupported_encrypted_opendocument failed=1"));
    assert!(stdout.contains("by_failure_decision: unsupported_encrypted failed=1"));
    assert!(stdout.contains("by_failure_evidence: encrypted_opendocument_package failed=1"));
    assert!(stdout.contains("failure: .ods protected.ods kind=unsupported_encrypted_opendocument decision=unsupported_encrypted evidence=encrypted_opendocument_package container=zip extension_mismatch=false parse: unsupported encrypted OpenDocument package"));
    assert!(stdout.contains("truncated: false"));
}

#[cfg(all(feature = "xlsx", feature = "xlsb", feature = "ods"))]
#[test]
fn cli_fixture_report_validates_committed_fixture_manifest() {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/MANIFEST.json");
    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args(["fixture-report", manifest, "--limit", "4"])
        .output()
        .expect("run rxls fixture-report");

    assert!(
        output.status.success(),
        "rxls fixture-report failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls fixture-report"));
    assert!(stdout.contains("manifest_entries: 4"));
    assert!(stdout.contains("opened: 4"));
    assert!(stdout.contains("hash_ok: 4"));
    assert!(stdout.contains("oracle_entries: 4"));
    assert!(stdout.contains("oracle_ok: 4"));
    assert!(stdout.contains("oracle_failed: 0"));
    assert!(stdout.contains("failed: 0"));
    assert!(stdout.contains("coverage_tags: "));
    assert!(stdout.contains("by_format: ods files=1 opened=1 hash_ok=1"));
    assert!(stdout.contains("by_format: xls files=1 opened=1 hash_ok=1"));
    assert!(stdout.contains("by_format: xlsb files=1 opened=1 hash_ok=1"));
    assert!(stdout.contains("by_format: xlsx files=1 opened=1 hash_ok=1"));
    assert!(stdout.contains(
        "fixture: xlsx tests/fixtures/xlsx/reader-structural.xlsx hash=ok open=ok oracle=ok covers="
    ));
    assert!(stdout.contains(
        "fixture: ods tests/fixtures/ods/repeated-hidden.ods hash=ok open=ok oracle=ok covers="
    ));
    assert!(stdout.contains("oracle: xls tests/fixtures/xls/reader-basic.xls sheets=2 cells=7 defined_names=1 hidden_sheets=1 merged_ranges=1 hyperlinks=1 comments=1 tables=0 data_validations=0 autofilters=0 page_setups=0 images=0 sheet_views=0 tab_colors=0 print_options=0 sparklines=0"));
    assert!(stdout.contains("oracle: xlsx tests/fixtures/xlsx/reader-structural.xlsx sheets=2 cells=12 defined_names=1 hidden_sheets=1 merged_ranges=1 hyperlinks=1 comments=1 tables=1 data_validations=0 autofilters=1 page_setups=1 images=0 sheet_views=1 tab_colors=1 print_options=1 sparklines=0"));
    assert!(stdout.contains("oracle: xlsb tests/fixtures/xlsb/reader-basic.xlsb sheets=2 cells=8 defined_names=0 hidden_sheets=1 merged_ranges=1 hyperlinks=1 comments=1 tables=1 data_validations=0 autofilters=0 page_setups=0 images=0 sheet_views=0 tab_colors=0 print_options=0 sparklines=0"));
    assert!(stdout.contains("oracle: ods tests/fixtures/ods/repeated-hidden.ods sheets=2 cells=10 defined_names=1 hidden_sheets=1 merged_ranges=1 hyperlinks=1 comments=1 tables=1 data_validations=2 autofilters=1 page_setups=1 images=1 sheet_views=1 tab_colors=0 print_options=0 sparklines=0"));
    assert!(stdout.contains("truncated: false"));
}

#[cfg(all(feature = "xlsx", feature = "xlsb", feature = "ods"))]
#[test]
fn cli_fixture_report_rejects_missing_metadata_oracle_counts() {
    let base = std::env::temp_dir().join(format!(
        "rxls_cli_fixture_manifest_missing_metadata_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base).expect("create temp fixture manifest dir");
    let manifest_path = base.join("manifest.json");
    std::fs::write(
        &manifest_path,
        r#"[
  {
    "path": "tests/fixtures/xlsx/reader-structural.xlsx",
    "format": "xlsx",
    "source": "generated in-repository",
    "license": "MIT",
    "sha256": "d4fe4e707529d60a1f9ca15079f516e77494a3398aab79c5ab8eee36ea9959e2",
    "covers": ["sheet_view"],
    "oracle": {
      "sheets": 2,
      "cells": 12,
      "defined_names": 1,
      "hidden_sheets": 1,
      "merged_ranges": 1,
      "hyperlinks": 1,
      "comments": 1,
      "tables": 1,
      "data_validations": 0,
      "autofilters": 1,
      "page_setups": 1,
      "images": 0,
      "tab_colors": 1,
      "print_options": 1,
      "sparklines": 0
    }
  }
]"#,
    )
    .expect("write fixture manifest");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "fixture-report",
            manifest_path.to_str().expect("utf8 manifest path"),
        ])
        .output()
        .expect("run rxls fixture-report");
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        !output.status.success(),
        "rxls fixture-report unexpectedly accepted missing metadata oracle count"
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");
    assert!(stderr.contains("fixture[0] oracle missing sheet_views"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_sheet_reports_focused_sheet_metadata() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xlsx/reader-structural.xlsx"
    );
    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args(["sheet", fixture, "--sheet", "0"])
        .output()
        .expect("run rxls sheet");

    assert!(
        output.status.success(),
        "rxls sheet failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("rxls sheet"));
    assert!(stdout.contains("sheet: 0 Data"));
    assert!(stdout.contains("type: worksheet"));
    assert!(stdout.contains("visible: visible"));
    assert!(stdout.contains("dimensions: A1:C5"));
    assert!(stdout.contains("cells: 11"));
    assert!(stdout.contains("merged_ranges: 1"));
    assert!(stdout.contains("hyperlinks: 1"));
    assert!(stdout.contains("comments: 1"));
    assert!(stdout.contains("tables: 1"));
    assert!(stdout.contains("charts: 0"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_sheet_reports_detailed_display_metadata() {
    use rxls::ProtectionOptions;

    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("Layout");
        sheet.write(0, 0, "status");
        sheet.freeze_panes(1, 1);
        sheet.hide_gridlines();
        sheet.set_zoom(125);
        sheet.set_show_headers(false);
        sheet.set_right_to_left(true);
        sheet.set_tab_color([0x12, 0x34, 0x56]);
        sheet.set_print_gridlines();
        sheet.set_print_headings();
        sheet.protect_with(ProtectionOptions {
            sort: true,
            auto_filter: true,
            ..Default::default()
        });
        sheet.group_rows(1, 3, 1);
        sheet.group_cols(1, 2, 1);
        sheet.collapse_row(4);
        sheet.set_outline_summary(false, false);
    }

    let path = std::env::temp_dir().join(format!(
        "rxls_cli_sheet_display_metadata_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, workbook.to_xlsx()).expect("write sheet metadata fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "sheet",
            path.to_str().expect("utf8 fixture path"),
            "--sheet",
            "0",
        ])
        .output()
        .expect("run rxls sheet");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls sheet failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains(
        "sheet_view: freeze=1,1 hide_gridlines=true zoom=125 show_headers=false right_to_left=true"
    ));
    assert!(stdout.contains("tab_color: 123456"));
    assert!(stdout.contains("print_options: gridlines=true headings=true"));
    assert!(stdout.contains("protection: protected=true options=sort,auto_filter"));
    assert!(stdout.contains("row_outline: 2:1,3:1,4:1"));
    assert!(stdout.contains("col_outline: B:1,C:1"));
    assert!(stdout.contains("collapsed_rows: 5"));
    assert!(stdout.contains("outline_summary: below=false,right=false"));
}

#[cfg(feature = "xlsx")]
#[test]
fn cli_sheet_reports_detailed_r3_metadata_values() {
    use rxls::{
        CfRule, Color, CondFormat, DataValidation, DvOp, PageSetup, Sparkline, SparklineKind, Table,
    };

    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("Meta");
        sheet.write(0, 0, "status");
        sheet.write(0, 1, "score");
        for row in 1..=3u32 {
            sheet.write(row, 0, if row % 2 == 0 { "Yes" } else { "No" });
            sheet.write(row, 1, f64::from(row) * 10.0);
        }
        sheet.add_table(Table {
            range: (0, 0, 3, 1),
            name: "StatusTable".into(),
            columns: vec!["status".into(), "score".into()],
            style: Some("TableStyleMedium9".into()),
        });
        let mut validation = DataValidation::list((1, 0, 3, 0), "\"Yes,No\"");
        validation.allow_blank = false;
        validation.prompt = Some(("Input".into(), "Pick Yes or No".into()));
        validation.error = Some(("Invalid".into(), "Use Yes or No".into()));
        sheet.add_data_validation(validation);
        sheet.add_conditional_format(CondFormat {
            sqref: (1, 1, 3, 1),
            rule: CfRule::CellIs {
                op: DvOp::GreaterThan,
                formula1: "15".into(),
                formula2: None,
                fill: Color::rgb(0xFF, 0xC7, 0xCE),
            },
        });
        sheet.autofilter(0, 0, 3, 1);
        sheet.set_page_setup(PageSetup {
            landscape: true,
            margins: Some((0.5, 0.6, 0.7, 0.8, 0.2, 0.25)),
            print_area: Some((0, 0, 3, 1)),
            repeat_rows: Some((0, 0)),
            repeat_cols: Some((0, 1)),
            fit_to_width: Some(1),
            fit_to_height: Some(2),
            header: Some("Header".into()),
            footer: Some("Footer".into()),
            paper_size: Some(9),
            scale: Some(95),
            center_horizontally: true,
            center_vertically: false,
            first_page_number: Some(3),
        });
        sheet.add_sparkline(Sparkline {
            location: (4, 1),
            range: "Meta!$B$2:$B$4".into(),
            kind: SparklineKind::Column,
        });
    }

    let path = std::env::temp_dir().join(format!(
        "rxls_cli_sheet_r3_metadata_{}_{}.xlsx",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, workbook.to_xlsx()).expect("write sheet R3 metadata fixture");

    let bin = std::env::var("CARGO_BIN_EXE_rxls").expect("rxls binary path");
    let output = Command::new(bin)
        .args([
            "sheet",
            path.to_str().expect("utf8 fixture path"),
            "--sheet",
            "0",
        ])
        .output()
        .expect("run rxls sheet");
    let _ = std::fs::remove_file(&path);

    assert!(
        output.status.success(),
        "rxls sheet failed: status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert!(stdout.contains("table[0]: StatusTable A1:B4 [status,score] style=TableStyleMedium9"));
    assert!(stdout.contains(
        "data_validation[0]: A2:A4 list between \"Yes,No\" allow_blank=false show_input=true show_error=true prompt=Input:Pick Yes or No error=Invalid:Use Yes or No"
    ));
    assert!(stdout.contains("conditional_format[0]: B2:B4 cell-is greater-than 15  fill=FFC7CE"));
    assert!(stdout.contains("autofilter_range: A1:B4"));
    assert!(stdout.contains(
        "page_setup_detail: landscape=true margins=0.5,0.6,0.7,0.8,0.2,0.25 print_area=A1:B4 repeat_rows=1:1 repeat_cols=A:B fit=1x2 scale=95 first_page=3 centered=truexfalse paper=9 header=Header footer=Footer"
    ));
    assert!(stdout.contains("sparkline[0]: B5 column Meta!$B$2:$B$4"));
}
