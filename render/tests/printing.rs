use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rxls::{
    CfRule, Chart, ChartKind, Color, CondFormat, Format, Image, ImageFmt, PageSetup, PrintFidelity,
    PrintPageOrder, Series, Sparkline, Table, Workbook,
};
use rxls_render::{
    build_print_document, build_print_page, build_scene, build_sheet_print_page,
    prepare_print_document, prepare_sheet_print_document, render_print_document_pdf, Fixed,
    GlyphCluster, GlyphPaint, GlyphRunNode, LimitKind, PathCommand, PrintLayoutOverride,
    PrintLimits, PrintOptions, PrintWarningCode, Rect, RenderError, RenderLimits, RenderOptions,
    RenderRange, RenderSelection, Rgb, Scene, SceneNode, WarningCode,
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

fn source_dimension_xlsx(dimension: &str, body: &str) -> Workbook {
    let worksheet = format!(
        r#"<worksheet>
          <dimension ref="{dimension}"/>
          {body}
        </worksheet>"#
    );
    Workbook::open(&zip_text_parts(&[
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Declared" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/worksheets/sheet1.xml", &worksheet),
    ]))
    .unwrap()
}

fn declared_blank_extent_xlsx(hidden_tail: bool) -> Workbook {
    let hidden = if hidden_tail { r#" hidden="1""# } else { "" };
    let body = format!(
        r#"
          <sheetFormatPr defaultRowHeight="15"/>
          <cols><col min="4" max="4" width="20" customWidth="1"{hidden}/></cols>
          <sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>anchor</t></is></c></row>
            <row r="6" ht="30" customHeight="1"{hidden}/>
          </sheetData>
          <mergeCells count="1"><mergeCell ref="B2:C3"/></mergeCells>
        "#
    );
    source_dimension_xlsx("A1:D6", &body)
}

fn blank_table_style_workbook(fill: &str) -> Workbook {
    let styles = format!(
        r#"<styleSheet><dxfs count="1"><dxf><fill><patternFill patternType="solid"><fgColor rgb="{fill}"/></patternFill></fill></dxf></dxfs><tableStyles count="1"><tableStyle name="CustomBlank" count="1"><tableStyleElement type="wholeTable" dxfId="0"/></tableStyle></tableStyles></styleSheet>"#
    );
    Workbook::open(&zip_text_parts(&[
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Identity" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm.Print_Area" localSheetId="0">'Identity'!$A$1:$A$2</definedName></definedNames></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/styles.xml", &styles),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData/><pageSetup paperSize="1" scale="100"/><tableParts count="1"><tablePart r:id="rIdTable"/></tableParts></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdTable" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
        ),
        (
            "xl/tables/table1.xml",
            r#"<table id="1" name="T" displayName="T" ref="A1:A2" headerRowCount="0" totalsRowCount="0"><tableColumns count="1"><tableColumn id="1" name="C"/></tableColumns><tableStyleInfo name="CustomBlank" showFirstColumn="0" showLastColumn="0" showRowStripes="0" showColumnStripes="0"/></table>"#,
        ),
    ]))
    .unwrap()
}

fn blank_table_style_with_offrange_overlays(
    fill: &str,
    overlay_font: &str,
    overlay_count: usize,
) -> Workbook {
    let styles = format!(
        r#"<styleSheet><fonts count="2"><font/><font>{overlay_font}</font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="1"/></cellXfs><dxfs count="1"><dxf><fill><patternFill patternType="solid"><fgColor rgb="{fill}"/></patternFill></fill></dxf></dxfs><tableStyles count="1"><tableStyle name="CustomBlank" count="1"><tableStyleElement type="wholeTable" dxfId="0"/></tableStyle></tableStyles></styleSheet>"#
    );
    let mut rows = String::new();
    for index in 0..overlay_count {
        let row = index + 1_001;
        std::fmt::Write::write_fmt(
            &mut rows,
            format_args!(r#"<row r="{row}"><c r="Z{row}" s="1"><v>{index}</v></c></row>"#),
        )
        .expect("writing XML into a String is infallible");
    }
    let worksheet = format!(
        r#"<worksheet><sheetData>{rows}</sheetData><pageSetup paperSize="1" scale="100"/><tableParts count="1"><tablePart r:id="rIdTable"/></tableParts></worksheet>"#
    );
    Workbook::open(&zip_text_parts(&[
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Identity" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm.Print_Area" localSheetId="0">'Identity'!$A$1:$A$2</definedName></definedNames></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/styles.xml", &styles),
        ("xl/worksheets/sheet1.xml", &worksheet),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdTable" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
        ),
        (
            "xl/tables/table1.xml",
            r#"<table id="1" name="T" displayName="T" ref="A1:A2" headerRowCount="0" totalsRowCount="0"><tableColumns count="1"><tableColumn id="1" name="C"/></tableColumns><tableStyleInfo name="CustomBlank" showFirstColumn="0" showLastColumn="0" showRowStripes="0" showColumnStripes="0"/></table>"#,
        ),
    ]))
    .unwrap()
}

fn scene_fills(nodes: &[SceneNode], output: &mut Vec<Rgb>) {
    for node in nodes {
        match node {
            SceneNode::ClipGroup(group) => scene_fills(&group.nodes, output),
            SceneNode::Rect(rect) => output.extend(rect.fill),
            _ => {}
        }
    }
}

fn one_pixel_png(rgba: [u8; 4]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&rgba).unwrap();
    }
    output
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

fn synthetic_fit_breaks_xlsx(
    print_area: &str,
    fit_to_page: Option<bool>,
    page_setup: &str,
) -> Vec<u8> {
    let workbook = format!(
        r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Fit Breaks" sheetId="1" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm.Print_Area" localSheetId="0">'Fit Breaks'!{print_area}</definedName></definedNames></workbook>"#
    );
    let relationships = r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;
    let columns = ["A", "B", "C", "D", "E", "F", "G", "H"];
    let mut rows = String::new();
    for row in 1..=20 {
        std::fmt::Write::write_fmt(
            &mut rows,
            format_args!(r#"<row r="{row}" ht="60" customHeight="1">"#),
        )
        .expect("writing XML into a String is infallible");
        for column in columns {
            std::fmt::Write::write_fmt(
                &mut rows,
                format_args!(
                    r#"<c r="{column}{row}" t="inlineStr"><is><t>{column}{row}</t></is></c>"#
                ),
            )
            .expect("writing XML into a String is infallible");
        }
        rows.push_str("</row>");
    }
    let sheet_pr = fit_to_page.map_or_else(String::new, |enabled| {
        format!(
            r#"<sheetPr><pageSetUpPr fitToPage="{}"/></sheetPr>"#,
            u8::from(enabled)
        )
    });
    let worksheet = format!(
        r#"<worksheet>{sheet_pr}<cols><col min="1" max="8" width="40" customWidth="1"/></cols><sheetData>{rows}</sheetData><pageSetup paperSize="1" {page_setup}/><rowBreaks count="1" manualBreakCount="1"><brk id="1" min="0" max="16383" man="1"/></rowBreaks><colBreaks count="1" manualBreakCount="1"><brk id="1" min="0" max="1048575" man="1"/></colBreaks></worksheet>"#
    );
    zip_text_parts(&[
        ("xl/workbook.xml", &workbook),
        ("xl/_rels/workbook.xml.rels", relationships),
        ("xl/worksheets/sheet1.xml", &worksheet),
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

fn retained_scene_nodes(nodes: &[SceneNode]) -> u64 {
    nodes
        .iter()
        .map(|node| match node {
            SceneNode::ClipGroup(group) => 1 + retained_scene_nodes(&group.nodes),
            _ => 1,
        })
        .sum()
}

fn first_cell_bounds(nodes: &[SceneNode]) -> Option<Rect> {
    nodes.iter().find_map(|node| match node {
        SceneNode::ClipGroup(group) => first_cell_bounds(&group.nodes),
        SceneNode::Rect(node) => Some(node.rect),
        SceneNode::Text(node) => Some(node.bounds),
        SceneNode::GlyphRun(node) => Some(node.clip_bounds),
        _ => None,
    })
}

fn scene_has_fill(nodes: &[SceneNode], color: Rgb) -> bool {
    nodes.iter().any(|node| match node {
        SceneNode::ClipGroup(group) => scene_has_fill(&group.nodes, color),
        SceneNode::Rect(node) => node.fill == Some(color),
        _ => false,
    })
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
        cluster_metrics: Vec::new(),
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
fn distant_titles_charge_only_the_disjoint_measurement_union() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Distant titles");
    sheet.write(0, 0, "corner");
    sheet.write(10_000, 100, "body");
    for row in 20_000..21_024 {
        sheet.write(row, 200, "unrelated");
    }
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((10_000, 100, 10_000, 100))
            .with_repeat_rows(0, 0)
            .with_repeat_cols(0, 0),
    );
    let mut options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    options.render.limits.max_rows = 2;
    options.render.limits.max_columns = 2;
    options.render.limits.max_cells = 4;
    let prepared = prepare_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(prepared.report.pages.len(), 1);
    assert_eq!(prepared.report.pages[0].repeat_rows, Some((0, 0)));
    assert_eq!(prepared.report.pages[0].repeat_cols, Some((0, 0)));

    options.render.limits.max_cells = 3;
    assert_eq!(
        prepare_print_document(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Cells,
            limit: 3,
            actual: 4,
        })
    );
}

#[test]
fn prepared_print_pages_fail_closed_after_selected_source_geometry_changes() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Prepared identity");
    sheet.write(0, 0, "selected");
    sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 0, 0)));
    let options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    let prepared = prepare_print_document(&workbook, 0, &options).unwrap();
    assert!(build_print_page(&workbook, &prepared, 0).is_ok());

    // Unrelated cell content is outside the prepared source union and cannot
    // affect its geometry.
    workbook.sheets[0].write(50, 50, "outside");
    assert!(build_print_page(&workbook, &prepared, 0).is_ok());

    workbook.sheets[0].set_col_width(0, 42.0);
    assert_eq!(
        build_print_page(&workbook, &prepared, 0),
        Err(RenderError::Backend {
            reason: "prepared_print_source_changed",
        })
    );

    let mut used_workbook = Workbook::new();
    used_workbook.add_sheet("Used identity").write(0, 0, "A1");
    let used_options = PrintOptions {
        single_page_sheets: true,
        ..PrintOptions::default()
    };
    let used_prepared = prepare_print_document(&used_workbook, 0, &used_options).unwrap();
    used_workbook.sheets[0].write(50, 0, "expands-used-range");
    assert_eq!(
        build_print_page(&used_workbook, &used_prepared, 0),
        Err(RenderError::Backend {
            reason: "prepared_print_source_changed",
        })
    );

    fn image_workbook(image_data: Vec<u8>) -> Workbook {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Image identity");
        sheet.write(0, 0, "image");
        sheet.add_image(Image::new(image_data, ImageFmt::Png, (0, 0)));
        sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 0, 0)));
        workbook
    }
    let first_image = one_pixel_png([1, 2, 3, 255]);
    let second_image = one_pixel_png([9, 8, 7, 255]);
    assert_eq!(first_image.len(), second_image.len());
    assert_ne!(first_image, second_image);
    let image_source = image_workbook(first_image);
    let image_prepared = prepare_print_document(&image_source, 0, &options).unwrap();
    let replacement = image_workbook(second_image);
    assert_eq!(
        build_print_page(&replacement, &image_prepared, 0),
        Err(RenderError::Backend {
            reason: "prepared_print_source_changed",
        })
    );

    let mut styled_table = Workbook::new();
    let sheet = styled_table.add_sheet("Table identity");
    sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 0, 0)));
    let table_prepared = prepare_print_document(&styled_table, 0, &options).unwrap();
    styled_table.sheets[0].add_table(Table::new((0, 0, 0, 0), "IdentityTable", ["Header"]));
    styled_table.sheets[0]
        .set_table_header_format("IdentityTable", &Format::new().fill(Color::rgb(12, 34, 56)));
    assert_eq!(
        build_print_page(&styled_table, &table_prepared, 0),
        Err(RenderError::Backend {
            reason: "prepared_print_source_changed",
        })
    );

    for formula in ["$A$2>0", "$A2>0"] {
        let mut conditional = Workbook::new();
        let sheet = conditional.add_sheet("Conditional identity");
        sheet.write(1, 0, 0.0);
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 0, 0),
            CfRule::expression(formula, Color::rgb(80, 90, 100)),
        ));
        sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 0, 0)));
        let conditional_prepared =
            prepare_print_document(&conditional, 0, &PrintOptions::default()).unwrap();
        conditional.sheets[0].write(1, 0, 1.0);
        assert_eq!(
            build_print_page(&conditional, &conditional_prepared, 0),
            Err(RenderError::Backend {
                reason: "prepared_print_source_changed",
            })
        );
    }

    for source_column in 0..=2 {
        let mut chart_source = Workbook::new();
        let sheet = chart_source.add_sheet("Chart identity");
        for row in 100..=102 {
            sheet.write(row, 0, (row - 99) as f64);
            sheet.write(row, 1, (row - 98) as f64);
            sheet.write(row, 2, (row - 97) as f64);
        }
        sheet.add_chart(
            Chart::new(ChartKind::Bubble, (0, 0), (5, 3)).add_series(
                Series::new("$A$101:$A$103")
                    .with_categories("$B$101:$B$103")
                    .with_bubble_sizes("$C$101:$C$103"),
            ),
        );
        sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 5, 3)));
        let chart_prepared =
            prepare_print_document(&chart_source, 0, &PrintOptions::default()).unwrap();
        chart_source.sheets[0].write(100, source_column, 99.0);
        assert_eq!(
            build_print_page(&chart_source, &chart_prepared, 0),
            Err(RenderError::Backend {
                reason: "prepared_print_source_changed",
            })
        );
    }

    let mut sparkline_source = Workbook::new();
    let sheet = sparkline_source.add_sheet("Sparkline identity");
    for row in 100..=102 {
        sheet.write(row, 0, (row - 99) as f64);
    }
    sheet.add_sparkline(Sparkline::new((0, 0), "$A$101:$A$103"));
    sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 0, 0)));
    let sparkline_prepared =
        prepare_print_document(&sparkline_source, 0, &PrintOptions::default()).unwrap();
    sparkline_source.sheets[0].write(100, 0, 99.0);
    assert_eq!(
        build_print_page(&sparkline_source, &sparkline_prepared, 0),
        Err(RenderError::Backend {
            reason: "prepared_print_source_changed",
        })
    );
}

#[test]
fn private_blank_table_style_sidecar_is_part_of_prepared_print_identity() {
    let red = blank_table_style_workbook("FFFF0000");
    let identical_red = blank_table_style_workbook("FFFF0000");
    let blue = blank_table_style_workbook("FF0000FF");
    assert!(red.sheets[0].display_cells().next().is_none());
    assert_eq!(
        red.sheets[0].resolved_cell_style(0, 0),
        identical_red.sheets[0].resolved_cell_style(0, 0)
    );
    assert_ne!(
        red.sheets[0].resolved_cell_style(0, 0),
        blue.sheets[0].resolved_cell_style(0, 0)
    );
    let options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };

    let prepared = prepare_print_document(&red, 0, &options).unwrap();
    assert!(build_print_page(&red, &prepared, 0).is_ok());
    assert!(
        build_print_page(&identical_red, &prepared, 0).is_ok(),
        "independently parsed but structurally identical sidecars must match"
    );
    assert_eq!(
        build_print_page(&blue, &prepared, 0),
        Err(RenderError::Backend {
            reason: "prepared_print_source_changed",
        })
    );

    let prepared_sheet = prepare_sheet_print_document(&red.sheets[0], 0, &options).unwrap();
    assert!(build_sheet_print_page(&identical_red.sheets[0], &prepared_sheet, 0).is_ok());
    assert_eq!(
        build_sheet_print_page(&blue.sheets[0], &prepared_sheet, 0),
        Err(RenderError::Backend {
            reason: "prepared_print_source_changed",
        })
    );

    let red_document = build_print_document(&red, 0, &options).unwrap();
    let blue_document = build_print_document(&blue, 0, &options).unwrap();
    let mut red_fills = Vec::new();
    let mut blue_fills = Vec::new();
    scene_fills(&red_document.pages[0].scene.nodes, &mut red_fills);
    scene_fills(&blue_document.pages[0].scene.nodes, &mut blue_fills);
    assert_eq!(red_fills, [Rgb::new(255, 0, 0), Rgb::new(255, 0, 0)]);
    assert_eq!(blue_fills, [Rgb::new(0, 0, 255), Rgb::new(0, 0, 255)]);

    let mut merged_red = blank_table_style_workbook("FFFF0000");
    let mut merged_identical = blank_table_style_workbook("FFFF0000");
    let mut merged_blue = blank_table_style_workbook("FF0000FF");
    for workbook in [&mut merged_red, &mut merged_identical, &mut merged_blue] {
        workbook.sheets[0].merge(0, 0, 0, 1);
        workbook.sheets[0].set_page_setup(PageSetup::new().with_print_area((0, 1, 0, 1)));
    }
    let merged_prepared = prepare_print_document(&merged_red, 0, &PrintOptions::default()).unwrap();
    assert!(build_print_page(&merged_identical, &merged_prepared, 0).is_ok());
    assert_eq!(
        build_print_page(&merged_blue, &merged_prepared, 0),
        Err(RenderError::Backend {
            reason: "prepared_print_source_changed",
        }),
        "the off-range merge anchor supplies the selected blank cell's table style"
    );
}

#[test]
fn offrange_private_overlays_do_not_consume_selected_identity_budget() {
    let bold = blank_table_style_with_offrange_overlays("FFFF0000", "<b/>", 4_096);
    let italic = blank_table_style_with_offrange_overlays("FFFF0000", "<i/>", 4_096);
    let options = PrintOptions {
        omit_sparse_pages: false,
        render: RenderOptions {
            limits: rxls_render::RenderLimits {
                max_rows: 2,
                max_columns: 1,
                max_cells: 2,
                ..rxls_render::RenderLimits::default()
            },
            ..RenderOptions::default()
        },
        ..PrintOptions::default()
    };
    let prepared = prepare_print_document(&bold, 0, &options).unwrap();
    assert_eq!(prepared.report.source.cells_considered, 2);
    assert!(
        build_print_page(&italic, &prepared, 0).is_ok(),
        "4,096 unrelated private overlays must neither be hashed nor charged"
    );
}

#[test]
fn inherited_and_conditional_blank_style_structures_invalidate_prepared_pages() {
    enum StyleLayer {
        Default,
        Row,
        Column,
        Conditional,
    }

    fn workbook(layer: &StyleLayer, fill: Color) -> Workbook {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Blank styles");
        let format = Format::new().fill(fill);
        match layer {
            StyleLayer::Default => sheet.set_default_format(&format),
            StyleLayer::Row => sheet.set_row_format(0, &format),
            StyleLayer::Column => sheet.set_col_format(0, &format),
            StyleLayer::Conditional => sheet.add_conditional_format(CondFormat::new(
                (0, 0, 1, 0),
                CfRule::expression("1=1", fill),
            )),
        }
        sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 1, 0)));
        workbook
    }

    for layer in [
        StyleLayer::Default,
        StyleLayer::Row,
        StyleLayer::Column,
        StyleLayer::Conditional,
    ] {
        let red = workbook(&layer, Color::rgb(255, 0, 0));
        let identical = workbook(&layer, Color::rgb(255, 0, 0));
        let blue = workbook(&layer, Color::rgb(0, 0, 255));
        let prepared = prepare_print_document(&red, 0, &PrintOptions::default()).unwrap();
        assert!(build_print_page(&identical, &prepared, 0).is_ok());
        assert_eq!(
            build_print_page(&blue, &prepared, 0),
            Err(RenderError::Backend {
                reason: "prepared_print_source_changed",
            })
        );
    }
}

#[test]
fn sparse_page_omission_retains_blank_cells_with_inherited_table_paint() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Styled sparse page");
    sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 200, 0)));
    sheet.add_table(Table::new(
        (150, 0, 150, 0),
        "VisibleBlankTable",
        ["Header"],
    ));
    sheet.set_table_header_format(
        "VisibleBlankTable",
        &Format::new().fill(Color::rgb(12, 34, 56)),
    );

    let document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
    assert!(document.report.logical_pages > 1);
    assert_eq!(
        document.report.sparse_pages_omitted,
        document.report.logical_pages - document.pages.len() as u64
    );
    let painted_page = document
        .pages
        .iter()
        .find(|page| page.map.body_range.first_row <= 150 && page.map.body_range.last_row >= 150)
        .expect("page containing the styled blank table header");
    assert!(scene_has_fill(
        &painted_page.scene.nodes,
        Rgb::new(12, 34, 56)
    ));
}

#[test]
fn sparse_page_omission_retains_conditional_format_only_paint() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Conditional sparse page");
    sheet.write(0, 1, 1.0);
    sheet.add_conditional_format(CondFormat::new(
        (150, 0, 150, 0),
        CfRule::expression("$B$1>0", Color::rgb(90, 80, 70)),
    ));
    sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 200, 0)));

    let document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
    let painted_page = document
        .pages
        .iter()
        .find(|page| page.map.body_range.first_row <= 150 && page.map.body_range.last_row >= 150)
        .expect("page containing the conditional-format-only paint");
    assert!(scene_has_fill(
        &painted_page.scene.nodes,
        Rgb::new(90, 80, 70)
    ));
}

#[test]
fn off_range_chart_and_sparkline_sources_do_not_consume_selected_dependency_budget() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Scoped dependencies");
    sheet.write(0, 0, "selected");
    for row in 100..110 {
        sheet.write(row, 0, (row - 99) as f64);
    }
    sheet.add_chart(
        Chart::new(ChartKind::Line, (100, 2), (110, 8)).add_series(Series::new("$A$101:$A$110")),
    );
    sheet.add_sparkline(Sparkline::new((100, 9), "$A$101:$A$110"));
    sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 0, 0)));
    let mut options = PrintOptions::default();
    options.render.limits.max_chart_points = 5;

    let prepared = prepare_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(prepared.report.pages.len(), 1);
    assert!(build_print_page(&workbook, &prepared, 0).is_ok());
}

#[test]
fn placeholder_chart_source_does_not_consume_selected_dependency_budget() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Placeholder dependency");
    sheet.write(0, 0, "selected");
    for row in 100..110 {
        sheet.write(row, 0, (row - 99) as f64);
    }
    sheet.add_chart(
        Chart::new(ChartKind::Line, (0, 0), (1, 1)).add_series(Series::new("$A$101:$A$110")),
    );
    sheet.set_page_setup(PageSetup::new().with_print_area((0, 0, 1, 1)));
    let mut options = PrintOptions::default();
    options.render.limits.max_chart_points = 5;

    let prepared = prepare_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(prepared.report.pages.len(), 1);
    assert!(build_print_page(&workbook, &prepared, 0).is_ok());
}

#[test]
fn overlapping_print_title_ranges_charge_conditional_targets_once() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Overlapping conditional targets");
    sheet.write(0, 1, 1.0);
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 9, 0),
        CfRule::expression("$B$1>0", Color::rgb(90, 80, 70)),
    ));
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 9, 0))
            .with_repeat_rows(0, 4),
    );
    let mut options = PrintOptions::default();
    options.render.limits.max_conditional_evaluations = 12;

    let prepared = prepare_print_document(&workbook, 0, &options).unwrap();
    assert!(!prepared.report.pages.is_empty());
    assert!(build_print_page(&workbook, &prepared, 0).is_ok());
}

#[test]
fn empty_paginated_blocks_do_not_consume_scene_node_budget() {
    let mut workbook = Workbook::new();
    workbook
        .add_sheet("Blank page")
        .set_page_setup(PageSetup::new().with_print_area((0, 0, 0, 0)));
    let mut options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    options.render.gridlines = false;
    options.limits.max_total_scene_nodes = 0;
    let document = build_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(document.pages.len(), 1);
    assert!(document.pages[0].scene.nodes.is_empty());
}

#[test]
fn overflowing_paginated_blocks_are_clipped_to_the_printable_content_rect() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Overflow clip");
    sheet.write(0, 0, "wide");
    sheet.set_col_width(0, 1_000.0);
    sheet.set_row_height(0, 1_000.0);
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 0, 0))
            .with_scale(100),
    );
    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            omit_sparse_pages: false,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert!(document.report.warnings.iter().any(|warning| {
        warning.code == PrintWarningCode::PageContentOverflow && warning.occurrences >= 1
    }));
    let content = document.report.content_rect;
    let clips = document.pages[0]
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::ClipGroup(group) => Some(group.clip),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!clips.is_empty());
    for clip in clips {
        assert!(clip.x >= content.x);
        assert!(clip.y >= content.y);
        assert!(
            clip.x.raw() + clip.width.raw() <= content.x.raw() + content.width.raw(),
            "clip extends into a horizontal margin: {clip:?} vs {content:?}"
        );
        assert!(
            clip.y.raw() + clip.height.raw() <= content.y.raw() + content.height.raw(),
            "clip extends into a vertical margin: {clip:?} vs {content:?}"
        );
    }
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
        .map(|page| retained_scene_nodes(&page.scene.nodes))
        .max()
        .unwrap();
    assert!(
        document
            .pages
            .iter()
            .map(|page| retained_scene_nodes(&page.scene.nodes))
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
    assert_eq!(metadata.fit_to_page(), Some(false));

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
    assert_eq!(metadata.fit_to_page(), None);

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
    assert_eq!(metadata.fit_to_page(), None);

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
    assert_eq!(workbook.sheets[0].print_metadata().fit_to_page(), None);
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
fn fit_to_page_ignores_manual_breaks_without_enlarging_small_ranges() {
    let workbook = Workbook::open(&synthetic_fit_breaks_xlsx(
        "$A$1:$B$2",
        Some(true),
        r#"scale="85" fitToWidth="1" fitToHeight="1""#,
    ))
    .unwrap();
    let metadata = workbook.sheets[0].print_metadata();
    assert_eq!(metadata.manual_row_breaks(), &[1]);
    assert_eq!(metadata.manual_col_breaks(), &[1]);

    let options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    let document = build_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(
        document,
        build_print_document(&workbook, 0, &options).unwrap()
    );
    assert_eq!(document.report.scale_permille, 1_000);
    assert_eq!(document.report.logical_pages, 1);
    assert_eq!(document.report.pages.len(), 1);
    assert_eq!(
        document.report.pages[0].body_range,
        RenderRange::new(0, 0, 1, 1)
    );
    assert_eq!(document.report.pages[0].scale_permille, 1_000);
    assert!(document.report.manual_row_breaks.is_empty());
    assert!(document.report.manual_col_breaks.is_empty());
    assert!(!document.report.pages[0].manual_row_break_before);
    assert!(!document.report.pages[0].manual_col_break_before);
    assert!(!document
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == PrintWarningCode::FitTargetUnreachable));
    assert_eq!(
        workbook.sheets[0].print_metadata().manual_row_breaks(),
        &[1]
    );
    assert_eq!(
        workbook.sheets[0].print_metadata().manual_col_breaks(),
        &[1]
    );
    let json = document.report.to_json();
    assert!(json.contains("\"manual_row_breaks\":[]"));
    assert!(json.contains("\"manual_col_breaks\":[]"));
    assert!(!json.contains("\"manual_row_break_before\":true"));
    assert!(!json.contains("\"manual_col_break_before\":true"));
}

#[test]
fn fit_to_page_shrinks_large_ranges_to_target_without_manual_break_pages() {
    let workbook = Workbook::open(&synthetic_fit_breaks_xlsx(
        "$A$1:$H$20",
        Some(true),
        r#"scale="85" fitToWidth="1" fitToHeight="1""#,
    ))
    .unwrap();
    let metadata = workbook.sheets[0].print_metadata();
    assert_eq!(metadata.manual_row_breaks(), &[1]);
    assert_eq!(metadata.manual_col_breaks(), &[1]);

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            omit_sparse_pages: false,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert!((100..1_000).contains(&document.report.scale_permille));
    assert_eq!(document.report.logical_pages, 1);
    assert_eq!(document.report.pages.len(), 1);
    assert_eq!(
        document.report.pages[0].body_range,
        RenderRange::new(0, 0, 19, 7)
    );
    assert_eq!(
        document.report.pages[0].scale_permille,
        document.report.scale_permille
    );
    assert!(document.report.manual_row_breaks.is_empty());
    assert!(document.report.manual_col_breaks.is_empty());
    assert!(!document.report.pages[0].manual_row_break_before);
    assert!(!document.report.pages[0].manual_col_break_before);
    assert!(!document
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == PrintWarningCode::FitTargetUnreachable));
    assert_eq!(
        workbook.sheets[0].print_metadata().manual_row_breaks(),
        &[1]
    );
    assert_eq!(
        workbook.sheets[0].print_metadata().manual_col_breaks(),
        &[1]
    );
}

#[test]
fn active_fit_defaults_missing_dimensions_to_one() {
    let workbook = Workbook::open(&synthetic_fit_breaks_xlsx(
        "$A$1:$H$20",
        Some(true),
        r#"scale="85""#,
    ))
    .unwrap();

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            omit_sparse_pages: false,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert!((100..1_000).contains(&document.report.scale_permille));
    assert_eq!(document.report.logical_pages, 1);
    assert_eq!(document.report.pages.len(), 1);
    assert!(document.report.manual_row_breaks.is_empty());
    assert!(document.report.manual_col_breaks.is_empty());
}

#[test]
fn active_fit_treats_zero_dimensions_as_unconstrained() {
    let workbook = Workbook::open(&synthetic_fit_breaks_xlsx(
        "$A$1:$H$20",
        Some(true),
        r#"scale="85" fitToWidth="0" fitToHeight="0""#,
    ))
    .unwrap();

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            omit_sparse_pages: false,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(document.report.scale_permille, 1_000);
    assert!(document.report.logical_pages > 1);
    assert!(document.report.manual_row_breaks.is_empty());
    assert!(document.report.manual_col_breaks.is_empty());
}

#[test]
fn active_fit_one_by_zero_constrains_only_page_width() {
    let workbook = Workbook::open(&synthetic_fit_breaks_xlsx(
        "$A$1:$B$20",
        Some(true),
        r#"scale="85" fitToWidth="1" fitToHeight="0""#,
    ))
    .unwrap();

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            omit_sparse_pages: false,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(document.report.scale_permille, 1_000);
    assert!(document.report.logical_pages > 1);
    assert!(document
        .report
        .pages
        .iter()
        .all(|page| page.horizontal_index == 0));
    assert!(document
        .report
        .pages
        .iter()
        .any(|page| page.vertical_index > 0));
    assert!(document.report.manual_row_breaks.is_empty());
    assert!(document.report.manual_col_breaks.is_empty());
}

#[test]
fn omitted_or_false_fit_flag_keeps_fixed_scale_and_manual_breaks() {
    for fit_to_page in [None, Some(false)] {
        let workbook = Workbook::open(&synthetic_fit_breaks_xlsx(
            "$A$1:$B$2",
            fit_to_page,
            r#"scale="100" fitToWidth="1" fitToHeight="1""#,
        ))
        .unwrap();
        let metadata = workbook.sheets[0].print_metadata();
        assert_eq!(metadata.fit_to_page(), Some(false));
        assert_eq!(metadata.manual_row_breaks(), &[1]);
        assert_eq!(metadata.manual_col_breaks(), &[1]);

        let document = build_print_document(
            &workbook,
            0,
            &PrintOptions {
                omit_sparse_pages: false,
                ..PrintOptions::default()
            },
        )
        .unwrap();
        assert_eq!(document.report.scale_permille, 1_000);
        assert_eq!(document.report.logical_pages, 4);
        assert_eq!(document.report.pages.len(), 4);
        assert_eq!(document.report.manual_row_breaks, [1]);
        assert_eq!(document.report.manual_col_breaks, [1]);
        assert_eq!(
            document
                .report
                .pages
                .iter()
                .map(|page| {
                    (
                        page.horizontal_index,
                        page.vertical_index,
                        page.manual_col_break_before,
                        page.manual_row_break_before,
                    )
                })
                .collect::<Vec<_>>(),
            [
                (0, 0, false, false),
                (0, 1, false, true),
                (1, 0, true, false),
                (1, 1, true, true),
            ]
        );
        assert_eq!(
            workbook.sheets[0].print_metadata().manual_row_breaks(),
            &[1]
        );
        assert_eq!(
            workbook.sheets[0].print_metadata().manual_col_breaks(),
            &[1]
        );
        let json = document.report.to_json();
        assert!(json.contains("\"manual_row_breaks\":[1]"));
        assert!(json.contains("\"manual_col_breaks\":[1]"));
        assert!(json.contains("\"manual_row_break_before\":true"));
        assert!(json.contains("\"manual_col_break_before\":true"));
    }
}

#[test]
fn single_page_uses_declared_blank_extent_without_changing_used_rendering() {
    let workbook = declared_blank_extent_xlsx(false);
    assert_eq!(
        workbook.sheets[0].source_used_dimensions(),
        Some((0, 0, 5, 3))
    );
    assert_eq!(workbook.sheets[0].dimensions(), Some((0, 0, 0, 0)));

    let render = RenderOptions {
        gridlines: false,
        ..RenderOptions::default()
    };
    let used = build_scene(&workbook, 0, &render).unwrap();
    assert_eq!(used.report.range, RenderRange::new(0, 0, 0, 0));
    assert_eq!(used.scene.width, Fixed::from_pixels(64));
    assert_eq!(used.scene.height, Fixed::from_pixels(20));
    assert_eq!(used.report.merged_regions, 0);

    let options = PrintOptions {
        render: render.clone(),
        single_page_sheets: true,
        ..PrintOptions::default()
    };
    let document = build_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(document.pages.len(), 1);
    assert_eq!(document.report.source.range, RenderRange::new(0, 0, 5, 3));
    assert_eq!(
        document.report.pages[0].body_range,
        RenderRange::new(0, 0, 5, 3)
    );
    assert_eq!(document.pages[0].scene.width, Fixed::from_pixels(334));
    assert_eq!(document.pages[0].scene.height, Fixed::from_pixels(140));
    assert_eq!(document.report.source.visible_columns, 4);
    assert_eq!(document.report.source.visible_rows, 6);
    assert_eq!(document.report.source.merged_regions, 0);
    assert_eq!(document.report.paper.width, document.pages[0].scene.width);
    assert_eq!(document.report.paper.height, document.pages[0].scene.height);
    assert_eq!(
        document.report.content_rect.width,
        document.pages[0].scene.width
    );
    assert_eq!(
        document.report.content_rect.height,
        document.pages[0].scene.height
    );

    let prepared = prepare_print_document(&workbook, 0, &options).unwrap();
    let rebuilt = build_print_page(&workbook, &prepared, 0).unwrap();
    assert_eq!(rebuilt, document.pages[0]);

    let explicit = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            render: RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(1, 1, 2, 2)),
                gridlines: false,
                ..RenderOptions::default()
            },
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(explicit.report.source.range, RenderRange::new(1, 1, 2, 2));
    assert_eq!(explicit.pages[0].scene.width, Fixed::from_pixels(128));
    assert_eq!(explicit.pages[0].scene.height, Fixed::from_pixels(40));
}

#[test]
fn single_page_declared_extent_respects_hidden_axis_policy() {
    let workbook = declared_blank_extent_xlsx(true);
    let hidden = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            render: RenderOptions {
                gridlines: false,
                ..RenderOptions::default()
            },
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(hidden.report.source.range, RenderRange::new(0, 0, 5, 3));
    assert_eq!(hidden.pages[0].scene.width, Fixed::from_pixels(192));
    assert_eq!(hidden.pages[0].scene.height, Fixed::from_pixels(100));
    assert_eq!(hidden.report.source.visible_columns, 3);
    assert_eq!(hidden.report.source.visible_rows, 5);

    let included = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            render: RenderOptions {
                gridlines: false,
                include_hidden: true,
                ..RenderOptions::default()
            },
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(included.pages[0].scene.width, Fixed::from_pixels(334));
    assert_eq!(included.pages[0].scene.height, Fixed::from_pixels(140));
    assert_eq!(included.report.source.visible_columns, 4);
    assert_eq!(included.report.source.visible_rows, 6);
}

#[test]
fn single_page_unions_stale_small_declarations_with_actual_content() {
    let mut workbook = source_dimension_xlsx(
        "A1:B2",
        r#"<sheetFormatPr defaultRowHeight="15"/>
        <sheetData>
          <row r="6"><c r="D6" t="inlineStr"><is><t>outside</t></is></c></row>
        </sheetData>"#,
    );
    assert_eq!(
        workbook.sheets[0].source_used_dimensions(),
        Some((0, 0, 1, 1))
    );
    assert_eq!(workbook.sheets[0].dimensions(), Some((5, 3, 5, 3)));

    let render = RenderOptions {
        gridlines: false,
        ..RenderOptions::default()
    };
    let used = build_scene(&workbook, 0, &render).unwrap();
    assert_eq!(used.report.range, RenderRange::new(5, 3, 5, 3));
    assert_eq!(used.scene.width, Fixed::from_pixels(64));
    assert_eq!(used.scene.height, Fixed::from_pixels(20));

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            render,
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(document.report.source.range, RenderRange::new(0, 0, 5, 3));
    assert_eq!(document.pages[0].scene.width, Fixed::from_pixels(256));
    assert_eq!(document.pages[0].scene.height, Fixed::from_pixels(120));

    workbook.sheets[0].add_chart(Chart::new(ChartKind::Line, (8, 5), (9, 6)));
    let drawing_union = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            render: RenderOptions {
                gridlines: false,
                ..RenderOptions::default()
            },
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        drawing_union.report.source.range,
        RenderRange::new(0, 0, 9, 6)
    );
    assert_eq!(
        drawing_union.report.pages[0].body_range,
        RenderRange::new(0, 0, 9, 6)
    );
}

#[test]
fn single_page_declared_extent_fails_before_oversized_grid_materialization() {
    let workbook = source_dimension_xlsx(
        "A1:XFD1048576",
        r#"<sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>anchor</t></is></c></row>
        </sheetData>"#,
    );
    let render = RenderOptions {
        gridlines: false,
        ..RenderOptions::default()
    };
    assert_eq!(
        build_scene(&workbook, 0, &render).unwrap().report.range,
        RenderRange::new(0, 0, 0, 0)
    );

    assert_eq!(
        build_print_document(
            &workbook,
            0,
            &PrintOptions {
                render,
                single_page_sheets: true,
                ..PrintOptions::default()
            },
        ),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Rows,
            limit: RenderLimits::default().max_rows,
            actual: 1_048_576,
        })
    );
}

#[test]
fn single_page_keeps_an_empty_a1_dimension_sentinel_empty() {
    let workbook = source_dimension_xlsx("A1", "<sheetData/>");
    assert_eq!(
        workbook.sheets[0].source_used_dimensions(),
        Some((0, 0, 0, 0))
    );
    assert_eq!(workbook.sheets[0].dimensions(), None);

    let render = RenderOptions {
        gridlines: false,
        ..RenderOptions::default()
    };
    let used = build_scene(&workbook, 0, &render).unwrap();
    assert_eq!(used.report.range, RenderRange::new(0, 0, 0, 0));
    assert_eq!(used.scene.width, Fixed::from_pixels(1));
    assert_eq!(used.scene.height, Fixed::from_pixels(1));
    assert!(used.scene.nodes.is_empty());

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            render,
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(document.pages[0].scene.width, Fixed::from_pixels(1));
    assert_eq!(document.pages[0].scene.height, Fixed::from_pixels(1));
    assert!(document.pages[0].scene.nodes.is_empty());
}

#[test]
fn prepared_single_page_rechecks_current_source_dimensions() {
    let body = r#"<sheetFormatPr defaultRowHeight="15"/>
      <sheetData>
        <row r="1"><c r="A1" t="inlineStr"><is><t>anchor</t></is></c></row>
      </sheetData>"#;
    let original = source_dimension_xlsx("A1:D6", body);
    let replacement = source_dimension_xlsx("A1:B2", body);
    let options = PrintOptions {
        render: RenderOptions {
            gridlines: false,
            ..RenderOptions::default()
        },
        single_page_sheets: true,
        ..PrintOptions::default()
    };
    let prepared = prepare_print_document(&original, 0, &options).unwrap();

    assert_eq!(
        build_print_page(&replacement, &prepared, 0),
        Err(RenderError::Backend {
            reason: "prepared_print_source_changed",
        })
    );
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
    let body_clip = document.pages[0]
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::ClipGroup(group) => Some(group.clip),
            _ => None,
        })
        .expect("paginated body clip");
    assert!(body_clip.x >= document.report.content_rect.x);
    assert!(body_clip.y >= document.report.content_rect.y);
    assert!(
        body_clip.x.raw() + body_clip.width.raw()
            <= document.report.content_rect.x.raw() + document.report.content_rect.width.raw()
    );
    assert!(
        body_clip.y.raw() + body_clip.height.raw()
            <= document.report.content_rect.y.raw() + document.report.content_rect.height.raw()
    );
    let first_cell = first_cell_bounds(&document.pages[0].scene.nodes).unwrap();
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
