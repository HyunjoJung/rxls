//! Black-box integration tests over the **public API only**. A fresh checkout
//! reproduces the core read + authoring guarantees with `cargo test --test
//! integration`, with no access to private helpers. Small committed fixtures
//! keep structural reader coverage and OSS-Fuzz seeds reproducible; larger
//! public reference corpora stay in `local/` and are fetched on demand.

use rxls::Workbook;

/// The panic-free contract, exercised through the public entry points: arbitrary
/// / malformed bytes must yield an `Error`, never a panic or hang.
#[test]
fn open_rejects_garbage_without_panicking() {
    let cases: [&[u8]; 6] = [
        b"",
        b"not an office document",
        &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1], // OLE2 magic, then nothing
        &[0xFF; 96],
        b"PK\x03\x04 truncated-zip",
        &[0x09, 0x08, 0x10, 0x00], // a lone BIFF BOF header, no body
    ];
    for bad in cases {
        assert!(Workbook::open(bad).is_err(), "expected Err for {bad:?}");
        assert!(rxls::extract_text(bad).is_err(), "expected Err for {bad:?}");
    }
}

/// Committed fixture smoke coverage: keep at least one real `.xlsx` ZIP in the
/// tracked test corpus so reader regressions and OSS-Fuzz seeds do not depend
/// only on synthetic in-memory packages.
#[cfg(feature = "xlsx")]
#[test]
fn committed_xlsx_fixture_exposes_structural_reader_surface() {
    use rxls::{Cell, Color, SheetView};

    let wb =
        Workbook::open(include_bytes!("fixtures/xlsx/reader-structural.xlsx")).expect("fixture");

    assert_eq!(wb.sheets.len(), 2);
    assert_eq!(
        wb.defined_names(),
        &[("NamedTotal".into(), "Data!$B$2".into())]
    );
    assert_eq!(
        wb.properties.title.as_deref(),
        Some("rxls structural fixture")
    );

    let sheet = wb.sheet_by_name("Data").expect("Data sheet");
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("item".into())));
    assert_eq!(sheet.cell(1, 1), Some(&Cell::Number(12.5)));
    assert_eq!(sheet.formatted(1, 2), Some("TRUE"));
    assert_eq!(sheet.merged_ranges(), &[(3, 0, 3, 2)]);
    assert_eq!(
        sheet.hyperlinks(),
        &[(4, 0, "https://example.com/rxls".into())]
    );
    assert_eq!(sheet.comments()[0].text, "needs review");
    assert_eq!(sheet.comments()[0].author.as_deref(), Some("fixture"));
    assert_eq!(sheet.tables()[0].name, "DataTable");
    assert_eq!(sheet.tables()[0].range, (0, 0, 2, 2));
    assert_eq!(sheet.tables()[0].columns, ["item", "amount", "ok"]);
    assert_eq!(
        sheet.sheet_view(),
        SheetView {
            freeze: Some((1, 1)),
            hide_gridlines: true,
            zoom: Some(125),
            show_headers: Some(false),
            right_to_left: true,
        }
    );
    assert_eq!(sheet.autofilter_range(), Some((0, 0, 2, 2)));
    assert_eq!(sheet.tab_color(), Some(Color::rgb(0x12, 0x34, 0x56)));
    assert!(sheet.print_gridlines());
    assert!(sheet.print_headings());
    let page_setup = sheet.page_setup().expect("page setup");
    assert!(page_setup.landscape);
    assert_eq!(page_setup.margins, Some((0.5, 0.6, 0.7, 0.8, 0.2, 0.25)));
    assert_eq!(page_setup.print_area, Some((0, 0, 9, 4)));
    assert_eq!(page_setup.repeat_rows, Some((0, 1)));
    assert_eq!(page_setup.repeat_cols, Some((0, 2)));
    assert_eq!(page_setup.fit_to_width, Some(1));
    assert_eq!(page_setup.fit_to_height, Some(2));
    assert_eq!(page_setup.header.as_deref(), Some("&CFixture"));
    assert_eq!(page_setup.footer.as_deref(), Some("&RPage &P"));
    assert_eq!(page_setup.paper_size, Some(9));
    assert_eq!(page_setup.scale, Some(85));
    assert!(page_setup.center_horizontally);
    assert!(page_setup.center_vertically);
    assert_eq!(page_setup.first_page_number, Some(3));

    assert!(wb
        .sheet_by_name("Hidden")
        .expect("Hidden sheet")
        .is_hidden());
}

#[cfg(feature = "xlsx")]
fn synthetic_xlsm_with_vba() -> Vec<u8> {
    synthetic_xlsm(false)
}

#[cfg(feature = "xlsx")]
fn synthetic_xlsm_with_multisurface_parts() -> Vec<u8> {
    synthetic_xlsm(true)
}

#[cfg(feature = "xlsx")]
fn synthetic_xlsm(include_multisurface_parts: bool) -> Vec<u8> {
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
    let content_types: &[u8] = if include_multisurface_parts {
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="bin" ContentType="application/vnd.ms-office.vbaProject"/><Default Extension="png" ContentType="image/png"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.ms-excel.sheet.macroEnabled.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/tables/table1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml"/><Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/><Override PartName="/xl/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/><Override PartName="/xl/pivotTables/pivotTable1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.pivotTable+xml"/><Override PartName="/customXml/item1.xml" ContentType="application/xml"/><Override PartName="/_xmlsignatures/origin.sigs" ContentType="application/vnd.openxmlformats-package.digital-signature-origin"/><Override PartName="/_xmlsignatures/sig1.xml" ContentType="application/vnd.openxmlformats-package.digital-signature-xmlsignature+xml"/><Override PartName="/xl/unknown/opaque.bin" ContentType="application/vnd.example.rxls-opaque"/></Types>"#
    } else {
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="bin" ContentType="application/vnd.ms-office.vbaProject"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.ms-excel.sheet.macroEnabled.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/tables/table1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml"/></Types>"#
    };
    add(&mut zip, opt, "[Content_Types].xml", content_types);
    let root_relationships: &[u8] = if include_multisurface_parts {
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/><Relationship Id="rIdSig" Type="http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin" Target="_xmlsignatures/origin.sigs"/></Relationships>"#
    } else {
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#
    };
    add(&mut zip, opt, "_rels/.rels", root_relationships);
    add(
        &mut zip,
        opt,
        "xl/workbook.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
    );
    let workbook_relationships: &[u8] = if include_multisurface_parts {
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.microsoft.com/office/2006/relationships/vbaProject" Target="vbaProject.bin"/><Relationship Id="rId3" Type="https://example.invalid/relationships/opaque-feature" Target="unknown/opaque.bin"/><Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml" Target="../customXml/item1.xml"/></Relationships>"#
    } else {
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.microsoft.com/office/2006/relationships/vbaProject" Target="vbaProject.bin"/></Relationships>"#
    };
    add(
        &mut zip,
        opt,
        "xl/_rels/workbook.xml.rels",
        workbook_relationships,
    );
    add(
        &mut zip,
        opt,
        "xl/worksheets/sheet1.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>macro book</t></is></c></row></sheetData><tableParts count="1"><tablePart r:id="rId1"/></tableParts></worksheet>"#,
    );
    let sheet_relationships: &[u8] = if include_multisurface_parts {
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotTable" Target="../pivotTables/pivotTable1.xml"/></Relationships>"#
    } else {
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#
    };
    add(
        &mut zip,
        opt,
        "xl/worksheets/_rels/sheet1.xml.rels",
        sheet_relationships,
    );
    add(
        &mut zip,
        opt,
        "xl/tables/table1.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="MacroTable" displayName="MacroTable" ref="A1:A1" totalsRowShown="0"><autoFilter ref="A1:A1"/><tableColumns count="1"><tableColumn id="1" name="macro book"/></tableColumns><tableStyleInfo name="TableStyleMedium2" showFirstColumn="0" showLastColumn="0" showRowStripes="1" showColumnStripes="0"/></table>"#,
    );
    add(&mut zip, opt, "xl/vbaProject.bin", b"rxls macro payload");
    if include_multisurface_parts {
        add(
        &mut zip,
        opt,
        "xl/drawings/drawing1.xml",
        br#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" opaque="drawing"><xdr:oneCellAnchor><xdr:graphicFrame/><xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#,
    );
        add(
        &mut zip,
        opt,
        "xl/drawings/_rels/drawing1.xml.rels",
        br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/><Relationship Id="rIdImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/></Relationships>"#,
    );
        add(
        &mut zip,
        opt,
        "xl/charts/chart1.xml",
        br#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" opaque="chart"/>"#,
    );
        add(
            &mut zip,
            opt,
            "xl/media/image1.png",
            b"\x89PNG\r\n\x1a\nrxls-image",
        );
        add(
        &mut zip,
        opt,
        "xl/pivotTables/pivotTable1.xml",
        br#"<pivotTableDefinition xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" name="OpaquePivot"/>"#,
    );
        add(
            &mut zip,
            opt,
            "customXml/item1.xml",
            br#"<rxls:opaque xmlns:rxls="urn:rxls:test">custom XML</rxls:opaque>"#,
        );
        add(
        &mut zip,
        opt,
        "customXml/_rels/item1.xml.rels",
        br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="opaqueRel" Type="https://example.invalid/relationships/custom-opaque" Target="../itemProps1.xml"/></Relationships>"#,
    );
        add(
        &mut zip,
        opt,
        "customXml/itemProps1.xml",
        br#"<ds:datastoreItem xmlns:ds="http://schemas.openxmlformats.org/officeDocument/2006/customXml" ds:itemID="{00000000-0000-0000-0000-000000000001}"/>"#,
    );
        add(
            &mut zip,
            opt,
            "_xmlsignatures/origin.sigs",
            b"rxls-signature-origin",
        );
        add(
        &mut zip,
        opt,
        "_xmlsignatures/_rels/origin.sigs.rels",
        br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/signature" Target="sig1.xml"/></Relationships>"#,
    );
        add(
        &mut zip,
        opt,
        "_xmlsignatures/sig1.xml",
        br#"<Signature xmlns="http://www.w3.org/2000/09/xmldsig#"><Object>signature-adjacent sentinel</Object></Signature>"#,
    );
        add(
            &mut zip,
            opt,
            "xl/unknown/opaque.bin",
            b"\x00opaque relationship payload\xff",
        );
    }
    zip.finish().unwrap().into_inner()
}

#[cfg(feature = "xlsx")]
fn synthetic_xlsx_with_calc_chain() -> Vec<u8> {
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/calcChain.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.calcChain+xml"/></Types>"#,
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/calcChain" Target="calcChain.xml"/></Relationships>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/worksheets/sheet1.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/calcChain.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><calcChain xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><c r="A1" i="1"/></calcChain>"#,
    );
    zip.finish().unwrap().into_inner()
}

/// A worksheet whose `<row>` children are in DESCENDING (non-ascending) `r=`
/// order: `r="10"` then `r="5"`. Syntactically valid XML -- `XmlTree::parse`
/// is schema-agnostic and imposes no ascending-order requirement -- but
/// violates the OOXML convention that a correct find-or-create scan must not
/// assume.
#[cfg(feature = "xlsx")]
fn synthetic_xlsx_with_out_of_order_rows() -> Vec<u8> {
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/worksheets/sheet1.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="10"><c r="A10"><v>100</v></c></row><row r="5"><c r="A5"><v>50</v></c></row></sheetData></worksheet>"#,
    );
    zip.finish().unwrap().into_inner()
}

/// A worksheet whose `<row r="1">` has `<c>` children in DESCENDING
/// (non-ascending) column order: `r="J1"` then `r="B1"`. Same defect shape as
/// [`synthetic_xlsx_with_out_of_order_rows`], one level down (cells within a
/// row rather than rows within `sheetData`).
#[cfg(feature = "xlsx")]
fn synthetic_xlsx_with_out_of_order_cells() -> Vec<u8> {
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/worksheets/sheet1.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="J1"><v>10</v></c><c r="B1"><v>20</v></c></row></sheetData></worksheet>"#,
    );
    zip.finish().unwrap().into_inner()
}

#[cfg(feature = "xlsx")]
fn zip_part(bytes: &[u8], name: &str) -> Vec<u8> {
    use std::io::Read;

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut part = zip.by_name(name).unwrap();
    let mut out = Vec::new();
    part.read_to_end(&mut out).unwrap();
    out
}

#[cfg(feature = "xlsx")]
fn zip_part_string(bytes: &[u8], name: &str) -> String {
    String::from_utf8(zip_part(bytes, name)).unwrap()
}

#[cfg(feature = "xlsx")]
fn zip_has_part(bytes: &[u8], name: &str) -> bool {
    zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .unwrap()
        .by_name(name)
        .is_ok()
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_xlsm_noop_save_preserves_vba_project() {
    use rxls::{EditCapability, Spreadsheet};

    let input = synthetic_xlsm_with_vba();
    let spreadsheet = Spreadsheet::open(&input).expect("open editable xlsm");

    assert_eq!(spreadsheet.edit_capability(), &EditCapability::ReadWrite);
    assert!(spreadsheet.edited_parts().is_empty());
    assert_eq!(
        spreadsheet.workbook().sheet_names(),
        vec!["Data"],
        "editable wrapper should expose parsed workbook"
    );

    let saved = spreadsheet.save().expect("save retained package");
    assert_eq!(zip_part(&saved, "xl/vbaProject.bin"), b"rxls macro payload");
    Workbook::open(&saved).expect("saved xlsm reopens");
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_xlsm_set_cell_value_touches_only_sheet_part() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsm_with_vba();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsm");

    spreadsheet
        .set_cell_value("Data", 0, 0, Cell::Text("edited <&>".into()))
        .expect("edit cell");

    assert_eq!(spreadsheet.edited_parts(), &["xl/worksheets/sheet1.xml"]);
    let saved = spreadsheet.save().expect("save edited package");
    assert_eq!(zip_part(&saved, "xl/vbaProject.bin"), b"rxls macro payload");

    let reopened = Workbook::open(&saved).expect("reopen edited xlsm");
    assert_eq!(
        reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Text("edited <&>".into()))
    );
}

/// A single unrelated edit must leave a representative multi-surface OOXML
/// package byte-for-byte intact outside the declared worksheet part. Keeping
/// these sentinels together catches broad ZIP rebuild or relationship-pruning
/// regressions that isolated VBA-only tests cannot detect.
#[cfg(feature = "xlsx")]
#[test]
fn editable_unrelated_cell_edit_byte_preserves_multisurface_package() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsm_with_multisurface_parts();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open multisurface xlsm");
    spreadsheet
        .set_cell_value("Data", 0, 0, Cell::Text("unrelated edit".into()))
        .expect("edit worksheet cell");
    assert_eq!(spreadsheet.edited_parts(), &["xl/worksheets/sheet1.xml"]);

    let saved = spreadsheet.save().expect("save multisurface xlsm");
    let untouched = [
        "[Content_Types].xml",
        "_rels/.rels",
        "xl/_rels/workbook.xml.rels",
        "xl/worksheets/_rels/sheet1.xml.rels",
        "xl/vbaProject.bin",
        "xl/drawings/drawing1.xml",
        "xl/drawings/_rels/drawing1.xml.rels",
        "xl/charts/chart1.xml",
        "xl/media/image1.png",
        "xl/pivotTables/pivotTable1.xml",
        "customXml/item1.xml",
        "customXml/_rels/item1.xml.rels",
        "customXml/itemProps1.xml",
        "_xmlsignatures/origin.sigs",
        "_xmlsignatures/_rels/origin.sigs.rels",
        "_xmlsignatures/sig1.xml",
        "xl/unknown/opaque.bin",
    ];
    for part in untouched {
        assert_eq!(
            zip_part(&saved, part),
            zip_part(&input, part),
            "unrelated edit changed retained part {part}"
        );
    }

    let reopened = Workbook::open(&saved).expect("reopen multisurface xlsm");
    assert_eq!(
        reopened
            .sheet_by_name("Data")
            .and_then(|sheet| sheet.cell(0, 0)),
        Some(&Cell::Text("unrelated edit".into()))
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_xlsm_comment_and_hyperlink_crud_preserves_vba() {
    use rxls::{Comment, Spreadsheet};

    let input = synthetic_xlsm_with_vba();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsm");
    spreadsheet
        .set_comment("Data", 0, 0, "created note", Some("Alice"))
        .expect("create legacy note");
    spreadsheet
        .set_external_hyperlink("Data", 0, 0, "https://example.com/created")
        .expect("create external hyperlink");
    let created = spreadsheet.save().expect("save created metadata");
    assert_eq!(
        zip_part(&created, "xl/vbaProject.bin"),
        b"rxls macro payload"
    );
    let reopened = Workbook::open(&created).expect("reopen created xlsm");
    let sheet = reopened.sheet_by_name("Data").expect("Data");
    assert_eq!(
        sheet.comments(),
        &[Comment {
            row: 0,
            col: 0,
            text: "created note".into(),
            author: Some("Alice".into()),
        }]
    );
    assert_eq!(
        sheet.hyperlinks(),
        &[(0, 0, "https://example.com/created".into())]
    );

    let mut spreadsheet = Spreadsheet::open(&created).expect("reopen xlsm for update");
    spreadsheet
        .set_comment("Data", 0, 0, "updated note", Some("Bob"))
        .expect("update legacy note");
    spreadsheet
        .set_internal_hyperlink("Data", 0, 0, "Data!A1")
        .expect("replace external hyperlink with internal hyperlink");
    let updated = spreadsheet.save().expect("save updated metadata");
    assert_eq!(
        zip_part(&updated, "xl/vbaProject.bin"),
        b"rxls macro payload"
    );
    let reopened = Workbook::open(&updated).expect("reopen updated xlsm");
    let sheet = reopened.sheet_by_name("Data").expect("Data");
    assert_eq!(sheet.comments()[0].text, "updated note");
    assert_eq!(sheet.comments()[0].author.as_deref(), Some("Bob"));
    assert!(sheet.hyperlinks().is_empty());

    let mut spreadsheet = Spreadsheet::open(&updated).expect("reopen xlsm for delete");
    spreadsheet
        .delete_comment("Data", 0, 0)
        .expect("delete legacy note");
    spreadsheet
        .delete_hyperlink("Data", 0, 0)
        .expect("delete internal hyperlink");
    let deleted = spreadsheet.save().expect("save deleted metadata");
    assert_eq!(
        zip_part(&deleted, "xl/vbaProject.bin"),
        b"rxls macro payload"
    );
    assert!(!zip_has_part(&deleted, "xl/comments1.xml"));
    assert!(!zip_has_part(&deleted, "xl/drawings/vmlDrawing1.vml"));
    let reopened = Workbook::open(&deleted).expect("reopen deleted xlsm");
    let sheet = reopened.sheet_by_name("Data").expect("Data");
    assert!(sheet.comments().is_empty());
    assert!(sheet.hyperlinks().is_empty());
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_xlsm_delete_sheet_preserves_vba_and_reopens() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsm_with_vba();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsm");
    spreadsheet.add_sheet("Keep").expect("add surviving sheet");
    spreadsheet
        .set_cell_value("Keep", 0, 0, Cell::Text("survives".into()))
        .expect("write surviving sheet");
    let two_sheet = spreadsheet.save().expect("save two-sheet xlsm");

    let mut spreadsheet = Spreadsheet::open(&two_sheet).expect("reopen two-sheet xlsm");
    spreadsheet
        .delete_sheet("Data")
        .expect("delete first sheet");
    let saved = spreadsheet.save().expect("save deleted-sheet xlsm");

    assert_eq!(zip_part(&saved, "xl/vbaProject.bin"), b"rxls macro payload");
    let reopened = Workbook::open(&saved).expect("reopen deleted-sheet xlsm");
    assert_eq!(reopened.sheet_names(), vec!["Keep"]);
    assert_eq!(
        reopened
            .sheet_by_name("Keep")
            .and_then(|sheet| sheet.cell(0, 0)),
        Some(&Cell::Text("survives".into()))
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_xlsm_validation_and_existing_table_range_edits_preserve_vba() {
    use rxls::{DataValidation, DvKind, DvOp, Spreadsheet};

    let input = synthetic_xlsm_with_vba();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsm");
    spreadsheet
        .set_data_validation("Data", DataValidation::list((1, 0, 3, 0), "\"A,B\""))
        .expect("create validation");
    spreadsheet
        .set_table_range("Data", "MacroTable", (0, 0, 3, 0))
        .expect("expand existing table");
    let created = spreadsheet.save().expect("save created xlsm metadata");
    assert_eq!(
        zip_part(&created, "xl/vbaProject.bin"),
        b"rxls macro payload"
    );
    let reopened = Workbook::open(&created).expect("reopen created xlsm metadata");
    let sheet = reopened.sheet_by_name("Data").expect("Data");
    assert_eq!(sheet.data_validations().len(), 1);
    assert_eq!(sheet.tables()[0].range, (0, 0, 3, 0));

    let mut spreadsheet = Spreadsheet::open(&created).expect("reopen xlsm for update");
    spreadsheet
        .set_data_validation(
            "Data",
            DataValidation::new((1, 0, 3, 0), DvKind::Whole, DvOp::Between, "1").with_formula2("9"),
        )
        .expect("update validation");
    spreadsheet
        .set_table_range("Data", "MacroTable", (0, 0, 1, 0))
        .expect("shrink existing table");
    let updated = spreadsheet.save().expect("save updated xlsm metadata");
    assert_eq!(
        zip_part(&updated, "xl/vbaProject.bin"),
        b"rxls macro payload"
    );
    let reopened = Workbook::open(&updated).expect("reopen updated xlsm metadata");
    let sheet = reopened.sheet_by_name("Data").expect("Data");
    assert_eq!(sheet.data_validations()[0].kind, DvKind::Whole);
    assert_eq!(sheet.tables()[0].range, (0, 0, 1, 0));

    let mut spreadsheet = Spreadsheet::open(&updated).expect("reopen xlsm for delete");
    spreadsheet
        .delete_data_validation("Data", (1, 0, 3, 0))
        .expect("delete validation");
    let deleted = spreadsheet.save().expect("save deleted validation");
    assert_eq!(
        zip_part(&deleted, "xl/vbaProject.bin"),
        b"rxls macro payload"
    );
    assert!(Workbook::open(&deleted)
        .expect("reopen deleted validation xlsm")
        .sheet_by_name("Data")
        .expect("Data")
        .data_validations()
        .is_empty());
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_formula_edit_removes_calc_chain() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsx_with_calc_chain();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .set_cell_formula("Data", 0, 0, "SUM(1,2)", 3.0)
        .expect("edit formula");

    let saved = spreadsheet.save().expect("save edited package");
    assert!(!zip_has_part(&saved, "xl/calcChain.xml"));
    assert!(!zip_part_string(&saved, "[Content_Types].xml").contains("calcChain"));
    assert!(!zip_part_string(&saved, "xl/_rels/workbook.xml.rels").contains("calcChain"));

    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    assert_eq!(
        reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Formula {
            formula: "SUM(1,2)".into(),
            cached: Box::new(Cell::Number(3.0)),
        })
    );
}

/// Regression test for a `worksheet_path` bug: it resolved `xl/workbook.xml`
/// through the promotion-aware `part_xml_bytes` helper (which falls back to
/// serializing an already-promoted `XmlTree`), but resolved the workbook's
/// `.rels` part through the raw-only `Package::part_bytes`, which only
/// returns `Some` while that part is still `Part::Raw`. The very first edit
/// on a workbook containing a real calc chain (i.e. virtually every
/// Excel-produced workbook with a formula) promotes `xl/_rels/workbook.xml.rels`
/// as a side effect of `invalidate_calc_chain` removing the calc-chain
/// `Relationship` from it -- so every subsequent edit in the same session
/// used to fail with `Error::MissingWorkbook`, even though the workbook and
/// its relationships were completely intact.
#[cfg(feature = "xlsx")]
#[test]
fn editable_second_edit_after_calc_chain_invalidation_succeeds() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsx_with_calc_chain();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    // First edit: succeeds, and as a side effect promotes + touches
    // `xl/_rels/workbook.xml.rels` while removing the calc-chain
    // relationship from it.
    spreadsheet
        .set_cell_formula("Data", 0, 0, "SUM(1,2)", 3.0)
        .expect("first edit (formula) must succeed");

    // Second edit, a DIFFERENT cell on the SAME sheet, same session: must
    // still succeed -- `worksheet_path` must resolve the now-promoted `.rels`
    // part instead of reporting the workbook missing.
    spreadsheet
        .set_cell_value("Data", 0, 1, Cell::Number(42.0))
        .expect("second edit (same session, same sheet) must succeed");

    // Third edit, to be extra sure it's not "works twice by coincidence".
    spreadsheet
        .set_cell_value("Data", 0, 2, Cell::Number(7.0))
        .expect("third edit (same session, same sheet) must succeed");

    let saved = spreadsheet.save().expect("save edited package");
    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    let sheet = reopened.sheet_by_name("Data").expect("Data sheet");
    assert_eq!(
        sheet.cell(0, 0),
        Some(&Cell::Formula {
            formula: "SUM(1,2)".into(),
            cached: Box::new(Cell::Number(3.0)),
        })
    );
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Number(42.0)));
    assert_eq!(sheet.cell(0, 2), Some(&Cell::Number(7.0)));
}

/// Regression test for a `sml_row_node` bug: its find-or-create scan `break`s
/// as soon as it sees a sibling `<row>` whose `r=` exceeds the target,
/// treating that as "not found, insert here" -- correct only if rows are
/// already in ascending `r=` order, which `XmlTree::parse` does not enforce.
/// On `<row r="10">...</row><row r="5">...</row>`, editing 0-based row 4
/// (`r="5"`, the SECOND child) used to stop at the first child (`r="10" >
/// 5`) and silently insert a brand-new, duplicate `<row r="5">` instead of
/// finding and updating the real one.
#[cfg(feature = "xlsx")]
#[test]
fn editable_out_of_order_row_edit_updates_existing_row_not_duplicate() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsx_with_out_of_order_rows();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    // 0-based row 4 == r="5", which already exists as the worksheet's
    // *second* row child (after r="10").
    spreadsheet
        .set_cell_value("Data", 4, 1, Cell::Number(99.0))
        .expect("edit existing out-of-order row");

    let saved = spreadsheet.save().expect("save edited package");
    let xml = zip_part_string(&saved, "xl/worksheets/sheet1.xml");
    assert_eq!(
        xml.matches(r#"r="5""#).count(),
        1,
        "exactly one <row r=\"5\"> must exist, got: {xml}"
    );

    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    let sheet = reopened.sheet_by_name("Data").expect("Data sheet");
    // The existing cell in that row survives untouched...
    assert_eq!(sheet.cell(4, 0), Some(&Cell::Number(50.0)));
    // ...alongside the newly written cell in the SAME row (not a fresh,
    // otherwise-empty duplicate row).
    assert_eq!(sheet.cell(4, 1), Some(&Cell::Number(99.0)));
    // The untouched sibling row survives unchanged.
    assert_eq!(sheet.cell(9, 0), Some(&Cell::Number(100.0)));
}

/// Analogous regression test for `sml_cell_node`, the same defect shape one
/// level down: a `<row>` whose `<c>` children are in descending column order
/// (`r="J1"` then `r="B1"`). Editing column B (0-based col 1, the SECOND
/// child) used to stop at the first child (`J1`'s column 9 > 1) and silently
/// insert a duplicate `<c r="B1">` instead of finding and updating the real
/// one.
#[cfg(feature = "xlsx")]
#[test]
fn editable_out_of_order_cell_edit_updates_existing_cell_not_duplicate() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsx_with_out_of_order_cells();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    // 0-based col 1 == B1, which already exists as the row's *second* cell
    // child (after J1, a higher column).
    spreadsheet
        .set_cell_value("Data", 0, 1, Cell::Number(21.0))
        .expect("edit existing out-of-order cell");

    let saved = spreadsheet.save().expect("save edited package");
    let xml = zip_part_string(&saved, "xl/worksheets/sheet1.xml");
    assert_eq!(
        xml.matches(r#"r="B1""#).count(),
        1,
        "exactly one <c r=\"B1\"> must exist, got: {xml}"
    );

    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    let sheet = reopened.sheet_by_name("Data").expect("Data sheet");
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Number(21.0)));
    // The untouched sibling cell survives unchanged.
    assert_eq!(sheet.cell(0, 9), Some(&Cell::Number(10.0)));
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_document_properties_touch_only_core_part() {
    use rxls::{DocProperties, Spreadsheet};

    let mut workbook = Workbook::new();
    workbook.properties.title = Some("Old Title".into());
    workbook.add_sheet("Data").write(0, 0, "value");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .set_document_properties(
            DocProperties::new()
                .with_title("New <Title>")
                .with_creator("rxls editor"),
        )
        .expect("edit document properties");

    assert_eq!(spreadsheet.edited_parts(), &["docProps/core.xml"]);
    let saved = spreadsheet.save().expect("save edited package");
    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    assert_eq!(reopened.properties.title.as_deref(), Some("New <Title>"));
    assert_eq!(reopened.properties.creator.as_deref(), Some("rxls editor"));
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_defined_name_touches_only_workbook_and_preserves_vba() {
    use rxls::Spreadsheet;

    let input = synthetic_xlsm_with_vba();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsm");

    spreadsheet
        .set_defined_name("TaxRate", "Data!$B$1")
        .expect("edit defined name");

    assert_eq!(spreadsheet.edited_parts(), &["xl/workbook.xml"]);
    let saved = spreadsheet.save().expect("save edited package");
    assert_eq!(zip_part(&saved, "xl/vbaProject.bin"), b"rxls macro payload");

    let reopened = Workbook::open(&saved).expect("reopen edited xlsm");
    assert_eq!(
        reopened.defined_names(),
        &[("TaxRate".to_string(), "Data!$B$1".to_string())]
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_sheet_metadata_touches_only_workbook_part() {
    use rxls::{SheetVisible, Spreadsheet};

    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "value");
    workbook.add_sheet("Other").write(0, 0, "other");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .rename_sheet("Data", "Renamed")
        .expect("rename sheet");
    spreadsheet
        .set_sheet_visibility("Other", SheetVisible::Hidden)
        .expect("hide sheet");
    spreadsheet
        .set_active_sheet("Renamed")
        .expect("set active sheet");

    assert_eq!(spreadsheet.edited_parts(), &["xl/workbook.xml"]);
    let saved = spreadsheet.save().expect("save edited package");
    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    assert_eq!(reopened.sheet_names(), vec!["Renamed", "Other"]);
    assert_eq!(reopened.active_sheet_name(), Some("Renamed"));
    assert_eq!(
        reopened.sheet_by_name("Other").map(|sheet| sheet.visible()),
        Some(SheetVisible::Hidden)
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_cell_edit_after_sheet_metadata_edit_in_same_session() {
    // `xl/workbook.xml` is promoted to an edited tree by a sheet-metadata
    // edit; a later cell edit in the same session must still be able to
    // resolve the target worksheet's part path (which reads workbook.xml)
    // rather than treating the now-promoted part as missing.
    use rxls::{Cell, Spreadsheet};

    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, "value");
    let input = workbook.to_xlsx();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .rename_sheet("Data", "Renamed")
        .expect("rename sheet");
    spreadsheet
        .set_cell_value("Renamed", 0, 0, Cell::Text("after rename".into()))
        .expect("set cell value on the renamed sheet");

    assert_eq!(
        spreadsheet.edited_parts(),
        &["xl/workbook.xml", "xl/worksheets/sheet1.xml"]
    );
    let saved = spreadsheet.save().expect("save edited package");
    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    assert_eq!(reopened.sheet_names(), vec!["Renamed"]);
    assert_eq!(
        reopened.sheet_by_name("Renamed").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Text("after rename".into()))
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_sheet_tab_color_touches_only_sheet_part_and_preserves_vba() {
    use rxls::{Color, Spreadsheet};

    let input = synthetic_xlsm_with_vba();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsm");

    spreadsheet
        .set_sheet_tab_color("Data", Color::rgb(0x12, 0x34, 0x56))
        .expect("set tab color");

    assert_eq!(spreadsheet.edited_parts(), &["xl/worksheets/sheet1.xml"]);
    let saved = spreadsheet.save().expect("save edited package");
    assert_eq!(zip_part(&saved, "xl/vbaProject.bin"), b"rxls macro payload");

    let reopened = Workbook::open(&saved).expect("reopen edited xlsm");
    assert_eq!(
        reopened
            .sheet_by_name("Data")
            .and_then(|sheet| sheet.tab_color()),
        Some(Color::rgb(0x12, 0x34, 0x56))
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_append_row_and_clear_range_touch_only_sheet_part() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsm_with_vba();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsm");

    spreadsheet
        .clear_range("Data", 0, 0, 0, 0)
        .expect("clear existing cell");
    let appended = spreadsheet
        .append_row("Data", vec![Cell::Text("next".into()), Cell::Number(42.0)])
        .expect("append row");

    assert_eq!(appended, 1);
    assert_eq!(spreadsheet.edited_parts(), &["xl/worksheets/sheet1.xml"]);
    let saved = spreadsheet.save().expect("save edited package");
    assert_eq!(zip_part(&saved, "xl/vbaProject.bin"), b"rxls macro payload");

    let reopened = Workbook::open(&saved).expect("reopen edited xlsm");
    let sheet = reopened.sheet_by_name("Data").expect("data sheet");
    assert_eq!(sheet.cell(0, 0), None);
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Text("next".into())));
    assert_eq!(sheet.cell(1, 1), Some(&Cell::Number(42.0)));
}

#[cfg(feature = "xlsx")]
fn synthetic_xlsx_with_styled_cell() -> Vec<u8> {
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/worksheets/sheet1.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" s="3"><v>1</v></c></row></sheetData></worksheet>"#,
    );
    zip.finish().unwrap().into_inner()
}

/// BUG 1 (regression): `set_cell_value` must preserve the target cell's
/// existing `s="N"` style index instead of silently dropping it when it
/// rebuilds the `<c>` tag.
#[cfg(feature = "xlsx")]
#[test]
fn editable_set_cell_value_preserves_existing_style() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsx_with_styled_cell();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .set_cell_value("Data", 0, 0, Cell::Number(42.0))
        .expect("edit cell value");

    let saved = spreadsheet.save().expect("save edited package");
    let sheet_xml = zip_part_string(&saved, "xl/worksheets/sheet1.xml");
    assert!(
        sheet_xml.contains(r#"s="3""#),
        "edited cell must keep its original style index: {sheet_xml}"
    );

    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    assert_eq!(
        reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Number(42.0))
    );
}

/// BUG 1 (regression): the same style-preservation guarantee applies to the
/// `set_cell_formula` path, which is built on the same `inline_cell_xml`
/// helper.
#[cfg(feature = "xlsx")]
#[test]
fn editable_set_cell_formula_preserves_existing_style() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsx_with_styled_cell();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .set_cell_formula("Data", 0, 0, "SUM(1,2)", 3.0)
        .expect("edit cell formula");

    let saved = spreadsheet.save().expect("save edited package");
    let sheet_xml = zip_part_string(&saved, "xl/worksheets/sheet1.xml");
    assert!(
        sheet_xml.contains(r#"s="3""#),
        "edited formula cell must keep its original style index: {sheet_xml}"
    );

    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    assert_eq!(
        reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Formula {
            formula: "SUM(1,2)".into(),
            cached: Box::new(Cell::Number(3.0)),
        })
    );
}

#[cfg(feature = "xlsx")]
fn synthetic_xlsx_with_quoted_gt_in_cell_attr() -> Vec<u8> {
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
    );
    // The `quirk` attribute value contains a literal '>' immediately preceded
    // by a multi-byte UTF-8 character (the Korean syllable Han, 3 bytes) --
    // legal XML, but a naive `find('>')` tag-boundary scan matches this inner
    // '>' instead of the real one.
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" quirk="한>y"><v>1</v></c></row></sheetData></worksheet>"#;
    add(
        &mut zip,
        opt,
        "xl/worksheets/sheet1.xml",
        sheet_xml.as_bytes(),
    );
    zip.finish().unwrap().into_inner()
}

/// BUG 2 (regression): a `>` embedded in a quoted attribute value on the
/// `<c>` tag, immediately preceded by a multi-byte UTF-8 character, must not
/// panic (nor mis-detect the tag boundary) when editing that cell.
#[cfg(feature = "xlsx")]
#[test]
fn editable_set_cell_value_handles_quoted_gt_in_cell_tag_without_panic() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsx_with_quoted_gt_in_cell_attr();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        spreadsheet.set_cell_value("Data", 0, 0, Cell::Number(99.0))
    }));
    let edit_result = result.expect("editing a cell with a quoted '>' must not panic");
    edit_result.expect("edit should succeed with the correct tag boundary");

    let saved = spreadsheet.save().expect("save edited package");
    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    assert_eq!(
        reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Number(99.0))
    );
}

#[cfg(feature = "xlsx")]
fn synthetic_xlsx_with_quoted_gt_in_row_attr() -> Vec<u8> {
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
    );
    // Same quoted-'>'-after-multibyte-char hazard, but on the `<row>` tag
    // itself, to exercise `insert_cell`'s row-tag-boundary lookup.
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1" quirk="한>y"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#;
    add(
        &mut zip,
        opt,
        "xl/worksheets/sheet1.xml",
        sheet_xml.as_bytes(),
    );
    zip.finish().unwrap().into_inner()
}

/// BUG 2 (regression): the same quoted-`>` hazard on a `<row>` tag must not
/// panic `insert_cell` when adding a not-yet-existing cell to that row.
#[cfg(feature = "xlsx")]
#[test]
fn editable_set_cell_value_handles_quoted_gt_in_row_tag_without_panic() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsx_with_quoted_gt_in_row_attr();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    // B1 does not exist yet, so this exercises insert_cell's row lookup.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        spreadsheet.set_cell_value("Data", 0, 1, Cell::Number(7.0))
    }));
    let edit_result = result.expect("inserting into a row with a quoted '>' must not panic");
    edit_result.expect("edit should succeed with the correct row boundary");

    let saved = spreadsheet.save().expect("save edited package");
    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    let sheet = reopened.sheet_by_name("Data").expect("data sheet");
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Number(1.0)));
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Number(7.0)));
}

#[cfg(feature = "xlsx")]
fn synthetic_xlsx_with_quoted_gt_and_no_target_row() -> Vec<u8> {
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
    );
    // The only existing row carries the same quoted-'>'-after-multibyte-char
    // hazard as the other regression fixtures, but this test's edit targets a
    // row that does NOT exist yet -- exercising the tree-based *row/cell
    // creation* insertion scan (which must walk past this row's `r`
    // attribute while picking a sorted insertion index), not just an edit of
    // an already-present row/cell like the two fixtures above.
    let sheet_xml = r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1" quirk="한>y"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#;
    add(
        &mut zip,
        opt,
        "xl/worksheets/sheet1.xml",
        sheet_xml.as_bytes(),
    );
    zip.finish().unwrap().into_inner()
}

/// BUG 2 (regression), new code path: the quoted-'>'-after-multibyte-char
/// hazard must not panic when the edit needs to CREATE a brand-new `<row>`
/// (not just insert into, or edit, an already-present row/cell) in a part
/// that also contains a tag with this hazard -- proving the tree-based
/// create path is exercised safely too, not just the edit path the two
/// fixtures above cover.
#[cfg(feature = "xlsx")]
#[test]
fn editable_set_cell_value_creates_new_row_in_part_with_quoted_gt_without_panic() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsx_with_quoted_gt_and_no_target_row();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    // Row index 5 (r="6") does not exist yet, forcing a brand-new <row> (and
    // a brand-new <c> inside it) to be created via the sorted-insertion path.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        spreadsheet.set_cell_value("Data", 5, 0, Cell::Number(99.0))
    }));
    let edit_result =
        result.expect("creating a new row in a part with a quoted '>' must not panic");
    edit_result.expect("edit should succeed with the correct row/cell boundaries");

    let saved = spreadsheet.save().expect("save edited package");
    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    let sheet = reopened.sheet_by_name("Data").expect("data sheet");
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Number(1.0)));
    assert_eq!(sheet.cell(5, 0), Some(&Cell::Number(99.0)));
}

#[cfg(feature = "xlsx")]
fn synthetic_xlsx_with_similarly_named_calc_chain_part() -> Vec<u8> {
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
        br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/precalcChained.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/calcChain.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.calcChain+xml"/></Types>"#,
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
        br#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Other" sheetId="2" r:id="rId2"/></sheets></workbook>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/_rels/workbook.xml.rels",
        // rId2's Target contains the literal substring "calcChain" (inside
        // "precalcChained.xml") without referring to the real calc chain part.
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/precalcChained.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/calcChain" Target="calcChain.xml"/></Relationships>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/worksheets/sheet1.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/worksheets/precalcChained.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>7</v></c></row></sheetData></worksheet>"#,
    );
    add(
        &mut zip,
        opt,
        "xl/calcChain.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><calcChain xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><c r="A1" i="1"/></calcChain>"#,
    );
    zip.finish().unwrap().into_inner()
}

/// BUG 3 (regression): calc-chain invalidation must only remove the
/// `Override`/`Relationship` that actually refers to `xl/calcChain.xml`, not
/// any part whose path merely contains the substring "calcChain" (here
/// `xl/worksheets/precalcChained.xml`).
#[cfg(feature = "xlsx")]
#[test]
fn editable_cell_edit_preserves_part_whose_path_merely_contains_calc_chain_substring() {
    use rxls::{Cell, Spreadsheet};

    let input = synthetic_xlsx_with_similarly_named_calc_chain_part();
    let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

    spreadsheet
        .set_cell_value("Data", 0, 0, Cell::Number(123.0))
        .expect("edit unrelated sheet's cell");

    let saved = spreadsheet.save().expect("save edited package");

    // The real calc chain part and its references are gone...
    assert!(!zip_has_part(&saved, "xl/calcChain.xml"));
    let content_types = zip_part_string(&saved, "[Content_Types].xml");
    assert!(!content_types.contains(r#"PartName="/xl/calcChain.xml""#));
    let rels = zip_part_string(&saved, "xl/_rels/workbook.xml.rels");
    assert!(!rels.contains(r#"Target="calcChain.xml""#));

    // ...but the unrelated part whose path merely contains "calcChain"
    // survives untouched, in both its own bytes and its registrations.
    assert!(zip_has_part(&saved, "xl/worksheets/precalcChained.xml"));
    assert!(content_types.contains("precalcChained.xml"));
    assert!(rels.contains(r#"Target="worksheets/precalcChained.xml""#));
    assert_eq!(
        zip_part_string(&saved, "xl/worksheets/precalcChained.xml"),
        zip_part_string(&input, "xl/worksheets/precalcChained.xml"),
    );

    let reopened = Workbook::open(&saved).expect("reopen edited xlsx");
    assert_eq!(
        reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Number(123.0))
    );
    assert_eq!(
        reopened.sheet_by_name("Other").and_then(|s| s.cell(0, 0)),
        Some(&Cell::Number(7.0)),
        "the sheet with a similarly-named part must still parse as a valid worksheet"
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_open_reports_xls_as_read_only() {
    use rxls::{EditCapability, EditReadOnlyReason, Spreadsheet};

    let spreadsheet =
        Spreadsheet::open(include_bytes!("fixtures/xls/reader-basic.xls")).expect("open xls");

    assert_eq!(
        spreadsheet.edit_capability(),
        &EditCapability::ReadOnly(EditReadOnlyReason::LegacyBiff)
    );
    assert!(spreadsheet.save().is_err());
}

#[cfg(all(feature = "xlsx", feature = "xlsb"))]
#[test]
fn editable_open_reports_xlsb_as_read_only() {
    use rxls::{EditCapability, EditReadOnlyReason, Spreadsheet};

    let spreadsheet =
        Spreadsheet::open(include_bytes!("fixtures/xlsb/reader-basic.xlsb")).expect("open xlsb");

    assert_eq!(
        spreadsheet.edit_capability(),
        &EditCapability::ReadOnly(EditReadOnlyReason::BinaryPackage)
    );
}

#[cfg(all(feature = "xlsx", feature = "ods"))]
#[test]
fn editable_open_reports_ods_as_read_only() {
    use rxls::{EditCapability, EditReadOnlyReason, Spreadsheet};

    let spreadsheet =
        Spreadsheet::open(include_bytes!("fixtures/ods/repeated-hidden.ods")).expect("open ods");

    assert_eq!(
        spreadsheet.edit_capability(),
        &EditCapability::ReadOnly(EditReadOnlyReason::OpenDocument)
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn editable_open_reports_meta_lossy_package_as_read_only() {
    use rxls::{Cell, EditCapability, EditReadOnlyReason, Spreadsheet};
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    // No `[Content_Types].xml` at all: the read side (which never consults
    // Content_Types) still opens fine, but `Package::from_bytes` marks the
    // package `meta_lossy` because OPC metadata can't be regenerated
    // faithfully on save. Edit capability must reflect that, not silently
    // claim `ReadWrite`.
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let add = |zip: &mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>, name: &str, bytes: &[u8]| {
        zip.start_file(name, opt).unwrap();
        zip.write_all(bytes).unwrap();
    };
    add(
        &mut zip,
        "_rels/.rels",
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
    );
    add(
        &mut zip,
        "xl/workbook.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
    );
    add(
        &mut zip,
        "xl/_rels/workbook.xml.rels",
        br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
    );
    add(
        &mut zip,
        "xl/worksheets/sheet1.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#,
    );
    let bytes = zip.finish().unwrap().into_inner();

    let mut spreadsheet = Spreadsheet::open(&bytes).expect("open despite missing Content_Types");
    assert_eq!(
        spreadsheet.edit_capability(),
        &EditCapability::ReadOnly(EditReadOnlyReason::PackageMetadataLoss)
    );
    assert!(
        spreadsheet
            .set_cell_value("Data", 0, 0, Cell::Number(2.0))
            .is_err(),
        "an edit must be refused, not silently regenerate lossy OPC metadata"
    );
    assert!(spreadsheet.edited_parts().is_empty());

    // The edit gate blocks *edits*, not the whole spreadsheet: reading still
    // works, and since nothing was (or could be) touched, `save()` still
    // succeeds as a byte-preserving no-op passthrough of the original lossy
    // package -- it must never try to regenerate the missing Content_Types
    // from an incomplete in-memory view.
    assert_eq!(spreadsheet.workbook().worksheets().len(), 1);
    let saved = spreadsheet.save().expect("no-op save of a lossy package");
    assert!(!zip_has_part(&saved, "[Content_Types].xml"));
    assert_eq!(
        zip_part(&saved, "xl/worksheets/sheet1.xml"),
        zip_part(&bytes, "xl/worksheets/sheet1.xml")
    );
}

/// Keep a committed `.xls` fixture so the legacy BIFF/CFB reader has real
/// container coverage in the tracked corpus, not only synthetic unit-test bytes.
#[test]
fn committed_xls_fixture_exposes_legacy_reader_surface() {
    use rxls::Cell;

    let wb = Workbook::open(include_bytes!("fixtures/xls/reader-basic.xls")).expect("fixture");

    assert_eq!(wb.sheets.len(), 2);
    assert_eq!(wb.sheets[0].name, "Data");
    assert_eq!(wb.sheets[1].name, "Hidden");
    assert_eq!(wb.defined_names(), &[("LegacyAnswer".into(), "42".into())]);

    let sheet = wb.sheet_by_name("Data").expect("Data sheet");
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("item".into())));
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Text("amount".into())));
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Text("road".into())));
    assert_eq!(sheet.cell(1, 1), Some(&Cell::Number(42.0)));
    assert_eq!(sheet.cell(2, 0), Some(&Cell::Text("입찰공고".into())));
    assert_eq!(sheet.cell(2, 1), Some(&Cell::Date(45366.0)));
    assert_eq!(sheet.formatted(2, 1), Some("2024-03-15"));
    assert_eq!(sheet.merged_ranges(), &[(3, 0, 3, 2)]);
    assert_eq!(
        sheet.hyperlinks(),
        &[(4, 0, "https://example.com/xls".into())]
    );
    assert_eq!(sheet.comments()[0].row, 1);
    assert_eq!(sheet.comments()[0].col, 1);
    assert_eq!(sheet.comments()[0].text, "legacy review");
    assert_eq!(sheet.comments()[0].author.as_deref(), Some("fixture"));

    assert!(wb
        .sheet_by_name("Hidden")
        .expect("Hidden sheet")
        .is_hidden());
}

/// The tracked legacy fixture is a licensed, deterministic derivative of a
/// real Korean workbook and exercises BIFF5's byte-oriented `Book` stream.
#[test]
fn committed_korean_biff5_fixture_decodes_cp949_exactly() {
    use rxls::Cell;

    let wb = Workbook::open(include_bytes!("fixtures/xls/korean-cp949-biff5.xls"))
        .expect("Korean BIFF5 fixture");
    let sheet = wb.sheet_by_name("작업표").expect("Korean sheet name");

    assert_eq!(
        sheet.cell(0, 0),
        Some(&Cell::Text("조립 작업 표준서".into()))
    );
    assert_eq!(
        sheet.cell(1, 0),
        Some(&Cell::Text("체결(TIGHTENING)".into()))
    );
    assert_eq!(
        sheet.cell(2, 0),
        Some(&Cell::Text("클램핑(CLAMPING)".into()))
    );
    assert_eq!(
        sheet.cell(3, 0),
        Some(&Cell::Text("확인(CONFIRMATION)".into()))
    );
}

/// The synthetic BIFF8 Korean fixture exercises an SST/CONTINUE compression
/// transition, numeric adjacency, and a cached-string formula end to end.
#[test]
fn committed_korean_biff8_fixture_matches_the_exact_typed_oracle() {
    use rxls::{Cell, ContainerParseMode, RecoveryCode, WorkbookReport};

    let wb = Workbook::open(include_bytes!("fixtures/xls/korean-unicode-biff8.xls"))
        .expect("Korean BIFF8 fixture");
    assert_eq!(wb.sheets.len(), 1);
    let sheet = wb.sheet_by_name("한글표").expect("Korean sheet name");
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("K-한글".into())));
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Number(949.0)));
    assert_eq!(
        sheet.cell(1, 0),
        Some(&Cell::Formula {
            formula: "\"확인\"".into(),
            cached: Box::new(Cell::Text("확인".into())),
        })
    );

    let provenance = wb.parse_provenance();
    assert_eq!(provenance.container, ContainerParseMode::Primary);
    assert_eq!(provenance.recoveries(), &[] as &[RecoveryCode]);
    assert!(!provenance.recoveries_truncated());
    assert!(!provenance.partial);

    assert_eq!(
        WorkbookReport::from_workbook("xls", &wb).to_json(),
        r#"{"schema_version":2,"format":"xls","stats":{"sheets":1,"cells":3,"formulas":1,"text_truncated":false},"properties":{"title":null,"subject":null,"creator":null,"keywords":null,"description":null,"last_modified_by":null,"company":null,"created":null},"defined_names_count":0,"local_defined_names_count":0,"features":{"comments":0,"data_validations":0,"tables":0,"merged_ranges":0,"hyperlinks":0,"images":0,"charts":0,"sparklines":0,"conditional_formatting":0,"hidden_sheets":0,"frozen_panes":0,"page_setup":0,"protection":0,"pivot_tables":0,"vba_project":false,"threaded_comments":0,"external_links":0,"custom_xml":0},"evaluation":{"computed":1,"errors":0,"cached":0,"unsupported":0,"truncated":false,"by_reason":{}},"provenance":{"container":"primary","recoveries":[],"recoveries_truncated":false,"partial":false},"warnings":[]}"#
    );
}

/// Formula text is reconstructed from BIFF tokens, not from cached results.
/// This workbook was saved as BIFF8 by LibreOffice from the committed OOXML
/// source, so these expectations are independent of rxls' token writer/tests.
#[test]
fn libreoffice_biff8_fixture_preserves_exact_formula_sources() {
    use rxls::Cell;

    fn formula_source(cell: Option<&Cell>) -> &str {
        match cell {
            Some(Cell::Formula { formula, .. }) => formula,
            other => panic!("expected formula cell, got {other:?}"),
        }
    }

    let wb = Workbook::open(include_bytes!("fixtures/formula/biff8/formula-source.xls"))
        .expect("LibreOffice BIFF8 formula fixture");
    let sheet = wb.sheet_by_name("Calc").expect("Calc sheet");
    let expected = [
        ((0, 1), "ABS($A$1)"),
        ((0, 2), "TRUE"),
        ((0, 3), "FALSE"),
        ((0, 4), "NOW()"),
        ((1, 1), "$A$1+A$1+$A1+A1"),
        ((2, 1), "'Input Data'!$B$3"),
        ((3, 1), "Answer"),
        ((4, 1), "A5*2"),
        ((5, 1), "A6*2"),
    ];
    for ((row, col), source) in expected {
        assert_eq!(formula_source(sheet.cell(row, col)), source, "R{row}C{col}");
    }
}

#[test]
fn libreoffice_biff8_formula_sources_feed_deterministic_evaluation() {
    use rxls::{Cell, FormulaEvaluation, FormulaUnsupportedReason};

    let wb = Workbook::open(include_bytes!("fixtures/formula/biff8/formula-source.xls"))
        .expect("LibreOffice BIFF8 formula fixture");
    for ((row, col), expected) in [
        ((0, 1), Cell::Number(5.0)),
        ((0, 2), Cell::Bool(true)),
        ((0, 3), Cell::Bool(false)),
        ((1, 1), Cell::Number(20.0)),
        ((2, 1), Cell::Number(7.0)),
        ((3, 1), Cell::Number(7.0)),
        ((4, 1), Cell::Number(6.0)),
        ((5, 1), Cell::Number(8.0)),
    ] {
        assert_eq!(
            wb.evaluate_cell("Calc", row, col),
            FormulaEvaluation::Computed(expected),
            "R{row}C{col}"
        );
    }
    assert!(matches!(
        wb.evaluate_cell("Calc", 0, 4),
        FormulaEvaluation::Fallback {
            reason: FormulaUnsupportedReason::Volatile,
            ..
        }
    ));
}

/// Keep a committed `.xlsb` fixture so the binary workbook reader has real ZIP
/// package coverage in the tracked corpus, not only synthetic unit-test bytes.
#[cfg(feature = "xlsb")]
#[test]
fn committed_xlsb_fixture_exposes_binary_reader_surface() {
    use rxls::Cell;

    let wb = Workbook::open(include_bytes!("fixtures/xlsb/reader-basic.xlsb")).expect("fixture");

    assert_eq!(wb.sheets.len(), 2);
    assert_eq!(wb.sheets[0].name, "Data");
    assert_eq!(wb.sheets[1].name, "Hidden");

    let sheet = wb.sheet_by_name("Data").expect("Data sheet");
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("item".into())));
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Text("amount".into())));
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Text("road".into())));
    assert_eq!(sheet.cell(1, 1), Some(&Cell::Number(42.0)));
    assert_eq!(sheet.cell(2, 0), Some(&Cell::Text("reported".into())));
    assert_eq!(sheet.cell(2, 1), Some(&Cell::Date(45366.0)));
    assert_eq!(sheet.formatted(2, 1), Some("2024-03-15"));
    assert_eq!(sheet.merged_ranges(), &[(3, 0, 3, 2)]);
    assert_eq!(
        sheet.hyperlinks(),
        &[(4, 0, "https://example.com/xlsb".into())]
    );
    let comments = sheet.comments();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].row, 1);
    assert_eq!(comments[0].col, 1);
    assert_eq!(comments[0].text, "binary review");
    assert_eq!(comments[0].author.as_deref(), Some("fixture"));
    let tables = sheet.tables();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "BinaryTable");
    assert_eq!(tables[0].range, (0, 0, 2, 1));
    assert_eq!(tables[0].columns, ["item", "amount"]);
    assert_eq!(tables[0].style.as_deref(), Some("TableStyleMedium9"));

    assert!(wb
        .sheet_by_name("Hidden")
        .expect("Hidden sheet")
        .is_hidden());
}

/// R3: workbook-level table lookup mirrors calamine's table discovery helpers
/// while borrowing the existing sheet-owned metadata.
#[cfg(feature = "xlsx")]
#[test]
fn workbook_exposes_table_lookup_facades() {
    use rxls::{Cell, Reader};

    let wb =
        Workbook::open(include_bytes!("fixtures/xlsx/reader-structural.xlsx")).expect("fixture");

    assert_eq!(wb.table_names(), vec!["DataTable"]);
    assert_eq!(wb.table_names_in_sheet("Data"), vec!["DataTable"]);
    assert!(wb.table_names_in_sheet("Hidden").is_empty());
    assert!(wb.table_names_in_sheet("Missing").is_empty());

    let (sheet_name, table) = wb.table_by_name("DataTable").expect("DataTable");
    assert_eq!(sheet_name, "Data");
    assert_eq!(table.name(), "DataTable");
    assert_eq!(table.range, (0, 0, 2, 2));
    assert_eq!(table.range(), (0, 0, 2, 2));
    assert_eq!(table.columns, ["item", "amount", "ok"]);
    assert_eq!(table.columns(), ["item", "amount", "ok"]);

    let sheet = wb.sheet_by_name(sheet_name).expect("table parent sheet");
    let data_from_table = table.data(sheet);
    assert_eq!(data_from_table.start(), Some((1, 0)));
    assert_eq!(data_from_table.end(), Some((2, 2)));
    assert_eq!(
        data_from_table.get((0, 0)),
        Some(&Cell::Text("road".into()))
    );

    let (sheet_name, table_ref) = wb
        .table_by_name_ref("DATATABLE")
        .expect("borrowed table alias");
    assert_eq!(sheet_name, "Data");
    assert_eq!(table_ref.name(), "DataTable");

    let (sheet_name, table) = wb.table_by_name("datatable").expect("case-insensitive");
    assert_eq!(sheet_name, "Data");
    assert_eq!(table.name, "DataTable");
    assert!(wb.table_by_name("Missing").is_none());
    assert!(wb.table_by_name_ref("Missing").is_none());

    let (sheet_name, data) = wb.table_data_by_name("DataTable").expect("table data");
    assert_eq!(sheet_name, "Data");
    assert_eq!(data.start(), Some((1, 0)));
    assert_eq!(data.end(), Some((2, 2)));
    assert_eq!(data.get((0, 0)), Some(&Cell::Text("road".into())));
    assert_eq!(data.get((0, 1)), Some(&Cell::Number(12.5)));
    assert_eq!(data.get((1, 2)), Some(&Cell::Bool(false)));
    assert_eq!(
        data.used_cells().collect::<Vec<_>>(),
        vec![
            (0, 0, &Cell::Text("road".into())),
            (0, 1, &Cell::Number(12.5)),
            (0, 2, &Cell::Bool(true)),
            (1, 0, &Cell::Text("bridge".into())),
            (1, 1, &Cell::Number(7.0)),
            (1, 2, &Cell::Bool(false)),
        ]
    );
    let (sheet_name, data) = wb
        .table_data_by_name("DATATABLE")
        .expect("case-insensitive table data");
    assert_eq!(sheet_name, "Data");
    assert_eq!(data.get((0, 0)), Some(&Cell::Text("road".into())));
    assert!(wb.table_data_by_name("Missing").is_none());
    let (sheet_name, data) = wb
        .table_data_by_name_ref("datatable")
        .expect("borrowed table data alias");
    assert_eq!(sheet_name, "Data");
    assert_eq!(data.get((1, 0)), Some(&Cell::Text("bridge".into())));
    assert!(wb.table_data_by_name_ref("Missing").is_none());

    let (reader_sheet, reader_data) =
        Reader::table_data_by_name(&wb, "DataTable").expect("reader table data");
    assert_eq!(reader_sheet, "Data");
    assert_eq!(reader_data.size(), (2, 3));
    let (reader_sheet, reader_table) =
        Reader::table_by_name(&wb, "datAtable").expect("reader table lookup");
    assert_eq!(reader_sheet, "Data");
    assert_eq!(reader_table.name, "DataTable");
    let (reader_sheet, reader_table) =
        Reader::table_by_name_ref(&wb, "DataTable").expect("reader table ref lookup");
    assert_eq!(reader_sheet, "Data");
    assert_eq!(reader_table.columns(), ["item", "amount", "ok"]);
    let (reader_sheet, reader_data) =
        Reader::table_data_by_name_ref(&wb, "DataTable").expect("reader table data ref");
    assert_eq!(reader_sheet, "Data");
    assert_eq!(reader_data.get((0, 1)), Some(&Cell::Number(12.5)));
}

/// R3: workbook-level merged-cell lookup mirrors calamine's direct worksheet
/// merge helpers while borrowing the existing sheet-owned metadata.
#[cfg(feature = "xlsx")]
#[test]
fn workbook_exposes_merge_lookup_facades() {
    use rxls::{Dimensions, Reader};

    fn data_merge_count<R: Reader>(reader: &R) -> usize {
        reader
            .worksheet_merge_cells("Data")
            .map_or(0, |merges| merges.len())
    }

    let wb =
        Workbook::open(include_bytes!("fixtures/xlsx/reader-structural.xlsx")).expect("fixture");

    assert_eq!(data_merge_count(&wb), 1);
    assert_eq!(wb.worksheet_merge_cells("Data"), Some(&[(3, 0, 3, 2)][..]));
    assert_eq!(wb.worksheet_merge_cells_at(0), Some(&[(3, 0, 3, 2)][..]));
    assert_eq!(wb.worksheet_merge_cells("Hidden"), Some(&[][..]));
    assert_eq!(wb.worksheet_merge_cells("Missing"), None);
    assert_eq!(wb.worksheet_merge_cells_at(99), None);
    assert_eq!(
        wb.merged_regions(),
        vec![("Data", Dimensions::new((3, 0), (3, 2)))]
    );
    assert_eq!(
        wb.merged_regions_by_sheet("Data"),
        vec![Dimensions::new((3, 0), (3, 2))]
    );
    assert!(wb.merged_regions_by_sheet("Hidden").is_empty());
    assert!(wb.merged_regions_by_sheet("Missing").is_empty());

    assert_eq!(
        Reader::worksheet_merge_cells(&wb, "Data"),
        Some(&[(3, 0, 3, 2)][..])
    );
    assert_eq!(
        Reader::worksheet_merge_cells_at(&wb, 0),
        Some(&[(3, 0, 3, 2)][..])
    );
    assert_eq!(
        Reader::merged_regions(&wb),
        vec![("Data", Dimensions::new((3, 0), (3, 2)))]
    );
    assert_eq!(
        Reader::merged_regions_by_sheet(&wb, "Data"),
        vec![Dimensions::new((3, 0), (3, 2))]
    );
}

/// R3: grouped worksheet metadata gives generic readers direct access to the
/// sheet-owned metadata surface without naming the concrete `Workbook` type.
#[cfg(feature = "xlsx")]
#[test]
fn workbook_exposes_grouped_worksheet_metadata_facade() {
    use rxls::{Reader, SheetType, SheetVisible};

    fn worksheet_metadata_names<R: Reader>(reader: &R) -> Vec<String> {
        reader
            .worksheets_metadata()
            .into_iter()
            .map(|metadata| metadata.name.to_string())
            .collect()
    }

    fn summarize<R: Reader>(reader: &R, sheet: &str) -> Option<(String, usize, usize, usize)> {
        let metadata = reader.worksheet_metadata(sheet)?;
        Some((
            metadata.name.to_string(),
            metadata.merged_ranges.len(),
            metadata.hyperlinks.len(),
            metadata.tables.len(),
        ))
    }

    let wb =
        Workbook::open(include_bytes!("fixtures/xlsx/reader-structural.xlsx")).expect("fixture");

    assert_eq!(
        worksheet_metadata_names(&wb),
        vec!["Data".to_string(), "Hidden".to_string()]
    );
    let worksheet_metadata = wb.worksheets_metadata();
    assert_eq!(worksheet_metadata.len(), 2);
    assert_eq!(worksheet_metadata[0].name, "Data");
    assert_eq!(worksheet_metadata[0].merged_ranges, &[(3, 0, 3, 2)]);
    assert_eq!(worksheet_metadata[1].name, "Hidden");
    assert_eq!(worksheet_metadata[1].visible, SheetVisible::Hidden);

    assert_eq!(summarize(&wb, "Data"), Some(("Data".to_string(), 1, 1, 1)));

    let data = wb.worksheet_metadata("Data").expect("Data metadata");
    assert_eq!(data.name, "Data");
    assert_eq!(data.sheet_type, SheetType::WorkSheet);
    assert_eq!(data.visible, SheetVisible::Visible);
    assert_eq!(data.merged_ranges, &[(3, 0, 3, 2)]);
    assert_eq!(
        data.hyperlinks,
        &[(4, 0, "https://example.com/rxls".to_string())]
    );
    assert_eq!(data.comments[0].text, "needs review");
    assert_eq!(data.tables[0].name, "DataTable");
    assert!(data.dimensions.is_some());
    assert!(data.data_validations.is_empty());
    assert!(data.conditional_formats.is_empty());
    assert!(data.page_setup.is_some());
    assert_eq!(data.autofilter_range, Some((0, 0, 2, 2)));
    assert_eq!(data.tab_color, Some(rxls::Color::rgb(0x12, 0x34, 0x56)));
    assert!(data.print_gridlines);
    assert!(data.print_headings);
    assert!(data.images.is_empty());
    assert!(data.charts.is_empty());
    assert!(data.sparklines.is_empty());
    assert_eq!(
        data,
        wb.worksheet_metadata("Data")
            .expect("same Data metadata remains comparable")
    );
    assert_eq!(data.clone(), data);

    let hidden = Reader::worksheet_metadata_at(&wb, 1).expect("Hidden metadata");
    assert_eq!(hidden.name, "Hidden");
    assert_eq!(hidden.visible, SheetVisible::Hidden);
    assert_ne!(data, hidden);
    assert!(Reader::worksheet_metadata(&wb, "Missing").is_none());
    assert!(Reader::worksheet_metadata_at(&wb, 99).is_none());
}

/// R3: reader-populated data validations are available through the same public
/// sheet metadata surface as merges, comments, hyperlinks, and tables.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_exposes_read_data_validations_public_api() {
    use rxls::{DataValidation, DvKind, DvOp};

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "pick");
        s.add_data_validation(DataValidation::list((1, 0, 3, 0), "\"Yes,No\""));
        s.add_data_validation(DataValidation {
            sqref: (1, 1, 3, 1),
            kind: DvKind::Whole,
            operator: DvOp::Between,
            formula1: "1".into(),
            formula2: Some("9".into()),
            allow_blank: false,
            show_input_message: true,
            show_error_message: true,
            prompt: None,
            error: Some(("Bounds".into(), "1..9 only".into())),
        });
    }

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let validations = back
        .sheet_by_name("Data")
        .expect("Data sheet")
        .data_validations();

    assert_eq!(validations.len(), 2);
    assert_eq!(validations[0].sqref, (1, 0, 3, 0));
    assert_eq!(validations[0].kind, DvKind::List);
    assert!(validations[0].formula1.contains("Yes,No"));
    assert_eq!(validations[1].sqref, (1, 1, 3, 1));
    assert_eq!(validations[1].kind, DvKind::Whole);
    assert_eq!(validations[1].operator, DvOp::Between);
    assert_eq!(validations[1].formula1, "1");
    assert_eq!(validations[1].formula2.as_deref(), Some("9"));
    assert!(!validations[1].allow_blank);
    assert_eq!(
        validations[1]
            .error
            .as_ref()
            .map(|(title, msg)| (title.as_str(), msg.as_str())),
        Some(("Bounds", "1..9 only"))
    );
}

/// R3: sheet-view and autofilter metadata parsed from OOXML is exposed through
/// the public worksheet metadata surface.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_exposes_sheet_view_and_autofilter_public_api() {
    use rxls::SheetView;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "name");
        s.write(1, 0, "road");
        s.freeze_panes(1, 2);
        s.autofilter(0, 0, 9, 3);
        s.hide_gridlines();
        s.set_show_headers(false);
        s.set_right_to_left(true);
        s.set_zoom(125);
    }

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let sheet = back.sheet_by_name("Data").expect("Data sheet");

    assert_eq!(
        sheet.sheet_view(),
        SheetView {
            freeze: Some((1, 2)),
            hide_gridlines: true,
            zoom: Some(125),
            show_headers: Some(false),
            right_to_left: true,
        }
    );
    assert_eq!(sheet.autofilter_range(), Some((0, 0, 9, 3)));
}

/// W1/W3: sheet-view authoring should expose an object helper matching the
/// public metadata shape, not only a sequence of mutating worksheet calls.
#[cfg(feature = "xlsx")]
#[test]
fn sheet_view_authoring_helper_round_trips_public_metadata() {
    use rxls::SheetView;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("View");
        s.write(0, 0, "name");
        s.set_sheet_view(
            SheetView::new()
                .with_freeze(2, 1)
                .with_hidden_gridlines()
                .with_zoom(150)
                .with_show_headers(false)
                .with_right_to_left(true),
        );
    }

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen sheet-view helper workbook");
    let sheet = back.sheet_by_name("View").expect("View sheet");

    assert_eq!(
        sheet.sheet_view(),
        SheetView {
            freeze: Some((2, 1)),
            hide_gridlines: true,
            zoom: Some(150),
            show_headers: Some(false),
            right_to_left: true,
        }
    );
    assert_eq!(sheet.metadata().sheet_view, sheet.sheet_view());
}

/// R3: worksheet tab color is read back through the public metadata surface
/// instead of remaining writer-only XML.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_exposes_read_tab_color_public_api() {
    use rxls::Color;

    let mut wb = Workbook::new();
    wb.add_sheet("Color").set_tab_color([0x12, 0x34, 0x56]);

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let sheet = back.sheet_by_name("Color").expect("Color sheet");

    assert_eq!(sheet.tab_color(), Some(Color::rgb(0x12, 0x34, 0x56)));
}

/// W1/R3: sheet tab-color authoring should use the shared `Color` model, not
/// only raw RGB arrays, matching read metadata and style color setters.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_tab_color_setter_accepts_color_model() {
    use rxls::{Color, Workbook};

    let mut wb = Workbook::new();
    wb.add_sheet("Color")
        .set_tab_color(Color::rgb(0x44, 0x55, 0x66));

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen color-model tab workbook");
    let sheet = back.sheet_by_name("Color").expect("Color sheet");

    assert_eq!(sheet.tab_color(), Some(Color::rgb(0x44, 0x55, 0x66)));
    assert_eq!(
        sheet.metadata().tab_color,
        Some(Color::rgb(0x44, 0x55, 0x66))
    );
}

/// R3: Excel commonly stores tab colors as theme indexes rather than direct
/// RGB values; the reader should resolve the workbook theme before exposing the
/// public metadata color.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_tab_color_resolves_ooxml_theme_color() {
    use rxls::Color;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Color" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rTheme" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/></Relationships>"#,
        ),
        (
            "xl/theme/theme1.xml",
            r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme name="Custom"><a:accent1><a:srgbClr val="123456"/></a:accent1></a:clrScheme></a:themeElements></a:theme>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetPr><tabColor theme="4"/></sheetPr><sheetData/></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).expect("open theme-color workbook");
    let sheet = wb.sheet_by_name("Color").expect("Color sheet");

    assert_eq!(sheet.tab_color(), Some(Color::rgb(0x12, 0x34, 0x56)));
}

/// R3: OOXML theme colors may carry a tint transform; preserve the resolved
/// color instead of exposing the untinted base theme color.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_tab_color_applies_ooxml_theme_tint() {
    use rxls::Color;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Color" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rTheme" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/></Relationships>"#,
        ),
        (
            "xl/theme/theme1.xml",
            r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme name="Custom"><a:accent1><a:srgbClr val="123456"/></a:accent1></a:clrScheme></a:themeElements></a:theme>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetPr><tabColor theme="4" tint="0.5"/></sheetPr><sheetData/></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).expect("open tinted theme-color workbook");
    let sheet = wb.sheet_by_name("Color").expect("Color sheet");

    assert_eq!(sheet.tab_color(), Some(Color::rgb(0x89, 0x9A, 0xAB)));
}

/// R3: OOXML color metadata can also refer to the workbook's indexed color
/// table. Preserve that resolved color through the public tab-color facade.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_tab_color_resolves_ooxml_indexed_color() {
    use rxls::Color;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Color" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
        ),
        (
            "xl/styles.xml",
            r#"<styleSheet><colors><indexedColors><rgbColor rgb="FF123456"/></indexedColors></colors></styleSheet>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetPr><tabColor indexed="0"/></sheetPr><sheetData/></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).expect("open indexed-color workbook");
    let sheet = wb.sheet_by_name("Color").expect("Color sheet");

    assert_eq!(sheet.tab_color(), Some(Color::rgb(0x12, 0x34, 0x56)));
}

/// R3: when a workbook does not provide a custom indexed color table, OOXML
/// indexed colors still resolve through the default spreadsheet palette.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_tab_color_resolves_ooxml_default_indexed_color() {
    use rxls::Color;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Color" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetPr><tabColor indexed="10"/></sheetPr><sheetData/></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).expect("open default-indexed-color workbook");
    let sheet = wb.sheet_by_name("Color").expect("Color sheet");

    assert_eq!(sheet.tab_color(), Some(Color::rgb(0xFF, 0, 0)));
}

/// R3: worksheet print options are read back through the public metadata
/// surface instead of remaining writer-only XML.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_exposes_read_print_options_public_api() {
    let mut wb = Workbook::new();
    {
        let sheet = wb.add_sheet("Print");
        sheet.set_print_gridlines();
        sheet.set_print_headings();
    }

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let sheet = back.sheet_by_name("Print").expect("Print sheet");

    assert!(sheet.print_gridlines());
    assert!(sheet.print_headings());
}

/// R3: worksheet outline metadata is read back through the public metadata
/// surface instead of remaining writer-only XML.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_exposes_read_outline_public_api() {
    let mut wb = Workbook::new();
    {
        let sheet = wb.add_sheet("Outline");
        sheet.group_rows(1, 3, 1);
        sheet.group_cols(2, 4, 2);
        sheet.collapse_row(4);
        sheet.set_outline_summary(false, false);
    }

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let sheet = back.sheet_by_name("Outline").expect("Outline sheet");

    assert_eq!(sheet.row_outline_levels().get(&1), Some(&1));
    assert_eq!(sheet.row_outline_levels().get(&3), Some(&1));
    assert_eq!(sheet.col_outline_levels().get(&2), Some(&2));
    assert_eq!(sheet.col_outline_levels().get(&4), Some(&2));
    assert!(sheet.collapsed_rows().contains(&4));
    assert!(!sheet.outline_summary_below());
    assert!(!sheet.outline_summary_right());

    let metadata = sheet.metadata();
    assert_eq!(metadata.row_outline_levels.get(&1), Some(&1));
    assert_eq!(metadata.col_outline_levels.get(&2), Some(&2));
    assert!(metadata.collapsed_rows.contains(&4));
    assert!(!metadata.outline_summary_below);
    assert!(!metadata.outline_summary_right);
}

/// R3: conditional-formatting metadata parsed from OOXML is exposed through the
/// same public sheet metadata surface as tables, comments, validations, and
/// drawings.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_exposes_read_conditional_formats_public_api() {
    use rxls::{CfRule, Color, CondFormat, DvOp};

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        for r in 0..5u32 {
            s.write(r, 0, f64::from(r) * 10.0);
            s.write(r, 1, f64::from(r));
        }
        s.add_conditional_format(CondFormat {
            sqref: (0, 0, 4, 0),
            rule: CfRule::CellIs {
                op: DvOp::GreaterThan,
                formula1: "20".into(),
                formula2: None,
                fill: Color::rgb(0xFF, 0xC7, 0xCE),
            },
        });
        s.add_conditional_format(CondFormat {
            sqref: (0, 1, 4, 1),
            rule: CfRule::ColorScale2 {
                min: Color::rgb(0xF8, 0x69, 0x6B),
                max: Color::rgb(0x63, 0xBE, 0x7B),
            },
        });
        s.add_conditional_format(CondFormat {
            sqref: (0, 0, 4, 0),
            rule: CfRule::DataBar {
                color: Color::rgb(0x44, 0xAA, 0x66),
            },
        });
        s.add_conditional_format(CondFormat {
            sqref: (0, 1, 4, 1),
            rule: CfRule::Expression {
                formula: "$B1>2".into(),
                fill: Color::rgb(0xDD, 0xEB, 0xF7),
            },
        });
    }

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let formats = back
        .sheet_by_name("Data")
        .expect("Data sheet")
        .conditional_formats();

    assert_eq!(formats.len(), 4);
    assert_eq!(formats[0].sqref, (0, 0, 4, 0));
    match &formats[0].rule {
        CfRule::CellIs {
            op,
            formula1,
            formula2,
            fill,
        } => {
            assert_eq!(*op, DvOp::GreaterThan);
            assert_eq!(formula1, "20");
            assert!(formula2.is_none());
            assert_eq!(*fill, Color::rgb(0xFF, 0xC7, 0xCE));
        }
        other => panic!("unexpected first conditional format: {other:?}"),
    }

    assert_eq!(formats[1].sqref, (0, 1, 4, 1));
    match &formats[1].rule {
        CfRule::ColorScale2 { min, max } => {
            assert_eq!(*min, Color::rgb(0xF8, 0x69, 0x6B));
            assert_eq!(*max, Color::rgb(0x63, 0xBE, 0x7B));
        }
        other => panic!("unexpected second conditional format: {other:?}"),
    }

    assert_eq!(formats[2].sqref, (0, 0, 4, 0));
    match &formats[2].rule {
        CfRule::DataBar { color } => assert_eq!(*color, Color::rgb(0x44, 0xAA, 0x66)),
        other => panic!("unexpected third conditional format: {other:?}"),
    }

    assert_eq!(formats[3].sqref, (0, 1, 4, 1));
    match &formats[3].rule {
        CfRule::Expression { formula, fill } => {
            assert_eq!(formula, "$B1>2");
            assert_eq!(*fill, Color::rgb(0xDD, 0xEB, 0xF7));
        }
        other => panic!("unexpected fourth conditional format: {other:?}"),
    }
}

/// R3: worksheet page setup metadata parsed from OOXML is exposed through the
/// public sheet metadata surface instead of remaining writer-only.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_exposes_read_page_setup_public_api() {
    use rxls::PageSetup;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "item");
        s.set_page_setup(PageSetup {
            landscape: true,
            margins: Some((0.5, 0.6, 0.7, 0.8, 0.2, 0.25)),
            print_area: Some((0, 0, 9, 4)),
            repeat_rows: Some((0, 1)),
            repeat_cols: Some((0, 2)),
            fit_to_width: Some(1),
            fit_to_height: Some(2),
            header: Some("&CReport".into()),
            footer: Some("&RPage &P".into()),
            paper_size: Some(9),
            scale: Some(85),
            center_horizontally: true,
            center_vertically: true,
            first_page_number: Some(3),
        });
    }

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let setup = back
        .sheet_by_name("Data")
        .expect("Data sheet")
        .page_setup()
        .expect("page setup");

    assert!(setup.landscape);
    assert_eq!(setup.margins, Some((0.5, 0.6, 0.7, 0.8, 0.2, 0.25)));
    assert_eq!(setup.print_area, Some((0, 0, 9, 4)));
    assert_eq!(setup.repeat_rows, Some((0, 1)));
    assert_eq!(setup.repeat_cols, Some((0, 2)));
    assert_eq!(setup.fit_to_width, Some(1));
    assert_eq!(setup.fit_to_height, Some(2));
    assert_eq!(setup.header.as_deref(), Some("&CReport"));
    assert_eq!(setup.footer.as_deref(), Some("&RPage &P"));
    assert_eq!(setup.paper_size, Some(9));
    assert_eq!(setup.scale, Some(85));
    assert!(setup.center_horizontally);
    assert!(setup.center_vertically);
    assert_eq!(setup.first_page_number, Some(3));
}

/// R3: sparklines parsed from OOXML are exposed through the public sheet
/// metadata surface instead of remaining writer-only.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_exposes_read_sparklines_public_api() {
    use rxls::{Sparkline, SparklineKind};

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        for r in 0..5u32 {
            s.write(r, 1, f64::from(r) + 1.0);
        }
        s.add_sparkline(Sparkline {
            location: (5, 0),
            range: "Data!$B$1:$B$5".into(),
            kind: SparklineKind::Column,
        });
    }

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let sparklines = back.sheet_by_name("Data").expect("Data sheet").sparklines();

    assert_eq!(sparklines.len(), 1);
    assert_eq!(sparklines[0].location, (5, 0));
    assert_eq!(sparklines[0].range, "Data!$B$1:$B$5");
    assert_eq!(sparklines[0].kind, SparklineKind::Column);
}

/// Keep an ODS fixture in the tracked corpus as well: repeat expansion, merges,
/// hyperlinks, images, and hidden table style are distinct from the OOXML path.
#[cfg(feature = "ods")]
#[test]
fn committed_ods_fixture_exposes_repeat_merge_hyperlink_surface() {
    use rxls::{Cell, DvKind, DvOp, ImageFmt, SheetView};

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0A, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0,
        0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let wb = Workbook::open(include_bytes!("fixtures/ods/repeated-hidden.ods")).expect("fixture");
    assert_eq!(
        wb.defined_names(),
        &[("VisibleTotal".into(), "Visible!$B$2".into())]
    );
    assert_eq!(
        wb.metadata().properties.title.as_deref(),
        Some("rxls ODS fixture")
    );
    assert_eq!(
        wb.metadata().properties.creator.as_deref(),
        Some("rxls fixture generator")
    );

    let sheet = wb.sheet_by_name("Visible").expect("Visible sheet");
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("name".into())));
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Text("road".into())));
    assert_eq!(sheet.cell(2, 0), Some(&Cell::Text("road".into())));
    assert_eq!(sheet.cell(1, 1), Some(&Cell::Number(125.0)));
    assert_eq!(sheet.merged_ranges(), &[(3, 0, 3, 1)]);
    assert_eq!(
        sheet.hyperlinks(),
        &[(4, 0, "https://example.com/ods".into())]
    );
    let images = sheet.images();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].format, ImageFmt::Png);
    assert_eq!(images[0].data, PNG_1X1);
    assert_eq!(images[0].from, (5, 1));
    assert_eq!(sheet.autofilter_range(), Some((0, 0, 2, 1)));
    assert_eq!(sheet.tables().len(), 1);
    assert_eq!(sheet.tables()[0].name, "VisibleBlock");
    assert_eq!(sheet.tables()[0].range, (0, 0, 2, 1));
    assert_eq!(sheet.tables()[0].columns, ["name", "amount"]);
    let comments = sheet.comments();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].row, 4);
    assert_eq!(comments[0].col, 0);
    assert_eq!(comments[0].text, "verify external link");
    assert_eq!(comments[0].author.as_deref(), Some("fixture"));
    let validations = sheet.data_validations();
    assert_eq!(validations.len(), 2);
    assert_eq!(validations[0].sqref, (1, 1, 1, 1));
    assert_eq!(validations[1].sqref, (2, 1, 2, 1));
    for validation in validations {
        assert_eq!(validation.kind, DvKind::Custom);
        assert_eq!(validation.operator, DvOp::Between);
        assert_eq!(validation.formula1, "cell-content() >= 0");
        assert!(validation.formula2.is_none());
        assert!(!validation.allow_blank);
    }
    let page_setup = sheet.page_setup().expect("page setup");
    assert_eq!(page_setup.print_area, Some((0, 0, 5, 1)));
    assert_eq!(page_setup.repeat_rows, Some((0, 0)));
    assert_eq!(page_setup.repeat_cols, Some((0, 1)));
    assert_eq!(
        sheet.sheet_view(),
        SheetView {
            freeze: Some((1, 1)),
            hide_gridlines: true,
            zoom: Some(125),
            show_headers: Some(false),
            right_to_left: false,
        }
    );

    assert!(wb
        .sheet_by_name("Hidden")
        .expect("Hidden sheet")
        .is_hidden());
}

/// R3: ODS table styles can carry sheet tab colors; surface them through the
/// same public sheet and generic metadata APIs as OOXML/XLS/XLSB readers.
#[cfg(feature = "ods")]
#[test]
fn ods_table_style_tab_color_surfaces_public_metadata() {
    use std::io::Write;

    use rxls::{Color, Reader};
    use zip::write::SimpleFileOptions;

    let content = r##"<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0">
  <office:automatic-styles>
    <style:style style:name="ta_color" style:family="table">
      <style:table-properties table:display="true" table:tab-color="#123456"/>
    </style:style>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Colored" table:style-name="ta_color"/>
      <table:table table:name="Plain"/>
    </office:spreadsheet>
  </office:body>
</office:document-content>"##;

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    zip.start_file("mimetype", options).unwrap();
    zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
        .unwrap();
    zip.start_file("content.xml", options).unwrap();
    zip.write_all(content.as_bytes()).unwrap();
    let bytes = zip.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).expect("open ODS");
    let colored = wb.sheet_by_name("Colored").expect("colored sheet");
    assert_eq!(colored.tab_color(), Some(Color::rgb(0x12, 0x34, 0x56)));
    assert_eq!(
        colored.metadata().tab_color,
        Some(Color::rgb(0x12, 0x34, 0x56))
    );
    assert_eq!(
        Reader::worksheet_metadata(&wb, "Colored")
            .expect("generic metadata")
            .tab_color,
        Some(Color::rgb(0x12, 0x34, 0x56))
    );
    assert_eq!(
        wb.sheet_by_name("Plain").expect("plain sheet").tab_color(),
        None
    );
}

/// Author a styled, multi-sheet workbook through the builder API, serialize, and
/// reopen it with the reader — the full public write→read surface round-trips
/// typed values (text / number / date / bool), Unicode, and multiple sheets.
#[cfg(feature = "xlsx")]
#[test]
fn authoring_roundtrips_through_the_public_api() {
    use rxls::{Cell, CellStyle, HAlign};

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("실적");
        s.write(0, 0, "공고명");
        s.write_styled(
            0,
            1,
            "추정가격",
            &CellStyle::new().bold().fill([0xDD, 0xEB, 0xF7]),
        );
        s.write(1, 0, "도로 포장 공사");
        s.write_styled(1, 1, 150_000_000.0, &CellStyle::new().num_fmt("₩#,##0"));
        s.write(1, 2, Cell::date(46_000.0));
        s.write(2, 0, true);
        s.merge(3, 0, 3, 2);
        s.write_styled(3, 0, "합계", &CellStyle::new().bold().align(HAlign::Center));
        s.set_col_width(0, 30.0);
        s.set_row_height(1, 24.0);
        s.hide_column(2);
        s.hide_row(2);
        s.freeze_panes(1, 0);
        s.autofilter(0, 0, 1, 2);
    }
    wb.add_sheet("Sheet2").write(0, 0, 42.0);

    let bytes = wb.to_xlsx();
    assert_eq!(
        &bytes[..4],
        b"PK\x03\x04",
        "authored .xlsx must be a ZIP container"
    );

    let reread = Workbook::open(&bytes).expect("authored .xlsx must reopen");
    assert_eq!(reread.sheets.len(), 2);

    let s0 = &reread.sheets[0];
    assert_eq!(s0.name, "실적");
    assert_eq!(s0.cell(0, 0), Some(&Cell::Text("공고명".into())));
    assert_eq!(s0.cell(1, 0), Some(&Cell::Text("도로 포장 공사".into())));
    assert_eq!(s0.cell(1, 1), Some(&Cell::Number(150_000_000.0)));
    assert!(
        matches!(s0.cell(1, 2), Some(Cell::Date(_))),
        "date round-trip"
    );
    assert_eq!(s0.cell(2, 0), Some(&Cell::Bool(true)));
    assert_eq!(reread.sheets[1].cell(0, 0), Some(&Cell::Number(42.0)));
    // Merged ranges round-trip: authored merge → <mergeCells> → read back.
    assert_eq!(s0.merged_ranges(), &[(3, 0, 3, 2)]);
    assert_eq!(s0.column_widths().get(&0), Some(&30.0));
    assert_eq!(s0.row_heights().get(&1), Some(&24.0));
    assert!(s0.hidden_columns().contains(&2));
    assert!(s0.hidden_rows().contains(&2));
    assert!(s0
        .cell_style(0, 1)
        .and_then(|style| style.font.as_ref())
        .is_some_and(|font| font.bold));
    assert_eq!(
        s0.cell_style(1, 1)
            .and_then(|style| style.num_fmt.as_deref()),
        Some("₩#,##0")
    );
}

/// The calamine-style range facade: by-name sheet lookup + grouped row iteration.
#[cfg(feature = "xlsx")]
#[test]
fn range_facade_by_name_and_rows() {
    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("데이터");
        s.write(0, 0, "a");
        s.write(0, 1, "b");
        s.write(2, 0, 1.0);
    }
    let reread = Workbook::open(&wb.to_xlsx()).expect("reopen");
    assert_eq!(reread.sheet_names(), vec!["데이터"]);
    let s = reread.sheet_by_name("데이터").expect("by name");
    let rows: Vec<_> = s.rows().collect();
    assert_eq!(rows.len(), 2); // only rows 0 and 2 have cells
    assert_eq!(rows[0].0, 0);
    assert_eq!(rows[0].1.len(), 2);
    assert_eq!(rows[1].0, 2);
}

#[cfg(feature = "xlsx")]
#[test]
fn csv_export_uses_display_text_and_escapes_fields() {
    use rxls::Format;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("CSV");
        s.write(0, 0, "label");
        s.write(0, 1, "note");
        s.write(0, 2, "percent");
        s.write(1, 0, "Road");
        s.write(1, 1, "quoted, \"fast\"\nline");
        s.write_number_with_format(1, 2, 0.5, &Format::new().set_num_format("0%"));
        s.write(2, 0, "Bridge");
        s.write(2, 2, true);
    }

    let reread = Workbook::open(&wb.to_xlsx()).expect("reopen csv fixture");
    let sheet = reread.sheet_by_name("CSV").expect("csv sheet");
    assert_eq!(
        sheet.to_csv(),
        "label,note,percent\nRoad,\"quoted, \"\"fast\"\"\nline\",50%\nBridge,,TRUE"
    );
    assert_eq!(
        reread.to_csv(0).expect("workbook csv"),
        sheet.to_csv(),
        "workbook wrapper should use the same default CSV output"
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn html_export_uses_display_text_escapes_and_preserves_merges() {
    use rxls::Format;

    let mut wb = Workbook::new();
    {
        let sheet = wb.add_sheet("HTML");
        sheet.write(0, 0, "Name");
        sheet.write(0, 1, "Score");
        sheet.write(1, 0, "Road <Bridge>");
        sheet.write_number_with_format(1, 1, 0.5, &Format::new().set_num_format("0%"));
        sheet.merge(2, 0, 2, 1);
        sheet.write(2, 0, "merged & ok");
    }

    let reread = Workbook::open(&wb.to_xlsx()).expect("reopen html fixture");
    let sheet = reread.sheet_by_name("HTML").expect("html sheet");

    assert_eq!(
        sheet.to_html(),
        "<table><tr><td>Name</td><td>Score</td></tr><tr><td>Road &lt;Bridge&gt;</td><td>50%</td></tr><tr><td colspan=\"2\">merged &amp; ok</td></tr></table>"
    );
    assert_eq!(reread.to_html(0), Some(sheet.to_html()));
}

#[cfg(feature = "xlsx")]
#[test]
fn markdown_export_uses_gfm_table_and_falls_back_to_html_for_merges() {
    let mut wb = Workbook::new();
    {
        let sheet = wb.add_sheet("Markdown");
        sheet.write(0, 0, "Name");
        sheet.write(0, 1, "Note");
        sheet.write(1, 0, "Road");
        sheet.write(1, 1, "fast | safe");
    }
    {
        let sheet = wb.add_sheet("Merged");
        sheet.merge(0, 0, 0, 1);
        sheet.write(0, 0, "wide");
    }

    let reread = Workbook::open(&wb.to_xlsx()).expect("reopen markdown fixture");
    let sheet = reread.sheet_by_name("Markdown").expect("markdown sheet");

    assert_eq!(
        sheet.to_markdown(),
        "| Name | Note |\n| --- | --- |\n| Road | fast \\| safe |"
    );
    assert_eq!(
        reread.to_markdown(0).as_deref(),
        Some("| Name | Note |\n| --- | --- |\n| Road | fast \\| safe |")
    );
    assert_eq!(
        reread.to_markdown(1).as_deref(),
        Some("<table><tr><td colspan=\"2\">wide</td></tr></table>")
    );
}

/// Calamine-style workbook facades: by-index ranges, all worksheet ranges,
/// formula-only ranges, and structured sheet metadata.
#[cfg(feature = "xlsx")]
#[test]
fn workbook_exposes_calamine_like_range_and_metadata_facades() {
    use rxls::{Cell, Dimensions, SheetType, SheetVisible};

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "name");
        s.write(1, 0, "Road");
        s.write(
            1,
            1,
            Cell::Formula {
                formula: "SUM(C2:C3)".into(),
                cached: Box::new(Cell::Number(12.0)),
            },
        );
    }
    {
        let s = wb.add_sheet("Hidden");
        s.write(0, 0, "secret");
        s.hide();
    }

    let by_index = wb.worksheet_range_at(0).expect("range at index");
    let dimensions = Dimensions::new((0, 0), (1, 1));
    assert_eq!(by_index.dimensions_info(), Some(dimensions));
    assert_eq!(dimensions.start, (0, 0));
    assert_eq!(dimensions.end, (1, 1));
    assert!(dimensions.contains(1, 1));
    assert!(!dimensions.contains(2, 1));
    assert_eq!(dimensions.len(), 4);
    assert_eq!(
        Dimensions::from_range_tuple((3, 2, 5, 4)),
        Dimensions::new((3, 2), (5, 4))
    );
    assert_eq!(by_index.get_abs(1, 0), Some(&Cell::Text("Road".into())));
    assert!(wb.worksheet_range_at(99).is_none());

    let worksheets = wb.worksheets();
    assert_eq!(worksheets.len(), 2);
    assert_eq!(worksheets[0].0, "Data");
    assert_eq!(worksheets[1].0, "Hidden");
    assert_eq!(
        worksheets[1].1.get_abs(0, 0),
        Some(&Cell::Text("secret".into()))
    );

    let formulas = wb.worksheet_formula("Data").expect("formula range");
    assert_eq!(
        formulas.dimensions_info(),
        Some(Dimensions::new((1, 1), (1, 1)))
    );
    assert_eq!(formulas.start(), Some((1, 1)));
    assert_eq!(formulas.get_abs(1, 1), Some("SUM(C2:C3)"));
    assert!(wb
        .worksheet_formula("Hidden")
        .expect("empty formula range")
        .is_empty());
    assert!(wb.worksheet_formula("Missing").is_none());

    let metadata = wb.sheets_metadata();
    assert_eq!(metadata.len(), 2);
    assert_eq!(metadata[0].name, "Data");
    assert_eq!(metadata[0].typ, SheetType::WorkSheet);
    assert_eq!(metadata[0].visible, SheetVisible::Visible);
    assert_eq!(metadata[1].name, "Hidden");
    assert_eq!(metadata[1].typ, SheetType::WorkSheet);
    assert_eq!(metadata[1].visible, SheetVisible::Hidden);

    assert_eq!(
        wb.worksheet_metadata("Data")
            .expect("Data metadata")
            .dimensions_info(),
        Some(dimensions)
    );

    assert_eq!(
        wb.sheet_by_name("Data").expect("Data").dimensions_info(),
        Some(dimensions)
    );
    assert!(Workbook::new()
        .add_sheet("Empty")
        .dimensions_info()
        .is_none());
}

/// R3: workbook-level metadata is available as one stable public facade instead
/// of requiring callers to know every individual field and helper method.
#[cfg(feature = "xlsx")]
#[test]
fn workbook_metadata_facade_groups_public_reader_metadata() {
    use rxls::{DocProperties, Reader, SheetMetadata, SheetType, SheetVisible};

    let mut wb = Workbook::new();
    wb.date1904 = true;
    wb.properties = DocProperties::default();
    wb.properties.title = Some("Quarterly Report".into());
    wb.properties.creator = Some("rxls author".into());
    wb.define_name("NamedTotal", "Data!$B$2");
    wb.define_local_name("Data", "LocalTotal", "Data!$B$2");
    wb.protect_structure();
    wb.add_sheet("Data").write(0, 0, "item");
    wb.add_sheet("Summary").write(0, 0, "total");
    wb.set_active_sheet(1);
    wb.add_sheet("Hidden").hide();
    wb.add_sheet("VeryHidden").hide_very();

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let metadata = back.metadata();

    assert!(metadata.date1904);
    assert!(!metadata.text_truncated);
    assert!(back.is_structure_protected());
    assert!(metadata.structure_protected);
    assert!(Reader::metadata(&back).structure_protected);
    assert_eq!(back.active_sheet_index(), Some(1));
    assert_eq!(back.active_sheet_name(), Some("Summary"));
    assert_eq!(metadata.active_sheet, Some(1));
    assert_eq!(metadata.active_sheet_name, Some("Summary"));
    assert_eq!(<Workbook as Reader>::active_sheet_index(&back), Some(1));
    assert_eq!(
        <Workbook as Reader>::active_sheet_name(&back),
        Some("Summary")
    );
    assert_eq!(Reader::metadata(&back).active_sheet_name, Some("Summary"));
    assert_eq!(
        metadata.defined_names,
        [("NamedTotal".to_string(), "Data!$B$2".to_string())].as_slice()
    );
    assert_eq!(
        metadata.local_defined_names,
        [rxls::LocalDefinedName {
            sheet: "Data".into(),
            name: "LocalTotal".into(),
            refers_to: "Data!$B$2".into(),
        }]
        .as_slice()
    );
    assert_eq!(
        metadata.properties.title.as_deref(),
        Some("Quarterly Report")
    );
    assert_eq!(metadata.properties.creator.as_deref(), Some("rxls author"));
    assert_eq!(
        metadata.sheets,
        vec![
            SheetMetadata {
                name: "Data".into(),
                typ: SheetType::WorkSheet,
                visible: SheetVisible::Visible,
            },
            SheetMetadata {
                name: "Summary".into(),
                typ: SheetType::WorkSheet,
                visible: SheetVisible::Visible,
            },
            SheetMetadata {
                name: "Hidden".into(),
                typ: SheetType::WorkSheet,
                visible: SheetVisible::Hidden,
            },
            SheetMetadata {
                name: "VeryHidden".into(),
                typ: SheetType::WorkSheet,
                visible: SheetVisible::VeryHidden,
            },
        ]
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn sheet_local_defined_names_round_trip_and_evaluate_after_reopen() {
    use rxls::{Cell, FormulaEvaluation, LocalDefinedName, Reader};

    let mut wb = Workbook::new();
    let data = wb.add_sheet("Data");
    data.write(0, 0, 7.0);
    data.write_formula(0, 1, "Rate*2", 0.0);
    wb.add_sheet("Other");
    wb.define_local_name("Data", "Rate", "Data!$A$1");

    let back = Workbook::open(&wb.to_xlsx_checked().expect("valid local name")).expect("reopen");
    assert_eq!(
        back.local_defined_names(),
        &[LocalDefinedName {
            sheet: "Data".into(),
            name: "Rate".into(),
            refers_to: "Data!$A$1".into(),
        }]
    );
    assert_eq!(
        Reader::local_defined_names(&back),
        back.local_defined_names()
    );
    assert_eq!(
        back.evaluate_cell("Data", 0, 1),
        FormulaEvaluation::Computed(Cell::Number(14.0))
    );
}

/// R3: workbook metadata should expose the same ergonomic helpers as the
/// underlying workbook so generic metadata consumers can avoid raw field access.
#[cfg(feature = "xlsx")]
#[test]
fn workbook_metadata_exposes_public_helper_methods() {
    use rxls::{DocProperties, Reader, SheetVisible};

    let mut wb = Workbook::new();
    wb.date1904 = true;
    wb.properties = DocProperties::default();
    wb.properties.title = Some("Metadata Helpers".into());
    wb.define_name("NamedTotal", "Summary!$A$1");
    wb.protect_structure();
    wb.add_sheet("Data").write(0, 0, "item");
    wb.add_sheet("Summary").write(0, 0, 42.0);
    wb.set_active_sheet(1);
    wb.add_sheet("Hidden").hide();

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen metadata helper workbook");
    let metadata = Reader::metadata(&back);

    assert!(metadata.has_1904_epoch());
    assert!(!metadata.is_text_truncated());
    assert!(metadata.is_structure_protected());
    assert_eq!(metadata.active_sheet_index(), Some(1));
    assert_eq!(metadata.active_sheet_name(), Some("Summary"));
    assert_eq!(
        metadata.document_properties().title.as_deref(),
        Some("Metadata Helpers")
    );
    assert_eq!(
        metadata.defined_names(),
        [("NamedTotal".to_string(), "Summary!$A$1".to_string())].as_slice()
    );
    assert_eq!(metadata.sheets().len(), 3);
    assert_eq!(metadata.sheets()[2].name(), "Hidden");
    assert_eq!(metadata.sheets()[2].visible(), SheetVisible::Hidden);
}

/// R3: sheet metadata should carry the same ergonomic type and visibility
/// predicates as full `Sheet` values so generic readers do not need to branch
/// back into workbook sheet storage.
#[cfg(feature = "xlsx")]
#[test]
fn sheet_metadata_exposes_type_and_visibility_helpers() {
    use rxls::{SheetType, SheetVisible};

    let mut wb = Workbook::new();
    wb.add_sheet("Visible");
    wb.add_sheet("Hidden").hide();
    wb.add_sheet("VeryHidden").hide_very();

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen metadata helper workbook");
    let metadata = back.sheets_metadata();

    assert_eq!(metadata[0].name(), "Visible");
    assert_eq!(metadata[0].sheet_type(), SheetType::WorkSheet);
    assert_eq!(metadata[0].visible(), SheetVisible::Visible);
    assert!(metadata[0].is_worksheet());
    assert!(metadata[0].is_visible());
    assert!(!metadata[0].is_hidden());
    assert!(!metadata[0].is_very_hidden());

    assert_eq!(metadata[1].name(), "Hidden");
    assert_eq!(metadata[1].visible(), SheetVisible::Hidden);
    assert!(!metadata[1].is_visible());
    assert!(metadata[1].is_hidden());
    assert!(!metadata[1].is_very_hidden());

    assert_eq!(metadata[2].name(), "VeryHidden");
    assert_eq!(metadata[2].visible(), SheetVisible::VeryHidden);
    assert!(!metadata[2].is_visible());
    assert!(!metadata[2].is_hidden());
    assert!(metadata[2].is_very_hidden());
}

/// R3: grouped worksheet metadata should expose common sheet predicates and
/// accessors so generic diagnostics can inspect it without raw field branching.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_metadata_exposes_public_helper_methods() {
    use rxls::{Reader, SheetType, SheetVisible};

    let mut wb = Workbook::new();
    wb.add_sheet("Visible").write(0, 0, "item");
    wb.add_sheet("Hidden").hide();
    wb.add_sheet("VeryHidden").hide_very();
    wb.add_sheet("Protected").protect();

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen worksheet metadata helper workbook");
    let visible = Reader::worksheet_metadata(&back, "Visible").expect("visible metadata");
    let hidden = Reader::worksheet_metadata(&back, "Hidden").expect("hidden metadata");
    let very_hidden =
        Reader::worksheet_metadata(&back, "VeryHidden").expect("very-hidden metadata");
    let protected = Reader::worksheet_metadata(&back, "Protected").expect("protected metadata");

    assert_eq!(visible.name(), "Visible");
    assert_eq!(visible.sheet_type(), SheetType::WorkSheet);
    assert_eq!(visible.visible(), SheetVisible::Visible);
    assert!(visible.is_worksheet());
    assert!(visible.is_visible());
    assert!(!visible.is_hidden());
    assert!(!visible.is_very_hidden());
    assert!(!visible.is_protected());
    assert_eq!(
        visible.dimensions_info(),
        Some(rxls::Dimensions::new((0, 0), (0, 0)))
    );

    assert_eq!(hidden.name(), "Hidden");
    assert_eq!(hidden.visible(), SheetVisible::Hidden);
    assert!(!hidden.is_visible());
    assert!(hidden.is_hidden());
    assert!(!hidden.is_very_hidden());

    assert_eq!(very_hidden.name(), "VeryHidden");
    assert_eq!(very_hidden.visible(), SheetVisible::VeryHidden);
    assert!(!very_hidden.is_visible());
    assert!(!very_hidden.is_hidden());
    assert!(very_hidden.is_very_hidden());

    assert_eq!(protected.name(), "Protected");
    assert!(protected.is_protected());

    let all = Reader::worksheets_metadata(&back);
    assert_eq!(all[0].name(), "Visible");
    assert_eq!(all[3].name(), "Protected");
    assert!(all[3].is_protected());
}

/// R3: grouped worksheet metadata should expose borrowed accessors for the
/// sheet-owned metadata collections that generic diagnostics already compare.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_metadata_exposes_r3_collection_accessors() {
    use rxls::Reader;

    let wb =
        Workbook::open(include_bytes!("fixtures/xlsx/reader-structural.xlsx")).expect("fixture");
    let data = Reader::worksheet_metadata(&wb, "Data").expect("Data metadata");

    assert_eq!(data.merged_ranges(), &[(3, 0, 3, 2)]);
    assert_eq!(
        data.hyperlinks(),
        &[(4, 0, "https://example.com/rxls".to_string())]
    );
    assert_eq!(data.comments()[0].text, "needs review");
    assert_eq!(data.tables()[0].name(), "DataTable");
    assert!(data.data_validations().is_empty());
    assert!(data.conditional_formats().is_empty());
    assert_eq!(data.protection_options(), None);
    assert!(data.page_setup().is_some());
    assert_eq!(data.sheet_view(), data.sheet_view);
    assert_eq!(data.autofilter_range(), Some((0, 0, 2, 2)));
    assert_eq!(data.tab_color(), data.tab_color);
    assert_eq!(data.print_gridlines(), data.print_gridlines);
    assert_eq!(data.print_headings(), data.print_headings);
    assert_eq!(data.row_outline_levels(), data.row_outline_levels);
    assert_eq!(data.col_outline_levels(), data.col_outline_levels);
    assert_eq!(data.collapsed_rows(), data.collapsed_rows);
    assert_eq!(data.outline_summary_below(), data.outline_summary_below);
    assert_eq!(data.outline_summary_right(), data.outline_summary_right);
    assert!(data.images().is_empty());
    assert_eq!(data.charts(), data.charts);
    assert!(data.sparklines().is_empty());
}

/// R3: OOXML workbooks may omit workbookView activeTab while still marking the
/// selected sheet through the worksheet sheetView tabSelected flag. Preserve that
/// view state through the same workbook active-sheet metadata facade.
#[cfg(feature = "xlsx")]
#[test]
fn xlsx_tab_selected_falls_back_to_active_sheet_metadata() {
    use rxls::Reader;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><bookViews><workbookView/></bookViews><sheets><sheet name="Data" r:id="rId1"/><sheet name="Summary" r:id="rId2"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetViews><sheetView workbookViewId="0"/></sheetViews><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>data</t></is></c></row></sheetData></worksheet>"#,
        ),
        (
            "xl/worksheets/sheet2.xml",
            r#"<worksheet><sheetViews><sheetView workbookViewId="0" tabSelected="1"/></sheetViews><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>summary</t></is></c></row></sheetData></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).expect("open selected-sheet workbook");

    assert_eq!(wb.active_sheet_index(), Some(1));
    assert_eq!(wb.active_sheet_name(), Some("Summary"));
    assert_eq!(wb.metadata().active_sheet, Some(1));
    assert_eq!(wb.metadata().active_sheet_name, Some("Summary"));
    assert_eq!(<Workbook as Reader>::active_sheet_index(&wb), Some(1));
    assert_eq!(
        <Workbook as Reader>::active_sheet_name(&wb),
        Some("Summary")
    );
}

/// R3: OOXML sheet metadata preserves workbook order, visibility, and the
/// relationship-level sheet type instead of collapsing every non-worksheet to a
/// chartsheet.
#[cfg(feature = "xlsx")]
#[test]
fn xlsx_sheet_metadata_preserves_non_worksheet_types() {
    use rxls::{SheetMetadata, SheetType, SheetVisible};
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Data" r:id="rId1"/><sheet name="Chart" state="hidden" r:id="rId2"/><sheet name="Dialog" state="veryHidden" r:id="rId3"/><sheet name="Macro" r:id="rId4"/><sheet name="Excel4Macro" state="hidden" r:id="rId5"/><sheet name="IntlMacro" state="veryHidden" r:id="rId6"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/dialogsheet" Target="dialogsheets/sheet1.xml"/><Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/macrosheet" Target="macrosheets/sheet1.xml"/><Relationship Id="rId5" Type="http://schemas.microsoft.com/office/2006/relationships/xlMacrosheet" Target="macrosheets/sheet2.xml"/><Relationship Id="rId6" Type="http://schemas.microsoft.com/office/2006/relationships/xlIntlMacrosheet" Target="macrosheets/sheet3.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>ok</t></is></c></row></sheetData></worksheet>"#,
        ),
        ("xl/chartsheets/sheet1.xml", r#"<chartsheet/>"#),
        ("xl/dialogsheets/sheet1.xml", r#"<dialogsheet/>"#),
        ("xl/macrosheets/sheet1.xml", r#"<macrosheet/>"#),
        (
            "xl/macrosheets/sheet2.xml",
            r#"<macrosheet xmlns="http://schemas.microsoft.com/office/excel/2006/main"/>"#,
        ),
        (
            "xl/macrosheets/sheet3.xml",
            r#"<macrosheet xmlns="http://schemas.microsoft.com/office/excel/2006/main"/>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).expect("open metadata workbook");

    assert_eq!(
        wb.sheets_metadata(),
        vec![
            SheetMetadata {
                name: "Data".into(),
                typ: SheetType::WorkSheet,
                visible: SheetVisible::Visible,
            },
            SheetMetadata {
                name: "Chart".into(),
                typ: SheetType::ChartSheet,
                visible: SheetVisible::Hidden,
            },
            SheetMetadata {
                name: "Dialog".into(),
                typ: SheetType::DialogSheet,
                visible: SheetVisible::VeryHidden,
            },
            SheetMetadata {
                name: "Macro".into(),
                typ: SheetType::MacroSheet,
                visible: SheetVisible::Visible,
            },
            SheetMetadata {
                name: "Excel4Macro".into(),
                typ: SheetType::MacroSheet,
                visible: SheetVisible::Hidden,
            },
            SheetMetadata {
                name: "IntlMacro".into(),
                typ: SheetType::MacroSheet,
                visible: SheetVisible::VeryHidden,
            },
        ]
    );
    assert_eq!(
        wb.worksheets()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["Data".to_string()]
    );
    assert!(wb.sheet_by_name("Chart").is_some());
    assert!(wb.worksheet_range("Chart").is_none());
    assert!(wb.worksheet_range("Dialog").is_none());
    assert!(wb.worksheet_range("Macro").is_none());
    assert!(wb.worksheet_range("Excel4Macro").is_none());
    assert!(wb.worksheet_range("IntlMacro").is_none());
    assert!(wb.worksheet_range_at(1).is_none());
    assert!(wb.worksheet_range_at(2).is_none());
    assert!(wb.worksheet_range_at(3).is_none());
    assert!(wb.worksheet_range_at(4).is_none());
    assert!(wb.worksheet_range_at(5).is_none());
    assert!(wb.worksheet_formula("Chart").is_none());
    assert!(wb.worksheet_formula("Dialog").is_none());
    assert!(wb.worksheet_formula("Macro").is_none());
    assert!(wb.worksheet_formula("Excel4Macro").is_none());
    assert!(wb.worksheet_formula("IntlMacro").is_none());
    assert!(wb.worksheet_formula_at(1).is_none());
    assert!(wb.worksheet_formula_at(2).is_none());
    assert!(wb.worksheet_formula_at(3).is_none());
    assert!(wb.worksheet_formula_at(4).is_none());
    assert!(wb.worksheet_formula_at(5).is_none());
}

/// R3: OOXML worksheet protection metadata is visible through the public sheet
/// and grouped worksheet-metadata facades.
#[cfg(feature = "xlsx")]
#[test]
fn xlsx_sheet_protection_surfaces_public_metadata() {
    use rxls::{ProtectionOptions, Reader};
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Protected" r:id="rId1"/><sheet name="Plain" r:id="rId2"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>locked</t></is></c></row></sheetData><sheetProtection sheet="1" objects="1" scenarios="1" sort="0" autoFilter="0" formatCells="0" insertRows="1"/></worksheet>"#,
        ),
        (
            "xl/worksheets/sheet2.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>plain</t></is></c></row></sheetData></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).expect("open protected workbook");
    let protected = wb.sheet_by_name("Protected").expect("protected sheet");
    let plain = wb.sheet_by_name("Plain").expect("plain sheet");

    assert!(protected.is_protected());
    assert_eq!(
        protected.protection_options(),
        Some(ProtectionOptions {
            sort: true,
            auto_filter: true,
            format_cells: true,
            insert_rows: false,
            ..Default::default()
        })
    );
    assert!(!plain.is_protected());
    assert_eq!(plain.protection_options(), None);

    let metadata = wb
        .worksheet_metadata("Protected")
        .expect("protected metadata");
    assert!(metadata.protected);
    assert_eq!(metadata.protection_options, protected.protection_options());

    let generic_metadata =
        Reader::worksheet_metadata(&wb, "Protected").expect("generic protected metadata");
    assert!(generic_metadata.protected);
    assert_eq!(
        generic_metadata.protection_options,
        Some(ProtectionOptions {
            sort: true,
            auto_filter: true,
            format_cells: true,
            insert_rows: false,
            ..Default::default()
        })
    );
}

/// R1: workbook reader access is available as a trait so generic diagnostics and
/// consumers can use the calamine-style surface without tying themselves to the
/// concrete `Workbook` type.
#[cfg(feature = "xlsx")]
#[test]
fn workbook_implements_reader_trait_surface() {
    use rxls::{Cell, Image, ImageFmt, Picture, Reader, Table};

    #[derive(Debug, PartialEq, Eq)]
    struct ReaderSummary {
        sheet_names: Vec<String>,
        data_size: Option<(usize, usize)>,
        worksheet_count: usize,
        metadata_count: usize,
        defined_name_count: usize,
        table_count: usize,
        picture_count: usize,
        picture_metadata_count: usize,
    }

    fn summarize<R: Reader>(reader: &R) -> ReaderSummary {
        ReaderSummary {
            sheet_names: reader
                .sheet_names()
                .into_iter()
                .map(str::to_string)
                .collect(),
            data_size: reader.worksheet_range("Data").map(|range| range.size()),
            worksheet_count: reader.worksheets().len(),
            metadata_count: reader.sheets_metadata().len(),
            defined_name_count: reader.defined_names().len(),
            table_count: reader.table_names().len(),
            picture_count: reader.pictures().map_or(0, |pictures| pictures.len()),
            picture_metadata_count: reader.pictures_with_metadata().len(),
        }
    }

    let mut wb = Workbook::new();
    {
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, "name");
        sheet.write(1, 0, "road");
        sheet.add_image(Image {
            data: vec![1, 2, 3],
            format: ImageFmt::Png,
            from: (2, 0),
            to: None,
        });
        sheet.add_table(Table {
            range: (0, 0, 1, 0),
            name: "ReaderTable".to_string(),
            columns: vec!["name".to_string()],
            style: None,
        });
    }
    wb.define_name("NamedData", "Data!$A$1");

    assert_eq!(
        summarize(&wb),
        ReaderSummary {
            sheet_names: vec!["Data".to_string()],
            data_size: Some((2, 1)),
            worksheet_count: 1,
            metadata_count: 1,
            defined_name_count: 1,
            table_count: 1,
            picture_count: 1,
            picture_metadata_count: 1,
        }
    );
    assert_eq!(
        Reader::worksheet_range_at(&wb, 0)
            .expect("worksheet range")
            .get((0, 0)),
        Some(&Cell::Text("name".to_string()))
    );
    assert_eq!(Reader::metadata(&wb).sheets[0].name, "Data");
    assert_eq!(Reader::sheets_metadata(&wb)[0].name, "Data");
    assert_eq!(
        Reader::defined_names(&wb),
        &[("NamedData".to_string(), "Data!$A$1".to_string())]
    );
    assert_eq!(Reader::table_names(&wb), vec!["ReaderTable"]);
    assert_eq!(
        Reader::table_names_in_sheet(&wb, "Data"),
        vec!["ReaderTable"]
    );
    let (sheet_name, table) = Reader::table_by_name(&wb, "ReaderTable").expect("ReaderTable");
    assert_eq!(sheet_name, "Data");
    assert_eq!(table.range, (0, 0, 1, 0));
    assert!(Reader::table_by_name(&wb, "Missing").is_none());

    let pictures = Reader::pictures(&wb).expect("pictures");
    assert_eq!(pictures, vec![("png".to_string(), vec![1, 2, 3])]);
    assert_eq!(
        Reader::pictures_with_metadata(&wb),
        vec![Picture {
            row: 2,
            col: 0,
            sheet_name: "Data".to_string(),
            extension: "png".to_string(),
            data: vec![1, 2, 3],
            name: String::new(),
        }]
    );
    assert!(Reader::pictures(&Workbook::new()).is_none());
    assert!(Reader::pictures_with_metadata(&Workbook::new()).is_empty());
}

/// R1: borrowed-range and epoch aliases let calamine-style consumers use the
/// same public names while rxls still returns borrowed sparse `Range` views.
#[cfg(feature = "xlsx")]
#[test]
fn workbook_exposes_borrowed_range_and_epoch_aliases() {
    use rxls::{Cell, Reader};

    let mut wb = Workbook::new();
    wb.date1904 = true;
    wb.add_sheet("Data").write(0, 0, "name");

    assert!(wb.has_1904_epoch());
    assert!(Reader::has_1904_epoch(&wb));

    assert_eq!(
        wb.worksheet_range_ref("Data")
            .expect("borrowed range by name")
            .get_abs(0, 0),
        Some(&Cell::Text("name".to_string()))
    );
    assert_eq!(
        wb.worksheet_range_at_ref(0)
            .expect("borrowed range by index")
            .get_abs(0, 0),
        Some(&Cell::Text("name".to_string()))
    );
    assert_eq!(
        <Workbook as Reader>::worksheet_range_ref(&wb, "Data")
            .expect("trait borrowed range by name")
            .size(),
        (1, 1)
    );
    assert_eq!(
        <Workbook as Reader>::worksheet_range_at_ref(&wb, 0)
            .expect("trait borrowed range by index")
            .size(),
        (1, 1)
    );
    assert!(wb.worksheet_range_ref("Missing").is_none());
    assert!(wb.worksheet_range_at_ref(99).is_none());
}

/// A calamine-style rectangular Range facade over the sparse sheet model.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_range_exposes_rectangular_cells_and_used_cells() {
    use rxls::Cell;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(1, 1, "first");
        s.write(1, 1, "last");
        s.write(1, 3, 10.0);
        s.write(3, 2, true);
    }
    wb.add_sheet("Empty");

    let range = wb.worksheet_range("Data").expect("range by name");
    assert_eq!(range.start(), Some((1, 1)));
    assert_eq!(range.end(), Some((3, 3)));
    assert_eq!(range.height(), 3);
    assert_eq!(range.width(), 3);
    assert_eq!(range.get((0, 0)), Some(&Cell::Text("last".into())));
    assert_eq!(range.get((0, 2)), Some(&Cell::Number(10.0)));
    assert_eq!(range.get((2, 1)), Some(&Cell::Bool(true)));
    assert_eq!(range.get((1, 1)), None);

    let mut row_iter = range.rows();
    assert_eq!(row_iter.size_hint(), (3, Some(3)));
    assert_eq!(row_iter.len(), 3);
    assert_eq!(
        row_iter.next(),
        Some(vec![
            Some(&Cell::Text("last".into())),
            None,
            Some(&Cell::Number(10.0))
        ])
    );
    assert_eq!(row_iter.size_hint(), (2, Some(2)));
    assert_eq!(row_iter.len(), 2);

    let rows: Vec<Vec<Option<&Cell>>> = range.rows().collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].len(), 3);
    assert_eq!(rows[0][0], Some(&Cell::Text("last".into())));
    assert_eq!(rows[0][1], None);
    assert_eq!(rows[0][2], Some(&Cell::Number(10.0)));
    assert_eq!(rows[1], vec![None, None, None]);
    assert_eq!(rows[2][1], Some(&Cell::Bool(true)));

    let used: Vec<_> = range.used_cells().collect();
    assert_eq!(
        used,
        vec![
            (0, 0, &Cell::Text("last".into())),
            (0, 2, &Cell::Number(10.0)),
            (2, 1, &Cell::Bool(true)),
        ]
    );
    let used_abs: Vec<_> = range.used_cells_abs().collect();
    assert_eq!(
        used_abs,
        vec![
            (1, 1, &Cell::Text("last".into())),
            (1, 3, &Cell::Number(10.0)),
            (3, 2, &Cell::Bool(true)),
        ]
    );
    assert!(wb.worksheet_range("Missing").is_none());
    assert!(wb.worksheet_range("Empty").expect("empty range").is_empty());
}

/// R1: empty Range construction is part of the calamine-style facade. rxls
/// still represents sparse/missing cells as `None`, so the public constructor
/// yields an empty rectangle rather than a rectangle of empty cell values.
#[test]
fn range_empty_constructor_exposes_empty_facade() {
    let range = rxls::Range::empty();

    assert!(range.is_empty());
    assert_eq!(range.start(), None);
    assert_eq!(range.end(), None);
    assert_eq!(range.get_size(), (0, 0));
    assert_eq!(range.width(), 0);
    assert_eq!(range.height(), 0);
    assert_eq!(range.headers(), None);
    assert_eq!(range.rows().count(), 0);
    assert_eq!(range.cells().count(), 0);
    assert!(range.used_cells().next().is_none());
    assert!(range.used_cells_abs().next().is_none());
    assert!(range.get((0, 0)).is_none());
    assert!(range.get_value((0, 0)).is_none());
}

/// R1: calamine-style Range construction can also describe a rectangular
/// sparse range without needing a worksheet. Missing positions remain `None`
/// rather than a synthetic empty cell value.
#[test]
fn range_new_constructor_exposes_sparse_rectangle() {
    let range = rxls::Range::new((2, 2), (4, 3));
    let start: Option<(u32, u32)> = range.start();
    let end: Option<(u32, u32)> = range.end();

    assert!(!range.is_empty());
    assert_eq!(start, Some((2, 2)));
    assert_eq!(end, Some((4, 3)));
    assert_eq!(range.get_size(), (3, 2));
    assert_eq!(range.width(), 2);
    assert_eq!(range.height(), 3);
    assert_eq!(range.headers(), Some(vec![String::new(), String::new()]));
    assert_eq!(range.rows().count(), 3);
    assert_eq!(range.cells().count(), 6);
    assert!(range.used_cells().next().is_none());
    assert!(range.used_cells_abs().next().is_none());
    assert!(range.get((0, 0)).is_none());
    assert!(range.get_value((2, 2)).is_none());
}

/// R1: calamine-style sparse construction and absolute mutation let generic
/// consumers build range fixtures without manufacturing a worksheet.
#[test]
fn range_sparse_construction_and_set_value_populate_owned_cells() {
    use std::panic::AssertUnwindSafe;

    use rxls::Cell;

    let mut range = rxls::Range::from_sparse(vec![
        ((5, 2), Cell::Number(1.0)),
        ((2, 3), Cell::Text("north".into())),
        ((5, 4), Cell::Bool(true)),
    ]);

    assert!(!range.is_empty());
    assert_eq!(range.start(), Some((2, 2)));
    assert_eq!(range.end(), Some((5, 4)));
    assert_eq!(range.get_size(), (4, 3));
    assert_eq!(range.get_value((2, 3)), Some(&Cell::Text("north".into())));
    assert_eq!(range.get((0, 1)), Some(&Cell::Text("north".into())));
    assert_eq!(range.formatted_abs(5, 2), Some("1"));
    assert_eq!(range.formatted_abs(5, 4), Some("TRUE"));
    assert_eq!(
        range.headers(),
        Some(vec!["".to_string(), "north".to_string(), "".to_string()])
    );
    assert_eq!(range.cells().count(), 12);
    assert_eq!(range.used_cells().count(), 3);

    range.set_value((6, 4), Cell::Error("#N/A".into()));
    assert_eq!(range.end(), Some((6, 4)));
    assert_eq!(range.get_value((6, 4)), Some(&Cell::Error("#N/A".into())));
    assert_eq!(range.formatted_abs(6, 4), Some("#N/A"));
    assert_eq!(
        range.used_cells_abs().collect::<Vec<_>>(),
        vec![
            (2, 3, &Cell::Text("north".into())),
            (5, 2, &Cell::Number(1.0)),
            (5, 4, &Cell::Bool(true)),
            (6, 4, &Cell::Error("#N/A".into())),
        ]
    );

    let mut bounded = rxls::Range::new((2, 2), (3, 3));
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        bounded.set_value((1, 2), Cell::Number(9.0));
    }));
    assert!(result.is_err());
}

/// R1: Range implements calamine-style default construction and equality
/// across worksheet-borrowed and owned sparse ranges.
#[test]
fn range_default_and_equality_support_borrowed_and_owned_ranges() {
    use rxls::Cell;

    fn assert_range_eq_across_lifetimes(left: &rxls::Range<'_>, right: &rxls::Range<'_>) {
        assert_eq!(left, right);
    }

    fn assert_range_ne_across_lifetimes(left: &rxls::Range<'_>, right: &rxls::Range<'_>) {
        assert_ne!(left, right);
    }

    let default: rxls::Range<'static> = rxls::Range::default();
    assert_eq!(default, rxls::Range::empty());

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(2, 1, "north");
        s.write(3, 2, 12.5);
        s.write(4, 3, true);
    }

    let borrowed = wb.worksheet_range("Data").expect("range");
    let owned: rxls::Range<'static> = rxls::Range::from_sparse(vec![
        ((2, 1), Cell::Text("north".into())),
        ((3, 2), Cell::Number(12.5)),
        ((4, 3), Cell::Bool(true)),
    ]);
    assert_range_eq_across_lifetimes(&borrowed, &owned);

    let mut changed = owned.clone();
    changed.set_value((4, 3), false);
    assert_range_ne_across_lifetimes(&borrowed, &changed);
}

/// R1: calamine-style absolute subranges keep their requested rectangle while
/// preserving rxls' sparse `Option<&Cell>` representation for missing cells.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_range_can_build_absolute_subranges() {
    use rxls::Cell;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(1, 1, "top-left");
        s.write(2, 2, 22.0);
        s.write(4, 4, true);
    }

    let range = wb.worksheet_range("Data").expect("range by name");
    let sub = range.range((1, 2), (4, 4));

    assert_eq!(sub.start(), Some((1, 2)));
    assert_eq!(sub.end(), Some((4, 4)));
    assert_eq!(sub.get_size(), (4, 3));
    assert_eq!(sub.get_value((1, 1)), None);
    assert_eq!(sub.get_value((2, 2)), Some(&Cell::Number(22.0)));
    assert_eq!(sub.get((1, 0)), Some(&Cell::Number(22.0)));
    assert_eq!(sub.get((3, 2)), Some(&Cell::Bool(true)));

    let cells: Vec<_> = sub.cells().collect();
    assert_eq!(cells.len(), 12);
    assert_eq!(cells[0], (0, 0, None));
    assert_eq!(cells[3], (1, 0, Some(&Cell::Number(22.0))));
    assert_eq!(cells[11], (3, 2, Some(&Cell::Bool(true))));

    let empty_rect = range.range((10, 10), (11, 11));
    assert_eq!(empty_rect.start(), Some((10, 10)));
    assert_eq!(empty_rect.end(), Some((11, 11)));
    assert_eq!(empty_rect.get_size(), (2, 2));
    assert_eq!(empty_rect.cells().count(), 4);
    assert!(empty_rect.used_cells().next().is_none());
    assert!(empty_rect.used_cells_abs().next().is_none());
}

/// Formula ranges should expose the same absolute subrange shape as value
/// ranges, but carry formula source text instead of cells.
#[cfg(feature = "xlsx")]
#[test]
fn formula_range_can_build_absolute_subranges() {
    use rxls::{Cell, Reader};

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Formulas");
        s.write(
            0,
            0,
            Cell::Formula {
                formula: "A2+A3".into(),
                cached: Box::new(Cell::Number(3.0)),
            },
        );
        s.write(
            2,
            2,
            Cell::Formula {
                formula: "C2*C3".into(),
                cached: Box::new(Cell::Number(6.0)),
            },
        );
    }

    let formulas = wb.worksheet_formula("Formulas").expect("formula range");
    let by_index = wb.worksheet_formula_at(0).expect("formula range by index");
    let by_name_ref = wb
        .worksheet_formula_ref("Formulas")
        .expect("formula range ref by name");
    let by_index_ref = wb
        .worksheet_formula_at_ref(0)
        .expect("formula range ref by index");
    assert_eq!(by_index.get_value((0, 0)), Some("A2+A3"));
    assert_eq!(by_name_ref, formulas);
    assert_eq!(by_index_ref.get_value((2, 2)), Some("C2*C3"));
    assert_eq!(
        <Workbook as Reader>::worksheet_formula_ref(&wb, "Formulas")
            .expect("trait formula range ref by name")
            .get_value((0, 0)),
        Some("A2+A3")
    );
    assert_eq!(
        <Workbook as Reader>::worksheet_formula_at_ref(&wb, 0)
            .expect("trait formula range ref by index")
            .get_value((2, 2)),
        Some("C2*C3")
    );
    assert_eq!(
        formulas.headers(),
        Some(vec!["A2+A3".to_string(), String::new(), String::new()])
    );
    assert!(wb.worksheet_formula_ref("Missing").is_none());
    assert!(wb.worksheet_formula_at(99).is_none());
    assert!(wb.worksheet_formula_at_ref(99).is_none());

    let sub = formulas.range((0, 1), (2, 2));

    assert_eq!(sub.start(), Some((0, 1)));
    assert_eq!(sub.end(), Some((2, 2)));
    assert_eq!(sub.get_size(), (3, 2));
    assert_eq!(sub.headers(), Some(vec![String::new(), String::new()]));
    assert_eq!(sub.get((0, 0)), None);
    assert_eq!(sub.get_value((2, 2)), Some("C2*C3"));
    assert_eq!(sub.get((2, 1)), Some("C2*C3"));
    assert_eq!(sub.used_cells().collect::<Vec<_>>(), vec![(2, 1, "C2*C3")]);
    assert_eq!(
        sub.used_cells_abs().collect::<Vec<_>>(),
        vec![(2, 2, "C2*C3")]
    );

    let mut sub_rows = sub.rows();
    assert_eq!(sub_rows.size_hint(), (3, Some(3)));
    assert_eq!(sub_rows.len(), 3);
    assert_eq!(sub_rows.next(), Some(vec![None, None]));
    assert_eq!(sub_rows.size_hint(), (2, Some(2)));
    assert_eq!(sub_rows.len(), 2);

    let cells: Vec<_> = sub.cells().collect();
    assert_eq!(cells.len(), 6);
    assert_eq!(cells[0], (0, 0, None));
    assert_eq!(cells[5], (2, 1, Some("C2*C3")));
}

/// R1: OOXML array formulas carry one source formula over a rectangular range.
/// Preserve that source through the public formula facade for each covered cell,
/// including cached-value followers with self-closing or omitted `<f>` nodes.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_formula_preserves_ooxml_array_formula_range() {
    use rxls::{Cell, Reader};
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Array" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData>
                <row r="1">
                    <c r="A1"><f t="array" ref="A1:B2">SUM(A1:B2)</f><v>2</v></c>
                    <c r="B1"><f t="array" ref="A1:B2"/><v>4</v></c>
                </row>
                <row r="2">
                    <c r="A2"><v>6</v></c>
                    <c r="B2"><f t="array" ref="A1:B2"/><v>8</v></c>
                </row>
            </sheetData></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).expect("open array-formula workbook");
    let sheet = wb.sheet_by_name("Array").expect("Array sheet");
    let formulas = wb.worksheet_formula("Array").expect("array formula range");

    assert_eq!(formulas.start(), Some((0, 0)));
    assert_eq!(formulas.end(), Some((1, 1)));
    assert_eq!(formulas.size(), (2, 2));
    assert_eq!(formulas.get_abs(0, 0), Some("SUM(A1:B2)"));
    assert_eq!(formulas.get_abs(0, 1), Some("SUM(A1:B2)"));
    assert_eq!(formulas.get_abs(1, 0), Some("SUM(A1:B2)"));
    assert_eq!(formulas.get_abs(1, 1), Some("SUM(A1:B2)"));
    assert_eq!(
        <Workbook as Reader>::worksheet_formula(&wb, "Array")
            .expect("trait formula range")
            .get_value((1, 1)),
        Some("SUM(A1:B2)")
    );
    assert_eq!(
        sheet.cell(0, 1),
        Some(&Cell::Formula {
            formula: "SUM(A1:B2)".into(),
            cached: Box::new(Cell::Number(4.0)),
        })
    );
}

/// Formula ranges mirror the allocation-free row-view surface available on
/// value ranges, keeping sparse rectangular scans cheap for formula-heavy
/// worksheets.
#[cfg(feature = "xlsx")]
#[test]
fn formula_range_exposes_lazy_borrowed_row_views() {
    use rxls::Cell;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Formulas");
        s.write(
            1,
            1,
            Cell::Formula {
                formula: "B1+B3".into(),
                cached: Box::new(Cell::Number(4.0)),
            },
        );
        s.write(
            1,
            3,
            Cell::Formula {
                formula: "D1*2".into(),
                cached: Box::new(Cell::Number(8.0)),
            },
        );
        s.write(
            2,
            2,
            Cell::Formula {
                formula: "C1-C2".into(),
                cached: Box::new(Cell::Number(1.0)),
            },
        );
    }

    let formulas = wb.worksheet_formula("Formulas").expect("formula range");
    let mut rows = formulas.row_views();
    assert_eq!(rows.size_hint(), (2, Some(2)));
    assert_eq!(rows.len(), 2);

    let row1: rxls::FormulaRangeRow<'_, '_> = rows.next().expect("first formula row");
    assert_eq!(rows.size_hint(), (1, Some(1)));
    assert_eq!(rows.len(), 1);
    assert_eq!(row1.row(), 1);
    assert_eq!(row1.start_col(), 1);
    assert_eq!(row1.end_col(), 3);
    assert_eq!(row1.len(), 3);
    assert_eq!(row1.get(0), Some("B1+B3"));
    assert_eq!(row1.get(1), None);
    assert_eq!(row1.get(2), Some("D1*2"));
    assert_eq!(row1.get_abs(1), Some("B1+B3"));
    assert_eq!(row1.get_abs(2), None);
    assert_eq!(row1.get_abs(4), None);
    assert_eq!(
        row1.cells().collect::<Vec<_>>(),
        vec![(0, Some("B1+B3")), (1, None), (2, Some("D1*2"))]
    );
    let mut rectangular_cells = formulas.cells();
    assert_eq!(rectangular_cells.size_hint(), (6, Some(6)));
    assert_eq!(rectangular_cells.len(), 6);
    assert_eq!(rectangular_cells.next(), Some((0, 0, Some("B1+B3"))));
    assert_eq!(rectangular_cells.size_hint(), (5, Some(5)));
    assert_eq!(rectangular_cells.len(), 5);
    let mut row1_cells = row1.iter();
    assert_eq!(row1_cells.size_hint(), (3, Some(3)));
    assert_eq!(row1_cells.len(), 3);
    assert_eq!(row1_cells.next(), Some(Some("B1+B3")));
    assert_eq!(row1_cells.size_hint(), (2, Some(2)));
    assert_eq!(row1_cells.len(), 2);
    assert_eq!(
        row1.used_cells().collect::<Vec<_>>(),
        vec![(1, "B1+B3"), (3, "D1*2")]
    );

    let row2: rxls::FormulaRangeRow<'_, '_> = rows.next().expect("second formula row");
    assert_eq!(row2.row(), 2);
    assert_eq!(row2.start_col(), 1);
    assert_eq!(row2.end_col(), 3);
    assert_eq!(
        row2.iter().collect::<Vec<_>>(),
        vec![None, Some("C1-C2"), None]
    );
    assert!(rows.next().is_none());
}

/// FormulaRange exposes the same sparse rectangular construction surface as
/// Range, but carries only formula source text.
#[test]
fn formula_range_constructors_expose_sparse_formula_rectangles() {
    let empty = rxls::FormulaRange::empty();
    assert!(empty.is_empty());
    assert_eq!(empty.start(), None);
    assert_eq!(empty.end(), None);
    assert_eq!(empty.get_size(), (0, 0));
    assert_eq!(empty.headers(), None);
    assert_eq!(empty.rows().count(), 0);
    assert_eq!(empty.cells().count(), 0);
    assert!(empty.used_cells().next().is_none());
    assert!(empty.used_cells_abs().next().is_none());

    let formulas = rxls::FormulaRange::new((3, 1), (4, 3));
    let start: Option<(u32, u32)> = formulas.start();
    let end: Option<(u32, u32)> = formulas.end();
    assert!(!formulas.is_empty());
    assert_eq!(start, Some((3, 1)));
    assert_eq!(end, Some((4, 3)));
    assert_eq!(formulas.get_size(), (2, 3));
    assert_eq!(formulas.headers(), Some(vec![String::new(); 3]));
    assert_eq!(formulas.rows().count(), 2);
    assert_eq!(formulas.cells().count(), 6);
    assert!(formulas.used_cells().next().is_none());
    assert!(formulas.used_cells_abs().next().is_none());
    assert!(formulas.get((0, 0)).is_none());
    assert!(formulas.get_value((3, 1)).is_none());
}

/// FormulaRange mirrors the owned sparse construction and mutation surface for
/// formula source text.
#[test]
fn formula_range_sparse_construction_and_set_value_populate_owned_formulas() {
    use std::panic::AssertUnwindSafe;

    let mut formulas = rxls::FormulaRange::from_sparse(vec![((4, 1), "B5*2"), ((2, 3), "D3+1")]);

    assert!(!formulas.is_empty());
    assert_eq!(formulas.start(), Some((2, 1)));
    assert_eq!(formulas.end(), Some((4, 3)));
    assert_eq!(formulas.get_size(), (3, 3));
    assert_eq!(formulas.get_value((2, 3)), Some("D3+1"));
    assert_eq!(formulas.get((0, 2)), Some("D3+1"));
    assert_eq!(
        formulas.headers(),
        Some(vec!["".to_string(), "".to_string(), "D3+1".to_string()])
    );
    assert_eq!(formulas.cells().count(), 9);
    assert_eq!(formulas.used_cells().count(), 2);

    formulas.set_value((5, 3), "D6-1");
    assert_eq!(formulas.end(), Some((5, 3)));
    assert_eq!(formulas.get_value((5, 3)), Some("D6-1"));
    assert_eq!(
        formulas.used_cells_abs().collect::<Vec<_>>(),
        vec![(2, 3, "D3+1"), (4, 1, "B5*2"), (5, 3, "D6-1")]
    );

    let mut bounded = rxls::FormulaRange::new((2, 2), (3, 3));
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        bounded.set_value((1, 2), "A1");
    }));
    assert!(result.is_err());
}

/// R1: FormulaRange mirrors default construction and equality across
/// worksheet-borrowed and owned sparse formula ranges.
#[test]
fn formula_range_default_and_equality_support_borrowed_and_owned_ranges() {
    use rxls::Cell;

    fn assert_formula_range_eq_across_lifetimes(
        left: &rxls::FormulaRange<'_>,
        right: &rxls::FormulaRange<'_>,
    ) {
        assert_eq!(left, right);
    }

    fn assert_formula_range_ne_across_lifetimes(
        left: &rxls::FormulaRange<'_>,
        right: &rxls::FormulaRange<'_>,
    ) {
        assert_ne!(left, right);
    }

    let default: rxls::FormulaRange<'static> = rxls::FormulaRange::default();
    assert_eq!(default, rxls::FormulaRange::empty());

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Formulas");
        s.write(
            1,
            1,
            Cell::Formula {
                formula: "B1+B3".into(),
                cached: Box::new(Cell::Number(4.0)),
            },
        );
        s.write(
            2,
            2,
            Cell::Formula {
                formula: "C1-C2".into(),
                cached: Box::new(Cell::Number(1.0)),
            },
        );
    }

    let borrowed = wb.worksheet_formula("Formulas").expect("formula range");
    let owned: rxls::FormulaRange<'static> =
        rxls::FormulaRange::from_sparse(vec![((1, 1), "B1+B3"), ((2, 2), "C1-C2")]);
    assert_formula_range_eq_across_lifetimes(&borrowed, &owned);

    let mut changed = owned.clone();
    changed.set_value((2, 2), "C1+C2");
    assert_formula_range_ne_across_lifetimes(&borrowed, &changed);
}

/// Borrowed row views let callers scan a rectangular Range without allocating a
/// Vec for each row.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_range_exposes_lazy_borrowed_row_views() {
    use rxls::Cell;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(1, 1, "first");
        s.write(1, 3, 10.0);
        s.write(2, 2, true);
    }

    let range = wb.worksheet_range("Data").expect("range by name");
    let mut rows = range.row_views();
    assert_eq!(rows.size_hint(), (2, Some(2)));
    assert_eq!(rows.len(), 2);

    let row1 = rows.next().expect("first row");
    assert_eq!(rows.size_hint(), (1, Some(1)));
    assert_eq!(rows.len(), 1);
    assert_eq!(row1.row(), 1);
    assert_eq!(row1.start_col(), 1);
    assert_eq!(row1.end_col(), 3);
    assert_eq!(row1.len(), 3);
    assert_eq!(row1.get(0), Some(&Cell::Text("first".into())));
    assert_eq!(row1.get(1), None);
    assert_eq!(row1.get(2), Some(&Cell::Number(10.0)));
    assert_eq!(row1.get_abs(1), Some(&Cell::Text("first".into())));
    assert_eq!(row1.get_abs(2), None);
    assert_eq!(row1.get_abs(4), None);
    assert_eq!(
        row1.cells().collect::<Vec<_>>(),
        vec![
            (0, Some(&Cell::Text("first".into()))),
            (1, None),
            (2, Some(&Cell::Number(10.0))),
        ]
    );
    let mut rectangular_cells = range.cells();
    assert_eq!(rectangular_cells.size_hint(), (6, Some(6)));
    assert_eq!(rectangular_cells.len(), 6);
    assert_eq!(
        rectangular_cells.next(),
        Some((0, 0, Some(&Cell::Text("first".into()))))
    );
    assert_eq!(rectangular_cells.size_hint(), (5, Some(5)));
    assert_eq!(rectangular_cells.len(), 5);
    let mut row1_cells = row1.iter();
    assert_eq!(row1_cells.size_hint(), (3, Some(3)));
    assert_eq!(row1_cells.len(), 3);
    assert_eq!(row1_cells.next(), Some(Some(&Cell::Text("first".into()))));
    assert_eq!(row1_cells.size_hint(), (2, Some(2)));
    assert_eq!(row1_cells.len(), 2);
    assert_eq!(
        row1.used_cells().collect::<Vec<_>>(),
        vec![(1, &Cell::Text("first".into())), (3, &Cell::Number(10.0))]
    );

    let row2 = rows.next().expect("second row");
    assert_eq!(row2.row(), 2);
    assert_eq!(row2.start_col(), 1);
    assert_eq!(row2.end_col(), 3);
    assert_eq!(
        row2.iter().collect::<Vec<_>>(),
        vec![None, Some(&Cell::Bool(true)), None]
    );
    assert!(rows.next().is_none());
}

/// R1: calamine-style rectangular and used-cell iterators should expose
/// bidirectional traversal where the underlying rxls sparse range is ordered.
#[cfg(feature = "xlsx")]
#[test]
fn range_and_formula_iterators_are_double_ended() {
    use rxls::Cell;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(1, 1, "first");
        s.write(1, 3, 10.0);
        s.write(2, 2, true);
        s.write(
            3,
            1,
            Cell::Formula {
                formula: "B1+B2".into(),
                cached: Box::new(Cell::Number(11.0)),
            },
        );
        s.write(
            4,
            3,
            Cell::Formula {
                formula: "D4*2".into(),
                cached: Box::new(Cell::Number(20.0)),
            },
        );
    }

    let range = wb.worksheet_range("Data").expect("range by name");
    let mut rows = range.rows();
    assert_eq!(
        rows.next_back(),
        Some(vec![
            None,
            None,
            Some(&Cell::Formula {
                formula: "D4*2".into(),
                cached: Box::new(Cell::Number(20.0)),
            })
        ])
    );
    let mut cells = range.cells();
    assert_eq!(
        cells.next_back(),
        Some((
            3,
            2,
            Some(&Cell::Formula {
                formula: "D4*2".into(),
                cached: Box::new(Cell::Number(20.0)),
            })
        ))
    );
    let mut used = range.used_cells();
    assert_eq!(
        used.next_back(),
        Some((
            3,
            2,
            &Cell::Formula {
                formula: "D4*2".into(),
                cached: Box::new(Cell::Number(20.0)),
            },
        ))
    );
    let mut used_abs = range.used_cells_abs();
    assert_eq!(
        used_abs.next_back(),
        Some((
            4,
            3,
            &Cell::Formula {
                formula: "D4*2".into(),
                cached: Box::new(Cell::Number(20.0)),
            },
        ))
    );
    let mut row_views = range.row_views();
    let last_row = row_views.next_back().expect("last row view");
    assert_eq!(last_row.row(), 4);
    assert_eq!(
        last_row.iter().next_back(),
        Some(Some(&Cell::Formula {
            formula: "D4*2".into(),
            cached: Box::new(Cell::Number(20.0)),
        }))
    );
    assert_eq!(
        last_row.cells().next_back(),
        Some((
            2,
            Some(&Cell::Formula {
                formula: "D4*2".into(),
                cached: Box::new(Cell::Number(20.0)),
            }),
        ))
    );
    assert_eq!(
        last_row.used_cells().next_back(),
        Some((
            3,
            &Cell::Formula {
                formula: "D4*2".into(),
                cached: Box::new(Cell::Number(20.0)),
            },
        ))
    );

    let formulas = wb.worksheet_formula("Data").expect("formula range");
    let mut formula_rows = formulas.rows();
    assert_eq!(
        formula_rows.next_back(),
        Some(vec![None, None, Some("D4*2")])
    );
    let mut formula_cells = formulas.cells();
    assert_eq!(formula_cells.next_back(), Some((1, 2, Some("D4*2"))));
    let mut formula_used = formulas.used_cells();
    assert_eq!(formula_used.next_back(), Some((1, 2, "D4*2")));
    let mut formula_used_abs = formulas.used_cells_abs();
    assert_eq!(formula_used_abs.next_back(), Some((4, 3, "D4*2")));
    let mut formula_row_views = formulas.row_views();
    let last_formula_row = formula_row_views
        .next_back()
        .expect("last formula row view");
    assert_eq!(last_formula_row.row(), 4);
    assert_eq!(last_formula_row.iter().next_back(), Some(Some("D4*2")));
    assert_eq!(
        last_formula_row.cells().next_back(),
        Some((2, Some("D4*2")))
    );
    assert_eq!(last_formula_row.used_cells().next_back(), Some((3, "D4*2")));
}

/// R1: row-view used-cell scans should expose the same exact/fused iterator
/// metadata as the surrounding range and row-cell iterators.
#[cfg(feature = "xlsx")]
#[test]
fn range_row_used_cell_iterators_are_exact_size() {
    use rxls::Cell;

    fn assert_exact<I>(iter: I) -> I
    where
        I: ExactSizeIterator + DoubleEndedIterator + std::iter::FusedIterator,
    {
        iter
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(2, 1, "name");
        s.write(2, 3, 12.5);
        s.write(
            3,
            2,
            Cell::Formula {
                formula: "D3*2".into(),
                cached: Box::new(Cell::Number(25.0)),
            },
        );
    }

    let range = wb.worksheet_range("Data").expect("range by name");
    let first_row = range.row_views().next().expect("first row");
    let mut used = assert_exact(first_row.used_cells());
    assert_eq!(used.size_hint(), (2, Some(2)));
    assert_eq!(used.len(), 2);
    assert_eq!(used.next(), Some((1, &Cell::Text("name".into()))));
    assert_eq!(used.size_hint(), (1, Some(1)));
    assert_eq!(used.len(), 1);
    assert_eq!(used.next_back(), Some((3, &Cell::Number(12.5))));
    assert_eq!(used.size_hint(), (0, Some(0)));
    assert_eq!(used.len(), 0);
    assert_eq!(used.next(), None);
    assert_eq!(used.next_back(), None);

    let formulas = wb.worksheet_formula("Data").expect("formula range");
    let formula_row = formulas.row_views().next().expect("formula row");
    let mut formula_used = assert_exact(formula_row.used_cells());
    assert_eq!(formula_used.size_hint(), (1, Some(1)));
    assert_eq!(formula_used.len(), 1);
    assert_eq!(formula_used.next_back(), Some((2, "D3*2")));
    assert_eq!(formula_used.size_hint(), (0, Some(0)));
    assert_eq!(formula_used.len(), 0);
    assert_eq!(formula_used.next(), None);
}

/// The Range facade also exposes calamine-style absolute lookup, rectangular
/// cell iteration, and first-row headers.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_range_exposes_headers_lookup_and_cells() {
    use rxls::Cell;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(2, 1, "name");
        s.write(2, 3, "score");
        s.write(3, 1, "Road");
        s.write(3, 3, 12.5);
    }

    let range = wb.worksheet_range("Data").expect("range by name");
    assert_eq!(
        range.headers(),
        Some(vec!["name".into(), "".into(), "score".into()])
    );
    assert_eq!(range.get_value((2, 1)), Some(&Cell::Text("name".into())));
    assert_eq!(range.get_value((3, 2)), None);
    assert_eq!(range.get_value((3, u32::MAX)), None);

    let cells: Vec<_> = range.cells().collect();
    assert_eq!(
        cells,
        vec![
            (0, 0, Some(&Cell::Text("name".into()))),
            (0, 1, None),
            (0, 2, Some(&Cell::Text("score".into()))),
            (1, 0, Some(&Cell::Text("Road".into()))),
            (1, 1, None),
            (1, 2, Some(&Cell::Number(12.5))),
        ]
    );
}

#[cfg(feature = "xlsx")]
#[test]
fn lazy_borrowed_row_views_terminate_at_max_coordinates() {
    use rxls::Cell;

    let mut wb = Workbook::new();
    wb.add_sheet("Max").write(u32::MAX, u16::MAX, "x");
    let range = wb.worksheet_range("Max").expect("range by name");

    let mut rows = range.row_views();
    let row = rows.next().expect("row at max coordinate");
    let mut cells = row.iter();

    assert_eq!(row.row(), u32::MAX);
    assert_eq!(row.len(), 1);
    assert_eq!(cells.next(), Some(Some(&Cell::Text("x".into()))));
    assert_eq!(cells.next(), None);
    assert!(rows.next().is_none());
}

#[cfg(feature = "xlsx")]
#[test]
fn range_dimensions_and_row_views_handle_full_coordinate_span() {
    use rxls::Cell;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Wide");
        s.write(0, 0, "first");
        s.write(0, u16::MAX, "last-in-first-row");
        s.write(u32::MAX, u16::MAX, "last");
    }
    let range = wb.worksheet_range("Wide").expect("range by name");

    let expected_height = usize::try_from(u64::from(u32::MAX) + 1).unwrap_or(usize::MAX);
    let expected_width = usize::from(u16::MAX) + 1;

    assert_eq!(range.height(), expected_height);
    assert_eq!(range.width(), expected_width);
    assert_eq!(range.size(), (expected_height, expected_width));

    let first_row = range.row_views().next().expect("first row");
    assert_eq!(first_row.len(), expected_width);
    assert_eq!(first_row.get(0), Some(&Cell::Text("first".into())));
    assert_eq!(
        first_row.get(usize::from(u16::MAX)),
        Some(&Cell::Text("last-in-first-row".into()))
    );
}

/// Edge shapes through the public surface: an empty sheet alongside a long
/// Korean string that exercises shared-string interning on round-trip.
#[cfg(feature = "xlsx")]
#[test]
fn authoring_handles_empty_sheet_and_long_unicode() {
    use rxls::Cell;

    let mut wb = Workbook::new();
    wb.add_sheet("빈"); // empty sheet — no cells
    let long = "조달청".repeat(400); // 1200 chars
    wb.add_sheet("긴글").write(0, 0, long.as_str());

    let reread = Workbook::open(&wb.to_xlsx()).expect("reopen");
    assert_eq!(reread.sheets.len(), 2);
    assert_eq!(reread.sheets[0].cells().count(), 0);
    assert_eq!(reread.sheets[1].cell(0, 0), Some(&Cell::Text(long)));
}

/// rust_xlsxwriter-style Format aliases make the writer API recognizable without
/// changing the existing CellStyle implementation.
#[cfg(feature = "xlsx")]
#[test]
fn format_facade_writes_styled_cells() {
    use rxls::{Cell, Format, FormatAlign, FormatBorder};

    let header = Format::new()
        .set_bold()
        .set_font_name("Calibri")
        .set_font_size(12)
        .set_font_color([0x11, 0x22, 0x33])
        .set_strikethrough()
        .set_bg_color([0xDD, 0xEB, 0xF7])
        .set_align(FormatAlign::Center)
        .set_border(FormatBorder::Thin)
        .set_num_format("0.00");
    assert!(
        header
            .as_cell_style()
            .font
            .as_ref()
            .is_some_and(|font| font.strikethrough),
        "Format::set_strikethrough should set the public font decoration flag"
    );

    let mut wb = Workbook::new();
    let s = wb.add_sheet("fmt");
    s.write_with_format(0, 0, "amount", &header);
    s.write_number_with_format(1, 0, 12.5, &Format::new().set_num_format("0.00"));

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    assert_eq!(
        back.sheets[0].cell(0, 0),
        Some(&Cell::Text("amount".into()))
    );
    assert_eq!(back.sheets[0].cell(1, 0), Some(&Cell::Number(12.5)));
}

/// W1: typed writer aliases should cover the unformatted string, number, and
/// formula paths, not only the generic `write` and formatted helper variants.
#[cfg(feature = "xlsx")]
#[test]
fn writer_exposes_unformatted_typed_value_helpers() {
    use rxls::{Cell, Workbook};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("typed");
    s.write_string(0, 0, "label");
    s.write_number(1, 0, 12.5);
    s.write_formula(2, 0, "SUM(A1:A2)", 12.5);

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen typed helper workbook");
    let sheet = &back.sheets[0];
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("label".into())));
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Number(12.5)));
    match sheet.cell(2, 0).expect("formula helper cell") {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "SUM(A1:A2)");
            assert_eq!(cached.as_ref(), &Cell::Number(12.5));
        }
        other => panic!("expected formula helper cell, got {other:?}"),
    }

    let formula_range = back
        .worksheet_formula("typed")
        .expect("formula helper range");
    assert_eq!(formula_range.get_value((2, 0)), Some("SUM(A1:A2)"));
}

/// W1: the typed number helpers should accept the same common numeric primitive
/// families as rust_xlsxwriter-style number authoring examples, not only `f64`.
#[cfg(feature = "xlsx")]
#[test]
fn writer_number_helpers_accept_common_numeric_primitives() {
    use rxls::{Cell, Format, Workbook};

    let unsigned: u8 = 7;
    let signed: i16 = -8;
    let float: f32 = 9.5;

    let mut wb = Workbook::new();
    let s = wb.add_sheet("numbers");
    s.write_number(0, 0, unsigned);
    s.write_number_with_format(1, 0, signed, &Format::new().set_bold());
    s.write_number(2, 0, float);

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen typed number workbook");
    let sheet = &back.sheets[0];
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Number(7.0)));
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Number(-8.0)));
    assert_eq!(sheet.cell(2, 0), Some(&Cell::Number(9.5)));
}

/// W1: Excel-serial datetime helpers should accept common numeric primitive
/// variables, not only f64 literals.
#[cfg(feature = "xlsx")]
#[test]
fn writer_datetime_helpers_accept_common_numeric_serial_primitives() {
    use rxls::{Cell, Format, Workbook};

    let whole_days: u16 = 2;
    let fractional_days: f32 = 3.5;

    let mut wb = Workbook::new();
    let s = wb.add_sheet("dates");
    s.write_datetime(0, 0, whole_days);
    s.write_datetime_with_format(
        1,
        0,
        fractional_days,
        &Format::new().set_num_format("yyyy-mm-dd hh:mm"),
    );

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen typed datetime workbook");
    let sheet = &back.sheets[0];
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Date(2.0)));
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Date(3.5)));
}

/// W1: typed text-bearing helpers should accept owned string values, matching
/// the generic writer path that already accepts `String` through `Into<Cell>`.
#[cfg(feature = "xlsx")]
#[test]
fn writer_text_helpers_accept_owned_strings() {
    use rxls::{Cell, Format, Workbook};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("text");
    s.write_string(0, 0, String::from("owned label"));
    s.write_string_with_format(
        1,
        0,
        String::from("owned formatted"),
        &Format::new().set_bold(),
    );
    s.write_formula(2, 0, String::from("SUM(A1:A2)"), 3.0);
    s.write_formula_with_format(
        3,
        0,
        String::from("A2*2"),
        4.0,
        &Format::new().set_num_format("0.0"),
    );
    s.write_url(
        4,
        0,
        String::from("https://example.test/plain"),
        String::from("plain owned"),
    );
    s.write_url_with_text(
        5,
        0,
        String::from("https://example.test/text"),
        String::from("text owned"),
    );
    s.write_url_with_format(
        6,
        0,
        String::from("https://example.test/formatted"),
        &Format::new().set_underline(),
    );
    s.write_url_with_text_and_format(
        7,
        0,
        String::from("https://example.test/custom"),
        String::from("custom owned"),
        &Format::new().set_bold(),
    );
    s.merge_range(
        8,
        0,
        8,
        1,
        String::from("merged owned"),
        &Format::new().set_bold(),
    );

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen owned text helper workbook");
    let sheet = &back.sheets[0];
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("owned label".into())));
    assert_eq!(
        sheet.cell(1, 0),
        Some(&Cell::Text("owned formatted".into()))
    );
    assert_eq!(sheet.cell(4, 0), Some(&Cell::Text("plain owned".into())));
    assert_eq!(sheet.cell(5, 0), Some(&Cell::Text("text owned".into())));
    assert_eq!(
        sheet.cell(6, 0),
        Some(&Cell::Text("https://example.test/formatted".into()))
    );
    assert_eq!(sheet.cell(7, 0), Some(&Cell::Text("custom owned".into())));
    assert_eq!(sheet.cell(8, 0), Some(&Cell::Text("merged owned".into())));
    assert_eq!(
        sheet.hyperlinks(),
        &[
            (4, 0, "https://example.test/plain".to_string()),
            (5, 0, "https://example.test/text".to_string()),
            (6, 0, "https://example.test/formatted".to_string()),
            (7, 0, "https://example.test/custom".to_string())
        ]
    );
    assert_eq!(sheet.merged_ranges(), &[(8, 0, 8, 1)]);

    let formulas = back
        .worksheet_formula("text")
        .expect("formula helper range");
    assert_eq!(formulas.get_value((2, 0)), Some("SUM(A1:A2)"));
    assert_eq!(formulas.get_value((3, 0)), Some("A2*2"));
}

/// W1/W2: writer metadata text helpers should accept owned strings, matching
/// the owned text support on cell value helpers and style text setters.
#[cfg(feature = "xlsx")]
#[test]
fn writer_metadata_text_helpers_accept_owned_strings() {
    use rxls::{DataValidation, Format, Table, Workbook};

    let mut wb = Workbook::new();
    wb.define_name(String::from("OwnedChoice"), String::from("OwnedSheet!$A$2"));

    {
        let s = wb.add_sheet(String::from("OwnedSheet"));
        s.write_string(0, 0, "choice");
        s.write_string(1, 0, "Yes");
        s.add_data_validation(DataValidation::list(
            (1, 0, 1, 0),
            String::from("\"Yes,No\""),
        ));

        let table_name = String::from("ChoiceTable");
        s.add_table(Table {
            range: (0, 0, 1, 0),
            name: table_name.clone(),
            columns: vec!["choice".to_string()],
            style: None,
        });
        s.set_table_header_format(table_name, &Format::new().set_bold());

        assert_eq!(s.data_validations()[0].formula1, "\"Yes,No\"");
        assert_eq!(s.tables()[0].name(), "ChoiceTable");
    }

    assert_eq!(
        wb.defined_names(),
        &[("OwnedChoice".to_string(), "OwnedSheet!$A$2".to_string())]
    );

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen owned metadata text workbook");
    assert_eq!(back.sheet_names(), vec!["OwnedSheet"]);
    assert_eq!(
        back.defined_names(),
        &[("OwnedChoice".to_string(), "OwnedSheet!$A$2".to_string())]
    );
    assert_eq!(back.table_names(), vec!["ChoiceTable"]);
    assert_eq!(back.sheets[0].data_validations()[0].formula1, "\"Yes,No\"");
}

/// W1/W3: workbook document properties should have the same object-construction
/// style as other writer metadata, including owned text and write/read coverage.
#[cfg(feature = "xlsx")]
#[test]
fn doc_properties_authoring_helpers_accept_owned_text() {
    use rxls::{DocProperties, Workbook};

    let mut wb = Workbook::new();
    wb.add_sheet("props").write_string(0, 0, "payload");
    wb.set_properties(
        DocProperties::new()
            .with_title(String::from("Owned Report"))
            .with_subject(String::from("Procurement"))
            .with_creator(String::from("rxls author"))
            .with_keywords(String::from("owned,metadata"))
            .with_description(String::from("Owned workbook description"))
            .with_last_modified_by(String::from("reviewer"))
            .with_company(String::from("ACME"))
            .with_created(String::from("2024-01-02T03:04:05Z")),
    );

    assert_eq!(wb.properties.title.as_deref(), Some("Owned Report"));
    assert_eq!(wb.properties.subject.as_deref(), Some("Procurement"));
    assert_eq!(wb.properties.creator.as_deref(), Some("rxls author"));
    assert_eq!(wb.properties.keywords.as_deref(), Some("owned,metadata"));
    assert_eq!(
        wb.properties.description.as_deref(),
        Some("Owned workbook description")
    );
    assert_eq!(wb.properties.last_modified_by.as_deref(), Some("reviewer"));
    assert_eq!(wb.properties.company.as_deref(), Some("ACME"));
    assert_eq!(
        wb.properties.created.as_deref(),
        Some("2024-01-02T03:04:05Z")
    );

    let bytes = wb
        .to_xlsx_checked()
        .expect("metadata helper workbook validates");
    let back = Workbook::open(&bytes).expect("reopen metadata helper workbook");
    let properties = back.metadata().properties;
    assert_eq!(properties.title.as_deref(), Some("Owned Report"));
    assert_eq!(properties.subject.as_deref(), Some("Procurement"));
    assert_eq!(properties.creator.as_deref(), Some("rxls author"));
    assert_eq!(properties.keywords.as_deref(), Some("owned,metadata"));
    assert_eq!(
        properties.description.as_deref(),
        Some("Owned workbook description")
    );
    assert_eq!(properties.last_modified_by.as_deref(), Some("reviewer"));
    assert_eq!(properties.company.as_deref(), Some("ACME"));
    assert_eq!(properties.created.as_deref(), Some("2024-01-02T03:04:05Z"));
}

/// W1: table authoring should expose an object helper that accepts owned table
/// names, iterable column labels, and owned style names without struct-literal
/// string conversion noise.
#[cfg(feature = "xlsx")]
#[test]
fn table_authoring_helper_accepts_owned_and_iterable_text() {
    use rxls::{Table, Workbook};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("tables");
    s.write_string(0, 0, "item");
    s.write_string(0, 1, "amount");
    s.write_string(1, 0, "cable");
    s.write_number(1, 1, 12);

    s.add_table(
        Table::new(
            (0, 0, 1, 1),
            String::from("OwnedTable"),
            [String::from("item"), String::from("amount")],
        )
        .with_style(String::from("TableStyleMedium4")),
    );

    assert_eq!(s.tables()[0].name(), "OwnedTable");
    assert_eq!(
        s.tables()[0].columns(),
        &[String::from("item"), String::from("amount")]
    );
    assert_eq!(s.tables()[0].style.as_deref(), Some("TableStyleMedium4"));

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen table helper workbook");
    let table = &back.sheet_by_name("tables").expect("tables sheet").tables()[0];
    assert_eq!(table.name(), "OwnedTable");
    assert_eq!(
        table.columns(),
        &[String::from("item"), String::from("amount")]
    );
    assert_eq!(table.style.as_deref(), Some("TableStyleMedium4"));
}

/// W1: sparkline authoring should expose a small object helper that accepts
/// owned range text and keeps the visual-kind selection fluent.
#[cfg(feature = "xlsx")]
#[test]
fn sparkline_authoring_helper_accepts_owned_range_and_kind() {
    use rxls::{Sparkline, SparklineKind, Workbook};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("spark");
    for row in 0..5u32 {
        s.write_number(row, 0, row + 1);
    }

    let line = Sparkline::new((5, 0), String::from("spark!$A$1:$A$5"));
    assert_eq!(line.kind, SparklineKind::Line);
    assert_eq!(line.range, "spark!$A$1:$A$5");

    s.add_sparkline(line.with_kind(SparklineKind::Column));

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen sparkline helper workbook");
    let sparkline = &back
        .sheet_by_name("spark")
        .expect("spark sheet")
        .sparklines()[0];
    assert_eq!(sparkline.location, (5, 0));
    assert_eq!(sparkline.range, "spark!$A$1:$A$5");
    assert_eq!(sparkline.kind, SparklineKind::Column);
}

/// W1: chart and series authoring should expose object helpers for common
/// text-bearing fields and iterable series collections.
#[cfg(feature = "xlsx")]
#[test]
fn chart_authoring_helpers_accept_owned_text_and_iterable_series() {
    use rxls::{Chart, ChartKind, Series, Workbook};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("charts");
    for row in 0..3u32 {
        s.write_string(row, 0, format!("c{row}"));
        s.write_number(row, 1, row + 1);
    }

    s.add_chart(
        Chart::new(ChartKind::Line, (5, 1), (14, 7))
            .with_title(String::from("Owned Trend"))
            .with_x_axis_title(String::from("Owned Category"))
            .with_y_axis_title(String::from("Owned Amount"))
            .with_legend(true)
            .with_data_labels(true)
            .with_series([Series::new(String::from("charts!$B$1:$B$3"))
                .with_name(String::from("Owned Value"))
                .with_categories(String::from("charts!$A$1:$A$3"))]),
    );

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen chart helper workbook");
    let chart = &back.sheet_by_name("charts").expect("charts sheet").charts()[0];
    assert_eq!(chart.kind, ChartKind::Line);
    assert_eq!(chart.title.as_deref(), Some("Owned Trend"));
    assert_eq!(chart.x_axis_title.as_deref(), Some("Owned Category"));
    assert_eq!(chart.y_axis_title.as_deref(), Some("Owned Amount"));
    assert!(chart.legend);
    assert!(chart.data_labels);
    assert_eq!(chart.from, (5, 1));
    assert_eq!(chart.to, (14, 7));
    assert_eq!(chart.series.len(), 1);
    assert_eq!(chart.series[0].name.as_deref(), Some("Owned Value"));
    assert_eq!(
        chart.series[0].categories.as_deref(),
        Some("charts!$A$1:$A$3")
    );
    assert_eq!(chart.series[0].values, "charts!$B$1:$B$3");
}

/// W1: image authoring should not require raw struct literals for common
/// embedded-picture construction.
#[cfg(feature = "xlsx")]
#[test]
fn image_authoring_helper_accepts_borrowed_bytes_and_anchor() {
    use rxls::{Image, ImageFmt, Workbook};

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0A, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0,
        0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let mut wb = Workbook::new();
    wb.add_sheet("img")
        .add_image(Image::new(PNG_1X1, ImageFmt::Png, (1, 2)).with_to((4, 5)));

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen image helper workbook");
    let image = &back.sheet_by_name("img").expect("image sheet").images()[0];
    assert_eq!(image.data, PNG_1X1);
    assert_eq!(image.format, ImageFmt::Png);
    assert_eq!(image.from, (1, 2));
    assert_eq!(image.to, Some((4, 5)));

    let pictures = back.pictures().expect("workbook pictures");
    assert_eq!(pictures, vec![("png".to_string(), PNG_1X1.to_vec())]);
    let picture = &back.pictures_with_metadata()[0];
    assert_eq!(picture.sheet_name, "img");
    assert_eq!(picture.row, 1);
    assert_eq!(picture.col, 2);
    assert_eq!(picture.extension, "png");
}

/// W1: conditional-format authoring should expose constructors for common
/// formula-bearing rules instead of requiring nested struct literals.
#[cfg(feature = "xlsx")]
#[test]
fn conditional_format_authoring_helpers_accept_owned_formula_text() {
    use rxls::{CfRule, Color, CondFormat, DvOp, Workbook};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("cf");
    for row in 0..3u32 {
        s.write_number(row, 0, row + 1);
        s.write_number(row, 1, row + 3);
    }

    s.add_conditional_format(CondFormat::new(
        (0, 0, 2, 0),
        CfRule::cell_is(
            DvOp::Between,
            String::from("1"),
            Some(String::from("3")),
            Color::rgb(0xFF, 0xC7, 0xCE),
        ),
    ));
    s.add_conditional_format(CondFormat::new(
        (0, 1, 2, 1),
        CfRule::expression(String::from("$B1>3"), Color::rgb(0x44, 0xAA, 0x66)),
    ));

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen conditional format helper workbook");
    let cond_formats = back
        .sheet_by_name("cf")
        .expect("conditional format sheet")
        .conditional_formats();
    assert_eq!(cond_formats.len(), 2);
    assert_eq!(cond_formats[0].sqref, (0, 0, 2, 0));
    match &cond_formats[0].rule {
        CfRule::CellIs {
            op,
            formula1,
            formula2,
            fill,
        } => {
            assert_eq!(*op, DvOp::Between);
            assert_eq!(formula1, "1");
            assert_eq!(formula2.as_deref(), Some("3"));
            assert_eq!(*fill, Color::rgb(0xFF, 0xC7, 0xCE));
        }
        other => panic!("unexpected helper cell-is rule: {other:?}"),
    }
    assert_eq!(cond_formats[1].sqref, (0, 1, 2, 1));
    match &cond_formats[1].rule {
        CfRule::Expression { formula, fill } => {
            assert_eq!(formula, "$B1>3");
            assert_eq!(*fill, Color::rgb(0x44, 0xAA, 0x66));
        }
        other => panic!("unexpected helper expression rule: {other:?}"),
    }
}

/// W1/W3: conditional-format helper constructors should cover the remaining
/// non-formula rule variants as well as cell-is/expression.
#[cfg(feature = "xlsx")]
#[test]
fn conditional_format_rule_helpers_cover_non_formula_variants() {
    use rxls::{CfRule, Color, CondFormat, Workbook};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("cf-rules");
    for row in 0..8u32 {
        s.write_number(row, 0, row + 1);
    }

    let fill = Color::rgb(0xFF, 0xC7, 0xCE);
    for (idx, rule) in [
        CfRule::color_scale2(Color::rgb(0xF8, 0x69, 0x6B), Color::rgb(0x63, 0xBE, 0x7B)),
        CfRule::color_scale3(
            Color::rgb(0xF8, 0x69, 0x6B),
            Color::rgb(0xFF, 0xEB, 0x84),
            Color::rgb(0x63, 0xBE, 0x7B),
        ),
        CfRule::data_bar(Color::rgb(0x44, 0xAA, 0x66)),
        CfRule::top_bottom(3, false, false, fill),
        CfRule::above_average(false, fill),
        CfRule::duplicate_values(true, fill),
    ]
    .into_iter()
    .enumerate()
    {
        s.add_conditional_format(CondFormat::new((0, idx as u16, 7, idx as u16), rule));
    }

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen conditional rule helper workbook");
    let formats = back
        .sheet_by_name("cf-rules")
        .expect("conditional rule sheet")
        .conditional_formats();
    assert_eq!(formats.len(), 6);

    match &formats[0].rule {
        CfRule::ColorScale2 { min, max } => {
            assert_eq!(*min, Color::rgb(0xF8, 0x69, 0x6B));
            assert_eq!(*max, Color::rgb(0x63, 0xBE, 0x7B));
        }
        other => panic!("unexpected color-scale-2 rule: {other:?}"),
    }
    match &formats[1].rule {
        CfRule::ColorScale3 { min, mid, max } => {
            assert_eq!(*min, Color::rgb(0xF8, 0x69, 0x6B));
            assert_eq!(*mid, Color::rgb(0xFF, 0xEB, 0x84));
            assert_eq!(*max, Color::rgb(0x63, 0xBE, 0x7B));
        }
        other => panic!("unexpected color-scale-3 rule: {other:?}"),
    }
    match &formats[2].rule {
        CfRule::DataBar { color } => assert_eq!(*color, Color::rgb(0x44, 0xAA, 0x66)),
        other => panic!("unexpected data-bar rule: {other:?}"),
    }
    match &formats[3].rule {
        CfRule::TopBottom {
            rank,
            bottom,
            percent,
            fill: rule_fill,
        } => {
            assert_eq!(*rank, 3);
            assert!(!bottom);
            assert!(!percent);
            assert_eq!(*rule_fill, fill);
        }
        other => panic!("unexpected top-bottom rule: {other:?}"),
    }
    match &formats[4].rule {
        CfRule::AboveAverage {
            below,
            fill: rule_fill,
        } => {
            assert!(!below);
            assert_eq!(*rule_fill, fill);
        }
        other => panic!("unexpected above-average rule: {other:?}"),
    }
    match &formats[5].rule {
        CfRule::DuplicateValues {
            unique,
            fill: rule_fill,
        } => {
            assert!(unique);
            assert_eq!(*rule_fill, fill);
        }
        other => panic!("unexpected duplicate-values rule: {other:?}"),
    }
}

/// W1: non-list data-validation authoring should not require full struct
/// literals for formula2, prompt/error metadata, or blank-cell behavior.
#[cfg(feature = "xlsx")]
#[test]
fn data_validation_authoring_helpers_accept_owned_prompt_error_text() {
    use rxls::{DataValidation, DvKind, DvOp, Workbook};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("dv");
    s.add_data_validation(
        DataValidation::new(
            (1, 0, 4, 0),
            DvKind::Whole,
            DvOp::Between,
            String::from("1"),
        )
        .with_formula2(String::from("10"))
        .with_allow_blank(false)
        .with_prompt(String::from("Whole"), String::from("Enter 1 to 10"))
        .with_error(String::from("Bounds"), String::from("Use 1 to 10")),
    );

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen data validation helper workbook");
    let validations = back
        .sheet_by_name("dv")
        .expect("data validation sheet")
        .data_validations();
    assert_eq!(validations.len(), 1);
    let validation = &validations[0];
    assert_eq!(validation.sqref, (1, 0, 4, 0));
    assert_eq!(validation.kind, DvKind::Whole);
    assert_eq!(validation.operator, DvOp::Between);
    assert_eq!(validation.formula1, "1");
    assert_eq!(validation.formula2.as_deref(), Some("10"));
    assert!(!validation.allow_blank);
    assert!(validation.show_input_message);
    assert!(validation.show_error_message);
    assert_eq!(
        validation
            .prompt
            .as_ref()
            .map(|(title, msg)| (title.as_str(), msg.as_str())),
        Some(("Whole", "Enter 1 to 10"))
    );
    assert_eq!(
        validation
            .error
            .as_ref()
            .map(|(title, msg)| (title.as_str(), msg.as_str())),
        Some(("Bounds", "Use 1 to 10"))
    );
}

/// W1/W3: worksheet protection allowances should have object helpers instead
/// of requiring callers to spell every raw struct field directly.
#[cfg(feature = "xlsx")]
#[test]
fn protection_options_authoring_helpers_cover_allowance_flags() {
    use rxls::{ProtectionOptions, Workbook};

    let expected = ProtectionOptions {
        sort: true,
        auto_filter: true,
        format_cells: true,
        format_columns: true,
        format_rows: true,
        insert_columns: true,
        insert_rows: true,
        insert_hyperlinks: true,
        delete_columns: true,
        delete_rows: true,
        pivot_tables: true,
    };

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("protect");
        s.write_string(0, 0, "locked");
        s.protect_with(
            ProtectionOptions::new()
                .allow_sort()
                .allow_auto_filter()
                .allow_format_cells()
                .allow_format_columns()
                .allow_format_rows()
                .allow_insert_columns()
                .allow_insert_rows()
                .allow_insert_hyperlinks()
                .allow_delete_columns()
                .allow_delete_rows()
                .allow_pivot_tables(),
        );

        assert!(s.is_protected());
        assert_eq!(s.protection_options(), Some(expected));
    }

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen protection helper workbook");
    let sheet = back.sheet_by_name("protect").expect("protect sheet");
    assert!(sheet.is_protected());
    assert_eq!(sheet.protection_options(), Some(expected));
    assert_eq!(sheet.metadata().protection_options, Some(expected));
}

/// W1/W3: page setup authoring should expose a builder-style object API for
/// common print metadata instead of requiring full struct literals.
#[cfg(feature = "xlsx")]
#[test]
fn page_setup_authoring_helpers_accept_owned_header_footer_text() {
    use rxls::{PageSetup, Workbook};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("print");
    s.write_string(0, 0, "item");
    s.set_page_setup(
        PageSetup::new()
            .with_landscape()
            .with_margins(0.5, 0.6, 0.7, 0.8, 0.2, 0.25)
            .with_print_area((0, 0, 9, 4))
            .with_repeat_rows(0, 1)
            .with_repeat_cols(0, 2)
            .with_fit_to_pages(1, 2)
            .with_header(String::from("&CReport"))
            .with_footer(String::from("&RPage &P"))
            .with_paper_size(9)
            .with_scale(85)
            .with_center_horizontally(true)
            .with_center_vertically(true)
            .with_first_page_number(3),
    );

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen page setup helper workbook");
    let setup = back
        .sheet_by_name("print")
        .expect("print sheet")
        .page_setup()
        .expect("page setup");

    assert!(setup.landscape);
    assert_eq!(setup.margins, Some((0.5, 0.6, 0.7, 0.8, 0.2, 0.25)));
    assert_eq!(setup.print_area, Some((0, 0, 9, 4)));
    assert_eq!(setup.repeat_rows, Some((0, 1)));
    assert_eq!(setup.repeat_cols, Some((0, 2)));
    assert_eq!(setup.fit_to_width, Some(1));
    assert_eq!(setup.fit_to_height, Some(2));
    assert_eq!(setup.header.as_deref(), Some("&CReport"));
    assert_eq!(setup.footer.as_deref(), Some("&RPage &P"));
    assert_eq!(setup.paper_size, Some(9));
    assert_eq!(setup.scale, Some(85));
    assert!(setup.center_horizontally);
    assert!(setup.center_vertically);
    assert_eq!(setup.first_page_number, Some(3));
}

/// W1/W2: comment authoring should store owned text without caller-side
/// borrowing while preserving the no-author path.
#[cfg(feature = "xlsx")]
#[test]
fn writer_comment_helper_accepts_owned_strings() {
    use rxls::Workbook;

    let mut wb = Workbook::new();
    let s = wb.add_sheet("comments");
    s.write_string(0, 0, "owned comment");
    s.add_comment(
        0,
        0,
        String::from("owned body"),
        String::from("owned author"),
    );
    s.add_comment(1, 0, String::from("anonymous body"), None);

    assert_eq!(s.comments()[0].text, "owned body");
    assert_eq!(s.comments()[0].author.as_deref(), Some("owned author"));
    assert_eq!(s.comments()[1].text, "anonymous body");
    assert_eq!(s.comments()[1].author.as_deref(), None);

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen owned comment workbook");
    let comments = back.sheets[0].comments();
    assert_eq!(comments[0].text, "owned body");
    assert_eq!(comments[0].author.as_deref(), Some("owned author"));
    assert_eq!(comments[1].text, "anonymous body");
    assert_eq!(comments[1].author.as_deref(), None);
}

/// W1: text-bearing Format/CellStyle builder setters should accept owned
/// strings, matching the owned text support on writer value helpers.
#[cfg(feature = "xlsx")]
#[test]
fn format_text_setters_accept_owned_strings() {
    use rxls::{Cell, CellStyle, Format, Workbook};

    let header_font = String::from("Aptos");
    let header_format_code = String::from("0.00");
    let body_font = String::from("Calibri");
    let body_format_code = String::from("#,##0");

    let header = Format::new()
        .set_font_name(header_font)
        .set_num_format(header_format_code)
        .set_bold();
    let body = CellStyle::new()
        .font_name(body_font)
        .num_fmt(body_format_code)
        .italic();

    assert_eq!(
        header
            .as_cell_style()
            .font
            .as_ref()
            .and_then(|font| font.name.as_deref()),
        Some("Aptos")
    );
    assert_eq!(header.as_cell_style().num_fmt.as_deref(), Some("0.00"));
    assert_eq!(
        body.font.as_ref().and_then(|font| font.name.as_deref()),
        Some("Calibri")
    );
    assert_eq!(body.num_fmt.as_deref(), Some("#,##0"));

    let mut wb = Workbook::new();
    let s = wb.add_sheet("fmt-text");
    s.write_with_format(0, 0, "header", &header);
    s.write_styled(1, 0, "body", &body);

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen owned format text workbook");
    assert_eq!(
        back.sheets[0].cell(0, 0),
        Some(&Cell::Text("header".into()))
    );
    assert_eq!(back.sheets[0].cell(1, 0), Some(&Cell::Text("body".into())));
}

/// W1: formula cells have an explicit Format helper, matching the other
/// value-plus-format paths while preserving rxls' cached formula value model.
#[cfg(feature = "xlsx")]
#[test]
fn format_facade_writes_formula_cells() {
    use rxls::{Cell, Format};
    use std::io::Read;

    fn part(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut file = zip.by_name(name).expect(name);
        let mut xml = String::new();
        file.read_to_string(&mut xml).expect("xml");
        xml
    }

    fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        let needle = format!(r#"{name}=""#);
        let start = tag.find(&needle)? + needle.len();
        let end = tag[start..].find('"')?;
        Some(&tag[start..start + end])
    }

    let mut wb = Workbook::new();
    let s = wb.add_sheet("formulas");
    s.write(0, 1, 40.0);
    s.write(1, 1, 2.0);
    s.write_formula_with_format(
        0,
        0,
        "SUM(B1:B2)",
        42.0,
        &Format::new().set_num_format("0.00"),
    );

    let bytes = wb.to_xlsx();
    let sheet_xml = part(&bytes, "xl/worksheets/sheet1.xml");
    let cell_start = sheet_xml.find(r#"<c r="A1""#).expect("A1 cell");
    let cell_end = sheet_xml[cell_start..].find("</c>").expect("A1 end") + cell_start;
    let cell_tag_end = sheet_xml[cell_start..].find('>').expect("A1 tag") + cell_start;
    let cell_tag = &sheet_xml[cell_start..=cell_tag_end];
    let style_idx = attr(cell_tag, "s")
        .expect("formula cell style")
        .parse::<usize>()
        .expect("style index");
    assert!(style_idx > 0);
    assert!(sheet_xml[cell_start..cell_end].contains("<f>SUM(B1:B2)</f>"));
    assert!(sheet_xml[cell_start..cell_end].contains("<v>42</v>"));

    let styles_xml = part(&bytes, "xl/styles.xml");
    let fmt_pos = styles_xml
        .find(r#"formatCode="0.00""#)
        .expect("custom number format");
    let fmt_tag_start = styles_xml[..fmt_pos].rfind("<numFmt ").expect("numFmt tag");
    let fmt_tag_end = styles_xml[fmt_pos..].find("/>").expect("numFmt end") + fmt_pos;
    let fmt_tag = &styles_xml[fmt_tag_start..fmt_tag_end];
    let numfmt_id = attr(fmt_tag, "numFmtId").expect("numFmtId");

    let cell_xfs_start = styles_xml.find("<cellXfs").expect("cellXfs");
    let cell_xfs_end = styles_xml.find("</cellXfs>").expect("cellXfs end");
    let mut xfs = Vec::new();
    let mut rest = &styles_xml[cell_xfs_start..cell_xfs_end];
    while let Some(pos) = rest.find("<xf ") {
        rest = &rest[pos..];
        let end = rest.find("/>").expect("xf end");
        xfs.push(attr(&rest[..end], "numFmtId").unwrap_or("0"));
        rest = &rest[end + 2..];
    }
    assert_eq!(xfs.get(style_idx).copied(), Some(numfmt_id));

    let back = Workbook::open(&bytes).expect("reopen");
    let sheet = &back.sheets[0];

    match sheet.cell(0, 0).expect("formula cell") {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "SUM(B1:B2)");
            assert_eq!(cached.as_ref(), &Cell::Number(42.0));
        }
        other => panic!("expected formula cell, got {other:?}"),
    }
}

/// W1: typed boolean and datetime helpers keep the Format application surface
/// close to rust_xlsxwriter-style authoring while preserving rxls' typed model.
#[cfg(feature = "xlsx")]
#[test]
fn format_facade_writes_typed_boolean_and_datetime_helpers() {
    use rxls::{Cell, Format};
    use std::io::Read;

    fn part(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut file = zip.by_name(name).expect(name);
        let mut xml = String::new();
        file.read_to_string(&mut xml).expect("xml");
        xml
    }

    fn cell_fragment<'a>(sheet_xml: &'a str, cell_ref: &str) -> &'a str {
        let needle = format!(r#"<c r="{cell_ref}""#);
        let start = sheet_xml.find(&needle).expect(cell_ref);
        let end = sheet_xml[start..]
            .find("</c>")
            .map(|offset| start + offset + "</c>".len())
            .or_else(|| {
                sheet_xml[start..]
                    .find("/>")
                    .map(|offset| start + offset + 2)
            })
            .expect("cell end");
        &sheet_xml[start..end]
    }

    let mut wb = Workbook::new();
    let s = wb.add_sheet("typed");
    s.write_boolean(0, 0, true);
    s.write_boolean_with_format(1, 0, false, &Format::new().set_bold());
    s.write_datetime(0, 1, 45_366.5);
    s.write_datetime_with_format(
        1,
        1,
        45_366.5,
        &Format::new().set_num_format("yyyy-mm-dd hh:mm"),
    );

    let bytes = wb.to_xlsx();
    let sheet_xml = part(&bytes, "xl/worksheets/sheet1.xml");
    let styles_xml = part(&bytes, "xl/styles.xml");

    assert_eq!(
        cell_fragment(&sheet_xml, "A1"),
        r#"<c r="A1" t="b"><v>1</v></c>"#
    );
    assert!(cell_fragment(&sheet_xml, "A2").contains(r#" t="b"><v>0</v></c>"#));
    assert!(cell_fragment(&sheet_xml, "A2").contains(r#" s=""#));
    assert!(cell_fragment(&sheet_xml, "B1").contains("<v>45366.5</v>"));
    assert!(cell_fragment(&sheet_xml, "B1").contains(r#" s=""#));
    assert!(cell_fragment(&sheet_xml, "B2").contains("<v>45366.5</v>"));
    assert!(cell_fragment(&sheet_xml, "B2").contains(r#" s=""#));
    assert!(styles_xml.contains(r#"formatCode="yyyy-mm-dd hh:mm""#));

    let back = Workbook::open(&bytes).expect("reopen");
    let sheet = &back.sheets[0];
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Bool(true)));
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Bool(false)));
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Date(45_366.5)));
    assert_eq!(sheet.cell(1, 1), Some(&Cell::Date(45_366.5)));
}

/// W1: typed spreadsheet errors have explicit authoring helpers instead of
/// forcing callers to spell Excel error strings by hand.
#[cfg(feature = "xlsx")]
#[test]
fn format_facade_writes_typed_error_helpers() {
    use rxls::{Cell, CellErrorType, Format};
    use std::io::Read;

    fn part(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut file = zip.by_name(name).expect(name);
        let mut xml = String::new();
        file.read_to_string(&mut xml).expect("xml");
        xml
    }

    fn cell_fragment<'a>(sheet_xml: &'a str, cell_ref: &str) -> &'a str {
        let needle = format!(r#"<c r="{cell_ref}""#);
        let start = sheet_xml.find(&needle).expect(cell_ref);
        let end = sheet_xml[start..]
            .find("</c>")
            .map(|offset| start + offset + "</c>".len())
            .or_else(|| {
                sheet_xml[start..]
                    .find("/>")
                    .map(|offset| start + offset + 2)
            })
            .expect("cell end");
        &sheet_xml[start..end]
    }

    let mut wb = Workbook::new();
    let s = wb.add_sheet("errors");
    s.write_error(0, 0, CellErrorType::Div0);
    s.write_error_with_format(1, 0, CellErrorType::Ref, &Format::new().set_bold());
    s.write_formula_with_format(2, 0, "NA()", CellErrorType::NA, &Format::new().set_italic());

    let bytes = wb.to_xlsx();
    let sheet_xml = part(&bytes, "xl/worksheets/sheet1.xml");

    assert_eq!(
        cell_fragment(&sheet_xml, "A1"),
        r#"<c r="A1" t="e"><v>#DIV/0!</v></c>"#
    );
    assert!(cell_fragment(&sheet_xml, "A2").contains(r#" s=""#));
    assert!(cell_fragment(&sheet_xml, "A2").contains(r#" t="e"><v>#REF!</v></c>"#));
    assert!(cell_fragment(&sheet_xml, "A3").contains(r#" t="e"><f>NA()</f><v>#N/A</v></c>"#));
    assert!(cell_fragment(&sheet_xml, "A3").contains(r#" s=""#));

    let back = Workbook::open(&bytes).expect("reopen");
    let sheet = &back.sheets[0];
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Error("#DIV/0!".into())));
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Error("#REF!".into())));
    match sheet.cell(2, 0).expect("formula error") {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "NA()");
            assert_eq!(cached.as_ref(), &Cell::Error("#N/A".into()));
        }
        other => panic!("expected formula error, got {other:?}"),
    }
}

/// W1: hyperlink authoring has rust_xlsxwriter-style text and Format helpers,
/// instead of forcing callers through the lower-level generic value writer.
#[cfg(feature = "xlsx")]
#[test]
fn format_facade_writes_formatted_hyperlinks() {
    use rxls::{Cell, Color, Format};
    use std::io::Read;

    fn part(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut file = zip.by_name(name).expect(name);
        let mut xml = String::new();
        file.read_to_string(&mut xml).expect("xml");
        xml
    }

    fn cell_fragment<'a>(sheet_xml: &'a str, cell_ref: &str) -> &'a str {
        let needle = format!(r#"<c r="{cell_ref}""#);
        let start = sheet_xml.find(&needle).expect(cell_ref);
        let end = sheet_xml[start..]
            .find("</c>")
            .map(|offset| start + offset + "</c>".len())
            .or_else(|| {
                sheet_xml[start..]
                    .find("/>")
                    .map(|offset| start + offset + 2)
            })
            .expect("cell end");
        &sheet_xml[start..end]
    }

    let mut wb = Workbook::new();
    let s = wb.add_sheet("links");
    s.write_url_with_text(0, 0, "https://example.test/plain", "plain text");
    s.write_url_with_format(
        1,
        0,
        "https://example.test/formatted",
        &Format::new()
            .set_font_color(Color::rgb(0x11, 0x55, 0xAA))
            .set_underline(),
    );
    s.write_url_with_text_and_format(
        2,
        0,
        "https://example.test/custom",
        "custom text",
        &Format::new().set_bold(),
    );

    let bytes = wb.to_xlsx();
    let sheet_xml = part(&bytes, "xl/worksheets/sheet1.xml");
    let rels_xml = part(&bytes, "xl/worksheets/_rels/sheet1.xml.rels");
    let styles_xml = part(&bytes, "xl/styles.xml");

    assert!(sheet_xml.contains(r#"<hyperlink ref="A1" r:id="rId1"/>"#));
    assert!(sheet_xml.contains(r#"<hyperlink ref="A2" r:id="rId2"/>"#));
    assert!(sheet_xml.contains(r#"<hyperlink ref="A3" r:id="rId3"/>"#));
    assert!(cell_fragment(&sheet_xml, "A2").contains(r#" s=""#));
    assert!(cell_fragment(&sheet_xml, "A3").contains(r#" s=""#));
    assert!(rels_xml.contains(r#"Target="https://example.test/plain""#));
    assert!(rels_xml.contains(r#"Target="https://example.test/formatted""#));
    assert!(rels_xml.contains(r#"Target="https://example.test/custom""#));
    assert!(styles_xml.contains("FF1155AA"));
    assert!(styles_xml.contains("<u/>"));
    assert!(styles_xml.contains("<b/>"));

    let back = Workbook::open(&bytes).expect("reopen");
    let sheet = &back.sheets[0];
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("plain text".into())));
    assert_eq!(
        sheet.cell(1, 0),
        Some(&Cell::Text("https://example.test/formatted".into()))
    );
    assert_eq!(sheet.cell(2, 0), Some(&Cell::Text("custom text".into())));
    assert_eq!(
        sheet.hyperlinks(),
        &[
            (0, 0, "https://example.test/plain".to_string()),
            (1, 0, "https://example.test/formatted".to_string()),
            (2, 0, "https://example.test/custom".to_string())
        ]
    );
}

/// W1: merged-range authoring has a Format-aware helper that writes the
/// top-left value and merge metadata through one public API.
#[cfg(feature = "xlsx")]
#[test]
fn format_facade_writes_merged_range_with_format() {
    use rxls::{Cell, Format, FormatAlign, FormatBorder};
    use std::io::Read;

    fn part(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut file = zip.by_name(name).expect(name);
        let mut xml = String::new();
        file.read_to_string(&mut xml).expect("xml");
        xml
    }

    fn cell_fragment<'a>(sheet_xml: &'a str, cell_ref: &str) -> &'a str {
        let needle = format!(r#"<c r="{cell_ref}""#);
        let start = sheet_xml.find(&needle).expect(cell_ref);
        let end = sheet_xml[start..]
            .find("</c>")
            .map(|offset| start + offset + "</c>".len())
            .or_else(|| {
                sheet_xml[start..]
                    .find("/>")
                    .map(|offset| start + offset + 2)
            })
            .expect("cell end");
        &sheet_xml[start..end]
    }

    let mut wb = Workbook::new();
    let s = wb.add_sheet("merge");
    s.merge_range(
        0,
        0,
        0,
        2,
        "Merged title",
        &Format::new()
            .set_align(FormatAlign::Center)
            .set_border(FormatBorder::Thin)
            .set_bg_color([0xDD, 0xEB, 0xF7]),
    );
    s.write(0, 1, "under merge should not emit");

    let bytes = wb.to_xlsx();
    let sheet_xml = part(&bytes, "xl/worksheets/sheet1.xml");
    let styles_xml = part(&bytes, "xl/styles.xml");

    assert!(sheet_xml.contains(r#"<mergeCell ref="A1:C1"/>"#));
    assert!(cell_fragment(&sheet_xml, "A1").contains(r#" s=""#));
    assert!(!sheet_xml.contains(r#"<c r="B1""#));
    assert!(styles_xml.contains(r#"horizontal="center""#));
    assert!(styles_xml.contains(r#"<left style="thin""#));
    assert!(styles_xml.contains("FFDDEBF7"));

    let back = Workbook::open(&bytes).expect("reopen");
    let sheet = &back.sheets[0];
    assert_eq!(sheet.merged_ranges(), &[(0, 0, 0, 2)]);
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("Merged title".into())));
    assert_eq!(sheet.cell(0, 1), None);
}

/// W1: color construction is stable and reusable across the Format facade,
/// instead of requiring callers to pass raw RGB arrays at every setter.
#[test]
fn format_color_constructor_feeds_format_facade() {
    use rxls::{Color, Format, FormatBorder};

    let font = Color::rgb(0x11, 0x22, 0x33);
    let fill = Color::rgb(0xDD, 0xEB, 0xF7);
    let border = Color::rgb(0x44, 0x55, 0x66);

    assert_eq!(font.as_rgb(), [0x11, 0x22, 0x33]);

    let format = Format::new()
        .set_font_color(font)
        .set_bg_color(fill)
        .set_border(FormatBorder::Thin)
        .set_border_color(border);
    let style = format.as_cell_style();

    assert_eq!(style.font.as_ref().and_then(|font| font.color), Some(font));
    assert_eq!(style.fill, Some(fill));
    assert_eq!(
        style.border.as_ref().and_then(|border| border.color),
        Some(border)
    );
}

/// W1: public style subobjects are constructible without field literals and
/// bridge directly through the Format/CellStyle object model.
#[test]
fn format_subobject_helpers_feed_object_model() {
    use rxls::{
        Alignment, Border, CellStyle, Color, Fill, Format, FormatAlign, FormatBorder,
        FormatPattern, VAlign,
    };

    let fill = Fill::new()
        .with_pattern(FormatPattern::DarkGrid)
        .with_background(Color::rgb(0x11, 0x22, 0x33))
        .with_foreground([0xDD, 0xEB, 0xF7]);
    let border = Border::new()
        .with_all(FormatBorder::Thin)
        .with_color(Color::rgb(0x44, 0x55, 0x66))
        .with_top(FormatBorder::Thick)
        .with_top_color(Color::rgb(0x77, 0x88, 0x99));
    let alignment = Alignment::new()
        .with_horizontal(FormatAlign::Center)
        .with_vertical(VAlign::Middle)
        .wrapped()
        .with_indent(2)
        .with_rotation(45)
        .with_shrink_to_fit();

    let format = Format::new()
        .set_pattern_fill(fill)
        .border(border.clone())
        .set_alignment(alignment.clone());
    let style = format.as_cell_style();

    assert_eq!(style.pattern_fill, Some(fill));
    assert_eq!(style.border, Some(border.clone()));
    assert_eq!(style.align, Some(alignment.clone()));

    let cell_style = CellStyle::new()
        .set_pattern_fill(fill)
        .set_alignment(alignment.clone())
        .border(border.clone());

    assert_eq!(cell_style.pattern_fill, Some(fill));
    assert_eq!(cell_style.border, Some(border));
    assert_eq!(cell_style.align, Some(alignment));
}

/// W1: borders can carry distinct colors per edge instead of only one shared
/// color for every configured border side.
#[cfg(feature = "xlsx")]
#[test]
fn format_facade_writes_individual_border_colors() {
    use rxls::{Color, Format, FormatBorder};
    use std::io::Read;

    fn part(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut file = zip.by_name(name).expect(name);
        let mut xml = String::new();
        file.read_to_string(&mut xml).expect("xml");
        xml
    }

    let mut wb = Workbook::new();
    let s = wb.add_sheet("border");
    let format = Format::new()
        .set_border_top(FormatBorder::Thick)
        .set_border_top_color(Color::rgb(0x11, 0x22, 0x33))
        .set_border_bottom(FormatBorder::Double)
        .set_border_bottom_color(Color::rgb(0x44, 0x55, 0x66))
        .set_border_left(FormatBorder::Thin)
        .set_border_left_color(Color::rgb(0x77, 0x88, 0x99))
        .set_border_right(FormatBorder::Medium)
        .set_border_right_color(Color::rgb(0xAA, 0xBB, 0xCC));
    s.write_with_format(0, 0, "edge", &format);

    let styles = part(&wb.to_xlsx(), "xl/styles.xml");

    assert!(styles.contains(r#"<top style="thick"><color rgb="FF112233"/></top>"#));
    assert!(styles.contains(r#"<bottom style="double"><color rgb="FF445566"/></bottom>"#));
    assert!(styles.contains(r#"<left style="thin"><color rgb="FF778899"/></left>"#));
    assert!(styles.contains(r#"<right style="medium"><color rgb="FFAABBCC"/></right>"#));
}

/// Format is a writer-facing object with an explicit bridge to the lower-level
/// CellStyle model rather than only a type alias.
#[cfg(feature = "xlsx")]
#[test]
fn format_object_bridges_to_cell_style() {
    use rxls::{Cell, CellStyle, Format, FormatAlign};

    let format = Format::new()
        .set_bold()
        .set_align(FormatAlign::Right)
        .set_num_format("0.0");
    let style: CellStyle = format.clone().into_cell_style();

    assert_eq!(style.num_fmt.as_deref(), Some("0.0"));
    assert!(style.font.as_ref().is_some_and(|font| font.bold));

    let mut wb = Workbook::new();
    let s = wb.add_sheet("format");
    s.write_styled(0, 0, "style", format.as_cell_style());
    s.write_with_format(1, 0, "format", &format);

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    assert_eq!(back.sheets[0].cell(0, 0), Some(&Cell::Text("style".into())));
    assert_eq!(
        back.sheets[0].cell(1, 0),
        Some(&Cell::Text("format".into()))
    );
}

/// Rich strings can carry a cell-level Format in addition to per-run fonts.
#[cfg(feature = "xlsx")]
#[test]
fn rich_string_accepts_format_facade() {
    use rxls::{Cell, Font, Format, FormatAlign, TextRun};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("rich");
    let fmt = Format::new()
        .set_bg_color([0xDD, 0xEB, 0xF7])
        .set_align(FormatAlign::Center);
    s.write_rich_with_format(
        0,
        0,
        vec![
            TextRun::new(
                "Hello ",
                Font {
                    bold: true,
                    ..Default::default()
                },
            ),
            TextRun::new(
                "World",
                Font {
                    italic: true,
                    ..Default::default()
                },
            ),
        ],
        &fmt,
    );

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    assert_eq!(
        back.sheets[0].cell(0, 0),
        Some(&Cell::Text("Hello World".into()))
    );
    let runs = back.sheets[0]
        .rich_text_runs(0, 0)
        .expect("rich runs retained");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].text, "Hello ");
    assert!(runs[0].font.bold);
    assert_eq!(runs[1].text, "World");
    assert!(runs[1].font.italic);
    assert!(back.sheets[0]
        .cell_style(0, 0)
        .and_then(|style| style.align.as_ref())
        .is_some_and(|align| align.horizontal == Some(rxls::HAlign::Center)));
}

/// W1: rich-string helpers should accept common run collections directly,
/// matching other writer helpers that do not force caller-side conversions.
#[cfg(feature = "xlsx")]
#[test]
fn rich_string_helpers_accept_iterable_runs() {
    use rxls::{Cell, CellStyle, Font, Format, TextRun};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("rich");

    s.write_rich(
        0,
        0,
        [
            TextRun::new(
                String::from("array "),
                Font {
                    bold: true,
                    ..Default::default()
                },
            ),
            TextRun::new(
                String::from("runs"),
                Font {
                    italic: true,
                    ..Default::default()
                },
            ),
        ],
    );

    let styled_runs = vec![
        TextRun::new("styled ", Font::default()),
        TextRun::new("iter", Font::default()),
    ];
    s.write_rich_styled(1, 0, styled_runs, &CellStyle::new().set_bold());

    s.write_rich_with_format(
        2,
        0,
        std::iter::once(TextRun::new("format iter", Font::default())),
        &Format::new().set_italic(),
    );

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen iterable rich runs workbook");
    assert_eq!(
        back.sheets[0].cell(0, 0),
        Some(&Cell::Text("array runs".into()))
    );
    assert_eq!(
        back.sheets[0].cell(1, 0),
        Some(&Cell::Text("styled iter".into()))
    );
    assert_eq!(
        back.sheets[0].cell(2, 0),
        Some(&Cell::Text("format iter".into()))
    );
}

/// W1: rich-text run fonts should have a small object helper instead of forcing
/// raw `Font` struct literals for common run-level styling.
#[cfg(feature = "xlsx")]
#[test]
fn rich_string_font_helpers_cover_common_run_metadata() {
    use rxls::{Cell, Color, Font, FormatScript, TextRun};
    use std::io::Read;

    fn part(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut file = zip.by_name(name).expect(name);
        let mut xml = String::new();
        file.read_to_string(&mut xml).expect("xml");
        xml
    }

    let mut wb = Workbook::new();
    let s = wb.add_sheet("rich-fonts");
    s.write_rich(
        0,
        0,
        [
            TextRun::new(
                "owned ",
                Font::new()
                    .with_name(String::from("Aptos"))
                    .with_size(14)
                    .with_color(Color::rgb(0x11, 0x22, 0x33))
                    .bold()
                    .underline(),
            ),
            TextRun::new(
                "script",
                Font::new()
                    .italic()
                    .strikethrough()
                    .with_script(FormatScript::Superscript),
            ),
        ],
    );

    let bytes = wb.to_xlsx();
    let sheet_xml = part(&bytes, "xl/worksheets/sheet1.xml");
    assert!(sheet_xml.contains(r#"<rFont val="Aptos"/>"#));
    assert!(sheet_xml.contains("<b/>"));
    assert!(sheet_xml.contains(r#"<color rgb="FF112233"/>"#));
    assert!(sheet_xml.contains(r#"<sz val="14"/>"#));
    assert!(sheet_xml.contains("<u/>"));
    assert!(sheet_xml.contains("<i/>"));
    assert!(sheet_xml.contains("<strike/>"));
    assert!(sheet_xml.contains(r#"<vertAlign val="superscript"/>"#));

    let back = Workbook::open(&bytes).expect("reopen rich font helper workbook");
    assert_eq!(
        back.sheets[0].cell(0, 0),
        Some(&Cell::Text("owned script".into()))
    );
}

#[test]
fn date_serial_helpers_expose_calendar_parts() {
    use rxls::{excel_serial_to_datetime, Cell, ExcelDateTime};

    let dt = excel_serial_to_datetime(45_366.5, false).expect("valid serial");
    assert_eq!(
        dt,
        ExcelDateTime {
            year: 2024,
            month: 3,
            day: 15,
            hour: 12,
            minute: 0,
            second: 0,
        }
    );
    assert_eq!(dt.date_string(), "2024-03-15");
    assert_eq!(dt.time_string(), "12:00:00");
    assert_eq!(dt.to_string(), "2024-03-15 12:00:00");

    assert_eq!(Cell::date(45_366.5).as_datetime(false), Some(dt));
    assert_eq!(
        Cell::Formula {
            formula: "TODAY()".into(),
            cached: Box::new(Cell::date(45_366.0)),
        }
        .as_datetime(false)
        .expect("cached formula date")
        .date_string(),
        "2024-03-15"
    );
    assert_eq!(Cell::Number(45_366.5).get_datetime(), None);
    assert_eq!(Cell::Number(45_366.5).as_datetime(false), Some(dt));

    let mac_epoch = excel_serial_to_datetime(0.0, true).expect("1904 epoch");
    assert_eq!(mac_epoch.date_string(), "1904-01-01");
    assert!(excel_serial_to_datetime(f64::INFINITY, false).is_none());
}

/// R1: Cell exposes calamine-style Data predicates and conversions without
/// replacing rxls' richer `Cell::Formula { cached }` model.
#[test]
fn cell_data_facade_exposes_predicates_and_conversions() {
    use rxls::{Cell, CellErrorType, DataType};

    let text = Cell::Text("42".into());
    assert!(!text.is_empty());
    assert!(text.is_string());
    assert!(!text.is_datetime_iso());
    assert!(!text.is_duration_iso());
    assert_eq!(text.get_string(), Some("42"));
    assert_eq!(text.get_datetime_iso(), None);
    assert_eq!(text.get_duration_iso(), None);
    assert_eq!(text.as_string(), Some("42".into()));
    assert_eq!(text.as_i64(), Some(42));
    assert_eq!(text.as_f64(), Some(42.0));

    let int = Cell::Number(12.0);
    assert!(int.is_int());
    assert!(int.is_float());
    assert_eq!(int.get_int(), Some(12));
    assert_eq!(int.get_float(), Some(12.0));
    assert_eq!(int.as_string(), Some("12".into()));

    let frac = Cell::Number(12.5);
    assert!(!frac.is_int());
    assert!(frac.is_float());
    assert_eq!(frac.as_i64(), Some(12));
    assert_eq!(frac.as_f64(), Some(12.5));

    let b = Cell::Bool(true);
    assert!(b.is_bool());
    assert_eq!(b.get_bool(), Some(true));
    assert_eq!(b.as_i64(), Some(1));
    assert_eq!(b.as_f64(), Some(1.0));

    let date = Cell::date(45_366.0);
    assert!(date.is_datetime());
    assert!(!date.is_datetime_iso());
    assert!(!date.is_duration_iso());
    assert_eq!(date.get_float(), None);
    assert_eq!(date.get_datetime(), Some(45_366.0));
    assert_eq!(date.get_datetime_iso(), None);
    assert_eq!(date.get_duration_iso(), None);
    assert_eq!(date.as_f64(), Some(45_366.0));

    let err = Cell::Error("#N/A".into());
    assert!(err.is_error());
    assert_eq!(err.get_error(), Some("#N/A"));
    assert_eq!(err.get_error_type(), Some(CellErrorType::NA));
    assert_eq!(DataType::get_error_type(&err), Some(CellErrorType::NA));
    assert_eq!(
        CellErrorType::from_excel_error("#DIV/0!"),
        Some(CellErrorType::Div0)
    );
    assert_eq!(
        CellErrorType::from_excel_error("#GETTING_DATA"),
        Some(CellErrorType::GettingData)
    );
    assert_eq!(
        CellErrorType::from_excel_error("#DATA!"),
        Some(CellErrorType::GettingData)
    );
    assert_eq!(CellErrorType::GettingData.as_str(), "#DATA!");
    assert_eq!(CellErrorType::GettingData.to_string(), "#DATA!");
    assert_eq!(CellErrorType::from_excel_error("#ERR!"), None);
    assert_eq!(CellErrorType::Ref.as_str(), "#REF!");
    assert_eq!(err.as_string(), None);

    let formula = Cell::Formula {
        formula: "SUM(A1:A2)".into(),
        cached: Box::new(Cell::Number(3.0)),
    };
    assert!(formula.is_formula());
    assert!(!formula.is_datetime_iso());
    assert!(!formula.is_duration_iso());
    assert_eq!(formula.get_formula(), Some("SUM(A1:A2)"));
    assert_eq!(formula.get_datetime_iso(), None);
    assert_eq!(formula.get_duration_iso(), None);
    assert_eq!(formula.cached_value(), Some(&Cell::Number(3.0)));
    assert_eq!(formula.as_f64(), Some(3.0));

    let formula_date = Cell::Formula {
        formula: "TODAY()".into(),
        cached: Box::new(Cell::date(45_366.0)),
    };
    assert_eq!(formula_date.get_datetime(), Some(45_366.0));

    let formula_error = Cell::Formula {
        formula: "NA()".into(),
        cached: Box::new(Cell::Error("#N/A".into())),
    };
    assert!(formula_error.is_error());
    assert_eq!(formula_error.get_error(), Some("#N/A"));
    assert_eq!(formula_error.get_error_type(), Some(CellErrorType::NA));
}

/// R1: calamine-style Data/DataType names are available for generic consumer
/// code while preserving rxls' `Cell` storage and formula cached-value behavior.
#[test]
fn data_alias_and_datatype_trait_support_generic_cell_code() {
    use rxls::{Cell, Data, DataType};

    fn numeric_summary<T: DataType + ?Sized>(
        value: &T,
    ) -> (bool, bool, Option<i64>, Option<f64>, Option<String>) {
        (
            value.is_empty(),
            value.is_int(),
            value.get_int(),
            value.as_f64(),
            value.as_string(),
        )
    }

    let data: Data = Cell::Formula {
        formula: "A1+A2".into(),
        cached: Box::new(Cell::Number(7.0)),
    };
    assert_eq!(
        numeric_summary(&data),
        (false, true, Some(7), Some(7.0), Some("7".into()))
    );
    assert_eq!(DataType::get_formula(&data), Some("A1+A2"));

    let text = Cell::Text("9".into());
    let erased: &dyn DataType = &text;
    assert!(erased.is_string());
    assert_eq!(erased.get_string(), Some("9"));
    assert_eq!(erased.as_i64(), Some(9));
}

/// R1: the calamine-style `Data` name should also offer value constructors so
/// generic fixtures do not have to switch back to rxls' concrete `Cell`
/// variants for ordinary values.
#[test]
fn data_alias_exposes_value_constructors() {
    use rxls::{Cell, CellErrorType, Data};

    let text = Data::string("north");
    let owned_text = Data::text(String::from("owned"));
    let int = Data::int(7_i16);
    let float = Data::float(12.5_f32);
    let boolean = Data::boolean(true);
    let error = Data::error(CellErrorType::NA);
    let date = Data::date_time(45_366.5);
    let formula = Data::formula("A1+A2", Data::int(9));

    assert_eq!(text, Cell::Text("north".into()));
    assert_eq!(owned_text, Cell::Text("owned".into()));
    assert_eq!(int, Cell::Number(7.0));
    assert!(int.is_int());
    assert_eq!(float, Cell::Number(12.5));
    assert_eq!(boolean, Cell::Bool(true));
    assert_eq!(error, Cell::Error("#N/A".into()));
    assert_eq!(error.get_error_type(), Some(CellErrorType::NA));
    assert_eq!(date, Cell::Date(45_366.5));
    assert_eq!(formula.get_formula(), Some("A1+A2"));
    assert_eq!(formula.cached_value(), Some(&Cell::Number(9.0)));
}

/// R1: calamine-style borrowed DataRef values should work in generic
/// `DataType` code without cloning the underlying `Cell`.
#[test]
fn dataref_alias_supports_borrowed_datatype_code() {
    use rxls::{Cell, DataRef, DataType};

    fn borrowed_summary<T: DataType>(value: T) -> (bool, Option<String>, Option<i64>) {
        (value.is_string(), value.as_string(), value.get_int())
    }

    let text = Cell::Text("plain".into());
    let text_ref: DataRef<'_> = &text;
    assert_eq!(
        borrowed_summary(text_ref),
        (true, Some("plain".into()), None)
    );

    let formula = Cell::Formula {
        formula: "A1+A2".into(),
        cached: Box::new(Cell::Number(7.0)),
    };
    let formula_ref: DataRef<'_> = &formula;
    assert_eq!(formula_ref.get_formula(), Some("A1+A2"));
    assert_eq!(formula_ref.cached_value(), Some(&Cell::Number(7.0)));
    assert_eq!(
        borrowed_summary(formula_ref),
        (false, Some("7".into()), Some(7))
    );
}

/// R1: borrowed DataRef values should convert back into owned Data for
/// calamine-style generic code that switches between borrowed and owned values.
#[test]
fn dataref_converts_to_owned_data() {
    use rxls::{Cell, Data, DataRef};

    let text = Cell::Text("owned copy".into());
    let text_ref: DataRef<'_> = &text;
    let owned: Data = Data::from(text_ref);
    assert_eq!(owned, text);

    let formula = Cell::Formula {
        formula: "A1+A2".into(),
        cached: Box::new(Cell::Number(7.0)),
    };
    let formula_ref: DataRef<'_> = &formula;
    let owned_formula: Data = Data::from(formula_ref);
    assert_eq!(owned_formula, formula);
    assert_eq!(formula_ref, &formula);
}

/// R1: calamine-style `DataType::as_datetime` treats numeric Excel serials as
/// date/time candidates while leaving `get_datetime` reserved for explicit
/// date-typed cells.
#[test]
fn datatype_numeric_serials_can_be_decoded_as_datetimes() {
    use rxls::{Cell, DataType};

    let serial = Cell::Number(45_366.5);
    assert_eq!(serial.get_datetime(), None);
    let decoded = DataType::as_datetime(&serial, false).expect("numeric serial datetime");
    assert_eq!(decoded.date_string(), "2024-03-15");
    assert_eq!(decoded.time_string(), "12:00:00");

    let formula = Cell::Formula {
        formula: "A1".into(),
        cached: Box::new(Cell::Number(45_366.25)),
    };
    let decoded = DataType::as_datetime(&formula, false).expect("cached serial datetime");
    assert_eq!(decoded.date_string(), "2024-03-15");
    assert_eq!(decoded.time_string(), "06:00:00");
}

/// R1: Data/Cell can be compared and displayed directly in generic consumer
/// assertions without destructuring rxls' richer cell model.
#[test]
fn cell_data_facade_supports_primitive_equality_and_display() {
    use rxls::{Cell, Data};

    let text: Data = Cell::Text("road".into());
    assert_eq!(text, "road");
    assert_eq!(text, String::from("road"));
    assert_eq!(text, &String::from("road"));
    assert_eq!("road", text);
    assert_eq!(String::from("road"), text);
    assert_eq!(&String::from("road"), text);
    assert_ne!(text, "bridge");
    assert_eq!(text.to_string(), "road");

    let number = Cell::Number(12.5);
    assert_eq!(number, 12.5);
    assert_eq!(number, 12.5_f32);
    assert_eq!(12.5, number);
    assert_eq!(12.5_f32, number);
    assert_ne!(number, 12_i64);
    assert_eq!(number.to_string(), "12.5");

    let int = Cell::Number(12.0);
    assert_eq!(int, 12);
    assert_eq!(int, 12_i64);
    assert_eq!(int, 12_u32);
    assert_eq!(12, int);
    assert_eq!(12_i64, int);
    assert_eq!(12_u32, int);
    assert_ne!(int, 12_u64 + 1);
    assert_ne!(12_u64 + 1, int);
    assert_eq!(int.to_string(), "12");

    let boolean = Cell::Bool(true);
    assert_eq!(boolean, true);
    assert_eq!(true, boolean);
    assert_eq!(boolean.to_string(), "TRUE");

    let formula = Cell::Formula {
        formula: "A1+A2".into(),
        cached: Box::new(Cell::Number(7.0)),
    };
    assert_eq!(formula, 7.0);
    assert_eq!(formula, 7_i32);
    assert_eq!(formula, 7_i64);
    assert_eq!(formula, 7_u8);
    assert_eq!(7.0, formula);
    assert_eq!(7_i32, formula);
    assert_eq!(7_i64, formula);
    assert_eq!(7_u8, formula);
    assert_eq!(formula.to_string(), "7");

    let date = Cell::date(45_366.0);
    assert_ne!(date, 45_366.0);
    assert_ne!(45_366.0, date);
    assert_eq!(date.to_string(), "45366");
}

/// R1/W1: the same primitive families accepted by the Data equality facade can
/// also feed authored cells and sparse ranges through `Into<Cell>`.
#[cfg(feature = "xlsx")]
#[test]
fn cell_from_primitive_families_feeds_range_and_writer_facades() {
    use rxls::{Cell, Workbook};

    let signed: Cell = (-7_i16).into();
    let unsigned: Cell = 42_u32.into();
    let float: Cell = 12.5_f32.into();

    assert_eq!(signed, Cell::Number(-7.0));
    assert_eq!(unsigned, Cell::Number(42.0));
    assert_eq!(float, Cell::Number(12.5));

    let mut range = rxls::Range::from_sparse(vec![((0, 0), 5_u8)]);
    range.set_value((1, 0), -6_isize);
    range.set_value((2, 0), 7_usize);
    range.set_value((3, 0), 8.5_f32);
    range.set_value((4, 0), 9_u64);

    assert_eq!(range.get_value((0, 0)), Some(&Cell::Number(5.0)));
    assert_eq!(range.get_value((1, 0)), Some(&Cell::Number(-6.0)));
    assert_eq!(range.get_value((2, 0)), Some(&Cell::Number(7.0)));
    assert_eq!(range.get_value((3, 0)), Some(&Cell::Number(8.5)));
    assert_eq!(range.get_value((4, 0)), Some(&Cell::Number(9.0)));

    let mut wb = Workbook::new();
    let sheet = wb.add_sheet("primitives");
    sheet.write(0, 0, 11_u16);
    sheet.write_with_format(1, 0, -12_i8, &rxls::Format::new().set_bold());
    sheet.write(2, 0, 13.5_f32);

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen primitive workbook");
    let sheet = &back.sheets[0];
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Number(11.0)));
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Number(-12.0)));
    assert_eq!(sheet.cell(2, 0), Some(&Cell::Number(13.5)));
}

#[cfg(feature = "chrono")]
#[test]
fn chrono_date_helpers_expose_naive_datetime_date_time_and_duration() {
    use chrono::{Duration, NaiveDate, NaiveTime};
    use rxls::{excel_serial_to_duration, excel_serial_to_naive_datetime, Cell, DataType};

    let expected_date = NaiveDate::from_ymd_opt(2024, 3, 15).unwrap();
    let expected_time = NaiveTime::from_hms_opt(12, 0, 0).unwrap();
    let expected = expected_date.and_hms_opt(12, 0, 0).unwrap();
    let expected_duration = Duration::hours(36);

    assert_eq!(
        excel_serial_to_naive_datetime(45_366.5, false),
        Some(expected)
    );
    assert_eq!(excel_serial_to_duration(1.5), Some(expected_duration));
    assert_eq!(
        Cell::date(45_366.5).as_naive_datetime(false),
        Some(expected)
    );
    assert_eq!(
        Cell::date(45_366.5).as_naive_date(false),
        Some(expected_date)
    );
    assert_eq!(
        Cell::date(45_366.5).as_naive_time(false),
        Some(expected_time)
    );
    assert_eq!(Cell::date(45_366.5).as_date(false), Some(expected_date));
    assert_eq!(Cell::date(45_366.5).as_time(false), Some(expected_time));
    assert_eq!(Cell::date(1.5).as_duration(), Some(expected_duration));
    let formula = Cell::Formula {
        formula: "NOW()".into(),
        cached: Box::new(Cell::date(45_366.5)),
    };
    let formula_duration = Cell::Formula {
        formula: "A1-B1".into(),
        cached: Box::new(Cell::date(1.5)),
    };
    assert_eq!(formula.as_naive_datetime(false), Some(expected));
    assert_eq!(formula.as_naive_date(false), Some(expected_date));
    assert_eq!(formula.as_naive_time(false), Some(expected_time));
    assert_eq!(formula.as_date(false), Some(expected_date));
    assert_eq!(formula.as_time(false), Some(expected_time));
    assert_eq!(
        <Cell as DataType>::as_date(&formula, false),
        Some(expected_date)
    );
    assert_eq!(
        <Cell as DataType>::as_time(&formula, false),
        Some(expected_time)
    );
    assert_eq!(formula_duration.as_duration(), Some(expected_duration));
    assert_eq!(Cell::Number(45_366.5).get_datetime(), None);
    assert_eq!(
        Cell::Number(45_366.5).as_naive_datetime(false),
        Some(expected)
    );
    assert_eq!(
        Cell::Number(45_366.5).as_naive_date(false),
        Some(expected_date)
    );
    assert_eq!(
        Cell::Number(45_366.5).as_naive_time(false),
        Some(expected_time)
    );
    assert_eq!(Cell::Number(45_366.5).as_date(false), Some(expected_date));
    assert_eq!(Cell::Number(45_366.5).as_time(false), Some(expected_time));
    let numeric_duration = Cell::Number(1.5);
    assert_eq!(numeric_duration.as_duration(), Some(expected_duration));
    assert_eq!(
        <Cell as DataType>::as_duration(&numeric_duration),
        Some(expected_duration)
    );
    let formula_numeric_duration = Cell::Formula {
        formula: "A1-B1".into(),
        cached: Box::new(Cell::Number(0.25)),
    };
    assert_eq!(
        formula_numeric_duration.as_duration(),
        Some(Duration::hours(6))
    );

    let mac_epoch = NaiveDate::from_ymd_opt(1904, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    assert_eq!(excel_serial_to_naive_datetime(0.0, true), Some(mac_epoch));
    assert!(excel_serial_to_naive_datetime(f64::INFINITY, false).is_none());
    assert!(excel_serial_to_duration(f64::INFINITY).is_none());
}

#[test]
fn workbook_reports_partial_extraction_signal() {
    let mut wb = Workbook::default();
    assert!(!wb.text_truncated);
    assert!(!wb.is_partial());

    wb.text_truncated = true;
    assert!(wb.is_partial());
}

/// Embedded images should be visible on read and survive a write->read cycle.
#[cfg(feature = "xlsx")]
#[test]
fn xlsx_images_roundtrip_through_public_api() {
    use rxls::{Image, ImageFmt};

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0A, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0,
        0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let mut wb = Workbook::new();
    wb.add_sheet("img").add_image(Image {
        data: PNG_1X1.to_vec(),
        format: ImageFmt::Png,
        from: (1, 2),
        to: Some((4, 5)),
    });

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let images = back.sheets[0].images();

    assert_eq!(images.len(), 1);
    assert_eq!(images[0].format, ImageFmt::Png);
    assert_eq!(images[0].data, PNG_1X1);
    assert_eq!(images[0].from, (1, 2));
    assert_eq!(images[0].to, Some((4, 5)));

    let pictures = back.pictures().expect("workbook pictures");
    assert_eq!(pictures.len(), 1);
    assert_eq!(pictures[0].0, "png");
    assert_eq!(pictures[0].1, PNG_1X1);

    let pictures_with_metadata = back.pictures_with_metadata();
    assert_eq!(pictures_with_metadata.len(), 1);
    assert_eq!(pictures_with_metadata[0].sheet_name, "img");
    assert_eq!(pictures_with_metadata[0].row, 1);
    assert_eq!(pictures_with_metadata[0].col, 2);
    assert_eq!(pictures_with_metadata[0].extension, "png");
    assert_eq!(pictures_with_metadata[0].data, PNG_1X1);
    assert!(Workbook::new().pictures().is_none());
    assert!(Workbook::new().pictures_with_metadata().is_empty());
}

/// R3: worksheet chart metadata parsed from OOXML is exposed through the public
/// sheet metadata surface instead of remaining writer-only.
#[cfg(feature = "xlsx")]
#[test]
fn worksheet_exposes_read_charts_public_api() {
    use rxls::{Chart, ChartKind, Series};

    let mut wb = Workbook::new();
    let s = wb.add_sheet("viz");
    for row in 0..3u32 {
        s.write(row, 0, format!("c{row}"));
        s.write(row, 1, f64::from(row + 1));
    }
    s.add_chart(Chart {
        kind: ChartKind::Line,
        title: Some("Trend".into()),
        series: vec![Series {
            name: Some("Value".into()),
            categories: Some("viz!$A$1:$A$3".into()),
            values: "viz!$B$1:$B$3".into(),
            bubble_sizes: None,
        }],
        legend: true,
        data_labels: true,
        x_axis_title: Some("Category".into()),
        y_axis_title: Some("Amount".into()),
        from: (5, 1),
        to: (14, 7),
    });

    let back = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let charts = back.sheet_by_name("viz").expect("viz sheet").charts();

    assert_eq!(charts.len(), 1);
    assert_eq!(charts[0].kind, ChartKind::Line);
    assert_eq!(charts[0].title.as_deref(), Some("Trend"));
    assert!(charts[0].legend);
    assert!(charts[0].data_labels);
    assert_eq!(charts[0].x_axis_title.as_deref(), Some("Category"));
    assert_eq!(charts[0].y_axis_title.as_deref(), Some("Amount"));
    assert_eq!(charts[0].from, (5, 1));
    assert_eq!(charts[0].to, (14, 7));
    assert_eq!(charts[0].series.len(), 1);
    assert_eq!(charts[0].series[0].name.as_deref(), Some("Value"));
    assert_eq!(
        charts[0].series[0].categories.as_deref(),
        Some("viz!$A$1:$A$3")
    );
    assert_eq!(charts[0].series[0].values, "viz!$B$1:$B$3");
}

/// Optional serde support turns a Range into typed Rust rows.
#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializes_rows_with_and_without_headers() {
    use rxls::RangeDeserializerBuilder;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct BidRow {
        name: String,
        price: f64,
        awarded: bool,
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "name");
        s.write(0, 1, "price");
        s.write(0, 2, "awarded");
        s.write(1, 0, "Road");
        s.write(1, 1, 125.5);
        s.write(1, 2, true);
        s.write(2, 0, "Bridge");
        s.write(2, 1, 88.0);
        s.write(2, 2, false);
    }

    let range = wb.worksheet_range("Data").expect("range");
    let mut row_iter = RangeDeserializerBuilder::new()
        .from_range(&range)
        .expect("deserializer");
    assert_eq!(row_iter.size_hint(), (2, Some(2)));
    assert_eq!(row_iter.len(), 2);
    let rows: Vec<BidRow> = row_iter
        .by_ref()
        .collect::<Result<_, _>>()
        .expect("typed rows");
    assert_eq!(row_iter.size_hint(), (0, Some(0)));
    assert_eq!(row_iter.len(), 0);
    assert_eq!(
        rows,
        vec![
            BidRow {
                name: "Road".into(),
                price: 125.5,
                awarded: true,
            },
            BidRow {
                name: "Bridge".into(),
                price: 88.0,
                awarded: false,
            },
        ]
    );

    let tuples: Vec<(String, f64, bool)> = RangeDeserializerBuilder::new()
        .has_headers(false)
        .from_range(&range)
        .expect("tuple deserializer")
        .skip(1)
        .collect::<Result<_, _>>()
        .expect("tuple rows");
    assert_eq!(
        tuples,
        vec![("Road".into(), 125.5, true), ("Bridge".into(), 88.0, false)]
    );
}

#[cfg(feature = "serde")]
#[test]
fn range_deserializer_can_select_later_header_row() {
    use rxls::RangeDeserializerBuilder;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct BidRow {
        name: String,
        price: f64,
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "Bid export");
        s.write(1, 0, "generated metadata");
        s.write(2, 1, "price");
        s.write(2, 2, "name");
        s.write(3, 1, 125.5);
        s.write(3, 2, "Road");
        s.write(4, 1, 88.0);
        s.write(4, 2, "Bridge");
    }

    let range = wb.worksheet_range("Data").expect("range");
    let rows: Vec<BidRow> = RangeDeserializerBuilder::new()
        .with_header_row(2)
        .from_range(&range)
        .expect("deserializer")
        .collect::<Result<_, _>>()
        .expect("typed rows");

    assert_eq!(
        rows,
        vec![
            BidRow {
                name: "Road".into(),
                price: 125.5,
            },
            BidRow {
                name: "Bridge".into(),
                price: 88.0,
            },
        ]
    );
}

#[cfg(feature = "serde")]
#[test]
fn range_deserializer_header_row_enum_finds_first_non_empty_row() {
    use rxls::{HeaderRow, Range, RangeDeserializerBuilder};
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct BidRow {
        name: String,
        price: f64,
    }

    let mut range = Range::new((0, 0), (3, 1));
    range.set_value((2, 0), "name");
    range.set_value((2, 1), "price");
    range.set_value((3, 0), "Road");
    range.set_value((3, 1), 125.5);

    let default_rows: Vec<BidRow> = RangeDeserializerBuilder::new()
        .from_range(&range)
        .expect("first non-empty header row")
        .collect::<Result<_, _>>()
        .expect("typed rows");
    let explicit_rows: Vec<BidRow> = RangeDeserializerBuilder::new()
        .with_header_row(HeaderRow::Row(2))
        .from_range(&range)
        .expect("explicit header row")
        .collect::<Result<_, _>>()
        .expect("typed rows");
    let first_non_empty_rows: Vec<BidRow> = RangeDeserializerBuilder::new()
        .with_header_row(HeaderRow::FirstNonEmptyRow)
        .from_range(&range)
        .expect("named first non-empty header row")
        .collect::<Result<_, _>>()
        .expect("typed rows");

    let expected = vec![BidRow {
        name: "Road".into(),
        price: 125.5,
    }];
    assert_eq!(default_rows, expected);
    assert_eq!(explicit_rows, expected);
    assert_eq!(first_non_empty_rows, expected);
}

#[test]
fn reader_header_row_clips_worksheet_ranges() {
    use rxls::{Cell, HeaderRow, Reader};

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "export title");
        s.write(1, 0, "generated metadata");
        s.write(2, 0, "name");
        s.write(2, 1, "price");
        s.write(3, 0, "Road");
        s.write(3, 1, 125.5);
    }

    <Workbook as Reader>::with_header_row(&mut wb, HeaderRow::Row(2));
    assert_eq!(wb.header_row(), HeaderRow::Row(2));
    assert_eq!(wb.sheets[0].range().start(), Some((0, 0)));

    let by_name = Reader::worksheet_range(&wb, "Data").expect("range by name");
    assert_eq!(by_name.start(), Some((2, 0)));
    assert_eq!(by_name.end(), Some((3, 1)));
    assert_eq!(by_name.headers(), Some(vec!["name".into(), "price".into()]));
    assert_eq!(by_name.get_value((1, 0)), None);
    assert_eq!(by_name.get_value((3, 1)), Some(&Cell::Number(125.5)));

    let by_index = Reader::worksheet_range_at(&wb, 0).expect("range by index");
    assert_eq!(by_index.start(), Some((2, 0)));

    let worksheets = Reader::worksheets(&wb);
    assert_eq!(worksheets.len(), 1);
    assert_eq!(worksheets[0].0, "Data");
    assert_eq!(worksheets[0].1.start(), Some((2, 0)));

    <Workbook as Reader>::with_header_row(&mut wb, HeaderRow::FirstNonEmptyRow);
    let default_range = Reader::worksheet_range(&wb, "Data").expect("default range");
    assert_eq!(default_range.start(), Some((0, 0)));
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_supports_numeric_primitives_with_bounds() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        qty: i32,
        code: u16,
        ratio: f32,
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "qty");
        s.write(0, 1, "code");
        s.write(0, 2, "ratio");
        s.write(1, 0, 42.0);
        s.write(1, 1, 65_535.0);
        s.write(1, 2, 0.25);
    }

    let rows: Vec<Row> = wb
        .worksheet_range("Data")
        .expect("range")
        .deserialize()
        .expect("deserializer")
        .collect::<Result<_, _>>()
        .expect("numeric primitives");
    assert_eq!(
        rows,
        vec![Row {
            qty: 42,
            code: 65_535,
            ratio: 0.25,
        }]
    );

    let mut fractional = Workbook::new();
    {
        let s = fractional.add_sheet("Data");
        s.write(0, 0, "qty");
        s.write(1, 0, 1.5);
    }
    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    struct Qty {
        qty: i32,
    }
    let fractional_range = fractional.worksheet_range("Data").expect("range");
    let mut fractional_rows = fractional_range.deserialize::<Qty>().expect("deserializer");
    assert!(fractional_rows.next().expect("row").is_err());

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    struct Code {
        code: u16,
    }
    let mut overflow = Workbook::new();
    {
        let s = overflow.add_sheet("Data");
        s.write(0, 0, "code");
        s.write(1, 0, 65_536.0);
    }
    let overflow_range = overflow.worksheet_range("Data").expect("range");
    let mut overflow_rows = overflow_range.deserialize::<Code>().expect("deserializer");
    assert!(overflow_rows.next().expect("row").is_err());
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_supports_shorthand_and_explicit_headers() {
    use rxls::RangeDeserializerBuilder;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct BidRow {
        name: String,
        price: f64,
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "name");
        s.write(0, 1, "price");
        s.write(0, 2, "ignored");
        s.write(1, 0, "Road");
        s.write(1, 1, 125.5);
        s.write(1, 2, "x");
        s.write(2, 0, "Bridge");
        s.write(2, 1, 88.0);
        s.write(2, 2, "y");
    }

    let range = wb.worksheet_range("Data").expect("range");
    let rows: Vec<BidRow> = range
        .deserialize()
        .expect("range shorthand")
        .collect::<Result<_, _>>()
        .expect("typed rows");
    assert_eq!(
        rows,
        vec![
            BidRow {
                name: "Road".into(),
                price: 125.5,
            },
            BidRow {
                name: "Bridge".into(),
                price: 88.0,
            },
        ]
    );

    let selected: Vec<(f64, String)> = RangeDeserializerBuilder::with_headers(&["price", "name"])
        .from_range(&range)
        .expect("selected headers")
        .collect::<Result<_, _>>()
        .expect("selected rows");
    assert_eq!(
        selected,
        vec![(125.5, "Road".into()), (88.0, "Bridge".into())]
    );
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_rejects_missing_explicit_headers() {
    use rxls::RangeDeserializerBuilder;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "name");
        s.write(0, 1, "price");
        s.write(1, 0, "Road");
        s.write(1, 1, 125.5);
    }

    let range = wb.worksheet_range("Data").expect("range");
    let err = RangeDeserializerBuilder::with_headers(&["missing", "price"])
        .from_range::<(String, f64)>(&range)
        .expect_err("missing explicit header should be rejected before rows iterate");

    assert_eq!(err.to_string(), "missing range header: missing");
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_matches_explicit_headers_after_trimming() {
    use rxls::RangeDeserializerBuilder;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, " name ");
        s.write(0, 1, " unit price ");
        s.write(1, 0, "Road");
        s.write(1, 1, 125.5);
    }

    let range = wb.worksheet_range("Data").expect("range");
    let rows: Vec<(f64, String)> =
        RangeDeserializerBuilder::with_headers(&[" unit price", "name "])
            .from_range(&range)
            .expect("trimmed explicit headers")
            .collect::<Result<_, _>>()
            .expect("selected rows");

    assert_eq!(rows, vec![(125.5, "Road".into())]);
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_preserves_source_header_keys_after_trimmed_selection() {
    use std::collections::BTreeMap;

    use rxls::RangeDeserializerBuilder;

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, " unit price ");
        s.write(0, 1, "ignored");
        s.write(1, 0, 125.5);
        s.write(1, 1, "x");
    }

    let range = wb.worksheet_range("Data").expect("range");
    let rows: Vec<BTreeMap<String, f64>> = RangeDeserializerBuilder::with_headers(&["unit price"])
        .from_range(&range)
        .expect("trimmed explicit headers")
        .collect::<Result<_, _>>()
        .expect("selected map rows");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(" unit price "), Some(&125.5));
    assert!(
        !rows[0].contains_key("unit price"),
        "trimmed lookup must not replace the source header key"
    );
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_selects_headers_from_deserialize_struct() {
    use rxls::RangeDeserializerBuilder;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct BidRow {
        name: String,
        #[serde(rename = "unit price")]
        price: f64,
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "ignored");
        s.write(0, 1, "unit price");
        s.write(0, 2, "name");
        s.write(1, 0, "x");
        s.write(1, 1, 125.5);
        s.write(1, 2, "Road");
        s.write(2, 0, "y");
        s.write(2, 1, 88.0);
        s.write(2, 2, "Bridge");
    }

    let range = wb.worksheet_range("Data").expect("range");
    let rows: Vec<BidRow> = RangeDeserializerBuilder::with_deserialize_headers::<BidRow>()
        .from_range(&range)
        .expect("deserialize headers")
        .collect::<Result<_, _>>()
        .expect("typed rows");

    assert_eq!(
        rows,
        vec![
            BidRow {
                name: "Road".into(),
                price: 125.5,
            },
            BidRow {
                name: "Bridge".into(),
                price: 88.0,
            },
        ]
    );
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_headers_ignore_absent_serde_aliases() {
    use rxls::RangeDeserializerBuilder;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct BidRow {
        #[serde(alias = "legacy_name")]
        name: String,
        price: f64,
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "name");
        s.write(0, 1, "price");
        s.write(1, 0, "Road");
        s.write(1, 1, 125.5);
    }

    let range = wb.worksheet_range("Data").expect("range");
    let rows: Vec<BidRow> = RangeDeserializerBuilder::with_deserialize_headers::<BidRow>()
        .from_range(&range)
        .expect("deserialize headers")
        .collect::<Result<_, _>>()
        .expect("typed rows");

    assert_eq!(
        rows,
        vec![BidRow {
            name: "Road".into(),
            price: 125.5,
        }]
    );
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_borrows_text_cells() {
    use rxls::Cell;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct BidRow<'a> {
        name: &'a str,
        note: Option<&'a str>,
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "name");
        s.write(0, 1, "note");
        s.write(1, 0, "Road");
        s.write(1, 1, "urgent");
    }

    let range = wb.worksheet_range("Data").expect("range");
    let rows: Vec<BidRow<'_>> = range
        .deserialize()
        .expect("range shorthand")
        .collect::<Result<_, _>>()
        .expect("borrowed rows");

    assert_eq!(
        rows,
        vec![BidRow {
            name: "Road",
            note: Some("urgent"),
        }]
    );
    let source = match range.get_abs(1, 0).expect("source cell") {
        Cell::Text(s) => s.as_str(),
        other => panic!("expected text cell, got {other:?}"),
    };
    assert!(
        std::ptr::eq(rows[0].name.as_ptr(), source.as_ptr()),
        "deserialized &str should borrow from the cell storage"
    );
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_uses_formatted_text_for_string_fields() {
    use rxls::Format;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        label: String,
        percent: String,
        flag: String,
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "label");
        s.write(0, 1, "percent");
        s.write(0, 2, "flag");
        s.write(1, 0, "completion");
        s.write_number_with_format(1, 1, 0.5, &Format::new().set_num_format("0%"));
        s.write(1, 2, true);
    }

    let reread = Workbook::open(&wb.to_xlsx()).expect("reopen");
    let sheet = reread.sheet_by_name("Data").expect("sheet");
    assert_eq!(sheet.formatted(1, 1), Some("50%"));

    let range = reread.worksheet_range("Data").expect("range");
    assert_eq!(range.formatted((1, 1)), Some("50%"));
    assert_eq!(range.formatted_abs(1, 2), Some("TRUE"));

    let rows: Vec<Row> = range
        .deserialize()
        .expect("range shorthand")
        .collect::<Result<_, _>>()
        .expect("formatted string rows");

    assert_eq!(
        rows,
        vec![Row {
            label: "completion".into(),
            percent: "50%".into(),
            flag: "TRUE".into(),
        }]
    );
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_numeric_helpers_keep_invalid_cells_nonfatal() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        metric: String,
        #[serde(deserialize_with = "rxls::deserialize_as_f64_or_none")]
        measured: Option<f64>,
        #[serde(deserialize_with = "rxls::deserialize_as_i64_or_none")]
        count: Option<i64>,
        #[serde(deserialize_with = "rxls::deserialize_as_f64_or_string")]
        measured_raw: Result<f64, String>,
        #[serde(deserialize_with = "rxls::deserialize_as_i64_or_string")]
        count_raw: Result<i64, String>,
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "metric");
        s.write(0, 1, "measured");
        s.write(0, 2, "count");
        s.write(0, 3, "measured_raw");
        s.write(0, 4, "count_raw");
        s.write(1, 0, "ok");
        s.write(1, 1, 12.5);
        s.write(1, 2, 7.0);
        s.write(1, 3, "3.25");
        s.write(1, 4, "9");
        s.write(2, 0, "bad");
        s.write(2, 1, "N/A");
        s.write(2, 2, "missing");
        s.write(2, 3, "N/A");
        s.write(2, 4, "missing");
        s.write(3, 0, "empty");
    }

    let range = wb.worksheet_range("Data").expect("range");
    let rows: Vec<Row> = range
        .deserialize()
        .expect("range shorthand")
        .collect::<Result<_, _>>()
        .expect("numeric helper rows");

    assert_eq!(
        rows,
        vec![
            Row {
                metric: "ok".into(),
                measured: Some(12.5),
                count: Some(7),
                measured_raw: Ok(3.25),
                count_raw: Ok(9),
            },
            Row {
                metric: "bad".into(),
                measured: None,
                count: None,
                measured_raw: Err("N/A".into()),
                count_raw: Err("missing".into()),
            },
            Row {
                metric: "empty".into(),
                measured: None,
                count: None,
                measured_raw: Err(String::new()),
                count_raw: Err(String::new()),
            },
        ]
    );
}

#[cfg(all(feature = "serde", feature = "chrono"))]
#[test]
fn range_deserializer_duration_helpers_keep_invalid_cells_nonfatal() {
    use chrono::Duration;
    use rxls::Cell;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        metric: String,
        #[serde(deserialize_with = "rxls::deserialize_as_duration_or_none")]
        elapsed: Option<Duration>,
        #[serde(deserialize_with = "rxls::deserialize_as_duration_or_string")]
        elapsed_raw: Result<Duration, String>,
    }

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "metric");
        s.write(0, 1, "elapsed");
        s.write(0, 2, "elapsed_raw");
        s.write(1, 0, "ok");
        s.write(1, 1, 1.5);
        s.write(1, 2, Cell::date(0.25));
        s.write(2, 0, "bad");
        s.write(2, 1, "N/A");
        s.write(2, 2, "missing");
        s.write(3, 0, "empty");
    }

    let range = wb.worksheet_range("Data").expect("range");
    let rows: Vec<Row> = range
        .deserialize()
        .expect("range shorthand")
        .collect::<Result<_, _>>()
        .expect("duration helper rows");

    assert_eq!(
        rows,
        vec![
            Row {
                metric: "ok".into(),
                elapsed: Some(Duration::hours(36)),
                elapsed_raw: Ok(Duration::hours(6)),
            },
            Row {
                metric: "bad".into(),
                elapsed: None,
                elapsed_raw: Err("missing".into()),
            },
            Row {
                metric: "empty".into(),
                elapsed: None,
                elapsed_raw: Err(String::new()),
            },
        ]
    );
}

#[cfg(all(feature = "serde", feature = "chrono"))]
#[test]
fn range_deserializer_date_time_helpers_require_explicit_epoch() {
    use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
    use rxls::Cell;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        metric: String,
        #[serde(deserialize_with = "rxls::deserialize_as_date_1900_or_none")]
        date_1900: Option<NaiveDate>,
        #[serde(deserialize_with = "rxls::deserialize_as_date_1900_or_string")]
        date_1900_raw: Result<NaiveDate, String>,
        #[serde(deserialize_with = "rxls::deserialize_as_time_1900_or_none")]
        time_1900: Option<NaiveTime>,
        #[serde(deserialize_with = "rxls::deserialize_as_time_1900_or_string")]
        time_1900_raw: Result<NaiveTime, String>,
        #[serde(deserialize_with = "rxls::deserialize_as_datetime_1900_or_none")]
        datetime_1900: Option<NaiveDateTime>,
        #[serde(deserialize_with = "rxls::deserialize_as_datetime_1900_or_string")]
        datetime_1900_raw: Result<NaiveDateTime, String>,
        #[serde(deserialize_with = "rxls::deserialize_as_date_1904_or_none")]
        date_1904: Option<NaiveDate>,
        #[serde(deserialize_with = "rxls::deserialize_as_date_1904_or_string")]
        date_1904_raw: Result<NaiveDate, String>,
        #[serde(deserialize_with = "rxls::deserialize_as_time_1904_or_none")]
        time_1904: Option<NaiveTime>,
        #[serde(deserialize_with = "rxls::deserialize_as_time_1904_or_string")]
        time_1904_raw: Result<NaiveTime, String>,
        #[serde(deserialize_with = "rxls::deserialize_as_datetime_1904_or_none")]
        datetime_1904: Option<NaiveDateTime>,
        #[serde(deserialize_with = "rxls::deserialize_as_datetime_1904_or_string")]
        datetime_1904_raw: Result<NaiveDateTime, String>,
    }

    let headers = [
        "metric",
        "date_1900",
        "date_1900_raw",
        "time_1900",
        "time_1900_raw",
        "datetime_1900",
        "datetime_1900_raw",
        "date_1904",
        "date_1904_raw",
        "time_1904",
        "time_1904_raw",
        "datetime_1904",
        "datetime_1904_raw",
    ];

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        for (col, header) in headers.iter().enumerate() {
            s.write(0, col as u16, *header);
            s.write(1, col as u16, Cell::date(1.5));
            s.write(2, col as u16, format!("bad-{header}"));
        }
        s.write(1, 0, "ok");
        s.write(2, 0, "bad");
    }

    let rows: Vec<Row> = wb
        .worksheet_range("Data")
        .expect("range")
        .deserialize()
        .expect("date/time helper rows")
        .collect::<Result<_, _>>()
        .expect("date/time helper values");

    let datetime_1900 = NaiveDate::from_ymd_opt(1900, 1, 1)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    let datetime_1904 = NaiveDate::from_ymd_opt(1904, 1, 2)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();

    assert_eq!(
        rows,
        vec![
            Row {
                metric: "ok".into(),
                date_1900: Some(datetime_1900.date()),
                date_1900_raw: Ok(datetime_1900.date()),
                time_1900: Some(datetime_1900.time()),
                time_1900_raw: Ok(datetime_1900.time()),
                datetime_1900: Some(datetime_1900),
                datetime_1900_raw: Ok(datetime_1900),
                date_1904: Some(datetime_1904.date()),
                date_1904_raw: Ok(datetime_1904.date()),
                time_1904: Some(datetime_1904.time()),
                time_1904_raw: Ok(datetime_1904.time()),
                datetime_1904: Some(datetime_1904),
                datetime_1904_raw: Ok(datetime_1904),
            },
            Row {
                metric: "bad".into(),
                date_1900: None,
                date_1900_raw: Err("bad-date_1900_raw".into()),
                time_1900: None,
                time_1900_raw: Err("bad-time_1900_raw".into()),
                datetime_1900: None,
                datetime_1900_raw: Err("bad-datetime_1900_raw".into()),
                date_1904: None,
                date_1904_raw: Err("bad-date_1904_raw".into()),
                time_1904: None,
                time_1904_raw: Err("bad-time_1904_raw".into()),
                datetime_1904: None,
                datetime_1904_raw: Err("bad-datetime_1904_raw".into()),
            },
        ]
    );
}

#[cfg(feature = "serde")]
#[test]
fn range_deserializer_can_yield_typed_cells() {
    use rxls::{Cell, RangeDeserializerBuilder};

    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("Data");
        s.write(0, 0, "name");
        s.write(0, 1, "amount");
        s.write(0, 2, "due");
        s.write(0, 3, "ok");
        s.write(0, 4, "calc");
        s.write(1, 0, "Road");
        s.write(1, 1, 12.5);
        s.write(1, 2, Cell::date(45_366.0));
        s.write(1, 3, true);
        s.write(
            1,
            4,
            Cell::Formula {
                formula: "SUM(B2:B2)".into(),
                cached: Box::new(Cell::Number(12.5)),
            },
        );
    }

    let range = wb.worksheet_range("Data").expect("range");
    let rows: Vec<(Cell, Cell, Cell, Cell, Cell)> = range
        .deserialize()
        .expect("range shorthand")
        .collect::<Result<_, _>>()
        .expect("typed cell rows");

    assert_eq!(
        rows,
        vec![(
            Cell::Text("Road".into()),
            Cell::Number(12.5),
            Cell::date(45_366.0),
            Cell::Bool(true),
            Cell::Formula {
                formula: "SUM(B2:B2)".into(),
                cached: Box::new(Cell::Number(12.5)),
            },
        )]
    );

    let mut sparse = Workbook::new();
    {
        let s = sparse.add_sheet("Sparse");
        s.write(0, 0, "left");
        s.write(0, 1, "right");
        s.write(1, 0, "only-left");
    }
    let sparse_range = sparse.worksheet_range("Sparse").expect("sparse range");
    let sparse_rows: Vec<Vec<Option<Cell>>> = RangeDeserializerBuilder::new()
        .has_headers(false)
        .from_range(&sparse_range)
        .expect("raw row deserializer")
        .collect::<Result<_, _>>()
        .expect("raw optional cell rows");

    assert_eq!(
        sparse_rows,
        vec![
            vec![
                Some(Cell::Text("left".into())),
                Some(Cell::Text("right".into()))
            ],
            vec![Some(Cell::Text("only-left".into())), None],
        ]
    );
}

#[cfg(all(feature = "xlsx", feature = "serde"))]
#[test]
fn range_deserializer_terminates_at_max_row() {
    use rxls::RangeDeserializerBuilder;

    let mut wb = Workbook::new();
    wb.add_sheet("Max").write(u32::MAX, 0, "x");
    let range = wb.worksheet_range("Max").expect("range");

    let mut rows = RangeDeserializerBuilder::new()
        .has_headers(false)
        .from_range::<Vec<String>>(&range)
        .expect("deserializer");
    assert_eq!(rows.next().expect("row").expect("values"), vec!["x"]);
    assert!(rows.next().is_none());

    let mut header_only = RangeDeserializerBuilder::new()
        .from_range::<Vec<String>>(&range)
        .expect("header-only deserializer");
    assert!(header_only.next().is_none());
}
