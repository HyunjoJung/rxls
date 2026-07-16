use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rxls::{PageSetup, PrintFidelity, PrintPageOrder, Workbook};
use rxls_render::{
    build_print_document, build_print_page, build_scene, prepare_print_document,
    render_print_document_pdf, Fixed, GlyphCluster, GlyphPaint, GlyphRunNode, LimitKind,
    PathCommand, PrintLayoutOverride, PrintLimits, PrintOptions, PrintWarningCode, Rect,
    RenderError, RenderOptions, RenderRange, RenderSelection, Rgb, Scene, SceneNode, WarningCode,
};
use zip::write::SimpleFileOptions;

fn zip_text_parts(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for &(path, body) in parts {
        writer.start_file(path, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn zip_binary_parts(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for &(path, body) in parts {
        writer.start_file(path, options).unwrap();
        writer.write_all(body).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn xlsb_wide_string(value: &str) -> Vec<u8> {
    let units = value.encode_utf16().collect::<Vec<_>>();
    let mut output = (units.len() as u32).to_le_bytes().to_vec();
    for unit in units {
        output.extend_from_slice(&unit.to_le_bytes());
    }
    output
}

fn xlsb_record(record_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    if record_type < 0x80 {
        output.push(record_type as u8);
    } else {
        output.push((record_type & 0x7f) as u8 | 0x80);
        output.push(((record_type >> 7) & 0x7f) as u8);
    }
    let mut size = payload.len();
    loop {
        let mut byte = (size & 0x7f) as u8;
        size >>= 7;
        if size != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if size == 0 {
            break;
        }
    }
    output.extend_from_slice(payload);
    output
}

fn xlsb_page_break(index: u32) -> Vec<u8> {
    let mut output = index.to_le_bytes().to_vec();
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&u32::MAX.to_le_bytes());
    output.extend_from_slice(&1u32.to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output
}

fn synthetic_print_metadata_xlsb() -> Vec<u8> {
    let mut bundle = vec![0_u8; 8];
    bundle.extend_from_slice(&xlsb_wide_string("rId1"));
    bundle.extend_from_slice(&xlsb_wide_string("Print"));
    let workbook = xlsb_record(156, &bundle);

    let mut margins = Vec::new();
    for margin in [0.7_f64, 0.8, 0.9, 1.0, 0.3, 0.4] {
        margins.extend_from_slice(&margin.to_le_bytes());
    }
    let mut page_setup = Vec::new();
    page_setup.extend_from_slice(&9u32.to_le_bytes());
    page_setup.extend_from_slice(&100u32.to_le_bytes());
    page_setup.extend_from_slice(&600u32.to_le_bytes());
    page_setup.extend_from_slice(&600u32.to_le_bytes());
    page_setup.extend_from_slice(&1u32.to_le_bytes());
    page_setup.extend_from_slice(&3i32.to_le_bytes());
    page_setup.extend_from_slice(&0u32.to_le_bytes());
    page_setup.extend_from_slice(&0u32.to_le_bytes());
    page_setup.extend_from_slice(&((1u16 << 0) | (1u16 << 1) | (1u16 << 7)).to_le_bytes());
    page_setup.extend_from_slice(&u32::MAX.to_le_bytes());
    let mut header_footer = 0x000f_u16.to_le_bytes().to_vec();
    for text in [
        "&CODD &P",
        "&RODD-F",
        "&CEVEN &P",
        "&REVEN-F",
        "&CFIRST &P/&N",
        "&RFIRST-F",
    ] {
        header_footer.extend_from_slice(&xlsb_wide_string(text));
    }

    let mut sheet = Vec::new();
    for row in [0_u32, 20] {
        sheet.extend_from_slice(&xlsb_record(0, &row.to_le_bytes()));
        for column in [0_u32, 7] {
            let mut cell = column.to_le_bytes().to_vec();
            cell.extend_from_slice(&0u32.to_le_bytes());
            cell.extend_from_slice(&(f64::from(row) + f64::from(column)).to_le_bytes());
            sheet.extend_from_slice(&xlsb_record(5, &cell));
        }
    }
    sheet.extend_from_slice(&xlsb_record(476, &margins));
    sheet.extend_from_slice(&xlsb_record(477, &0b1111u16.to_le_bytes()));
    sheet.extend_from_slice(&xlsb_record(478, &page_setup));
    sheet.extend_from_slice(&xlsb_record(479, &header_footer));
    sheet.extend_from_slice(&xlsb_record(480, &[]));
    sheet.extend_from_slice(&xlsb_record(392, &[]));
    sheet.extend_from_slice(&xlsb_record(396, &xlsb_page_break(5)));
    sheet.extend_from_slice(&xlsb_record(396, &xlsb_page_break(20)));
    sheet.extend_from_slice(&xlsb_record(393, &[]));
    sheet.extend_from_slice(&xlsb_record(394, &[]));
    sheet.extend_from_slice(&xlsb_record(396, &xlsb_page_break(3)));
    sheet.extend_from_slice(&xlsb_record(396, &xlsb_page_break(7)));
    sheet.extend_from_slice(&xlsb_record(395, &[]));

    let relationships = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
    zip_binary_parts(&[
        ("xl/workbook.bin", &workbook),
        ("xl/_rels/workbook.bin.rels", relationships),
        ("xl/worksheets/sheet1.bin", &sheet),
    ])
}

fn synthetic_print_metadata_xlsx() -> Vec<u8> {
    let workbook = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Print Sheet" sheetId="1" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm.Print_Area" localSheetId="0">'Print Sheet'!$A$1:$B$2,'Print Sheet'!$D$4:$F$9</definedName></definedNames></workbook>"#;
    let relationships = r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;
    let worksheet = r#"<worksheet><cols><col min="1" max="6" width="12" customWidth="1"/></cols><sheetData>
        <row r="1"><c r="A1" t="inlineStr"><is><t>A1</t></is></c><c r="B1" t="inlineStr"><is><t>B1</t></is></c></row>
        <row r="2"><c r="A2" t="inlineStr"><is><t>A2</t></is></c><c r="B2" t="inlineStr"><is><t>B2</t></is></c></row>
        <row r="4"><c r="D4" t="inlineStr"><is><t>D4</t></is></c><c r="F4" t="inlineStr"><is><t>F4</t></is></c></row>
        <row r="5"><c r="D5" t="inlineStr"><is><t>D5</t></is></c><c r="F5" t="inlineStr"><is><t>F5</t></is></c></row>
        <row r="6"><c r="D6" t="inlineStr"><is><t>D6</t></is></c><c r="F6" t="inlineStr"><is><t>F6</t></is></c></row>
        <row r="7"><c r="D7" t="inlineStr"><is><t>D7</t></is></c><c r="F7" t="inlineStr"><is><t>F7</t></is></c></row>
        <row r="8"><c r="D8" t="inlineStr"><is><t>D8</t></is></c><c r="F8" t="inlineStr"><is><t>F8</t></is></c></row>
        <row r="9"><c r="D9" t="inlineStr"><is><t>D9</t></is></c><c r="F9" t="inlineStr"><is><t>F9</t></is></c></row>
        </sheetData><printOptions gridLines="0" headings="1" horizontalCentered="1" verticalCentered="0"/>
        <pageSetup paperSize="1" scale="50" pageOrder="overThenDown" firstPageNumber="3" useFirstPageNumber="1"/>
        <headerFooter differentOddEven="1" differentFirst="1" scaleWithDoc="0" alignWithMargins="1">
          <oddHeader>&amp;CODD &amp;P</oddHeader><oddFooter>&amp;LODD-F</oddFooter>
          <evenHeader>&amp;CEVEN &amp;P</evenHeader><evenFooter>&amp;LEVEN-F</evenFooter>
          <firstHeader>&amp;CFIRST &amp;P/&amp;N</firstHeader><firstFooter>&amp;LFIRST-F</firstFooter>
        </headerFooter>
        <rowBreaks count="2" manualBreakCount="2"><brk id="1" min="0" max="16383" man="1"/><brk id="5" min="0" max="16383" man="1"/></rowBreaks>
        <colBreaks count="2" manualBreakCount="2"><brk id="1" min="0" max="1048575" man="1"/><brk id="4" min="0" max="1048575" man="1"/></colBreaks>
        </worksheet>"#;
    zip_text_parts(&[
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", relationships),
        ("xl/worksheets/sheet1.xml", worksheet),
    ])
}

fn synthetic_lossy_print_metadata_xlsx() -> Vec<u8> {
    zip_text_parts(&[
        (
            "xl/workbook.xml",
            r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Lossy" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm.Print_Area" localSheetId="0">#REF!</definedName></definedNames></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>x</t></is></c></row></sheetData><pageSetup pageOrder="sideways"/><headerFooter differentFirst="maybe"><firstHeader>first</firstHeader></headerFooter><rowBreaks><brk id="bad" man="1"/></rowBreaks></worksheet>"#,
        ),
    ])
}

fn synthetic_print_metadata_ods() -> Vec<u8> {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Print" table:style-name="ta" table:print-ranges="$Print.$A$1:$Print.$B$2 $Print.$D$4:$Print.$F$9">
      <table:table-column table:style-name="ca"/><table:table-column table:number-columns-repeated="3"/><table:table-column table:style-name="cb"/><table:table-column/>
      <table:table-row><table:table-cell office:value-type="string"><text:p>A1</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>B1</text:p></table:table-cell></table:table-row>
      <table:table-row table:style-name="rb"><table:table-cell office:value-type="string"><text:p>A2</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>B2</text:p></table:table-cell></table:table-row>
      <table:table-row/><table:table-row><table:table-cell table:number-columns-repeated="3"/><table:table-cell office:value-type="string"><text:p>D4</text:p></table:table-cell><table:table-cell/><table:table-cell office:value-type="string"><text:p>F4</text:p></table:table-cell></table:table-row>
      <table:table-row/><table:table-row table:style-name="rb"><table:table-cell table:number-columns-repeated="3"/><table:table-cell office:value-type="string"><text:p>D6</text:p></table:table-cell><table:table-cell/><table:table-cell office:value-type="string"><text:p>F6</text:p></table:table-cell></table:table-row>
      <table:table-row/><table:table-row/><table:table-row><table:table-cell table:number-columns-repeated="3"/><table:table-cell office:value-type="string"><text:p>D9</text:p></table:table-cell><table:table-cell/><table:table-cell office:value-type="string"><text:p>F9</text:p></table:table-cell></table:table-row>
      </table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles>
      <style:style style:name="ta" style:family="table" style:master-page-name="mp"/>
      <style:style style:name="rb" style:family="table-row"><style:table-row-properties fo:break-before="page"/></style:style>
      <style:style style:name="ca" style:family="table-column"><style:table-column-properties fo:break-after="page"/></style:style>
      <style:style style:name="cb" style:family="table-column"><style:table-column-properties fo:break-before="page"/></style:style>
      </office:styles><office:automatic-styles><style:page-layout style:name="pm"><style:page-layout-properties style:print="headers" style:table-centering="horizontal" style:print-page-order="ltr"/></style:page-layout></office:automatic-styles>
      <office:master-styles><style:master-page style:name="mp" style:page-layout-name="pm"><style:header><text:p>odd-h</text:p></style:header><style:footer><text:p>odd-f</text:p></style:footer><style:header-left><text:p>even-h</text:p></style:header-left><style:footer-left><text:p>even-f</text:p></style:footer-left><style:header-first><text:p>first-h</text:p></style:header-first><style:footer-first><text:p>first-f</text:p></style:footer-first></style:master-page></office:master-styles></office:document-styles>"#;
    zip_text_parts(&[
        ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
        ("content.xml", content),
        ("styles.xml", styles),
    ])
}

fn scene_text(scene: &Scene) -> Vec<&str> {
    scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Text(node) => Some(node.text.as_str()),
            SceneNode::GlyphRun(node) => Some(node.text.as_str()),
            _ => None,
        })
        .collect()
}

fn paginated_workbook() -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Print");
    for row in 0..12 {
        sheet.set_row_height(row, 90.0);
        for column in 0..6 {
            sheet.set_col_width(column, 36.0);
            sheet.write(row, column, format!("{row}:{column}"));
        }
    }
    sheet.merge(3, 1, 5, 1);
    sheet.set_print_gridlines();
    sheet.set_print_headings();
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 11, 5))
            .with_repeat_rows(0, 0)
            .with_repeat_cols(0, 0)
            .with_header("&L&A&CPage &P of &N")
            .with_footer("&Rdeterministic")
            .with_center_horizontally(true),
    );
    workbook
}

fn append_rectangle(commands: &mut Vec<PathCommand>, x: i64, y: i64, width: i64, height: i64) {
    commands.extend([
        PathCommand::MoveTo {
            x: Fixed::from_pixels(x),
            y: Fixed::from_pixels(y),
        },
        PathCommand::LineTo {
            x: Fixed::from_pixels(x + width),
            y: Fixed::from_pixels(y),
        },
        PathCommand::LineTo {
            x: Fixed::from_pixels(x + width),
            y: Fixed::from_pixels(y + height),
        },
        PathCommand::LineTo {
            x: Fixed::from_pixels(x),
            y: Fixed::from_pixels(y + height),
        },
        PathCommand::Close,
    ]);
}

fn outlined_multiscript_node() -> GlyphRunNode {
    let mut commands = Vec::new();
    append_rectangle(&mut commands, 20, 20, 24, 10);
    append_rectangle(&mut commands, 52, 20, 24, 10);
    append_rectangle(&mut commands, 92, 20, 8, 10);
    append_rectangle(&mut commands, 104, 20, 8, 10);
    GlyphRunNode {
        text: "Latin 한글 אב".to_string(),
        clip_bounds: Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(160),
            height: Fixed::from_pixels(40),
        },
        commands,
        clusters: vec![
            GlyphCluster {
                source_start: 0,
                source_end: 5,
                command_start: 0,
                command_end: 5,
            },
            GlyphCluster {
                source_start: 5,
                source_end: 6,
                command_start: 5,
                command_end: 5,
            },
            GlyphCluster {
                source_start: 6,
                source_end: 12,
                command_start: 5,
                command_end: 10,
            },
            GlyphCluster {
                source_start: 12,
                source_end: 13,
                command_start: 10,
                command_end: 10,
            },
            // Visual RTL order; source ranges intentionally move backwards.
            GlyphCluster {
                source_start: 15,
                source_end: 17,
                command_start: 10,
                command_end: 15,
            },
            GlyphCluster {
                source_start: 13,
                source_end: 15,
                command_start: 15,
                command_end: 20,
            },
        ],
        paints: vec![GlyphPaint {
            command_start: 0,
            command_end: 20,
            color: Rgb::new(20, 40, 80),
        }],
        decorations: Vec::new(),
        color: Rgb::new(20, 40, 80),
        rotation_degrees: 0,
        pivot_x: Fixed::ZERO,
        pivot_y: Fixed::ZERO,
        hyperlink: None,
    }
}

fn poppler_tool_available(tool: &str) -> bool {
    let available = Command::new(tool)
        .arg("-v")
        .output()
        .is_ok_and(|output| output.status.success());
    if std::env::var_os("RXLS_REQUIRE_POPPLER").is_some() {
        assert!(available, "RXLS_REQUIRE_POPPLER requires {tool}");
    }
    available
}

#[test]
fn page_map_is_exact_merge_safe_and_repeat_aware() {
    let workbook = paginated_workbook();
    let options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    let document = build_print_document(&workbook, 0, &options).unwrap();
    assert!(document.pages.len() >= 4, "{:?}", document.report.pages);
    assert_eq!(document.report.pages[0].repeat_rows, Some((0, 0)));
    assert_eq!(document.report.pages[0].repeat_cols, Some((0, 0)));
    assert_eq!(document.report.pages[0].horizontal_index, 0);
    assert_eq!(document.report.pages[0].vertical_index, 0);
    assert_eq!(document.report.pages[1].horizontal_index, 0);
    assert_eq!(document.report.pages[1].vertical_index, 1);
    for adjacent in document
        .report
        .pages
        .iter()
        .filter(|page| page.horizontal_index == 0)
        .collect::<Vec<_>>()
        .windows(2)
    {
        let boundary = adjacent[1].body_range.first_row;
        assert!(!(4..=5).contains(&boundary), "merge split at {boundary}");
    }
    assert_eq!(document.report.to_json(), document.report.to_json());
    assert!(document
        .report
        .to_json()
        .contains("\"scale_permille\":1000"));
}

#[test]
fn prepared_page_map_builds_exactly_the_requested_original_page() {
    let workbook = paginated_workbook();
    let options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    let prepared = prepare_print_document(&workbook, 0, &options).unwrap();
    assert!(prepared.report.pages.len() >= 4);
    assert_eq!(
        prepared.report.pages.len() as u64,
        prepared.report.logical_pages
    );

    let document = build_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(prepared.report, document.report);
    assert_eq!(prepared.limits, document.limits);
    let independently_built = (0..prepared.report.pages.len())
        .map(|page_index| build_print_page(&workbook, &prepared, page_index).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(independently_built, document.pages);
    for (page_index, page) in independently_built.iter().enumerate() {
        assert_eq!(page.map.output_index, page_index);
        assert_eq!(page.map, prepared.report.pages[page_index]);
    }
    assert_eq!(
        build_print_page(&workbook, &prepared, prepared.report.pages.len()),
        Err(RenderError::Backend {
            reason: "print_page_index_out_of_range",
        })
    );

    // A per-page plan remains usable when the complete retained document would
    // exceed its aggregate scene-node budget. This is the memory distinction
    // required by browser page virtualization.
    let maximum_page_nodes = document
        .pages
        .iter()
        .map(|page| page.scene.nodes.len() as u64)
        .max()
        .unwrap();
    assert!(
        document
            .pages
            .iter()
            .map(|page| page.scene.nodes.len() as u64)
            .sum::<u64>()
            > maximum_page_nodes
    );
    let mut one_page_options = options;
    one_page_options.limits.max_total_scene_nodes = maximum_page_nodes;
    let one_page_plan = prepare_print_document(&workbook, 0, &one_page_options).unwrap();
    for page_index in 0..one_page_plan.report.pages.len() {
        build_print_page(&workbook, &one_page_plan, page_index).unwrap();
    }
    assert!(matches!(
        build_print_document(&workbook, 0, &one_page_options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TotalSceneNodes,
            ..
        })
    ));
}

#[test]
fn xlsx_sidecar_drives_multi_area_break_order_headers_and_override_isolation() {
    let workbook = Workbook::open(&synthetic_print_metadata_xlsx()).unwrap();
    let metadata = workbook.sheets[0].print_metadata();
    assert_eq!(metadata.print_areas(), &[(0, 0, 1, 1), (3, 3, 8, 5)]);
    assert_eq!(metadata.manual_row_breaks(), &[1, 5]);
    assert_eq!(metadata.manual_col_breaks(), &[1, 4]);
    assert_eq!(metadata.page_order(), Some(PrintPageOrder::OverThenDown));

    let options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    let document = build_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(
        document,
        build_print_document(&workbook, 0, &options).unwrap()
    );
    assert_eq!(document.report.schema_version, 2);
    assert_eq!(document.report.sources.len(), 2);
    assert_eq!(
        document.report.sources[0].range,
        RenderRange::new(0, 0, 1, 1)
    );
    assert_eq!(
        document.report.sources[1].range,
        RenderRange::new(3, 3, 8, 5)
    );
    assert_eq!(
        document.report.page_order,
        Some(PrintPageOrder::OverThenDown)
    );
    assert_eq!(document.report.manual_row_breaks, [1, 5]);
    assert_eq!(document.report.manual_col_breaks, [1, 4]);
    assert_eq!(document.report.logical_pages, 8);
    assert_eq!(document.report.pages.len(), 8);
    assert!(document
        .report
        .pages
        .iter()
        .all(|page| page.scale_permille == 500));
    assert!(document.pages.iter().all(|page| page
        .scene
        .nodes
        .iter()
        .all(|node| !matches!(node, SceneNode::Line(_)))));
    assert_eq!(
        document
            .report
            .pages
            .iter()
            .map(|page| {
                (
                    page.area_index,
                    page.horizontal_index,
                    page.vertical_index,
                    page.manual_col_break_before,
                    page.manual_row_break_before,
                )
            })
            .collect::<Vec<_>>(),
        [
            (0, 0, 0, false, false),
            (0, 1, 0, true, false),
            (0, 0, 1, false, true),
            (0, 1, 1, true, true),
            (1, 0, 0, false, false),
            (1, 1, 0, true, false),
            (1, 0, 1, false, true),
            (1, 1, 1, true, true),
        ]
    );
    assert!(scene_text(&document.pages[0].scene).contains(&"FIRST 3/8"));
    assert!(scene_text(&document.pages[1].scene).contains(&"EVEN 4"));
    assert!(scene_text(&document.pages[2].scene).contains(&"ODD 5"));
    let first_header = document.pages[0]
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Text(node) if node.text == "FIRST 3/8" => Some(node),
            _ => None,
        })
        .unwrap();
    assert_eq!(first_header.style.size.raw(), 12 * 1_024);
    assert_eq!(
        first_header.bounds.x.raw(),
        document.report.content_rect.x.raw() + document.report.content_rect.width.raw() / 3
    );
    let json = document.report.to_json();
    assert!(json.contains("\"source_reports\":["));
    assert!(json.contains("\"page_order\":\"over_then_down\""));
    assert!(json.contains("\"manual_row_break_before\":true"));

    let overridden = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(overridden.pages.len(), 1);
    assert_eq!(overridden.report.sources.len(), 1);
    assert_eq!(overridden.report.page_order, None);
    assert!(overridden.report.manual_row_breaks.is_empty());
    assert!(overridden.report.manual_col_breaks.is_empty());
    assert_eq!(overridden.report.pages[0].area_index, 0);
    assert!(!scene_text(&overridden.pages[0].scene)
        .iter()
        .any(|text| text.contains("FIRST") || text.contains("EVEN") || text.contains("ODD")));
}

#[test]
fn reader_print_losses_are_mapped_to_stable_render_warnings() {
    let workbook = Workbook::open(&synthetic_lossy_print_metadata_xlsx()).unwrap();
    let document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
    let warning_codes = document
        .report
        .warnings
        .iter()
        .map(|warning| warning.code)
        .collect::<Vec<_>>();
    assert!(warning_codes.contains(&PrintWarningCode::SourceInvalidPageBreak));
    assert!(warning_codes.contains(&PrintWarningCode::SourcePrintReferenceMissing));
    assert!(warning_codes.contains(&PrintWarningCode::SourcePrintPropertyUnsupported));
    assert!(warning_codes.contains(&PrintWarningCode::SourceHeaderFooterMalformed));
    let json = document.report.to_json();
    assert!(json.contains("\"code\":\"source_invalid_page_break\""));
    assert!(json.contains("\"code\":\"source_print_reference_missing\""));
    assert!(json.contains("\"code\":\"source_print_property_unsupported\""));
    assert!(json.contains("\"code\":\"source_header_footer_malformed\""));
}

#[test]
fn ods_sidecar_drives_the_same_multi_area_page_map_without_flattening() {
    let workbook = Workbook::open(&synthetic_print_metadata_ods()).unwrap();
    let metadata = workbook.sheets[0].print_metadata();
    assert_eq!(metadata.print_areas(), &[(0, 0, 1, 1), (3, 3, 8, 5)]);
    assert_eq!(metadata.manual_row_breaks(), &[1, 5]);
    assert_eq!(metadata.manual_col_breaks(), &[1, 4]);
    assert_eq!(metadata.page_order(), Some(PrintPageOrder::OverThenDown));

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            omit_sparse_pages: false,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(document.report.sources.len(), 2);
    assert_eq!(document.report.logical_pages, 8);
    assert_eq!(document.report.pages.len(), 8);
    assert_eq!(
        document
            .report
            .pages
            .iter()
            .map(|page| (page.area_index, page.horizontal_index, page.vertical_index))
            .collect::<Vec<_>>(),
        [
            (0, 0, 0),
            (0, 1, 0),
            (0, 0, 1),
            (0, 1, 1),
            (1, 0, 0),
            (1, 1, 0),
            (1, 0, 1),
            (1, 1, 1),
        ]
    );
    assert!(scene_text(&document.pages[0].scene).contains(&"first-h"));
    assert!(scene_text(&document.pages[1].scene).contains(&"even-h"));
    assert!(scene_text(&document.pages[2].scene).contains(&"odd-h"));
    assert_eq!(document.report.to_json(), document.report.to_json());
}

#[test]
fn xlsb_sidecar_drives_manual_breaks_order_and_header_variants() {
    let workbook = Workbook::open(&synthetic_print_metadata_xlsb()).unwrap();
    let metadata = workbook.sheets[0].print_metadata();
    assert_eq!(metadata.manual_row_breaks(), &[5, 20]);
    assert_eq!(metadata.manual_col_breaks(), &[3, 7]);
    assert_eq!(metadata.page_order(), Some(PrintPageOrder::OverThenDown));

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            omit_sparse_pages: false,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(document.report.sources.len(), 1);
    assert_eq!(document.report.manual_row_breaks, [5, 20]);
    assert_eq!(document.report.manual_col_breaks, [3, 7]);
    assert_eq!(document.report.logical_pages, 9);
    assert_eq!(document.report.pages.len(), 9);
    assert_eq!(
        document
            .report
            .pages
            .iter()
            .map(|page| (page.horizontal_index, page.vertical_index))
            .collect::<Vec<_>>(),
        [
            (0, 0),
            (1, 0),
            (2, 0),
            (0, 1),
            (1, 1),
            (2, 1),
            (0, 2),
            (1, 2),
            (2, 2),
        ]
    );
    assert!(scene_text(&document.pages[0].scene).contains(&"FIRST 3/9"));
    assert!(scene_text(&document.pages[1].scene).contains(&"EVEN 4"));
    assert!(scene_text(&document.pages[2].scene).contains(&"ODD 5"));
}

#[test]
fn xls_reader_without_a_sidecar_uses_page_setup_fallback_deterministically() {
    let workbook =
        Workbook::open(include_bytes!("../../tests/fixtures/xls/reader-basic.xls")).unwrap();
    assert_eq!(
        workbook.sheets[0].print_metadata().fidelity(),
        PrintFidelity::Unavailable
    );
    let options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    let document = build_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(
        document,
        build_print_document(&workbook, 0, &options).unwrap()
    );
    assert_eq!(document.report.sources.len(), 1);
    assert_eq!(
        document.report.page_order,
        Some(PrintPageOrder::DownThenOver)
    );
    assert!(document.report.manual_row_breaks.is_empty());
    assert!(document.report.manual_col_breaks.is_empty());
}

#[test]
fn fit_to_page_is_selected_before_sparse_pages_are_omitted() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Sparse");
    sheet.write(0, 0, "first");
    sheet.write(200, 20, "last");
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 200, 20))
            .with_fit_to_pages(2, 2),
    );
    let document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
    assert!(document.report.logical_pages <= 4);
    assert!(document.report.scale_permille >= 100);
    assert!(document.report.pages.len() <= document.report.logical_pages as usize);
    if document.report.sparse_pages_omitted != 0 {
        assert!(document
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == PrintWarningCode::SparsePagesOmitted));
    }
}

#[test]
fn single_page_override_uses_the_visible_content_scene_and_ignores_page_setup() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Single page");
    for row in 0..20 {
        sheet.set_row_height(row, 50.0);
        for column in 0..10 {
            sheet.set_col_width(column, 30.0);
            sheet.write(row, column, format!("{row}:{column}"));
        }
    }
    sheet.hide_row(18);
    sheet.hide_column(8);
    sheet.set_print_headings();
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 19, 15))
            .with_repeat_rows(0, 1)
            .with_repeat_cols(0, 1)
            .with_paper_size(9)
            .with_landscape()
            .with_margins(1.0, 0.5, 0.75, 0.25, 0.3, 0.3)
            .with_scale(400)
            .with_header("&LHEADER&A&C&P/&N")
            .with_footer("&RFOOTER"),
    );

    let authored = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
    assert!(authored.pages.len() > 1);
    let options = PrintOptions {
        single_page_sheets: true,
        ..PrintOptions::default()
    };
    let whole_scene_with_gridlines = build_scene(&workbook, 0, &options.render).unwrap();
    let mut override_render = options.render.clone();
    override_render.gridlines = false;
    let whole_scene = build_scene(&workbook, 0, &override_render).unwrap();
    let fitted = build_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(
        fitted,
        build_print_document(&workbook, 0, &options).unwrap()
    );

    assert_eq!(fitted.pages.len(), 1);
    assert_eq!(fitted.pages[0].scene, whole_scene.scene);
    assert_ne!(fitted.pages[0].scene, whole_scene_with_gridlines.scene);
    assert_eq!(fitted.report.logical_pages, 1);
    assert_eq!(fitted.report.scale_permille, 1_000);
    assert_eq!(fitted.report.pages[0].scale_permille, 1_000);
    assert_eq!(fitted.report.pages[0].body_range, whole_scene.report.range);
    assert_eq!(fitted.report.pages[0].repeat_rows, None);
    assert_eq!(fitted.report.pages[0].repeat_cols, None);
    assert_eq!(fitted.report.paper.paper_code, 0);
    assert_eq!(fitted.report.paper.width, whole_scene.scene.width);
    assert_eq!(fitted.report.paper.height, whole_scene.scene.height);
    assert_eq!(fitted.report.content_rect.x.raw(), 0);
    assert_eq!(fitted.report.content_rect.y.raw(), 0);
    assert_eq!(fitted.report.content_rect.width, whole_scene.scene.width);
    assert_eq!(fitted.report.content_rect.height, whole_scene.scene.height);
    assert_ne!(fitted.report.paper, authored.report.paper);
    assert_eq!(
        fitted.report.layout_override,
        Some(PrintLayoutOverride::SinglePageSheets)
    );
    assert!(!fitted
        .report
        .source
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::PaginationDeferred));
    assert!(fitted
        .report
        .to_json()
        .contains("\"layout_override\":\"single_page_sheets\""));
    assert!(!authored.report.to_json().contains("layout_override"));

    workbook.sheets[0].set_page_setup(
        PageSetup::new()
            .with_paper_size(1)
            .with_margins(0.1, 2.0, 0.2, 1.5, 0.6, 0.7)
            .with_scale(25)
            .with_header("different header"),
    );
    let differently_authored = build_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(differently_authored, fitted);

    let grid_requested = PrintOptions {
        single_page_sheets: true,
        render: RenderOptions {
            gridlines: true,
            ..RenderOptions::default()
        },
        ..PrintOptions::default()
    };
    let suppressed = build_print_document(&workbook, 0, &grid_requested).unwrap();
    let with_gridlines = build_scene(&workbook, 0, &grid_requested.render).unwrap();
    let mut without_gridlines = grid_requested.render.clone();
    without_gridlines.gridlines = false;
    let expected = build_scene(&workbook, 0, &without_gridlines).unwrap();
    assert_eq!(suppressed.pages[0].scene, expected.scene);
    assert_ne!(suppressed.pages[0].scene, with_gridlines.scene);
}

#[test]
fn paper_orientation_margins_scale_and_centering_are_fixed_point() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Geometry");
    sheet.write(0, 0, "x");
    sheet.set_print_gridlines();
    sheet.set_page_setup(
        PageSetup::new()
            .with_paper_size(9)
            .with_landscape()
            .with_margins(1.0, 0.5, 0.75, 0.25, 0.3, 0.3)
            .with_scale(125)
            .with_center_horizontally(true)
            .with_center_vertically(true),
    );
    let document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
    assert!(document.report.paper.width > document.report.paper.height);
    assert_eq!(document.report.paper.paper_code, 9);
    assert_eq!(document.report.content_rect.x.raw(), 96 * 1_024);
    assert_eq!(document.report.content_rect.y.raw(), 72 * 1_024);
    assert_eq!(document.report.scale_permille, 1_250);
    let first_cell = document.pages[0]
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            rxls_render::SceneNode::Rect(node) => Some(node.rect),
            _ => None,
        })
        .unwrap();
    assert!(first_cell.x > document.report.content_rect.x);
    assert!(first_cell.y > document.report.content_rect.y);
}

#[test]
fn deterministic_pdf_reopens_has_exact_page_count_and_extractable_text() {
    let mut document = build_print_document(
        &paginated_workbook(),
        0,
        &PrintOptions {
            omit_sparse_pages: false,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    document.pages[0]
        .scene
        .nodes
        .push(SceneNode::GlyphRun(outlined_multiscript_node()));
    let pdf = render_print_document_pdf(&document).unwrap();
    assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
    assert!(pdf.windows(10).any(|bytes| bytes == b"/CreationD"));
    assert!(pdf.windows(8).any(|bytes| bytes == b"/ActualT"));
    let source = String::from_utf8_lossy(&pdf);
    assert!(source.contains("/Subtype /Type3"));
    assert!(source.contains("/Name /RXLSRF+OutlinedSubset0000"));
    assert!(source.contains("/ToUnicode"));
    assert!(!source.contains("/Widths [0]"));

    let directory = unique_temp_dir("pdf");
    fs::create_dir(&directory).unwrap();
    let path = directory.join("print.pdf");
    fs::write(&path, &pdf).unwrap();
    if poppler_tool_available("pdfinfo") {
        let output = Command::new("pdfinfo").arg(&path).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let pages = stdout
            .lines()
            .find_map(|line| line.strip_prefix("Pages:"))
            .map(str::trim)
            .unwrap();
        assert_eq!(pages, document.pages.len().to_string());
    }
    if poppler_tool_available("pdffonts") {
        let output = Command::new("pdffonts").arg(&path).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let subset = stdout
            .lines()
            .find(|line| line.starts_with("RXLSRF+OutlinedSubset0000"))
            .unwrap_or_else(|| panic!("embedded subset absent: {stdout}"));
        assert!(subset.contains("Type 3"), "{subset}");
        assert!(
            subset
                .split_whitespace()
                .filter(|part| *part == "yes")
                .count()
                >= 3
        );
    }
    if poppler_tool_available("pdftotext") {
        let text_path = directory.join("print.txt");
        let output = Command::new("pdftotext")
            .arg(&path)
            .arg(&text_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = fs::read_to_string(text_path).unwrap();
        assert!(text.contains("0:0"), "{text:?}");
        assert!(text.contains("Print"), "{text:?}");
        assert!(text.contains("Latin"), "{text:?}");
        assert!(text.contains("한글"), "{text:?}");
        let bidi_stripped = text
            .chars()
            .filter(|character| !matches!(*character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
            .collect::<String>();
        assert!(
            bidi_stripped.contains("אב") || bidi_stripped.contains("בא"),
            "{text:?}"
        );
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn pdf_links_are_allowlisted_annotations_and_unsafe_links_remain_absent() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Links");
    sheet.write_url(0, 0, "https://example.com/safe", "safe");
    sheet.write_url(1, 0, "javascript:alert(1)", "unsafe");
    let document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
    let pdf = render_print_document_pdf(&document).unwrap();
    let text = String::from_utf8_lossy(&pdf);
    assert!(text.contains("/Subtype /Link"));
    assert!(text.contains("68747470733A2F2F6578616D706C652E636F6D2F73616665"));
    assert!(!text.contains("javascript"));
}

#[test]
fn print_limits_and_invalid_ranges_are_typed() {
    let workbook = paginated_workbook();
    let options = PrintOptions {
        omit_sparse_pages: false,
        limits: PrintLimits {
            max_logical_pages: 1,
            ..PrintLimits::default()
        },
        ..PrintOptions::default()
    };
    assert!(matches!(
        build_print_document(&workbook, 0, &options),
        Err(RenderError::LimitExceeded { .. })
    ));

    let options = PrintOptions {
        render: RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(10, 10, 1, 1)),
            ..RenderOptions::default()
        },
        ..PrintOptions::default()
    };
    assert!(matches!(
        build_print_document(&workbook, 0, &options),
        Err(RenderError::InvalidRange { .. })
    ));

    let options = PrintOptions {
        single_page_sheets: true,
        limits: PrintLimits {
            max_pages: 0,
            ..PrintLimits::default()
        },
        ..PrintOptions::default()
    };
    assert_eq!(
        build_print_document(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Pages,
            limit: 0,
            actual: 1,
        })
    );

    let options = PrintOptions {
        single_page_sheets: true,
        limits: PrintLimits {
            max_backend_commands: 0,
            ..PrintLimits::default()
        },
        ..PrintOptions::default()
    };
    assert!(matches!(
        build_print_document(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::BackendCommands,
            limit: 0,
            ..
        })
    ));
}

#[test]
fn cli_adds_print_artifacts_without_changing_default_svg_artifact() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest_dir.join("../tests/fixtures/xls/reader-basic.xls");
    let first = unique_temp_dir("cli-first");
    let second = unique_temp_dir("cli-second");
    for output_dir in [&first, &second] {
        let output = Command::new(env!("CARGO_BIN_EXE_rxls-render"))
            .arg("bundle")
            .arg(&fixture)
            .arg("--output-dir")
            .arg(output_dir)
            .arg("--print-backends")
            .arg("svg,pdf")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output_dir.join("sheet-0000.svg").is_file());
        assert!(output_dir.join("sheet-0000.pdf").is_file());
        assert!(output_dir.join("sheet-0000-pages.json").is_file());
        assert!(output_dir.join("sheet-0000-pages/page-0001.svg").is_file());
    }
    assert_eq!(
        fs::read(first.join("render-manifest.json")).unwrap(),
        fs::read(second.join("render-manifest.json")).unwrap()
    );
    let manifest = fs::read_to_string(first.join("render-manifest.json")).unwrap();
    assert!(manifest.contains("\"schema\":\"rxls.render.bundle.v1\""));
    assert!(manifest.contains("\"print\":{\"schema\":\"rxls.render.print-bundle.v1\""));
    assert!(!manifest.contains("single_page_sheets"));
    fs::remove_dir_all(first).unwrap();
    fs::remove_dir_all(second).unwrap();
}

#[test]
fn cli_single_page_override_is_exact_and_recorded() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest_dir.join("../tests/fixtures/xls/reader-basic.xls");
    let first = unique_temp_dir("single-page-first");
    let second = unique_temp_dir("single-page-second");
    for output_dir in [&first, &second] {
        let output = Command::new(env!("CARGO_BIN_EXE_rxls-render"))
            .arg("bundle")
            .arg(&fixture)
            .arg("--output-dir")
            .arg(output_dir)
            .arg("--single-page-sheets")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        fs::read(first.join("render-manifest.json")).unwrap(),
        fs::read(second.join("render-manifest.json")).unwrap()
    );

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(first.join("render-manifest.json")).unwrap()).unwrap();
    let sheets = manifest["sheets"].as_array().unwrap();
    assert_eq!(sheets.len(), 2);
    assert_eq!(sheets[0]["visibility"], "visible");
    assert_eq!(sheets[1]["visibility"], "hidden");
    for (index, sheet) in sheets.iter().enumerate() {
        assert_eq!(sheet["print"]["layout_override"], "single_page_sheets");
        assert_eq!(sheet["print"]["page_count"], 1);
        assert_eq!(
            sheet["scene"]["sha256"],
            sheet["print"]["page_scenes"][0]["sha256"]
        );
        let report: serde_json::Value = serde_json::from_slice(
            &fs::read(first.join(format!("sheet-{index:04}-pages.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(report["layout_override"], "single_page_sheets");
        assert_eq!(report["logical_pages"], 1);
        assert_eq!(report["scale_permille"], 1_000);
        assert_eq!(report["paper"]["code"], 0);
        assert_eq!(report["paper"]["width_raw"], sheet["canvas"]["width_raw"]);
        assert_eq!(report["paper"]["height_raw"], sheet["canvas"]["height_raw"]);
        assert_eq!(report["content_rect"]["x_raw"], 0);
        assert_eq!(report["content_rect"]["y_raw"], 0);
        assert_eq!(
            fs::read(first.join(format!("sheet-{index:04}.svg"))).unwrap(),
            fs::read(first.join(format!("sheet-{index:04}-pages/page-0001.svg"))).unwrap()
        );
        assert!(!first
            .join(format!("sheet-{index:04}-pages/page-0002.svg"))
            .exists());
    }
    fs::remove_dir_all(first).unwrap();
    fs::remove_dir_all(second).unwrap();
}

#[test]
fn cli_rolls_back_all_artifacts_when_a_late_png_backend_fails() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest_dir.join("../tests/fixtures/xls/reader-basic.xls");
    let output_dir = unique_temp_dir("png-rollback");
    let parent = output_dir.parent().unwrap().to_path_buf();
    let output = Command::new(env!("CARGO_BIN_EXE_rxls-render"))
        .arg("bundle")
        .arg(fixture)
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--print-backends")
        .arg("svg,png")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("png_requires_outlined_text"));
    assert!(!output_dir.exists());
    let stage_prefix = format!(
        ".{}.rxls-render-stage-",
        output_dir.file_name().unwrap().to_string_lossy()
    );
    assert!(parent
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .starts_with(&stage_prefix)));
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rxls-print-test-{}-{label}-{nonce}",
        std::process::id()
    ))
}
