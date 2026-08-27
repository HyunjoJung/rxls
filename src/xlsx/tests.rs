use super::chart::{
    apply_chart_luminance, chart_text_bounded_size, parse_chart, parse_chart_axis_semantics,
    parse_chart_data_labels, ChartAxisContext, ParsedChart, MAX_CHART_TEXT_STYLE_FACTS_PER_ROLE,
    MAX_XLSX_CHART_TEXT_FIELD_BYTES,
};
use super::chart::{parse_chart_with_theme, XLSX_CHART_XML_SCAN_PASSES};
use super::drawing::{
    parse_drawing_refs, parse_drawing_refs_bounded, MAX_XLSX_DRAWINGS, MAX_XLSX_DRAWING_TEXT,
};
use super::relationships::internal_relationship_target_by_id;
use super::*;
use crate::{
    ChartBarDirection, ChartFrameFill, ChartFrameStyleLossKind, ChartKind, ChartMarkerSymbol,
    ChartSeriesStyleLossKind, ChartTextStyle, ChartUnsupportedReason, DrawingAnchorBehavior,
    DrawingCrop, DrawingObjectKind,
};

fn complete_theme_xml(
    major_latin: &str,
    minor_latin: &str,
    accent1: &str,
    accent2: &str,
) -> String {
    format!(
        r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:accent1><a:srgbClr val="{accent1}"/></a:accent1><a:accent2><a:srgbClr val="{accent2}"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme><a:majorFont><a:latin typeface="{major_latin}"/></a:majorFont><a:minorFont><a:latin typeface="{minor_latin}"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#
    )
}

fn overlay_is_empty(overlay: &CellStyleOverlay) -> bool {
    !overlay.replace_font
        && !overlay.replace_fill
        && !overlay.replace_border
        && !overlay.replace_num_fmt
        && !overlay.replace_alignment
        && !overlay.replace_protection
}

fn shared_texts(xml: &str) -> Vec<String> {
    parse_shared_strings(xml, &ThemeColors::default(), &[])
        .into_iter()
        .map(|shared| shared.text)
        .collect()
}

fn workbook_with_worksheet_xml(worksheet: &str) -> Workbook {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/worksheets/sheet1.xml", worksheet),
    ];
    for (name, body) in parts {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    Workbook::open(&zip.finish().unwrap().into_inner()).unwrap()
}

#[test]
fn relationship_selection_is_exact_deterministic_and_internal_only() {
    let transitional = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="drawings/drawing1.xml"/></Relationships>"#;
    let strict = r#"<Relationships xmlns="http://purl.oclc.org/ooxml/package/relationships"><Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/drawing" Target="drawings/drawing1.xml"/></Relationships>"#;
    assert_eq!(
        unique_internal_relationship_target(transitional, "drawing"),
        RelationshipTarget::Internal("drawings/drawing1.xml".to_string())
    );
    assert_eq!(
        unique_internal_relationship_target(strict, "drawing"),
        RelationshipTarget::Internal("drawings/drawing1.xml".to_string())
    );

    let attacker_type = r#"<Relationships><Relationship Id="rId1" Type="https://attacker.invalid/officeDocument/2006/relationships/drawing" Target="evil.xml"/></Relationships>"#;
    assert_eq!(
        unique_internal_relationship_target(attacker_type, "drawing"),
        RelationshipTarget::Missing
    );

    let duplicate_id = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="a.xml"/><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="b.xml"/></Relationships>"#;
    assert_eq!(
        unique_internal_relationship_target(duplicate_id, "drawing"),
        RelationshipTarget::Invalid
    );
    assert!(parse_rels(duplicate_id).is_empty());

    let external = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="https://example.invalid/drawing.xml" TargetMode="External"/></Relationships>"#;
    assert_eq!(
        unique_internal_relationship_target(external, "drawing"),
        RelationshipTarget::Invalid
    );

    let foreign_namespace = r#"<Relationships xmlns="https://attacker.invalid/package/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="evil.xml"/></Relationships>"#;
    assert_eq!(
        unique_internal_relationship_target(foreign_namespace, "drawing"),
        RelationshipTarget::Invalid
    );

    let explicitly_closed = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="drawings/drawing1.xml"></Relationship></Relationships>"#;
    assert_eq!(
        unique_internal_relationship_target(explicitly_closed, "drawing"),
        RelationshipTarget::Internal("drawings/drawing1.xml".to_string())
    );

    let child_content = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="evil.xml"><extension/></Relationship></Relationships>"#;
    assert_eq!(
        unique_internal_relationship_target(child_content, "drawing"),
        RelationshipTarget::Invalid
    );

    let text_content = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="evil.xml">content</Relationship></Relationships>"#;
    assert_eq!(
        unique_internal_relationship_target(text_content, "drawing"),
        RelationshipTarget::Invalid
    );
}

#[test]
fn relationship_extensions_do_not_weaken_core_attribute_validation() {
    let extended = r#"<Relationships xmlns:ext="urn:producer:relationships" ext:producer="example"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="drawings/drawing1.xml" ext:metadata="kept-by-package-editor"/></Relationships>"#;
    assert_eq!(
        unique_internal_relationship_target(extended, "drawing"),
        RelationshipTarget::Internal("drawings/drawing1.xml".to_string())
    );

    for malformed in [
        r#"<Relationships xmlns:ext="urn:producer:relationships"><Relationship ext:Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="drawings/drawing1.xml"/></Relationships>"#,
        r#"<Relationships xmlns:ext="urn:producer:relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" ext:Target="drawings/drawing1.xml"/></Relationships>"#,
        r#"<Relationships xmlns:ext="urn:producer:relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="drawings/drawing1.xml" TargetMode="invalid" ext:TargetMode="External"/></Relationships>"#,
    ] {
        assert_eq!(
            unique_internal_relationship_target(malformed, "drawing"),
            RelationshipTarget::Invalid
        );
    }
}

#[test]
fn drawing_relationship_ids_require_the_exact_internal_object_type() {
    let xml = r#"<Relationships><Relationship Id="chart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/><Relationship Id="image" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/><Relationship Id="external" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="https://example.invalid/chart.xml" TargetMode="External"/></Relationships>"#;
    let relationships = parse_ooxml_relationships(xml).expect("valid relationship part");
    assert!(matches!(
        internal_relationship_target_by_id(&relationships, "chart", "chart"),
        RelationshipTarget::Internal(target) if target == "../charts/chart1.xml"
    ));
    assert_eq!(
        internal_relationship_target_by_id(&relationships, "image", "chart"),
        RelationshipTarget::Invalid
    );
    assert_eq!(
        internal_relationship_target_by_id(&relationships, "external", "chart"),
        RelationshipTarget::Invalid
    );
}

#[test]
fn chart_count_and_xml_work_budgets_are_shared_across_sheets() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    const CHART_XML: &str = r#"<chartSpace xmlns="http://schemas.openxmlformats.org/drawingml/2006/chart"><chart><plotArea><lineChart><grouping val="standard"/><varyColors val="0"/><axId val="1"/><axId val="2"/></lineChart><catAx><axId val="1"/><crossAx val="2"/></catAx><valAx><axId val="2"/><crossAx val="1"/></valAx></plotArea></chart></chartSpace>"#;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for index in 1..=2 {
        writer
            .start_file(format!("xl/drawings/drawing{index}.xml"), options)
            .unwrap();
        writer
                .write_all(
                    format!(r#"<wsDr><twoCellAnchor><from><col>0</col><row>0</row></from><to><col>4</col><row>8</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart{index}"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#).as_bytes(),
                )
                .unwrap();
        writer
            .start_file(
                format!("xl/drawings/_rels/drawing{index}.xml.rels"),
                options,
            )
            .unwrap();
        writer
                .write_all(
                    format!(r#"<Relationships><Relationship Id="rIdChart{index}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart{index}.xml"/></Relationships>"#).as_bytes(),
                )
                .unwrap();
        writer
            .start_file(format!("xl/charts/chart{index}.xml"), options)
            .unwrap();
        writer.write_all(CHART_XML.as_bytes()).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
    let sheet_rels = |index| {
        format!(
            r#"<Relationships><Relationship Id="rIdDraw{index}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing{index}.xml"/></Relationships>"#
        )
    };

    let mut count_budget = ChartImportBudget {
        charts_remaining: 1,
        ..ChartImportBudget::default()
    };
    let first = read_sheet_drawings(
        &mut zip,
        "xl/worksheets/sheet1.xml",
        Some(&sheet_rels(1)),
        &ThemeColors::default(),
        &mut count_budget,
    );
    let second = read_sheet_drawings(
        &mut zip,
        "xl/worksheets/sheet2.xml",
        Some(&sheet_rels(2)),
        &ThemeColors::default(),
        &mut count_budget,
    );
    assert_eq!(first.1.len(), 1);
    assert!(second.1.is_empty());
    assert!(second
        .3
        .iter()
        .any(|loss| loss.kind == StyleLossKind::LimitExceeded));

    let chart_work = CHART_XML.len() * XLSX_CHART_XML_SCAN_PASSES;
    let mut work_budget = ChartImportBudget {
        charts_remaining: 2,
        xml_work_remaining: chart_work,
        xml_work_limit: chart_work,
        ..ChartImportBudget::default()
    };
    let first = read_sheet_drawings(
        &mut zip,
        "xl/worksheets/sheet1.xml",
        Some(&sheet_rels(1)),
        &ThemeColors::default(),
        &mut work_budget,
    );
    let second = read_sheet_drawings(
        &mut zip,
        "xl/worksheets/sheet2.xml",
        Some(&sheet_rels(2)),
        &ThemeColors::default(),
        &mut work_budget,
    );
    assert_eq!(first.1.len(), 1);
    assert!(second.1.is_empty());
    assert!(second
        .3
        .iter()
        .any(|loss| loss.kind == StyleLossKind::LimitExceeded));
}

#[test]
fn cell_ref_parsing() {
    assert_eq!(parse_ref("A1"), Some((0, 0)));
    assert_eq!(parse_ref("B2"), Some((1, 1)));
    assert_eq!(parse_ref("Z1"), Some((0, 25)));
    assert_eq!(parse_ref("AA1"), Some((0, 26)));
    assert_eq!(parse_ref("XFD1048576"), Some((1_048_575, 16_383))); // Excel max
    assert_eq!(parse_ref("A"), None);
    assert_eq!(parse_ref("XFE1"), None); // past the last column
    assert_eq!(parse_ref("ZZZZZZZ1"), None); // overflow → None, NOT a panic
}

#[test]
fn shared_strings_concatenate_runs() {
    let xml = r#"<sst><si><t>Hello</t></si><si><r><rPr><b/><color rgb="FF112233"/></rPr><t>가</t></r><r><rPr><i/></rPr><t>나</t></r></si></sst>"#;
    assert_eq!(shared_texts(xml), vec!["Hello", "가나"]);
    let parsed = parse_shared_strings(xml, &ThemeColors::default(), &[]);
    assert_eq!(parsed[1].runs.len(), 2);
    assert!(parsed[1].runs[0].font.bold);
    assert_eq!(
        parsed[1].runs[0].font.color,
        Some(Color::rgb(0x11, 0x22, 0x33))
    );
    assert!(parsed[1].runs[1].font.italic);
}

#[test]
fn general_refs_are_reassembled_across_xlsx_text_surfaces() {
    assert_eq!(
        shared_texts("<sst><si><t>A&amp;B&#33;</t></si></sst>"),
        vec!["A&B!"]
    );

    let props = parse_doc_properties(
        Some("<coreProperties><title>A&amp;B&#33;</title></coreProperties>"),
        None,
    );
    assert_eq!(props.title.as_deref(), Some("A&B!"));

    let comments = parse_comments(
        r#"<comments><authors><author>R&amp;D</author></authors><commentList><comment ref="A1" authorId="0"><text><t>Check &lt;now&gt;</t></text></comment></commentList></comments>"#,
    );
    assert_eq!(comments[0].author.as_deref(), Some("R&D"));
    assert_eq!(comments[0].text, "Check <now>");

    let xml = r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>A&amp;B&#33;</t></is></c><c r="B1"><f>A1&amp;"!"</f><v>1&#48;</v></c></row></sheetData></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;
    assert_eq!(cells[0].value, Cell::Text("A&B!".to_string()));
    match &cells[1].value {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "A1&\"!\"");
            assert_eq!(**cached, Cell::Number(10.0));
        }
        other => panic!("expected formula cell, got {other:?}"),
    }
}

#[test]
fn unknown_and_illegal_general_refs_are_preserved_lexically_on_read() {
    assert_eq!(
        shared_texts("<sst><si><t>A&bogus;&#x1;</t></si></sst>"),
        vec!["A&bogus;&#x1;"]
    );
}

#[test]
fn attributes_accept_only_xml_predefined_entities() {
    let mut reader = Reader::from_str(r#"<x value="a&nbsp;b"/>"#);
    let Event::Empty(element) = reader.read_event().unwrap() else {
        panic!("expected empty element");
    };
    assert_eq!(attr(&element, b"value"), None);
}

#[test]
fn general_refs_are_reassembled_in_drawing_coordinates_and_chart_refs() {
    let drawing = r#"<wsDr><twoCellAnchor><from><col>1&#48;</col><row>2&#48;</row></from><to><col>3&#48;</col><row>4&#48;</row></to><graphicFrame><chart r:id="rId&amp;Chart"/></graphicFrame></twoCellAnchor></wsDr>"#;
    let refs = parse_drawing_refs(drawing);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].rid.as_deref(), Some("rId&Chart"));
    assert_eq!(refs[0].from, (20, 10));
    assert_eq!(refs[0].to, Some((40, 30)));

    let mut cache_points = 16;
    let mut chart_series = 16;
    let chart = parse_chart(
            r#"<chartSpace><chart><plotArea><lineChart><ser><tx><strRef><f>Data&amp;More!$A$1</f></strRef></tx><cat><strRef><f>Data!$A$2:$A$3</f></strRef></cat><val><numRef><f>Data!$B$2:$B$3</f></numRef></val></ser></lineChart></plotArea></chart></chartSpace>"#,
            (20, 10),
            (40, 30),
            &mut cache_points,
            &mut chart_series,
        )
        .unwrap()
        .chart;
    assert_eq!(chart.series[0].name.as_deref(), Some("Data&More!$A$1"));
    assert_eq!(
        chart.series[0].categories.as_deref(),
        Some("Data!$A$2:$A$3")
    );
    assert_eq!(chart.series[0].values, "Data!$B$2:$B$3");
}

#[test]
fn drawing_sidecars_retain_all_anchor_geometry_and_unsupported_shapes() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let drawing = r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"
                xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
                xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
            <xdr:twoCellAnchor editAs="oneCell">
                <xdr:from><xdr:col>1</xdr:col><xdr:colOff>123</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>456</xdr:rowOff></xdr:from>
                <xdr:to><xdr:col>4</xdr:col><xdr:colOff>789</xdr:colOff><xdr:row>5</xdr:row><xdr:rowOff>1011</xdr:rowOff></xdr:to>
                <xdr:pic>
                    <xdr:nvPicPr><xdr:cNvPr id="2" name="Logo &amp; mark" descr="Accessible logo"/></xdr:nvPicPr>
                    <xdr:blipFill><a:blip r:embed="rIdImage"/><a:srcRect l="1000" t="2000" r="3000" b="4000"/></xdr:blipFill>
                    <xdr:spPr><a:xfrm rot="60000"><a:ext cx="914400" cy="457200"/></a:xfrm></xdr:spPr>
                </xdr:pic>
            </xdr:twoCellAnchor>
            <xdr:oneCellAnchor>
                <xdr:from><xdr:col>6</xdr:col><xdr:colOff>-5</xdr:colOff><xdr:row>7</xdr:row><xdr:rowOff>6</xdr:rowOff></xdr:from>
                <xdr:ext cx="1828800" cy="914400"/>
                <xdr:graphicFrame>
                    <xdr:nvGraphicFramePr><xdr:cNvPr id="3" name="Sales chart" title="Chart fallback text"/></xdr:nvGraphicFramePr>
                    <a:graphic><a:graphicData><c:chart r:id="rIdChart"/></a:graphicData></a:graphic>
                </xdr:graphicFrame>
            </xdr:oneCellAnchor>
            <xdr:absoluteAnchor>
                <xdr:pos x="1234" y="5678"/><xdr:ext cx="777" cy="888"/>
                <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="4" name="Callout" descr="Unsupported callout"/></xdr:nvSpPr>
                    <xdr:spPr><a:xfrm rot="-120000"/></xdr:spPr>
                </xdr:sp>
            </xdr:absoluteAnchor>
        </xdr:wsDr>"#;
    let parts = [
            (
                "xl/workbook.xml",
                br#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#.as_slice(),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#.as_slice(),
            ),
            (
                "xl/worksheets/sheet1.xml",
                br#"<worksheet><sheetData/><drawing r:id="rIdDrawing"/></worksheet>"#.as_slice(),
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                br#"<Relationships><Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#.as_slice(),
            ),
            ("xl/drawings/drawing1.xml", drawing.as_bytes()),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                br#"<Relationships><Relationship Id="rIdImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#.as_slice(),
            ),
            (
                "xl/charts/chart1.xml",
                br#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea><c:lineChart><c:axId val="1"/><c:axId val="2"/></c:lineChart><c:catAx><c:axId val="1"/><c:crossAx val="2"/></c:catAx><c:valAx><c:axId val="2"/><c:crossAx val="1"/></c:valAx></c:plotArea></c:chart></c:chartSpace>"#.as_slice(),
            ),
            ("xl/media/image1.png", b"\x89PNG\r\n\x1a\n".as_slice()),
        ];
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in parts {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(body).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();

    let workbook = Workbook::open(&bytes).unwrap();
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.images().len(), 1);
    assert_eq!(sheet.images()[0].from, (2, 1));
    assert_eq!(sheet.images()[0].to, Some((5, 4)));
    assert_eq!(sheet.charts().len(), 1);
    assert_eq!(sheet.charts()[0].from, (7, 6));
    assert_eq!(sheet.charts()[0].to, (7, 6));

    let metadata = sheet.drawing_metadata();
    assert_eq!(metadata.len(), 3);
    assert_eq!(metadata[0].kind, DrawingObjectKind::Image);
    assert_eq!(metadata[0].object_index, 0);
    assert_eq!(metadata[0].from_cell, Some((2, 1)));
    assert_eq!(metadata[0].to_cell, Some((5, 4)));
    assert_eq!(metadata[0].from_offset_emu, Some((123, 456)));
    assert_eq!(metadata[0].to_offset_emu, Some((789, 1011)));
    assert_eq!(metadata[0].absolute_size_emu, Some((914400, 457200)));
    assert_eq!(
        metadata[0].crop,
        Some(DrawingCrop {
            left_ppm: 10_000,
            top_ppm: 20_000,
            right_ppm: 30_000,
            bottom_ppm: 40_000,
        })
    );
    assert_eq!(metadata[0].rotation_mdeg, Some(1000));
    assert_eq!(metadata[0].z_order, Some(0));
    assert_eq!(metadata[0].name.as_deref(), Some("Logo & mark"));
    assert_eq!(metadata[0].alt_text.as_deref(), Some("Accessible logo"));
    assert_eq!(metadata[0].behavior, DrawingAnchorBehavior::MoveOnly);

    assert_eq!(metadata[1].kind, DrawingObjectKind::Chart);
    assert_eq!(metadata[1].object_index, 0);
    assert_eq!(metadata[1].from_cell, Some((7, 6)));
    assert_eq!(metadata[1].to_cell, None);
    assert_eq!(metadata[1].from_offset_emu, Some((-5, 6)));
    assert_eq!(metadata[1].to_offset_emu, None);
    assert_eq!(metadata[1].absolute_size_emu, Some((1_828_800, 914_400)));
    assert_eq!(metadata[1].z_order, Some(1));
    assert_eq!(metadata[1].name.as_deref(), Some("Sales chart"));
    assert_eq!(metadata[1].alt_text.as_deref(), Some("Chart fallback text"));
    assert_eq!(metadata[1].behavior, DrawingAnchorBehavior::MoveOnly);

    assert_eq!(metadata[2].kind, DrawingObjectKind::Shape);
    assert_eq!(metadata[2].from_cell, None);
    assert_eq!(metadata[2].to_cell, None);
    assert_eq!(metadata[2].from_offset_emu, Some((1234, 5678)));
    assert_eq!(metadata[2].absolute_size_emu, Some((777, 888)));
    assert_eq!(metadata[2].rotation_mdeg, Some(-2000));
    assert_eq!(metadata[2].z_order, Some(2));
    assert_eq!(metadata[2].name.as_deref(), Some("Callout"));
    assert_eq!(metadata[2].alt_text.as_deref(), Some("Unsupported callout"));
    assert_eq!(metadata[2].behavior, DrawingAnchorBehavior::Absolute);
    assert_eq!(
        sheet.style_losses(),
        &[StyleLoss {
            kind: StyleLossKind::UnsupportedProperty,
            occurrences: 1,
        }]
    );
}

#[test]
fn drawing_sidecar_strings_are_utf8_bounded_and_loss_aware() {
    let long_name = format!("{}한", "a".repeat(MAX_XLSX_DRAWING_TEXT));
    let xml = format!(
        r#"<wsDr><absoluteAnchor><pos x="1" y="2"/><ext cx="3" cy="4"/><sp><nvSpPr><cNvPr name="{long_name}"/></nvSpPr></sp></absoluteAnchor></wsDr>"#
    );
    let mut losses = Vec::new();
    let refs = parse_drawing_refs_bounded(&xml, &mut losses);

    assert_eq!(refs.len(), 1);
    let name = refs[0].metadata.name.as_deref().unwrap();
    assert_eq!(name.len(), MAX_XLSX_DRAWING_TEXT);
    assert!(name.is_char_boundary(name.len()));
    assert_eq!(
        losses,
        vec![StyleLoss {
            kind: StyleLossKind::LimitExceeded,
            occurrences: 1,
        }]
    );
}

#[test]
fn drawing_anchor_behavior_matrix_and_zero_offsets_are_exact() {
    let cases = [
        (None, DrawingAnchorBehavior::MoveAndSize),
        (Some("twoCell"), DrawingAnchorBehavior::MoveAndSize),
        (Some("oneCell"), DrawingAnchorBehavior::MoveOnly),
        (Some("absolute"), DrawingAnchorBehavior::Absolute),
    ];
    for (edit_as, expected) in cases {
        let edit_as = edit_as
            .map(|value| format!(r#" editAs="{value}""#))
            .unwrap_or_default();
        let xml = format!(
            r#"<wsDr><twoCellAnchor{edit_as}><from><col>0</col><colOff>0</colOff><row>0</row><rowOff>0</rowOff></from><to><col>1</col><colOff>0</colOff><row>1</row><rowOff>0</rowOff></to><sp/></twoCellAnchor></wsDr>"#
        );
        let refs = parse_drawing_refs(&xml);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].metadata.behavior, expected);
        assert_eq!(refs[0].metadata.from_offset_emu, Some((0, 0)));
        assert_eq!(refs[0].metadata.to_offset_emu, Some((0, 0)));
    }

    let one_cell = parse_drawing_refs(
        "<wsDr><oneCellAnchor><from><col>0</col><row>0</row></from><sp/></oneCellAnchor></wsDr>",
    );
    assert_eq!(
        one_cell[0].metadata.behavior,
        DrawingAnchorBehavior::MoveOnly
    );
    let absolute = parse_drawing_refs(
            "<wsDr><absoluteAnchor><pos x=\"0\" y=\"0\"/><ext cx=\"1\" cy=\"1\"/><sp/></absoluteAnchor></wsDr>",
        );
    assert_eq!(
        absolute[0].metadata.behavior,
        DrawingAnchorBehavior::Absolute
    );
}

#[test]
fn drawing_anchor_count_is_bounded_and_reports_the_limit() {
    let anchor =
        "<absoluteAnchor><pos x=\"0\" y=\"0\"/><ext cx=\"1\" cy=\"1\"/><sp/></absoluteAnchor>";
    let xml = format!("<wsDr>{}</wsDr>", anchor.repeat(MAX_XLSX_DRAWINGS + 1));
    let mut losses = Vec::new();
    let refs = parse_drawing_refs_bounded(&xml, &mut losses);

    assert_eq!(refs.len(), MAX_XLSX_DRAWINGS);
    assert_eq!(
        losses,
        vec![StyleLoss {
            kind: StyleLossKind::LimitExceeded,
            occurrences: 1,
        }]
    );
}

#[test]
fn shared_strings_keep_empty_slots() {
    // A self-closing <si/> and an empty <si></si> must each occupy an index,
    // so later references don't shift.
    let xml = r#"<sst><si><t>품목</t></si><si/><si></si><si><t>가격</t></si></sst>"#;
    assert_eq!(shared_texts(xml), vec!["품목", "", "", "가격"]);
}

#[test]
fn implicit_cell_positions() {
    // No `r` on <row>/<c>: position is implicit (col by order, row by order).
    // Some writers (LibreOffice, EPPlus) emit this; every cell would be lost
    // without implicit-position tracking.
    let xml = "<worksheet><sheetData>\
            <row><c t=\"inlineStr\"><is><t>A</t></is></c><c t=\"inlineStr\"><is><t>B</t></is></c></row>\
            <row><c t=\"inlineStr\"><is><t>C</t></is></c></row></sheetData></worksheet>";
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;
    let got: Vec<_> = cells
        .iter()
        .map(|c| (c.row, c.col, c.text.as_str()))
        .collect();
    assert_eq!(got, vec![(0, 0, "A"), (0, 1, "B"), (1, 0, "C")]);
}

#[test]
fn mixed_explicit_and_implicit_positions() {
    // An explicit `r` resyncs the running position; following r-less cells
    // continue from there.
    let xml = "<worksheet><sheetData>\
            <row r=\"5\"><c r=\"C5\" t=\"inlineStr\"><is><t>X</t></is></c>\
            <c t=\"inlineStr\"><is><t>Y</t></is></c></row></sheetData></worksheet>";
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;
    let got: Vec<_> = cells
        .iter()
        .map(|c| (c.row, c.col, c.text.as_str()))
        .collect();
    assert_eq!(got, vec![(4, 2, "X"), (4, 3, "Y")]);
}

#[test]
fn inline_string_with_cached_value_uses_inline_text_not_concatenation() {
    let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\" t=\"inlineStr\"><v>1.0</v><is><t>1.</t></is></c>\
            </row></sheetData></worksheet>";
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;

    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].value, Cell::Text("1.".to_string()));
    assert_eq!(cells[0].text, "1.");
}

#[test]
fn text_budget_caps_shared_string_amplification() {
    // The shared-string DoS: one large pooled string referenced by very many
    // cells. Retained values, display text, and cell records must stay within
    // the budget (here, a deliberately small one).
    let shared = vec![SharedString {
        text: "X".repeat(100),
        runs: Vec::new(),
    }];
    let mut xml = String::from("<worksheet><sheetData><row>");
    for _ in 0..1000 {
        xml.push_str("<c t=\"s\"><v>0</v></c>");
    }
    xml.push_str("</row></sheetData></worksheet>");
    let initial_budget = 1_024usize;
    let mut budget = initial_budget;
    let cells = parse_sheet(
        &xml,
        &shared,
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;
    let total: usize = cells.iter().map(retained_cell_cost).sum();
    assert!(
        total <= initial_budget,
        "accumulated {total} bytes exceeded the {initial_budget} budget"
    );
    assert!(!cells.is_empty(), "should still extract up to the cap");
}

#[test]
fn text_budget_exhaustion_leaves_zero_budget_signal() {
    let shared = vec![SharedString {
        text: "X".repeat(100),
        runs: Vec::new(),
    }];
    let xml = "<worksheet><sheetData><row><c t=\"s\"><v>0</v></c></row></sheetData></worksheet>";
    let mut budget = 50usize;
    let cells = parse_sheet(
        xml,
        &shared,
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;

    assert!(cells.is_empty());
    assert_eq!(budget, 0);
}

#[test]
fn empty_number_formats_retain_typed_values_under_a_structural_budget() {
    // These are the two real-world cases that exposed the regression:
    // `#` hides zero, while an explicitly empty format hides every number.
    // Both cells remain semantically present even though their display text
    // is empty.
    let styles = parse_styles(
        r##"<styleSheet><numFmts count="2"><numFmt numFmtId="0" formatCode=""/><numFmt numFmtId="1" formatCode="#"/></numFmts><cellXfs count="2"><xf numFmtId="1"/><xf numFmtId="0"/></cellXfs></styleSheet>"##,
        &ThemeColors::default(),
    );
    let xml = r#"<worksheet><sheetData><row r="1"><c r="A1" s="0" t="n"><v>0</v></c><c r="B1" s="1" t="n"><v>3</v></c></row></sheetData></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &styles,
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;

    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0].value, Cell::Number(0.0));
    assert_eq!(cells[1].value, Cell::Number(3.0));
    assert_eq!(cells[0].text, "");
    assert_eq!(cells[1].text, "");

    let one_cell_xml = r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="n"><v>3</v></c></row></sheetData></worksheet>"#;
    let exact_cost = retained_cell_cost(&cells[1]);
    assert_eq!(
        exact_cost, RETAINED_CELL_RECORD_BYTES,
        "the explicitly empty format adds no variable bytes"
    );

    // The exact boundary admits the hidden value. One byte less rejects it
    // and leaves the same explicit partial-extraction signal used by other
    // text-budget exhaustion paths.
    let mut budget = exact_cost;
    let parsed = parse_sheet(
        one_cell_xml,
        &[],
        &styles,
        &ThemeColors::default(),
        false,
        &mut budget,
    );
    assert_eq!(parsed.cells.len(), 1);
    assert_eq!(parsed.cells[0].value, Cell::Number(3.0));
    assert_eq!(budget, 0);

    let mut budget = exact_cost.saturating_sub(1);
    let parsed = parse_sheet(
        one_cell_xml,
        &[],
        &styles,
        &ThemeColors::default(),
        false,
        &mut budget,
    );
    assert!(parsed.cells.is_empty());
    assert_eq!(budget, 0);
}

#[test]
fn retained_cell_budget_keeps_ordinary_high_cell_count_sheets_complete() {
    const CELLS: usize = 65_536;
    let row = "<row><c t=\"n\"><v>1</v></c></row>";
    let mut xml = String::with_capacity(
        "<worksheet><sheetData></sheetData></worksheet>".len() + row.len() * CELLS,
    );
    xml.push_str("<worksheet><sheetData>");
    for _ in 0..CELLS {
        xml.push_str(row);
    }
    xml.push_str("</sheetData></worksheet>");

    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        &xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;

    assert_eq!(cells.len(), CELLS);
    let charged = cells
        .iter()
        .map(retained_cell_cost)
        .fold(0usize, usize::saturating_add);
    assert_eq!(budget, crate::MAX_TEXT_BYTES - charged);
    assert!(budget > 0);
}

/// Build a minimal `.xlsx` in memory and read it end-to-end.
#[test]
fn custom_number_formats_are_applied_to_xlsx_display_text() {
    let styles = parse_styles(
        r#"<styleSheet><numFmts count="3"><numFmt numFmtId="164" formatCode="[$₩-412]#,##0.00"/><numFmt numFmtId="165" formatCode="yyyy&quot;년&quot; m&quot;월&quot; d&quot;일&quot;"/><numFmt numFmtId="166" formatCode="0;[Red](0);0;&quot;값: &quot;@"/></numFmts><cellXfs count="3"><xf numFmtId="164"/><xf numFmtId="165"/><xf numFmtId="166"/></cellXfs></styleSheet>"#,
        &ThemeColors::default(),
    );
    let xml = r#"<worksheet><sheetData><row r="1"><c r="A1" s="0"><v>1234.5</v></c><c r="B1" s="1"><v>45366</v></c><c r="C1" s="2" t="inlineStr"><is><t>한글</t></is></c></row></sheetData></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &styles,
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.cells[0].text, "₩1,234.50");
    assert_eq!(parsed.cells[1].text, "2024년 3월 15일");
    assert!(matches!(parsed.cells[1].value, Cell::Date(45_366.0)));
    assert_eq!(parsed.cells[2].text, "값: 한글");
}

/// Build a minimal `.xlsx` in memory and read it end-to-end.
#[test]
fn reads_a_minimal_xlsx() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><workbookPr/><sheets><sheet name="가격" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/sharedStrings.xml", r#"<sst><si><t>품목</t></si></sst>"#),
        (
            "xl/styles.xml",
            r#"<styleSheet><cellXfs><xf numFmtId="0"/><xf numFmtId="14"/></cellXfs></styleSheet>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>42</v></c><c r="C1" s="1"><v>45366</v></c><c r="D1" t="b"><v>1</v></c></row></sheetData></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 1);
    let s = &wb.sheets[0];
    assert_eq!(s.name, "가격");
    assert_eq!(s.cell(0, 0), Some(&Cell::Text("품목".to_string())));
    assert_eq!(s.cell(0, 1), Some(&Cell::Number(42.0)));
    assert_eq!(s.cell(0, 2), Some(&Cell::Date(45366.0))); // numFmt 14 → date
    assert_eq!(s.cell(0, 3), Some(&Cell::Bool(true)));
    assert!(s.to_text().contains("2024-03-15"));
    assert_eq!(s.default_column_width(), None);
    assert_eq!(s.implicit_ooxml_column_width(), Some(None));
    assert_eq!(s.default_row_height(), None);
    assert!(s.has_implicit_ooxml_row_height());
    assert_eq!(
        s.implicit_ooxml_row_height_source(),
        Some(OoxmlImplicitRowHeight::XlsxApplicationDefault)
    );
}

#[test]
fn sheet_format_retains_explicit_and_base_column_width_provenance() {
    let parse = |format: &str| {
        let xml = format!(
            r#"<worksheet>{format}<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
        );
        let mut budget = crate::MAX_TEXT_BYTES;
        parse_sheet(
            &xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
    };

    let absent = parse("");
    assert_eq!(absent.default_col_width, None);
    assert_eq!(absent.base_col_width, None);
    assert!(!absent.defaulted_base_col_width);

    let defaulted_base = parse(r#"<sheetFormatPr/>"#);
    assert_eq!(defaulted_base.default_col_width, None);
    assert_eq!(defaulted_base.base_col_width, None);
    assert!(defaulted_base.defaulted_base_col_width);

    let ignored_non_positive = parse(r#"<sheetFormatPr baseColWidth="0" defaultColWidth="-1"/>"#);
    assert_eq!(ignored_non_positive.default_col_width, None);
    assert_eq!(ignored_non_positive.base_col_width, None);
    assert!(!ignored_non_positive.defaulted_base_col_width);

    let explicit = parse(r#"<sheetFormatPr baseColWidth="8" defaultColWidth="8.43"/>"#);
    assert_eq!(explicit.default_col_width, Some(8.43));
    assert_eq!(explicit.base_col_width, Some(8.0));
    assert!(!explicit.defaulted_base_col_width);
    assert_eq!(
        explicit.imported_default_column_axis_measure,
        Some(ImportedAxisMeasure::CharacterWidthRatio(843, 100))
    );

    let base = parse(r#"<sheetFormatPr baseColWidth="10"/>"#);
    assert_eq!(base.default_col_width, None);
    assert_eq!(base.base_col_width, Some(10.0));
    assert!(!base.defaulted_base_col_width);
    assert_eq!(
        base.imported_default_column_axis_measure,
        Some(ImportedAxisMeasure::CharacterBaseWidth256(10 * 256))
    );
}

#[test]
fn worksheet_retains_exact_twip_and_ooxml_character_axis_sources() {
    let xml = r#"<worksheet><sheetFormatPr defaultRowHeight="15" defaultColWidth="8"/><cols><col min="1" max="2" width="14"/></cols><sheetData><row r="1" ht="18"/><row r="2" hidden="1" ht="12.75"/></sheetData></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(
        parsed.imported_default_row_axis_measure,
        Some(ImportedAxisMeasure::Twips(300))
    );
    assert_eq!(
        parsed.imported_default_column_axis_measure,
        Some(ImportedAxisMeasure::CharacterWidthRatio(8, 1))
    );
    assert_eq!(
        parsed
            .imported_column_axis_measures
            .values()
            .copied()
            .collect::<Vec<_>>(),
        [
            ImportedAxisMeasure::CharacterWidthRatio(14, 1),
            ImportedAxisMeasure::CharacterWidthRatio(14, 1),
        ]
    );
    assert_eq!(
        parsed.imported_row_axis_measures.get(&0),
        Some(&ImportedAxisMeasure::Twips(360))
    );
    assert_eq!(
        parsed.imported_row_axis_measures.get(&1),
        Some(&ImportedAxisMeasure::Twips(255))
    );
    assert!(parsed.hidden_rows.contains(&1));
}

#[test]
fn worksheet_retains_decimal_and_scientific_axis_sources_without_float_drift() {
    let xml = r#"<worksheet><sheetFormatPr defaultRowHeight=" 1.5E1 " defaultColWidth="8.43"/><cols><col min="1" max="1" width="8.43"/></cols><sheetData><row r="1" ht="1.2345E1"/></sheetData></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.default_row_height, Some(15.0));
    assert_eq!(
        parsed.imported_default_row_axis_measure,
        Some(ImportedAxisMeasure::Twips(300))
    );
    assert_eq!(
        parsed.imported_default_column_axis_measure,
        Some(ImportedAxisMeasure::CharacterWidthRatio(843, 100))
    );
    assert_eq!(
        parsed.imported_column_axis_measures.get(&0),
        Some(&ImportedAxisMeasure::CharacterWidthRatio(843, 100))
    );
    assert_eq!(
        parsed.imported_row_axis_measures.get(&0),
        Some(&ImportedAxisMeasure::PointRatio(2_469, 200))
    );
}

#[test]
fn worksheet_keeps_valid_row_heights_when_exact_provenance_is_unrepresentable() {
    let source = "15.1234567890123456789012345";
    let xml = format!(
        r#"<worksheet><sheetFormatPr defaultRowHeight="{source}"/><sheetData><row r="1" ht="{source}"/></sheetData></worksheet>"#
    );
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        &xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );
    let expected = source.parse::<f32>().expect("finite height");

    assert_eq!(parsed.default_row_height, Some(expected));
    assert_eq!(parsed.row_heights.get(&0), Some(&expected));
    assert_eq!(parsed.imported_default_row_axis_measure, None);
    assert!(!parsed.imported_row_axis_measures.contains_key(&0));
    assert!(parsed.automatic_default_row_height_candidate);
    assert!(parsed.automatic_row_height_candidates.contains(&0));
}

#[test]
fn worksheet_row_height_manuality_tracks_custom_height_separately() {
    let xml = r#"<worksheet><sheetData>
            <row r="1" ht="18" customHeight="1"/>
            <row r="2" ht="19" customHeight="true"/>
            <row r="3" ht="20" customHeight="0"/>
            <row r="4" ht="21" customHeight="false"/>
            <row r="5" ht="22"/>
            <row r="6" ht="23" customHeight="malformed"/>
            <row r="7" ht="NaN" customHeight="1"/>
            <row r="8" ht="-1" customHeight="1"/>
            <row r="9" ht="1e309" customHeight="1"/>
            <row r="10" ht="0" customHeight="1"/>
            <row r="1048576" ht="24" customHeight="TRUE"/>
            <row r="1048577" ht="25" customHeight="1"/>
        </sheetData></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(
        parsed.row_heights.keys().copied().collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 5, MAX_XLSX_ROW_INDEX]
    );
    assert_eq!(
        parsed
            .imported_row_axis_measures
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 5, MAX_XLSX_ROW_INDEX]
    );
    assert_eq!(
        parsed
            .automatic_row_height_candidates
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [2, 3, 4, 5]
    );
}

#[test]
fn imported_xlsx_exposes_row_height_manuality_to_renderers() {
    let workbook = workbook_with_worksheet_xml(
        r#"<worksheet><sheetData>
                <row r="1" ht="18" customHeight="1"/>
                <row r="2" ht="19" customHeight="false"/>
                <row r="3" ht="20"/>
                <row r="4" customHeight="1"/>
            </sheetData></worksheet>"#,
    );
    let sheet = &workbook.sheets[0];

    assert!(sheet.row_height_is_manual(0));
    assert!(!sheet.row_height_is_manual(1));
    assert!(!sheet.row_height_is_manual(2));
    assert!(!sheet.row_height_is_manual(3));
    assert!(!sheet.row_height_is_manual(4));
}

#[test]
fn imported_xlsx_exposes_default_row_height_manuality_to_renderers() {
    for (attributes, expected_manual) in [
        (r#"defaultRowHeight="15" customHeight="1""#, true),
        (r#"defaultRowHeight="15" customHeight="true""#, true),
        (r#"defaultRowHeight="15" customHeight="0""#, false),
        (r#"defaultRowHeight="15" customHeight="false""#, false),
        (r#"defaultRowHeight="15""#, false),
        (r#"defaultRowHeight="15" customHeight="malformed""#, false),
    ] {
        let workbook = workbook_with_worksheet_xml(&format!(
            r#"<worksheet><sheetFormatPr {attributes}/><sheetData/></worksheet>"#
        ));
        let sheet = &workbook.sheets[0];

        assert_eq!(sheet.default_row_height(), Some(15.0), "{attributes}");
        assert_eq!(
            sheet.default_row_height_is_manual(),
            expected_manual,
            "{attributes}"
        );
    }

    let workbook = workbook_with_worksheet_xml(r#"<worksheet><sheetData/></worksheet>"#);
    assert!(!workbook.sheets[0].default_row_height_is_manual());

    for invalid_height in ["NaN", "-1", "0", "1e309"] {
        let workbook = workbook_with_worksheet_xml(&format!(
            r#"<worksheet><sheetFormatPr defaultRowHeight="{invalid_height}" customHeight="1"/><sheetData/></worksheet>"#
        ));
        let sheet = &workbook.sheets[0];

        assert_eq!(sheet.default_row_height(), None, "{invalid_height}");
        assert_eq!(
            sheet.imported_default_row_axis_measure(),
            None,
            "{invalid_height}"
        );
        assert!(!sheet.default_row_height_is_manual(), "{invalid_height}");
    }
}

#[test]
fn worksheet_invalid_column_widths_keep_legacy_values_without_exact_provenance() {
    let xml = r#"<worksheet><cols>
            <col min="1" max="1" width="0" style="1"/>
            <col min="2" max="2" width="NaN" hidden="1" style="1"/>
        </cols><sheetData/></worksheet>"#;
    let styles = parse_styles(
        r#"<styleSheet><cellXfs count="2"><xf/><xf applyAlignment="1"><alignment wrapText="1"/></xf></cellXfs></styleSheet>"#,
        &ThemeColors::default(),
    );
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &styles,
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.col_widths.get(&0), Some(&0.0));
    assert!(parsed
        .col_widths
        .get(&1)
        .is_some_and(|width| width.is_nan()));
    assert!(parsed.imported_column_axis_measures.is_empty());
    assert_eq!(
        parsed.col_formats.keys().copied().collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(parsed.hidden_cols.iter().copied().collect::<Vec<_>>(), [1]);
}

#[test]
fn worksheet_implicit_cell_columns_fail_closed_outside_the_ooxml_grid() {
    let xml = r#"<worksheet><sheetData><row r="1">
            <c r="XFD1" t="inlineStr"><is><t>last</t></is></c>
            <c t="inlineStr"><is><t>overflow</t></is></c>
            <c t="inlineStr"><is><t>still-overflow</t></is></c>
            <c r="A1" t="inlineStr"><is><t>resynced</t></is></c>
            <c t="inlineStr"><is><t>next</t></is></c>
            <c r="XFE1" t="inlineStr"><is><t>invalid-explicit</t></is></c>
            <c t="inlineStr"><is><t>poisoned</t></is></c>
            <c r="C1" t="inlineStr"><is><t>resynced-again</t></is></c>
        </row></sheetData></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(
        parsed
            .cells
            .iter()
            .map(|cell| (cell.row, cell.col, cell.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (0, MAX_XLSX_COLUMN_INDEX, "last"),
            (0, 0, "resynced"),
            (0, 1, "next"),
            (0, 2, "resynced-again"),
        ]
    );
}

#[test]
fn worksheet_outline_levels_are_bounded_to_the_ooxml_depth() {
    let xml = r#"<worksheet><cols>
            <col min="1" max="1" outlineLevel="0"/>
            <col min="2" max="2" outlineLevel="7"/>
            <col min="3" max="3" outlineLevel="8"/>
        </cols><sheetData>
            <row r="1" outlineLevel="0"/>
            <row r="2" outlineLevel="7"/>
            <row r="3" outlineLevel="8"/>
        </sheetData></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.col_outline.into_iter().collect::<Vec<_>>(), [(1, 7)]);
    assert_eq!(parsed.row_outline.into_iter().collect::<Vec<_>>(), [(1, 7)]);
}

#[test]
fn worksheet_rows_and_implicit_cells_fail_closed_outside_the_ooxml_grid() {
    let xml = r#"<worksheet><sheetData>
            <row r="1048576" ht="18" hidden="1" outlineLevel="1" collapsed="1" s="1"><c t="inlineStr"><is><t>last</t></is></c></row>
            <row ht="19" outlineLevel="2" collapsed="1" s="1"><c r="A1" t="inlineStr"><is><t>invalid-explicit-cell</t></is></c><c t="inlineStr"><is><t>invalid-implicit-cell</t></is></c></row>
            <row ht="20"><c t="inlineStr"><is><t>still-invalid-implicit-row</t></is></c></row>
            <row r="1048577" ht="22" hidden="1" outlineLevel="4" collapsed="1" s="1"><c t="inlineStr"><is><t>invalid-explicit-row</t></is></c></row>
            <row r="1" ht="21" outlineLevel="3" collapsed="1" s="1"><c t="inlineStr"><is><t>resynced</t></is></c><c r="A1048577" t="inlineStr"><is><t>invalid-cell-ref</t></is></c></row>
        </sheetData></worksheet>"#;
    let styles = parse_styles(
        r#"<styleSheet><cellXfs count="2"><xf/><xf applyAlignment="1"><alignment wrapText="1"/></xf></cellXfs></styleSheet>"#,
        &ThemeColors::default(),
    );
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &styles,
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(
        parsed
            .cells
            .iter()
            .map(|cell| (cell.row, cell.col, cell.text.as_str()))
            .collect::<Vec<_>>(),
        [(MAX_XLSX_ROW_INDEX, 0, "last"), (0, 0, "resynced"),]
    );
    assert_eq!(
        parsed.row_heights.keys().copied().collect::<Vec<_>>(),
        [0, MAX_XLSX_ROW_INDEX]
    );
    assert_eq!(
        parsed
            .imported_row_axis_measures
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [0, MAX_XLSX_ROW_INDEX]
    );
    assert_eq!(
        parsed.row_outline.keys().copied().collect::<Vec<_>>(),
        [0, MAX_XLSX_ROW_INDEX]
    );
    assert_eq!(
        parsed.collapsed_rows.iter().copied().collect::<Vec<_>>(),
        [0, MAX_XLSX_ROW_INDEX]
    );
    assert_eq!(
        parsed.hidden_rows.iter().copied().collect::<Vec<_>>(),
        [MAX_XLSX_ROW_INDEX]
    );
    assert_eq!(
        parsed
            .explicit_visible_rows
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [0]
    );
    assert_eq!(
        parsed.row_formats.keys().copied().collect::<Vec<_>>(),
        [0, MAX_XLSX_ROW_INDEX]
    );
}

#[test]
fn sheet_format_zero_height_retains_explicit_visible_row_exceptions() {
    let xml = r#"<worksheet><sheetFormatPr zeroHeight="1"/><sheetData><row r="2"/><row r="3" hidden="1"/><row r="5" hidden="0"/></sheetData></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert!(parsed.default_rows_hidden);
    assert_eq!(
        parsed
            .explicit_visible_rows
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [1, 4]
    );
    assert_eq!(parsed.hidden_rows.iter().copied().collect::<Vec<_>>(), [2]);
}

#[test]
fn malformed_zip_container_reports_zip_error_not_biff() {
    let err = Workbook::open(b"PK\x03\x04 truncated").unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid ZIP package: not a valid spreadsheet ZIP container"
    );
}

#[test]
fn reads_xlsx_with_backslash_and_case_variant_package_paths() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl\\workbook.xml",
            r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl\\_rels\\workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets\sheet1.xml"/></Relationships>"#,
        ),
        ("xl\\sharedstrings.xml", r#"<sst><si><t>ok</t></si></sst>"#),
        (
            "xl\\worksheets\\sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 1);
    let s = &wb.sheets[0];
    assert_eq!(s.name, "Data");
    assert_eq!(s.cell(0, 0), Some(&Cell::Text("ok".to_string())));
}

#[test]
fn case_insensitive_part_lookup_is_fail_closed_when_ambiguous() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in [
        ("xl/SharedStrings.xml", "first"),
        ("xl/SHAREDSTRINGS.XML", "second"),
    ] {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();

    assert_eq!(part(&mut zip, "xl/sharedStrings.xml"), None);
    assert_eq!(
        part(&mut zip, "xl/SharedStrings.xml").as_deref(),
        Some("first")
    );
}

#[test]
fn reads_xlsx_with_root_office_document_part_and_above_root_target() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "_rels/.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="../../workbook.xml"/></Relationships>"#,
        ),
        (
            "workbook.xml",
            r#"<workbook><workbookPr/><sheets><sheet name="Root" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="sheet1.xml"/></Relationships>"#,
        ),
        (
            "sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>root</t></is></c></row></sheetData></worksheet>"#,
        ),
        ("styles.xml", r#"<styleSheet/>"#),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 1);
    let s = &wb.sheets[0];
    assert_eq!(s.name, "Root");
    assert_eq!(s.cell(0, 0), Some(&Cell::Text("root".to_string())));
}

#[test]
fn workbook_sheet_visibility_and_defined_names() {
    // `<sheet state>` carries visibility; `<definedNames>` carry workbook-global
    // names. A built-in `_xlnm.*` name is skipped; a user name is kept.
    let xml = r#"<workbook>
            <sheets>
                <sheet name="Vis" r:id="rId1"/>
                <sheet name="Hid" state="hidden" r:id="rId2"/>
                <sheet name="VHid" state="veryHidden" r:id="rId3"/>
            </sheets>
            <definedNames>
                <definedName name="TaxRate">Sheet1!$B$1</definedName>
                <definedName name="_xlnm.Print_Area" localSheetId="0">Sheet1!$A$1:$C$3</definedName>
                <definedName name="LocalOnly" localSheetId="1">Sheet2!$A$1</definedName>
            </definedNames>
        </workbook>"#;
    let parsed = parse_workbook(xml);
    assert_eq!(parsed.sheets.len(), 3);
    assert_eq!(parsed.sheets[0].visibility, Visibility::Visible);
    assert_eq!(parsed.sheets[1].visibility, Visibility::Hidden);
    assert_eq!(parsed.sheets[2].visibility, Visibility::VeryHidden);
    // Global and local user names remain distinct; the built-in print area
    // stays in the sheet-metadata path.
    assert_eq!(
        parsed.defined_names,
        vec![("TaxRate".to_string(), "Sheet1!$B$1".to_string())]
    );
    assert_eq!(
        parsed.local_defined_names,
        vec![crate::LocalDefinedName {
            sheet: "Hid".to_string(),
            name: "LocalOnly".to_string(),
            refers_to: "Sheet2!$A$1".to_string(),
        }]
    );
}

/// End-to-end `.xlsx` read: a hidden sheet + a defined name surface via the
/// public `is_hidden()` / `defined_names()` accessors.
#[test]
fn hidden_sheet_and_defined_name_end_to_end() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/><sheet name="Secret" state="hidden" r:id="rId2"/></sheets><definedNames><definedName name="TaxRate">Data!$A$1</definedName></definedNames></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="worksheets/sheet2.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData/></worksheet>"#,
        ),
        (
            "xl/worksheets/sheet2.xml",
            r#"<worksheet><sheetData/></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 2);
    assert!(!wb.sheets[0].is_hidden(), "Data is visible");
    assert!(wb.sheets[1].is_hidden(), "Secret is hidden");
    assert!(!wb.sheets[1].is_very_hidden());
    assert_eq!(
        wb.defined_names(),
        &[("TaxRate".to_string(), "Data!$A$1".to_string())]
    );
}

#[test]
fn chart_series_refs_retain_bounded_caches_and_theme_palette_sidecar() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rIdTheme" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/></Relationships>"#,
        ),
        (
            "xl/theme/theme1.xml",
            r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:accent1><a:srgbClr val="010203"/></a:accent1><a:accent2><a:srgbClr val="A0B0C0"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme><a:majorFont><a:latin typeface="Ignored Major"/></a:majorFont><a:minorFont><a:latin typeface="Source Sans 3"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData/><drawing r:id="rIdDraw"/></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
        ),
        (
            "xl/drawings/drawing1.xml",
            r#"<wsDr>
                    <twoCellAnchor>
                        <from><col>2</col><row>4</row></from>
                        <to><col>8</col><row>16</row></to>
                        <graphicFrame>
                            <graphic>
                                <graphicData>
                                    <chart r:id="rIdChart"/>
                                </graphicData>
                            </graphic>
                        </graphicFrame>
                    </twoCellAnchor>
                </wsDr>"#,
        ),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
        ),
        (
            "xl/charts/chart1.xml",
            r#"<chartSpace><chart><plotArea><lineChart><ser>
                    <tx><strRef><f>Data!$C$1</f><strCache><pt idx="0"><v>Cached Series</v></pt></strCache></strRef></tx>
                    <marker><symbol val="circle"/><size val="5"/></marker>
                    <spPr><a:ln w="38100"><a:solidFill><a:schemeClr val="accent2"/></a:solidFill></a:ln></spPr>
                    <cat><strRef><f>Data!$A$2:$A$4</f><strCache><pt idx="0"><v>Q1</v></pt><pt idx="1"><v>Q2</v></pt><pt idx="2"><v>Q3</v></pt></strCache></strRef></cat>
                    <val><numRef><f>Data!$B$2:$B$4</f><numCache><pt idx="0"><v>10</v></pt><pt idx="1"><v>20</v></pt><pt idx="2"><v>30</v></pt></numCache></numRef></val>
                </ser></lineChart></plotArea></chart></chartSpace>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    let charts = wb.sheets[0].charts();

    assert_eq!(charts.len(), 1);
    assert_eq!(charts[0].kind, ChartKind::Line);
    assert_eq!(charts[0].from, (4, 2));
    assert_eq!(charts[0].to, (16, 8));
    assert_eq!(charts[0].series.len(), 1);
    assert_eq!(charts[0].series[0].name.as_deref(), Some("Data!$C$1"));
    assert_eq!(
        charts[0].series[0].categories.as_deref(),
        Some("Data!$A$2:$A$4")
    );
    assert_eq!(charts[0].series[0].values, "Data!$B$2:$B$4");
    let sidecar = wb.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("chart rendering sidecar");
    assert_eq!(sidecar.chart_palette[0], Color::rgb(1, 2, 3));
    assert_eq!(sidecar.chart_palette[1], Color::rgb(160, 176, 192));
    assert_eq!(
        sidecar.chart_default_latin_font_family.as_deref(),
        Some("Source Sans 3")
    );
    assert_eq!(sidecar.chart_series_caches.len(), 1);
    assert_eq!(sidecar.chart_series_styles.len(), 1);
    assert_eq!(
        sidecar.chart_series_styles[0].marker,
        ChartMarkerSymbol::Circle
    );
    assert_eq!(sidecar.chart_series_styles[0].marker_size, Some(5));
    assert!(sidecar.chart_series_styles[0].line_visible);
    assert_eq!(
        sidecar.chart_series_styles[0].line_color,
        Some(Color::rgb(160, 176, 192))
    );
    assert_eq!(sidecar.chart_series_styles[0].line_width_emu, Some(38_100));
    assert!(sidecar.chart_series_styles[0].losses.is_empty());
    let cache = &sidecar.chart_series_caches[0];
    assert_eq!(cache.name[0].value, "Cached Series");
    assert_eq!(
        cache
            .categories
            .iter()
            .map(|point| point.value.as_str())
            .collect::<Vec<_>>(),
        ["Q1", "Q2", "Q3"]
    );
    assert_eq!(
        cache
            .values
            .iter()
            .map(|point| point.value.as_str())
            .collect::<Vec<_>>(),
        ["10", "20", "30"]
    );
}

#[test]
fn chart_sidecar_uses_calc_latin_fallback_when_package_theme_is_missing() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData/><drawing r:id="rIdDraw"/></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
        ),
        (
            "xl/drawings/drawing1.xml",
            r#"<wsDr><twoCellAnchor><from><col>2</col><row>4</row></from><to><col>8</col><row>16</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#,
        ),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
        ),
        (
            "xl/charts/chart1.xml",
            r#"<chartSpace><chart><plotArea><lineChart/></plotArea></chart></chartSpace>"#,
        ),
    ];
    for (name, body) in parts {
        writer.start_file(name, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }

    let bytes = writer.finish().unwrap().into_inner();
    let workbook = Workbook::open(&bytes).unwrap();
    let sidecar = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("chart rendering sidecar");

    assert_eq!(
        sidecar.chart_default_latin_font_family.as_deref(),
        Some(CALC_IMPORTED_CHART_LATIN_FONT_FAMILY)
    );
}

#[test]
fn minor_theme_latin_font_family_is_trimmed_and_bounded() {
    let theme = parse_theme(&complete_theme_xml(
        "Ignored Major",
        "  Theme Sans  ",
        "4472C4",
        "ED7D31",
    ));
    assert!(theme.source_valid);
    assert_eq!(theme.minor_latin_font_family.as_deref(), Some("Theme Sans"));

    let boundary = "x".repeat(MAX_IMPORTED_CHART_LATIN_FONT_FAMILY_BYTES);
    let xml = complete_theme_xml("Major", &boundary, "4472C4", "ED7D31");
    assert_eq!(
        parse_theme(&xml).minor_latin_font_family.as_deref(),
        Some(boundary.as_str())
    );

    assert_eq!(
        parse_theme(r#"<a:theme><a:themeElements><a:fontScheme/></a:themeElements></a:theme>"#)
            .chart_default_latin_font_family(),
        CALC_IMPORTED_CHART_LATIN_FONT_FAMILY
    );

    for invalid in [String::new(), " ".to_string(), "x".repeat(256)] {
        let xml = complete_theme_xml("Major", &invalid, "4472C4", "ED7D31");
        let theme = parse_theme(&xml);
        assert!(!theme.source_valid);
        assert!(theme.minor_latin_font_family.is_none());
        assert_eq!(
            theme.chart_default_latin_font_family(),
            CALC_IMPORTED_CHART_LATIN_FONT_FAMILY
        );
    }
}

#[test]
fn theme_requires_complete_structural_and_namespace_exact_source() {
    let valid = complete_theme_xml("Major", "Minor", "4472C4", "ED7D31");
    assert!(parse_theme(&valid).source_valid);

    let missing_slot = valid.replace(r#"<a:accent6><a:srgbClr val="70AD47"/></a:accent6>"#, "");
    let missing_scheme = valid
        .replace("<a:fontScheme>", "<a:notFontScheme>")
        .replace("</a:fontScheme>", "</a:notFontScheme>");
    let duplicate_slot = valid.replace(
        "<a:accent1>",
        r#"<a:accent1><a:srgbClr val="010203"/></a:accent1><a:accent1>"#,
    );
    let wrong_namespace = valid.replace(
        OOXML_DRAWING_NAMESPACE_TRANSITIONAL,
        OOXML_CHART_NAMESPACE_TRANSITIONAL,
    );
    let foreign_paint = valid
        .replace(
            "<a:theme ",
            r#"<a:theme xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" "#,
        )
        .replacen(
            r#"<a:srgbClr val="E7E6E6"/>"#,
            r#"<c:srgbClr val="E7E6E6"/>"#,
            1,
        );
    let wrapped_scheme = valid
        .replacen("<a:clrScheme>", "<a:wrapper><a:clrScheme>", 1)
        .replacen("</a:clrScheme>", "</a:clrScheme></a:wrapper>", 1);
    let hash_rgb = valid.replacen("4472C4", "#4472C4", 1);
    let argb = valid.replacen("4472C4", "FF4472C4", 1);
    let malformed = valid.trim_end_matches("</a:theme>").to_string();

    for invalid in [
        missing_slot,
        missing_scheme,
        duplicate_slot,
        wrong_namespace,
        foreign_paint,
        wrapped_scheme,
        hash_rgb,
        argb,
        malformed,
    ] {
        assert!(!parse_theme(&invalid).source_valid, "{invalid}");
    }
}

#[test]
fn chart_rgb_is_exact_and_drawingml_tint_endpoints_are_correct() {
    assert_eq!(
        parse_chart_rgb("112233"),
        Some(Color::rgb(0x11, 0x22, 0x33))
    );
    for invalid in ["#112233", " 112233", "112233 ", "FF112233", "11223G"] {
        assert_eq!(parse_chart_rgb(invalid), None, "{invalid}");
    }

    let source = Color::rgb(0x20, 0x40, 0x80);
    assert_eq!(apply_chart_luminance(source, 100_000, 0), source);
    let white = Color::rgb(255, 255, 255);
    assert_eq!(apply_chart_luminance(source, 0, 100_000), white);
    let midpoint = apply_chart_luminance(source, 50_000, 50_000);
    assert_ne!(midpoint, source);
    assert_ne!(midpoint, white);
}

#[test]
fn foreign_chart_markup_and_alternate_content_are_never_applied_silently() {
    fn reasons(xml: &str) -> Vec<ChartUnsupportedReason> {
        let mut cache_points = 16;
        let mut chart_series = 16;
        parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series)
            .expect("local-name parser still identifies the chart")
            .unsupported_reasons
    }

    let foreign_kind = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:evil="urn:evil"><c:chart><c:plotArea><evil:pieChart/></c:plotArea></c:chart></c:chartSpace>"#;
    assert!(reasons(foreign_kind).contains(&ChartUnsupportedReason::UnsupportedMarkup));

    let foreign_val = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><c:chart><c:plotArea><c:pieChart><c:varyColors a:val="0"/></c:pieChart></c:plotArea></c:chart></c:chartSpace>"#;
    assert!(reasons(foreign_val).contains(&ChartUnsupportedReason::UnsupportedMarkup));

    let alternate = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><c:chart><c:plotArea><mc:AlternateContent><mc:Choice Requires="c"><c:pieChart/></mc:Choice><mc:Fallback><c:barChart/></mc:Fallback></mc:AlternateContent></c:plotArea></c:chart></c:chartSpace>"#;
    assert!(reasons(alternate).contains(&ChartUnsupportedReason::UnsupportedMarkup));
}

#[test]
fn imported_chart_title_retains_uniform_painted_run_style() {
    let theme = parse_theme(&complete_theme_xml(
        "Major Face",
        "Minor Face",
        "4472C4",
        "ED7D31",
    ));
    assert!(theme.source_valid);
    let xml = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><c:chart><c:title><c:tx><c:rich>
            <a:bodyPr/><a:lstStyle/><a:p><a:pPr><a:defRPr sz="1000" b="0">
                <a:latin typeface="Calibri"/>
            </a:defRPr></a:pPr><a:r><a:rPr sz="2400" b="1" i="0" u="none" strike="noStrike">
                <a:solidFill><a:sysClr val="windowText" lastClr="000000"/></a:solidFill>
                <a:latin typeface="Eurostile"/>
            </a:rPr><a:t>Sales</a:t></a:r>
            <a:endParaRPr sz="2400" b="1" u="sng" strike="sngStrike"><a:latin typeface="Eurostile"/></a:endParaRPr>
            </a:p></c:rich></c:tx></c:title><c:plotArea><c:lineChart><c:axId val="1"/><c:axId val="2"/></c:lineChart><c:catAx><c:axId val="1"/><c:crossAx val="2"/></c:catAx><c:valAx><c:axId val="2"/><c:crossAx val="1"/></c:valAx></c:plotArea></c:chart></c:chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart_with_theme(
        xml,
        (0, 0),
        (10, 5),
        &mut cache_points,
        &mut chart_series,
        &theme,
    )
    .unwrap();

    assert!(parsed.unsupported_reasons.is_empty());
    assert_eq!(parsed.chart.title.as_deref(), Some("Sales"));
    assert_eq!(
        parsed.text_styles.chart_title,
        Some(ChartTextStyle {
            latin_font_family: "Eurostile".to_string(),
            size_hundredths_of_point: 2_400,
            color: Color::rgb(0, 0, 0),
            bold: true,
            italic: false,
            underline: false,
            strikethrough: false,
            kerning_minimum_hundredths_of_point: None,
            rotation_degrees: None,
        })
    );
}

#[test]
fn imported_chart_text_resolves_theme_roles_and_rejects_mixed_runs() {
    let theme = parse_theme(&complete_theme_xml(
        "Major Face",
        "Minor Face",
        "4472C4",
        "ED7D31",
    ));
    assert!(theme.source_valid);
    let uniform = r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><title><tx><rich><a:bodyPr/><a:lstStyle/><a:p>
            <a:r><a:rPr sz="1400"><a:latin typeface="+mj-lt"/></a:rPr><a:t>A</a:t></a:r>
            <a:r><a:rPr sz="1400"><a:latin typeface="+mj-lt"/></a:rPr><a:t>B</a:t></a:r>
            </a:p></rich></tx></title><plotArea><lineChart><axId val="1"/><axId val="2"/></lineChart><catAx><axId val="1"/><crossAx val="2"/></catAx><valAx><axId val="2"/><crossAx val="1"/></valAx></plotArea></chart></chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart_with_theme(
        uniform,
        (0, 0),
        (10, 5),
        &mut cache_points,
        &mut chart_series,
        &theme,
    )
    .unwrap();
    let style = parsed.text_styles.chart_title.unwrap();
    assert_eq!(style.latin_font_family, "Major Face");
    assert_eq!(style.size_hundredths_of_point, 1_400);
    assert!(parsed.unsupported_reasons.is_empty());

    let mixed = uniform.replacen(
        r#"sz="1400"><a:latin typeface="+mj-lt"/></a:rPr><a:t>B"#,
        r#"sz="1600"><a:latin typeface="+mn-lt"/></a:rPr><a:t>B"#,
        1,
    );
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart_with_theme(
        &mixed,
        (0, 0),
        (10, 5),
        &mut cache_points,
        &mut chart_series,
        &theme,
    )
    .unwrap();
    assert!(parsed.text_styles.chart_title.is_none());
    assert_eq!(
        parsed.unsupported_reasons,
        [ChartUnsupportedReason::MixedTextStyle]
    );
}

#[test]
fn imported_horizontal_bar_maps_semantic_axis_titles_after_direction() {
    let xml = r#"<chartSpace><chart><plotArea><barChart><barDir val="bar"/></barChart>
            <catAx><title><tx><rich><a:bodyPr/><a:p><a:r><a:rPr sz="900"><a:latin typeface="Category Face"/></a:rPr><a:t>Category</a:t></a:r></a:p></rich></tx></title></catAx>
            <valAx><title><tx><rich><a:bodyPr/><a:p><a:r><a:rPr sz="1200"><a:latin typeface="Value Face"/></a:rPr><a:t>Value</a:t></a:r></a:p></rich></tx></title></valAx>
            </plotArea></chart></chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();

    assert_eq!(parsed.chart.x_axis_title.as_deref(), Some("Value"));
    assert_eq!(parsed.chart.y_axis_title.as_deref(), Some("Category"));
    assert_eq!(
        parsed
            .text_styles
            .category_axis_title
            .as_ref()
            .map(|style| style.latin_font_family.as_str()),
        Some("Category Face")
    );
    assert_eq!(
        parsed
            .text_styles
            .value_axis_title
            .as_ref()
            .map(|style| style.latin_font_family.as_str()),
        Some("Value Face")
    );
}

#[test]
fn imported_chart_text_enforces_size_family_and_decoration_boundaries() {
    for size in ["100", "400000"] {
        assert_eq!(chart_text_bounded_size(size), Some(size.parse().unwrap()));
    }
    for size in ["99", "400001", "-1", "large"] {
        assert_eq!(chart_text_bounded_size(size), None);
    }
    assert!(bounded_imported_chart_latin_font_family(&"x".repeat(255)).is_some());
    assert!(bounded_imported_chart_latin_font_family(&"x".repeat(256)).is_none());

    for attributes in [
        r#"sz="99""#,
        r#"sz="400001""#,
        r#"u="dbl""#,
        r#"strike="dblStrike""#,
        r#"baseline="30000""#,
        r#"spc="150""#,
    ] {
        let xml = format!(
            r#"<chartSpace><chart><title><tx><rich><a:bodyPr/><a:p><a:r><a:rPr {attributes}><a:latin typeface="Face"/></a:rPr><a:t>X</a:t></a:r></a:p></rich></tx></title><plotArea><lineChart/></plotArea></chart></chartSpace>"#
        );
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert!(parsed
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::UnsupportedTextStyle));
    }
}

#[test]
fn chart_data_label_visibility_is_effective_and_extensions_fail_closed() {
    for (labels, expected_visible, expected_unsupported) in [
        ("<dLbls/>", false, false),
        ("<dLbls><showVal val=\"0\"/></dLbls>", false, false),
        ("<dLbls><showVal/></dLbls>", true, false),
        ("<dLbls><showVal/><delete/></dLbls>", false, false),
        ("<dLbls><showCatName/></dLbls>", false, true),
        (
            "<dLbls><extLst><ext><showVal/></ext></extLst></dLbls>",
            false,
            true,
        ),
        (
            "<dLbls><showVal/><dLbl><idx val=\"0\"/><delete/></dLbl></dLbls>",
            false,
            true,
        ),
        ("<dLbls><showVal val=\"TRUE\"/></dLbls>", false, true),
    ] {
        let xml = format!(
                "<chartSpace><chart><plotArea><pieChart>{labels}</pieChart></plotArea></chart></chartSpace>"
            );
        assert_eq!(
            parse_chart_data_labels(&xml),
            (expected_visible, expected_unsupported),
            "{labels}"
        );
    }
}

#[test]
fn chart_axis_semantics_follow_plot_ids_visibility_and_cross_links() {
    let xml = r#"<chartSpace><chart><plotArea>
            <scatterChart><axId val="10"/><axId val="20"/></scatterChart>
            <valAx><axId val="20"/><majorGridlines/><delete val="0"/><crossAx val="10"/></valAx>
            <valAx><axId val="10"/><delete/><crossAx val="20"/></valAx>
        </plotArea></chart></chartSpace>"#;
    let semantics = parse_chart_axis_semantics(xml);
    assert!(!semantics.unsupported_topology);
    assert!(!semantics.invalid_visibility);
    assert_eq!(
        semantics.axis_roles,
        [ChartAxisContext::Value, ChartAxisContext::Category]
    );
    assert_eq!(semantics.category_visible, Some(false));
    assert_eq!(semantics.value_visible, Some(true));
    assert!(!semantics.category_major_gridlines);
    assert!(semantics.value_major_gridlines);

    let invalid_cross = xml.replace(r#"crossAx val="10""#, r#"crossAx val="99""#);
    assert!(parse_chart_axis_semantics(&invalid_cross).unsupported_topology);

    for (plot, unsupported) in [
        ("<lineChart/>", true),
        ("<scatterChart/>", true),
        ("<pieChart/>", false),
        ("<doughnutChart/>", false),
    ] {
        let xml = format!("<chartSpace><chart><plotArea>{plot}</plotArea></chart></chartSpace>");
        assert_eq!(
            parse_chart_axis_semantics(&xml).unsupported_topology,
            unsupported,
            "{plot}"
        );
    }
}

#[test]
fn chart_axis_generated_defaults_preserve_supported_chart_semantics() {
    fn parsed_chart(xml: &str) -> ParsedChart {
        let mut cache_points = 16;
        let mut chart_series = 16;
        parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).expect("chart")
    }

    let default_axes = r#"<catAx><axId val="1"/><scaling><orientation val="minMax"/></scaling><delete val="0"/><axPos val="b"/><tickLblPos val="nextTo"/><crossAx val="2"/><crosses val="autoZero"/><auto val="1"/><lblAlgn val="ctr"/><lblOffset val="100"/></catAx><valAx><axId val="2"/><scaling><orientation val="minMax"/></scaling><delete val="0"/><axPos val="l"/><numFmt formatCode="General" sourceLinked="1"/><tickLblPos val="nextTo"/><crossAx val="1"/><crosses val="autoZero"/><crossBetween val="between"/></valAx>"#;
    for (plot, expected_kind) in [
        (
            r#"<lineChart><grouping val="standard"/><varyColors val="0"/><axId val="1"/><axId val="2"/></lineChart>"#,
            ChartKind::Line,
        ),
        (
            r#"<barChart><barDir val="col"/><grouping val="clustered"/><axId val="1"/><axId val="2"/></barChart>"#,
            ChartKind::Bar,
        ),
    ] {
        let xml = format!(
            "<chartSpace><chart><plotArea>{plot}{default_axes}</plotArea></chart></chartSpace>"
        );
        let semantics = parse_chart_axis_semantics(&xml);
        assert!(!semantics.unsupported_topology, "{expected_kind:?}");
        assert!(!semantics.unsupported_presentation, "{expected_kind:?}");
        assert_eq!(semantics.category_visible, Some(true));
        assert_eq!(semantics.value_visible, Some(true));
        assert_eq!(semantics.category_axis_shifted, Some(true));
        let parsed = parsed_chart(&xml);
        assert_eq!(parsed.chart.kind, expected_kind);
        assert_eq!(parsed.category_axis_shifted, Some(true));
        assert!(
            parsed.unsupported_reasons.is_empty(),
            "{expected_kind:?}: {:?}",
            parsed.unsupported_reasons
        );
    }

    let pie = parsed_chart(
        r#"<chartSpace><chart><plotArea><pieChart><varyColors val="1"/></pieChart></plotArea></chart></chartSpace>"#,
    );
    assert_eq!(pie.chart.kind, ChartKind::Pie);
    assert!(pie.unsupported_reasons.is_empty());
}

#[test]
fn chart_category_shifted_position_replays_cross_between_and_calc_defaults() {
    let axes = r#"<catAx><axId val="1"/><axPos val="b"/><crossAx val="2"/></catAx><valAx><axId val="2"/><axPos val="l"/><crossAx val="1"/>"#;
    let base = |cross_between: &str| {
        format!(
                "<chartSpace><chart><plotArea><lineChart><axId val=\"1\"/><axId val=\"2\"/></lineChart>{axes}{cross_between}</valAx></plotArea></chart></chartSpace>"
            )
    };

    let between = parse_chart_axis_semantics(&base("<crossBetween val=\"between\"/>"));
    assert!(!between.unsupported_presentation);
    assert_eq!(between.category_axis_shifted, Some(true));

    let mid_cat = parse_chart_axis_semantics(&base("<crossBetween val=\"midCat\"/>"));
    assert!(!mid_cat.unsupported_presentation);
    assert_eq!(mid_cat.category_axis_shifted, Some(false));

    let omitted = parse_chart_axis_semantics(&base(""));
    assert!(!omitted.unsupported_presentation);
    assert_eq!(omitted.category_axis_shifted, Some(true));
}

#[test]
fn chart_axis_positions_and_non_default_presentation_fail_closed() {
    fn unsupported_reasons(xml: &str) -> Vec<ChartUnsupportedReason> {
        let mut cache_points = 16;
        let mut chart_series = 16;
        parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series)
            .expect("chart")
            .unsupported_reasons
    }

    let column = r#"<chartSpace><chart><plotArea><barChart><barDir val="col"/><axId val="1"/><axId val="2"/></barChart><catAx><axId val="1"/><axPos val="b"/><crossAx val="2"/></catAx><valAx><axId val="2"/><axPos val="l"/><crossAx val="1"/></valAx></plotArea></chart></chartSpace>"#;
    assert!(
        !unsupported_reasons(column).contains(&ChartUnsupportedReason::UnsupportedAxisPresentation)
    );

    let horizontal = column
        .replace(r#"val="col""#, r#"val="bar""#)
        .replace(
            r#"<catAx><axId val="1"/><axPos val="b"/>"#,
            r#"<catAx><axId val="1"/><axPos val="l"/>"#,
        )
        .replace(
            r#"<valAx><axId val="2"/><axPos val="l"/>"#,
            r#"<valAx><axId val="2"/><axPos val="b"/>"#,
        );
    assert!(!unsupported_reasons(&horizontal)
        .contains(&ChartUnsupportedReason::UnsupportedAxisPresentation));

    let reversed = horizontal.replace(r#"val="bar""#, r#"val="col""#);
    assert!(unsupported_reasons(&reversed)
        .contains(&ChartUnsupportedReason::UnsupportedAxisPresentation));

    let generated_defaults = column
            .replace(
                r#"<crossAx val="2"/>"#,
                r#"<crossAx val="2"/><crosses val="autoZero"/><auto val="1"/><lblAlgn val="ctr"/><lblOffset val="100"/>"#,
            )
            .replace(
                r#"<crossAx val="1"/>"#,
                r#"<crossAx val="1"/><crosses val="autoZero"/><crossBetween val="between"/>"#,
            );
    assert!(!unsupported_reasons(&generated_defaults)
        .contains(&ChartUnsupportedReason::UnsupportedAxisPresentation));

    for (supported, unsupported) in [
        (r#"crosses val="autoZero""#, r#"crosses val="max""#),
        (r#"crosses val="autoZero""#, r#"crossesAt val="0""#),
        (r#"auto val="1""#, r#"auto val="0""#),
        (r#"lblAlgn val="ctr""#, r#"lblAlgn val="l""#),
        (r#"lblOffset val="100""#, r#"lblOffset val="101""#),
        (
            r#"crossBetween val="between""#,
            r#"crossBetween val="unsupported""#,
        ),
    ] {
        let xml = generated_defaults.replacen(supported, unsupported, 1);
        assert!(
            unsupported_reasons(&xml)
                .contains(&ChartUnsupportedReason::UnsupportedAxisPresentation),
            "{unsupported}"
        );
    }
}

#[test]
fn chart_text_inheritance_merges_chart_list_paragraph_and_run_properties() {
    let xml = r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><title><tx><rich>
            <a:bodyPr/><a:lstStyle><a:lvl1pPr><a:defRPr sz="1200"><a:latin typeface="List Face"/></a:defRPr></a:lvl1pPr></a:lstStyle>
            <a:p><a:pPr lvl="0"><a:defRPr b="1"/></a:pPr><a:r><a:rPr i="1"><a:solidFill><a:srgbClr val="112233"/></a:solidFill></a:rPr><a:t>X</a:t></a:r></a:p>
        </rich></tx></title><plotArea><pieChart/></plotArea></chart>
        <txPr><a:bodyPr/><a:p><a:pPr><a:defRPr sz="1600"><a:latin typeface="Chart Face"/></a:defRPr></a:pPr></a:p></txPr>
        </chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
    assert!(parsed.unsupported_reasons.is_empty());
    let title = parsed.text_styles.chart_title.unwrap();
    assert_eq!(title.latin_font_family, "List Face");
    assert_eq!(title.size_hundredths_of_point, 1_200);
    assert_eq!(title.color, Color::rgb(0x11, 0x22, 0x33));
    assert!(title.bold);
    assert!(title.italic);

    let category = parsed.text_styles.category_axis_labels.unwrap();
    assert_eq!(category.latin_font_family, "Chart Face");
    assert_eq!(category.size_hundredths_of_point, 1_600);
}

#[test]
fn chart_text_ignores_unpainted_invalid_runs_but_rejects_late_properties() {
    let valid = r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><title><tx><rich><a:bodyPr/><a:p>
            <a:r><a:rPr sz="99"/><a:t/></a:r>
            <a:r><a:rPr sz="1400"><a:latin typeface="Face"/></a:rPr><a:t>X</a:t></a:r>
        </a:p></rich></tx></title><plotArea><pieChart/></plotArea></chart></chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart(valid, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
    assert!(parsed.unsupported_reasons.is_empty());
    assert_eq!(
        parsed
            .text_styles
            .chart_title
            .as_ref()
            .map(|style| style.size_hundredths_of_point),
        Some(1_400)
    );

    for late in [
        r#"<a:r><a:t>X</a:t><a:rPr b="1"/></a:r>"#,
        r#"<a:r><a:t>X</a:t></a:r><a:pPr lvl="0"/>"#,
    ] {
        let xml = format!(
            r#"<chartSpace><chart><title><tx><rich><a:bodyPr/><a:p>{late}</a:p></rich></tx></title><plotArea><pieChart/></plotArea></chart></chartSpace>"#
        );
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert!(parsed
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::UnsupportedTextStyle));
    }
}

#[test]
fn chart_text_color_map_and_fact_budget_are_strict_and_semantic() {
    let theme = parse_theme(&complete_theme_xml(
        "Theme Major",
        "Theme Face",
        "4472C4",
        "102030",
    ));
    assert!(theme.source_valid);
    let entities = "&amp;".repeat(MAX_CHART_TEXT_STYLE_FACTS_PER_ROLE + 1);
    let xml = format!(
        r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><clrMapOvr><overrideClrMapping bg1="lt1" tx1="accent2" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/></clrMapOvr><chart><title><tx><rich><a:bodyPr/><a:p><a:r><a:t>{entities}</a:t></a:r></a:p></rich></tx></title><plotArea><pieChart/></plotArea></chart></chartSpace>"#
    );
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart_with_theme(
        &xml,
        (0, 0),
        (10, 5),
        &mut cache_points,
        &mut chart_series,
        &theme,
    )
    .unwrap();
    assert!(!parsed.limit_exceeded);
    assert!(parsed.unsupported_reasons.is_empty());
    assert_eq!(
        parsed.text_styles.chart_title.unwrap().color,
        Color::rgb(0x10, 0x20, 0x30)
    );

    let partial = xml.replace(
            r#" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink""#,
            "",
        );
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart_with_theme(
        &partial,
        (0, 0),
        (10, 5),
        &mut cache_points,
        &mut chart_series,
        &theme,
    )
    .unwrap();
    assert!(parsed
        .unsupported_reasons
        .contains(&ChartUnsupportedReason::UnsupportedTextStyle));
}

#[test]
fn chart_text_fields_use_exact_non_truncating_byte_limits() {
    for (length, retained) in [
        (MAX_XLSX_CHART_TEXT_FIELD_BYTES, true),
        (MAX_XLSX_CHART_TEXT_FIELD_BYTES + 1, false),
    ] {
        let title = "x".repeat(length);
        let xml = format!(
            r#"<chartSpace><chart><title><tx><rich><a:bodyPr/><a:p><a:r><a:t>{title}</a:t></a:r></a:p></rich></tx></title><plotArea><pieChart/></plotArea></chart></chartSpace>"#
        );
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert_eq!(
            parsed.chart.title.as_deref(),
            retained.then_some(title.as_str())
        );
        assert_eq!(parsed.limit_exceeded, !retained);
    }
}

#[test]
fn chart_cache_and_series_sidecars_stop_at_exact_budgets() {
    let xml = r#"<chartSpace><chart><plotArea><lineChart>
            <ser><val><numRef><f>S!$A$1:$A$2</f><numCache>
                <pt idx="0"><v>1</v></pt><pt idx="1"><v>2</v></pt>
            </numCache></numRef></val></ser>
            <ser><val><numRef><f>S!$B$1:$B$2</f><numCache>
                <pt idx="0"><v>3</v></pt><pt idx="1"><v>4</v></pt>
            </numCache></numRef></val></ser>
        </lineChart></plotArea></chart></chartSpace>"#;
    let mut cache_points = 2;
    let mut chart_series = 1;
    let parsed = parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
    assert_eq!(cache_points, 0);
    assert_eq!(chart_series, 0);
    assert!(parsed.limit_exceeded);
    assert_eq!(parsed.chart.series.len(), 1);
    assert_eq!(parsed.series_caches.len(), 1);
    assert_eq!(parsed.series_styles.len(), 1);
    assert_eq!(parsed.series_caches[0].values.len(), 2);
}

#[test]
fn unsupported_chart_series_style_metadata_is_typed_and_bounded() {
    let xml = r#"<chartSpace><chart><plotArea><lineChart><ser>
            <marker><symbol val="picture"/><size val="255"/></marker>
            <spPr><a:ln w="20116801"><a:gradFill/><a:prstDash val="dash"/></a:ln></spPr>
            <val><numRef><f>S!$A$1:$A$2</f></numRef></val>
        </ser></lineChart></plotArea></chart></chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
    assert_eq!(parsed.series_styles.len(), 1);
    let style = &parsed.series_styles[0];
    assert_eq!(style.marker, ChartMarkerSymbol::Automatic);
    assert_eq!(style.marker_size, None);
    assert_eq!(
        style.losses,
        [
            ChartSeriesStyleLossKind::UnsupportedMarkerSymbol,
            ChartSeriesStyleLossKind::InvalidMarkerSize,
            ChartSeriesStyleLossKind::InvalidLineWidth,
            ChartSeriesStyleLossKind::UnsupportedLinePaint,
        ]
    );
}

#[test]
fn visible_series_legend_and_line_semantics_are_never_dropped_silently() {
    fn parse_with_series_markup(markup: &str) -> ParsedChart {
        let xml = format!(
            r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><plotArea><barChart><barDir val="col"/><ser><idx val="0"/><order val="0"/>{markup}<val><numRef><f>S!$A$1:$A$2</f></numRef></val></ser></barChart></plotArea></chart></chartSpace>"#
        );
        let mut cache_points = 16;
        let mut chart_series = 16;
        parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).expect("chart")
    }

    for markup in [
        "<dPt><idx val=\"0\"/></dPt>",
        "<trendline/>",
        "<errBars/>",
        "<invertIfNegative/>",
        "<pictureOptions/>",
        "<marker><spPr/></marker>",
        r#"<spPr><a:solidFill><a:srgbClr val="112233"/></a:solidFill></spPr>"#,
    ] {
        assert!(parse_with_series_markup(markup)
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::UnsupportedPlotSemantics));
    }

    let line = parse_with_series_markup(
        r#"<spPr><a:ln cap="rnd"><a:solidFill><a:srgbClr val="112233"><a:tint val="50000"/></a:srgbClr></a:solidFill><a:headEnd/></a:ln></spPr>"#,
    );
    assert!(line.series_styles[0]
        .losses
        .contains(&ChartSeriesStyleLossKind::UnsupportedLinePaint));

    let legend = r#"<chartSpace><chart><plotArea><pieChart/></plotArea><legend><spPr/></legend></chart></chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart(
        legend,
        (0, 0),
        (10, 5),
        &mut cache_points,
        &mut chart_series,
    )
    .expect("chart");
    assert!(parsed
        .unsupported_reasons
        .contains(&ChartUnsupportedReason::UnsupportedLegend));
}

#[test]
fn multiple_chart_text_color_transforms_fail_closed() {
    let xml = r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><title><tx><rich><a:bodyPr/><a:p><a:r><a:rPr><a:solidFill><a:srgbClr val="204080"><a:tint val="50000"/><a:shade val="50000"/></a:srgbClr></a:solidFill><a:latin typeface="Face"/></a:rPr><a:t>X</a:t></a:r></a:p></rich></tx></title><plotArea><pieChart/></plotArea></chart></chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed =
        parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).expect("chart");
    assert!(parsed
        .unsupported_reasons
        .contains(&ChartUnsupportedReason::UnsupportedTextStyle));
}

#[test]
fn chart_space_fill_and_axis_gridline_presence_are_retained() {
    let xml = r#"<chartSpace><chart><plotArea><lineChart><ser>
            <spPr><a:ln><a:noFill/></a:ln></spPr>
            <cat><strRef><f>S!$A$1:$A$2</f></strRef></cat>
            <val><numRef><f>S!$B$1:$B$2</f></numRef></val>
        </ser></lineChart>
        <catAx><majorGridlines/></catAx><valAx/>
        </plotArea></chart>
        <spPr>
            <a:solidFill><a:srgbClr val="123456"/></a:solidFill>
            <a:ln><a:noFill/></a:ln>
        </spPr></chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();

    assert_eq!(
        parsed.frame_fill,
        ChartFrameFill::Solid(Color::rgb(0x12, 0x34, 0x56))
    );
    assert_eq!(
        parsed.frame_style_losses,
        [ChartFrameStyleLossKind::UnsupportedPaint]
    );
    assert!(parsed.category_major_gridlines);
    assert!(!parsed.value_major_gridlines);
    assert!(!parsed.series_styles[0].line_visible);
    assert_eq!(parsed.series_styles[0].line_width_emu, Some(12_700));

    let no_fill = xml.replace(
        r#"<a:solidFill><a:srgbClr val="123456"/></a:solidFill>"#,
        "<a:noFill/>",
    );
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart(
        &no_fill,
        (0, 0),
        (10, 5),
        &mut cache_points,
        &mut chart_series,
    )
    .unwrap();
    assert_eq!(parsed.frame_fill, ChartFrameFill::NoFill);

    let unsupported = no_fill.replace("<a:noFill/>", "<a:gradFill/>");
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart(
        &unsupported,
        (0, 0),
        (10, 5),
        &mut cache_points,
        &mut chart_series,
    )
    .unwrap();
    assert_eq!(parsed.frame_fill, ChartFrameFill::Automatic);
    assert_eq!(
        parsed.frame_style_losses,
        [ChartFrameStyleLossKind::UnsupportedPaint]
    );
}

#[test]
fn chart_series_line_width_enforces_ooxml_bounds() {
    for (width, expected, invalid) in [
        (None, Some(12_700), false),
        (Some("0"), Some(0), false),
        (Some("20116800"), Some(20_116_800), false),
        (Some("-1"), None, true),
        (Some("20116801"), None, true),
        (Some("wide"), None, true),
    ] {
        let width = width.map_or(String::new(), |value| format!(r#" w="{value}""#));
        let xml = format!(
            r#"<chartSpace><chart><plotArea><lineChart><ser>
                    <spPr><a:ln{width}/></spPr>
                    <val><numRef><f>S!$A$1:$A$2</f></numRef></val>
                </ser></lineChart></plotArea></chart></chartSpace>"#
        );
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        let style = &parsed.series_styles[0];
        assert_eq!(style.line_width_emu, expected, "{width}");
        assert_eq!(
            style
                .losses
                .contains(&ChartSeriesStyleLossKind::InvalidLineWidth),
            invalid,
            "{width}"
        );
    }
}

#[test]
fn bar_chart_direction_is_retained_without_changing_chart_kind() {
    for (value, expected) in [
        ("col", ChartBarDirection::Column),
        ("bar", ChartBarDirection::Horizontal),
    ] {
        let xml = format!(
            r#"<chartSpace><chart><plotArea><barChart><barDir val="{value}"/><ser><val><numRef><f>Data!$A$1:$A$2</f></numRef></val></ser></barChart></plotArea></chart></chartSpace>"#
        );
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert_eq!(parsed.chart.kind, ChartKind::Bar);
        assert_eq!(parsed.bar_direction, expected);
    }
}

#[test]
fn chart_plot_order_style_and_legend_semantics_fail_closed() {
    let supported = r#"<chartSpace><style val="2"/><chart><plotArea>
            <lineChart><grouping val="standard"/><ser><idx val="0"/><order val="0"/>
                <val><numRef><f>Data!$A$1:$A$2</f></numRef></val>
            </ser><axId val="1"/><axId val="2"/></lineChart>
            <catAx><axId val="1"/><crossAx val="2"/></catAx>
            <valAx><axId val="2"/><crossAx val="1"/></valAx>
        </plotArea><legend><legendPos val="r"/><overlay val="0"/></legend>
        <plotVisOnly val="1"/><dispBlanksAs val="gap"/></chart></chartSpace>"#;

    let parse_reasons = |xml: &str| {
        let mut cache_points = 16;
        let mut chart_series = 16;
        parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series)
            .unwrap()
            .unsupported_reasons
    };

    assert!(parse_reasons(supported).is_empty());
    for xml in [
        supported.replace(r#"grouping val="standard""#, r#"grouping val="stacked""#),
        supported.replace(r#"order val="0""#, r#"order val="1""#),
        supported.replace(r#"<order val="0"/>"#, ""),
        supported.replace(
            r#"<grouping val="standard"/>"#,
            r#"<grouping val="standard"/><overlap val="0"/>"#,
        ),
    ] {
        assert!(
            parse_reasons(&xml).contains(&ChartUnsupportedReason::UnsupportedPlotSemantics),
            "{xml}"
        );
    }

    let nondefault_style = supported.replace(r#"style val="2""#, r#"style val="3""#);
    assert!(
        parse_reasons(&nondefault_style).contains(&ChartUnsupportedReason::UnsupportedChartStyle)
    );

    for xml in [
        supported.replace(r#"legendPos val="r""#, r#"legendPos val="l""#),
        supported.replace(r#"overlay val="0""#, r#"overlay val="1""#),
        supported.replace(
            r#"<legendPos val="r"/>"#,
            r#"<legendPos val="r"/><legendEntry><idx val="0"/></legendEntry>"#,
        ),
    ] {
        assert!(
            parse_reasons(&xml).contains(&ChartUnsupportedReason::UnsupportedLegend),
            "{xml}"
        );
    }
}

#[test]
fn chart_kind_specific_plot_defaults_are_exact() {
    let supported = r#"<chartSpace><chart><plotArea>
            <bubbleChart><varyColors val="0"/><bubble3D val="0"/>
                <ser><idx val="0"/><order val="0"/><xVal><numRef><f>S!$A$1:$A$2</f></numRef></xVal><yVal><numRef><f>S!$B$1:$B$2</f></numRef></yVal><bubbleSize><numRef><f>S!$C$1:$C$2</f></numRef></bubbleSize></ser>
                <axId val="1"/><axId val="2"/>
            </bubbleChart>
            <valAx><axId val="1"/><crossAx val="2"/></valAx>
            <valAx><axId val="2"/><crossAx val="1"/></valAx>
        </plotArea></chart></chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart(
        supported,
        (0, 0),
        (10, 5),
        &mut cache_points,
        &mut chart_series,
    )
    .unwrap();
    assert!(parsed.unsupported_reasons.is_empty());

    for replacement in [
        (r#"bubble3D val="0""#, r#"bubble3D val="1""#),
        (r#"varyColors val="0""#, r#"varyColors val="1""#),
    ] {
        let xml = supported.replace(replacement.0, replacement.1);
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert!(parsed
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::UnsupportedPlotSemantics));
    }
}

#[test]
fn unsupported_combo_3d_pivot_and_external_charts_are_explicit() {
    let xml = r#"<chartSpace><pivotSource/><externalData/><chart><view3D/><plotArea>
            <barChart><ser><val><numRef><f>Data!$A$1:$A$2</f></numRef></val></ser></barChart>
            <lineChart><ser><val><numRef><f>'[Other.xlsx]Data'!$B$1:$B$2</f></numRef></val></ser></lineChart>
        </plotArea></chart></chartSpace>"#;
    let mut cache_points = 16;
    let mut chart_series = 16;
    let parsed = parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
    assert_eq!(parsed.chart.kind, ChartKind::Bar);
    assert_eq!(parsed.chart.series.len(), 2);
    for reason in [
        ChartUnsupportedReason::Combo,
        ChartUnsupportedReason::ThreeDimensional,
        ChartUnsupportedReason::Pivot,
        ChartUnsupportedReason::ExternalData,
    ] {
        assert!(parsed.unsupported_reasons.contains(&reason), "{reason:?}");
    }

    let mut cache_points = 16;
    let mut chart_series = 16;
    let surface = parse_chart(
            r#"<chartSpace><chart><plotArea><surface3DChart><ser><val><numRef><f>Data!$A$1:$A$2</f></numRef></val></ser></surface3DChart></plotArea></chart></chartSpace>"#,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
        )
        .unwrap();
    assert_eq!(surface.chart.kind, ChartKind::Area);
    assert!(surface
        .unsupported_reasons
        .contains(&ChartUnsupportedReason::ThreeDimensional));
    assert!(surface
        .unsupported_reasons
        .contains(&ChartUnsupportedReason::UnsupportedKind));
}

#[test]
fn sheet_local_filter_database_defined_name_surfaces_autofilter() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm._FilterDatabase" localSheetId="0">Data!$B$3:$E$10</definedName></definedNames></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData/></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();

    assert!(wb.defined_names().is_empty());
    assert_eq!(wb.sheets[0].autofilter_range(), Some((2, 1, 9, 4)));
    assert_eq!(wb.sheets[0].page_setup(), None);
}

/// End-to-end `.xlsx` read: package document properties surface through the
/// public `Workbook::properties` field instead of remaining writer-only.
#[test]
fn reads_xlsx_doc_properties_end_to_end() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData/></worksheet>"#,
        ),
        (
            "docProps/core.xml",
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"><dc:title>Quarterly Report</dc:title><dc:subject>Operations</dc:subject><dc:creator>rxls reader</dc:creator><cp:keywords>ops,report</cp:keywords><dc:description>Quarterly operations report</dc:description><cp:lastModifiedBy>reviewer</cp:lastModifiedBy><dcterms:created>2024-01-02T03:04:05Z</dcterms:created></cp:coreProperties>"#,
        ),
        (
            "docProps/app.xml",
            r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>Excel</Application><Company>ACME</Company></Properties>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();

    assert_eq!(wb.properties.title.as_deref(), Some("Quarterly Report"));
    assert_eq!(wb.properties.subject.as_deref(), Some("Operations"));
    assert_eq!(wb.properties.creator.as_deref(), Some("rxls reader"));
    assert_eq!(wb.properties.keywords.as_deref(), Some("ops,report"));
    assert_eq!(
        wb.properties.description.as_deref(),
        Some("Quarterly operations report")
    );
    assert_eq!(wb.properties.last_modified_by.as_deref(), Some("reviewer"));
    assert_eq!(
        wb.properties.created.as_deref(),
        Some("2024-01-02T03:04:05Z")
    );
    assert_eq!(wb.properties.company.as_deref(), Some("ACME"));
}

#[test]
fn chartsheet_is_not_marked_as_worksheet_end_to_end() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Data" r:id="rId1"/><sheet name="Chart" r:id="rId2"/></sheets></workbook>"#,
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
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();

    assert_eq!(wb.sheets.len(), 2);
    assert!(wb.sheets[0].is_worksheet);
    assert!(!wb.sheets[1].is_worksheet);
    assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Text("ok".into())));
    assert_eq!(wb.text(), "# Data\nok\n");
}

#[test]
fn dangling_sheet_ref_without_relationship_is_not_marked_as_worksheet() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/><sheet name="Module" r:id=""/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData/></worksheet>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();

    assert_eq!(wb.sheets.len(), 2);
    assert!(wb.sheets[0].is_worksheet);
    assert!(!wb.sheets[1].is_worksheet);
    assert_eq!(wb.sheets[1].sheet_type(), SheetType::Vba);
    assert_eq!(wb.text(), "# Sheet1\n\n");
}

#[test]
fn external_worksheet_relationship_never_dispatches_to_a_local_zip_part() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (name, body) in [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="External" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml" TargetMode="External"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>must-not-load</t></is></c></row></sheetData></worksheet>"#,
        ),
    ] {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }

    let workbook = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    assert_eq!(workbook.sheets[0].sheet_type(), SheetType::Vba);
    assert!(workbook.sheets[0].cells.is_empty());
}

#[test]
fn internal_relationship_resolution_uses_only_the_uri_path_component() {
    assert_eq!(
        resolve_internal_relationship_part("xl/worksheets/sheet1.xml", "#Sheet2!A1"),
        Some("xl/worksheets/sheet1.xml".to_string())
    );
    assert_eq!(
        resolve_internal_relationship_part("xl/workbook.xml", "worksheets/sheet1.xml?q#f"),
        Some("xl/worksheets/sheet1.xml".to_string())
    );
    assert_eq!(
        resolve_internal_relationship_part("xl\\workbook.xml", "worksheets\\sheet1.xml"),
        Some("xl/worksheets/sheet1.xml".to_string())
    );
    assert_eq!(
        resolve_internal_relationship_part("xl/a.xml", "%2e%2e/kept.xml"),
        Some("xl/%2e%2e/kept.xml".to_string())
    );
    assert_eq!(
        resolve_internal_relationship_part("xl/workbook.xml", "https://evil.invalid/a.xml"),
        None
    );
    assert_eq!(
        resolve_internal_relationship_part("xl/workbook.xml", "//evil.invalid/a.xml"),
        None
    );
}

#[test]
fn office_document_fragment_resolves_but_absolute_internal_uri_is_rejected() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let package = |target: &str, workbook_part: &str| {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        zw.start_file("_rels/.rels", opt).unwrap();
        zw.write_all(
                format!(r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{target}"/></Relationships>"#).as_bytes(),
            )
            .unwrap();
        zw.start_file(workbook_part, opt).unwrap();
        zw.write_all(b"<workbook><sheets/></workbook>").unwrap();
        zw.finish().unwrap().into_inner()
    };

    assert!(Workbook::open(&package("xl/workbook.xml#Sheet1", "xl/workbook.xml")).is_ok());
    assert!(Workbook::open(&package(
        "https://evil.invalid/workbook.xml",
        "https:/evil.invalid/workbook.xml"
    ))
    .is_err());
}

#[test]
fn sheet_rels_path_inserts_rels_segment() {
    assert_eq!(
        sheet_rels_path("xl/worksheets/sheet1.xml"),
        "xl/worksheets/_rels/sheet1.xml.rels"
    );
    assert_eq!(sheet_rels_path("sheet1.xml"), "_rels/sheet1.xml.rels");
}

/// End-to-end `.xlsx` read: a worksheet `<hyperlink>` whose `r:id` resolves
/// through the worksheet rels surfaces via the public `hyperlinks()` accessor
/// as `(row, col, url)`.
#[test]
fn reads_xlsx_hyperlinks_end_to_end() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><workbookPr/><sheets><sheet name="Links" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="2"><c r="B2" t="inlineStr"><is><t>click</t></is></c></row></sheetData><hyperlinks><hyperlink ref="B2:B4" r:id="rId1"/></hyperlinks></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/" TargetMode="External"/></Relationships>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 1);
    // The `B2:B4` range expands to every cell (0-based rows 1..=3, col 1), each
    // resolved to the URL from the worksheet rels.
    let url = "https://example.com/".to_string();
    assert_eq!(
        wb.sheets[0].hyperlinks(),
        &[(1u32, 1u16, url.clone()), (2, 1, url.clone()), (3, 1, url)]
    );
}

#[test]
fn normalize_part_target_resolves_relative() {
    assert_eq!(
        normalize_part_target("xl/worksheets/sheet1.xml", "../comments1.xml"),
        "xl/comments1.xml"
    );
    assert_eq!(
        normalize_part_target("xl/worksheets/sheet1.xml", "comments1.xml"),
        "xl/worksheets/comments1.xml"
    );
    assert_eq!(
        normalize_part_target("xl/worksheets/sheet1.xml", "/xl/comments1.xml"),
        "xl/comments1.xml"
    );
    assert_eq!(
        normalize_part_target("xl/drawings/drawing1.xml", "../../xl/charts/chart1.xml"),
        "xl/charts/chart1.xml"
    );
    assert_eq!(
        normalize_part_target("xl/drawings/drawing1.xml", "../../../xl/charts/chart1.xml"),
        "xl/charts/chart1.xml"
    );
    assert_eq!(
        normalize_part_target("xl/drawings/drawing1.xml", "/../../xl/charts/chart1.xml"),
        "xl/charts/chart1.xml"
    );
}

#[test]
fn parse_comments_resolves_author_and_ref() {
    let xml = r#"<comments>
            <authors><author>Alice</author><author>Bob</author></authors>
            <commentList>
                <comment ref="B2" authorId="1"><text><t>hello </t><t>world</t></text></comment>
                <comment ref="A1" authorId="0"><text><r><t>note</t></r></text></comment>
            </commentList>
        </comments>"#;
    let cs = parse_comments(xml);
    assert_eq!(cs.len(), 2);
    assert_eq!((cs[0].row, cs[0].col), (1, 1)); // B2
    assert_eq!(cs[0].text, "hello world");
    assert_eq!(cs[0].author.as_deref(), Some("Bob"));
    assert_eq!((cs[1].row, cs[1].col), (0, 0)); // A1
    assert_eq!(cs[1].text, "note");
    assert_eq!(cs[1].author.as_deref(), Some("Alice"));
}

/// End-to-end `.xlsx` read: a worksheet referencing a `comments1.xml` part
/// via its rels (relationship Type `.../comments`) surfaces the notes via the
/// public `comments()` accessor as `(row, col, text, author)`.
#[test]
fn reads_xlsx_comments_end_to_end() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><workbookPr/><sheets><sheet name="Notes" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="3"><c r="C3" t="inlineStr"><is><t>x</t></is></c></row></sheetData></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing" Target="../drawings/vmlDrawing1.vml"/></Relationships>"#,
        ),
        (
            "xl/comments1.xml",
            r#"<comments><authors><author>심사위원</author></authors><commentList><comment ref="C3" authorId="0"><text><t>검토 필요</t></text></comment></commentList></comments>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 1);
    let cs = wb.sheets[0].comments();
    assert_eq!(cs.len(), 1);
    // C3 → 0-based (row 2, col 2).
    assert_eq!((cs[0].row, cs[0].col), (2, 2));
    assert_eq!(cs[0].text, "검토 필요");
    assert_eq!(cs[0].author.as_deref(), Some("심사위원"));
}

#[test]
fn parse_table_reads_name_range_columns() {
    // `displayName` is preferred over `name`; `ref` → 0-based inclusive range;
    // `<tableColumn name>` list → header columns; `<tableStyleInfo name>` → style.
    let xml = r#"<table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="Table1" displayName="가격표" ref="A1:C3"><autoFilter ref="A1:C3"/><tableColumns count="3"><tableColumn id="1" name="품목"/><tableColumn id="2" name="단가"/><tableColumn id="3" name="수량"/></tableColumns><tableStyleInfo name="TableStyleMedium2"/></table>"#;
    let parsed = parse_table(xml).unwrap();
    let t = parsed.table;
    assert_eq!(t.name, "가격표");
    assert_eq!(t.range, (0, 0, 2, 2)); // A1:C3
    assert_eq!(t.columns, vec!["품목", "단가", "수량"]);
    assert_eq!(t.style.as_deref(), Some("TableStyleMedium2"));
}

/// End-to-end `.xlsx` read: a worksheet referencing a `tables/table1.xml` part
/// via its rels (relationship Type `.../table`) surfaces the table via the
/// public `tables()` accessor with its name, range, and header columns.
#[test]
fn reads_xlsx_tables_end_to_end() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rIdStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
        ),
        (
            "xl/styles.xml",
            r#"<styleSheet><dxfs count="1"><dxf><font><b/><color rgb="FFFFFFFF"/></font><fill><patternFill patternType="solid"><fgColor rgb="FF123456"/></patternFill></fill><border><bottom style="medium"><color rgb="FFABCDEF"/></bottom></border><alignment horizontal="center" wrapText="1"/></dxf></dxfs><tableStyles count="1" defaultTableStyle="NamedBlue"><tableStyle name="NamedBlue" pivot="0" count="1"><tableStyleElement type="headerRow" dxfId="0"/></tableStyle></tableStyles></styleSheet>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>품목</t></is></c></row></sheetData></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
        ),
        (
            "xl/tables/table1.xml",
            r#"<table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="Table1" displayName="가격표" ref="A1:B2"><tableColumns count="2"><tableColumn id="1" name="품목"/><tableColumn id="2" name="단가"/></tableColumns><tableStyleInfo name="NamedBlue"/></table>"#,
        ),
    ];
    for (name, body) in parts {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 1);
    let tables = wb.sheets[0].tables();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "가격표");
    assert_eq!(tables[0].range, (0, 0, 1, 1)); // A1:B2
    assert_eq!(tables[0].columns, vec!["품목", "단가"]);
    let header = wb.sheets[0]
        .table_header_styles()
        .get("가격표")
        .expect("imported named table header style");
    assert_eq!(header.fill, Some(Color::rgb(0x12, 0x34, 0x56)));
    assert_eq!(
        header.font.as_ref().and_then(|font| font.color),
        Some(Color::rgb(0xFF, 0xFF, 0xFF))
    );
    assert!(header.font.as_ref().is_some_and(|font| font.bold));
    assert_eq!(
        header.border.as_ref().map(|border| border.bottom),
        Some(BorderStyle::Medium)
    );
    assert_eq!(
        header
            .border
            .as_ref()
            .and_then(|border| border.bottom_color),
        Some(Color::rgb(0xAB, 0xCD, 0xEF))
    );
    assert_eq!(
        header.align.as_ref().and_then(|align| align.horizontal),
        Some(HAlign::Center)
    );
    assert!(header.align.as_ref().is_some_and(|align| align.wrap));
}

#[test]
fn built_in_table_style_header_uses_theme_accent() {
    let mut theme = ThemeColors::default();
    theme.colors[4] = Some(Color::rgb(1, 2, 3));
    let built_in = built_in_table_style("TableStyleMedium2", &theme).unwrap();
    let style = built_in_table_header_style("TableStyleMedium2", &theme).unwrap();
    assert_eq!(style.fill, Some(Color::rgb(1, 2, 3)));
    assert_eq!(
        style.font.as_ref().and_then(|font| font.color),
        Some(Color::rgb(0xFF, 0xFF, 0xFF))
    );
    assert!(style.font.as_ref().is_some_and(|font| font.bold));
    for region in [
        TableStyleRegion::HeaderRow,
        TableStyleRegion::TotalRow,
        TableStyleRegion::FirstColumn,
        TableStyleRegion::LastColumn,
        TableStyleRegion::FirstRowStripe,
        TableStyleRegion::FirstColumnStripe,
    ] {
        assert!(built_in.definition.get(region).is_some(), "{region:?}");
    }
    assert!(built_in_table_header_style("TableStyleMedium29", &theme).is_none());
}

#[test]
fn direct_xf_masks_retain_explicit_resets_and_complete_builtin_formats() {
    let xml = r#"<styleSheet>
            <fonts count="2"><font><name val="Base"/></font><font><b/></font></fonts>
            <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
            <borders count="1"><border/></borders>
            <cellXfs count="3">
                <xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="0"/>
                <xf numFmtId="0" fontId="0" fillId="0" borderId="0"
                    applyFont="1" applyBorder="1" applyNumberFormat="1"
                    applyAlignment="1"><alignment wrapText="0"/></xf>
                <xf numFmtId="46" fontId="0" fillId="0" borderId="0"
                    applyNumberFormat="1"/>
            </cellXfs>
        </styleSheet>"#;
    let styles = parse_styles(xml, &ThemeColors::default());

    let disabled = styles.cell_style_overlay(0).expect("disabled overlay");
    assert!(!disabled.replace_font, "explicit applyFont=0 must win");
    assert!(overlay_is_empty(disabled));

    let reset = styles.cell_style_overlay(1).expect("reset overlay");
    assert!(reset.replace_font);
    assert!(reset.replace_border);
    assert!(reset.replace_num_fmt);
    assert!(reset.replace_alignment);
    assert!(reset
        .style
        .font
        .as_ref()
        .is_some_and(|font| font.name.as_deref() == Some("Base") && !font.bold));
    assert_eq!(reset.style.border, None, "borderId=0 clears the border");
    assert_eq!(reset.style.num_fmt, None, "numFmtId=0 means General");
    assert_eq!(reset.style.align, Some(Alignment::default()));

    assert_eq!(styles.cell_styles[2].num_fmt.as_deref(), Some("[h]:mm:ss"));
    assert_eq!(
        styles.cell_style_overlays[2].style.num_fmt.as_deref(),
        Some("[h]:mm:ss")
    );
}

#[test]
fn xlsx_normal_font_provenance_requires_exact_integral_source_agreement() {
    let styles = |first_size: &str,
                  normal_size: &str,
                  first_family: &str,
                  normal_family: &str,
                  normal_descriptor: &str| {
        parse_styles(
            &format!(
                r#"<styleSheet><fonts count="2"><font><sz val="{first_size}"/><name val="{first_family}"/></font><font><sz val="{normal_size}"/><name val="{normal_family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="1"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle {normal_descriptor} xfId="0"/></cellStyles></styleSheet>"#
            ),
            &ThemeColors::default(),
        )
    };

    for (value, expected) in [
        ("1", 1),
        ("00011", 11),
        ("11.0", 11),
        ("1.1e1", 11),
        ("4.09E2", 409),
    ] {
        assert_eq!(
            styles(value, value, "Verified", "Verified", r#"name="Normal""#)
                .xlsx_normal_font_size_pt,
            Some(expected),
            "{value}"
        );
    }
    assert_eq!(
        styles("12", "12", "Verified", "Verified", r#"builtinId="0""#).xlsx_normal_font_size_pt,
        Some(12),
        "built-in Normal provenance must not depend on an English name"
    );

    for value in [
        "0",
        "-1",
        "11.5",
        "409.00000000000000001",
        "409.55",
        "410",
        "1e309",
        "NaN",
    ] {
        assert_eq!(
            styles(value, value, "Verified", "Verified", r#"name="Normal""#)
                .xlsx_normal_font_size_pt,
            None,
            "{value}"
        );
    }
    assert_eq!(
        styles("12", "11", "Verified", "Verified", r#"name="Normal""#).xlsx_normal_font_size_pt,
        None,
        "the first cell XF and Normal style must agree on source size"
    );
    assert_eq!(
        styles("11", "11", "First", "Normal", r#"name="Normal""#).xlsx_normal_font_size_pt,
        None,
        "the first cell XF and Normal style must resolve to the same font"
    );
    assert_eq!(
            parse_styles(
                r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Verified"/></font></fonts><cellXfs count="1"><xf fontId="0"/></cellXfs></styleSheet>"#,
                &ThemeColors::default(),
            )
            .xlsx_normal_font_size_pt,
            None,
            "a missing named/built-in Normal style is ambiguous"
        );
    assert_eq!(
            parse_styles(
                r#"<styleSheet><fonts count="1"><font><sz val="11"/><sz val="12"/><name val="Verified"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
                &ThemeColors::default(),
            )
            .xlsx_normal_font_size_pt,
            None,
            "duplicate source size declarations are ambiguous"
        );
    assert_eq!(
            parse_styles(
                r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="2"><cellStyle name="Normal" xfId="0"/><cellStyle builtinId="0" xfId="0"/></cellStyles></styleSheet>"#,
                &ThemeColors::default(),
            )
            .xlsx_normal_font_size_pt,
            None,
            "duplicate Normal declarations are ambiguous"
        );
    assert_eq!(
            parse_styles(
                r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" builtinId="1" xfId="0"/></cellStyles></styleSheet>"#,
                &ThemeColors::default(),
            )
            .xlsx_normal_font_size_pt,
            None,
            "a contradictory built-in identifier must not identify Normal"
        );
}

#[test]
fn xlsx_cell_xf_font_provenance_rejects_rounded_and_ambiguous_sources() {
    let styles = parse_styles(
        r#"<styleSheet>
                <fonts count="6">
                    <font><sz val="11"/><name val="Normal"/></font>
                    <font><sz val="14"/><name val="Exact"/></font>
                    <font><sz val="13.5"/><name val="Fractional"/></font>
                    <font><sz val="13.5"/><sz val="14"/><name val="Duplicate"/></font>
                    <font><sz val="malformed"/><name val="Malformed"/></font>
                    <font><sz val="410"/><name val="OutOfRange"/></font>
                </fonts>
                <cellXfs count="9">
                    <xf fontId="0"/>
                    <xf fontId="1" applyFont="1"/>
                    <xf fontId="2" applyFont="1"/>
                    <xf fontId="3" applyFont="1"/>
                    <xf fontId="4" applyFont="1"/>
                    <xf fontId="5" applyFont="1"/>
                    <xf fontId="1" fontId="0" applyFont="1"/>
                    <xf fontId="1" applyFont="0"/>
                    <xf applyFont="1"/>
                </cellXfs>
            </styleSheet>"#,
        &ThemeColors::default(),
    );

    assert_eq!(
        styles.xlsx_cell_xf_font_sizes_pt,
        [Some(11), Some(14), None, None, None, None, None, None, None,]
    );
}

#[test]
fn xlsx_cell_xf_font_provenance_validates_implicit_parent_references() {
    let styles = parse_styles(
        r#"<styleSheet>
                <fonts count="1">
                    <font><sz val="11"/><name val="Verified"/></font>
                </fonts>
                <cellStyleXfs count="1">
                    <xf fontId="0"/>
                </cellStyleXfs>
                <cellXfs count="6">
                    <xf fontId="0" xfId="0"/>
                    <xf fontId="0"/>
                    <xf fontId="0" xfId="1"/>
                    <xf fontId="0" xfId="malformed"/>
                    <xf fontId="0" xfId="0" xfId="0"/>
                    <xf fontId="0" xfId="99" applyFont="1"/>
                </cellXfs>
            </styleSheet>"#,
        &ThemeColors::default(),
    );

    assert_eq!(
        styles.xlsx_cell_xf_font_sizes_pt,
        [Some(11), Some(11), None, None, None, Some(11)]
    );

    let missing_parent_table = parse_styles(
        r#"<styleSheet>
                <fonts count="1">
                    <font><sz val="11"/><name val="Verified"/></font>
                </fonts>
                <cellXfs count="1">
                    <xf fontId="0" xfId="0"/>
                </cellXfs>
            </styleSheet>"#,
        &ThemeColors::default(),
    );
    assert_eq!(missing_parent_table.xlsx_cell_xf_font_sizes_pt, [None]);
}

#[test]
fn duplicate_cells_clear_stale_coordinate_sidecars() {
    let base = CellStyle::new().font_name("Base").set_font_size(11);
    let direct = CellStyle::new().font_name("Direct").set_font_size(14);
    let styles = Styles {
        cell_styles: vec![base, direct.clone()],
        xlsx_cell_xf_font_sizes_pt: vec![Some(11), Some(14)],
        cell_style_overlays: vec![
            CellStyleOverlay::default(),
            CellStyleOverlay {
                style: direct,
                replace_font: true,
                ..CellStyleOverlay::default()
            },
        ],
        ..Styles::default()
    };
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        r#"<worksheet><sheetData><row r="1">
                <c r="A1" s="1" t="inlineStr"><is><r><rPr><b/></rPr><t>first</t></r></is></c>
                <c r="A1" t="inlineStr"><is><t>last</t></is></c>
            </row></sheetData></worksheet>"#,
        &[],
        &styles,
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.cells.len(), 2);
    assert_eq!(
        parsed.cells.last().map(|cell| cell.text.as_str()),
        Some("last")
    );
    assert_eq!(
        parsed.cells.last().and_then(|cell| cell.xlsx_font_size_pt),
        Some(11)
    );
    assert!(!parsed.direct_cell_formats.contains_key(&(0, 0)));
    assert!(!parsed.rich.contains_key(&(0, 0)));
}

#[test]
fn xlsx_normal_font_provenance_fails_closed_on_malformed_metadata() {
    let invalid_documents = [
        (
            "malformed built-in identifier",
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" builtinId="bogus" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "duplicate source-size attribute",
            r#"<styleSheet><fonts><font><sz val="11" val="12"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "duplicate Normal-style reference",
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0" xfId="1"/></cellStyles></styleSheet>"#,
        ),
        (
            "duplicate local-name Normal-style reference",
            r#"<styleSheet xmlns:p="urn:test"><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0" p:xfId="1"/></cellStyles></styleSheet>"#,
        ),
        (
            "duplicate font table",
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><fonts/><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "nested font table",
            r#"<styleSheet><fonts><fonts/><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "font table below an extension",
            r#"<styleSheet><ext><fonts><font><sz val="11"/><name val="Verified"/></font></fonts></ext><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "font record below an extension",
            r#"<styleSheet><fonts><ext><font><sz val="11"/><name val="Verified"/></font></ext></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "font properties below an extension",
            r#"<styleSheet><fonts><font><ext><sz val="11"/><name val="Verified"/></ext></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "duplicate cell XF table",
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellXfs/><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "nested cell-style XF table",
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><cellStyleXfs><xf fontId="0"/></cellStyleXfs></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "cell-style XF below an extension",
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><ext><xf fontId="0"/></ext></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "cell XF below an extension",
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><ext><xf fontId="0" xfId="0"/></ext></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "Normal style below an extension",
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><ext><cellStyle name="Normal" xfId="0"/></ext></cellStyles></styleSheet>"#,
        ),
        (
            "cell XF table below an extension",
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><ext><cellXfs><xf fontId="0" xfId="0"/></cellXfs></ext><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
        ),
        (
            "truncated Normal-style table",
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/>"#,
        ),
    ];

    for (case, xml) in invalid_documents {
        assert_eq!(
            parse_styles(xml, &ThemeColors::default()).xlsx_normal_font_size_pt,
            None,
            "{case}"
        );
    }

    let truncated_fonts = r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font>"#;
    let mut losses = Vec::new();
    let (_, _, complete) =
        parse_font_table(truncated_fonts, &ThemeColors::default(), &[], &mut losses);
    assert!(!complete, "an open font table must not retain provenance");
}

#[test]
fn xlsx_normal_font_provenance_rejects_over_limit_font_tables() {
    let overflow = "<font/>".repeat(MAX_XLSX_STYLE_RECORDS);
    let xml = format!(
        r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font>{overflow}</fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#
    );
    let mut losses = Vec::new();
    let (fonts, exact_sizes, complete) =
        parse_font_table(&xml, &ThemeColors::default(), &[], &mut losses);

    assert_eq!(fonts.len(), MAX_XLSX_STYLE_RECORDS);
    assert_eq!(exact_sizes.first(), Some(&Some(11)));
    assert!(!complete);
    assert_eq!(
        verified_xlsx_normal_font_size(&xml, &fonts, &exact_sizes, complete),
        None,
        "retained leading records must not be trusted after table truncation"
    );
}

#[test]
fn xlsx_style_table_limits_are_bounded_and_typed() {
    let colors = r#"<rgbColor rgb="FF010203"/>"#.repeat(MAX_XLSX_INDEXED_COLORS + 1);
    let overlong_format = "0".repeat(MAX_XLSX_FORMAT_CODE_BYTES + 1);
    let xml = format!(
        r#"<styleSheet><numFmts count="1"><numFmt numFmtId="164" formatCode="{overlong_format}"/></numFmts><colors><indexedColors>{colors}</indexedColors></colors><cellXfs count="1"><xf numFmtId="0"/></cellXfs></styleSheet>"#
    );
    let styles = parse_styles(&xml, &ThemeColors::default());
    assert!(styles.custom.is_empty());
    assert_eq!(styles.indexed_colors.len(), MAX_XLSX_INDEXED_COLORS);
    assert!(styles
        .losses
        .iter()
        .any(|loss| loss.kind == StyleLossKind::LimitExceeded && loss.occurrences >= 2));

    let mut records = vec![(); MAX_XLSX_STYLE_RECORDS];
    let mut losses = Vec::new();
    retain_xlsx_style_record(&mut records, (), &mut losses);
    assert_eq!(records.len(), MAX_XLSX_STYLE_RECORDS);
    assert_eq!(
        losses,
        vec![StyleLoss {
            kind: StyleLossKind::LimitExceeded,
            occurrences: 1,
        }]
    );
}

#[test]
fn empty_direct_xf_mask_still_prevents_full_style_fallback() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
        ),
        (
            "xl/styles.xml",
            r#"<styleSheet><fonts count="2"><font><name val="Base"/></font><font><b/></font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="0"/></cellXfs></styleSheet>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>1</v></c></row></sheetData></worksheet>"#,
        ),
    ] {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    let workbook = Workbook::open(&zip.finish().unwrap().into_inner()).unwrap();
    let sheet = &workbook.sheets[0];
    assert!(sheet
        .direct_cell_formats
        .get(&(0, 0))
        .is_some_and(overlay_is_empty));
    let font = sheet
        .resolved_cell_style(0, 0)
        .and_then(|style| style.font)
        .expect("resolved base font");
    assert_eq!(font.name.as_deref(), Some("Base"));
    assert!(!font.bold, "explicit applyFont=0 must not use fontId=1");
}

#[test]
fn custom_table_style_parser_retains_regions_sizes_and_typed_losses() {
    let regions = [
        TableStyleRegion::WholeTable,
        TableStyleRegion::FirstColumnStripe,
        TableStyleRegion::SecondColumnStripe,
        TableStyleRegion::FirstRowStripe,
        TableStyleRegion::SecondRowStripe,
        TableStyleRegion::FirstColumn,
        TableStyleRegion::LastColumn,
        TableStyleRegion::HeaderRow,
        TableStyleRegion::TotalRow,
        TableStyleRegion::FirstHeaderCell,
        TableStyleRegion::LastHeaderCell,
        TableStyleRegion::FirstTotalCell,
        TableStyleRegion::LastTotalCell,
    ];
    let dxfs = regions
        .iter()
        .enumerate()
        .map(|(index, _)| DifferentialStyle {
            style: CellStyle::new().background_color([index as u8, 1, 2]),
            losses: (index == 0)
                .then_some(StyleLoss {
                    kind: StyleLossKind::UnresolvedColor,
                    occurrences: 1,
                })
                .into_iter()
                .collect(),
        })
        .collect::<Vec<_>>();
    let xml = r#"<styleSheet><tableStyles count="1"><tableStyle name="AllRegions" count="17">
            <tableStyleElement type="wholeTable" dxfId="0"/>
            <tableStyleElement type="firstColumnStripe" size="3" dxfId="1"/>
            <tableStyleElement type="secondColumnStripe" size="9999999" dxfId="2"/>
            <tableStyleElement type="firstRowStripe" size="2" dxfId="3"/>
            <tableStyleElement type="secondRowStripe" dxfId="4"/>
            <tableStyleElement type="firstColumn" dxfId="5"/>
            <tableStyleElement type="lastColumn" dxfId="6"/>
            <tableStyleElement type="headerRow" dxfId="7"/>
            <tableStyleElement type="totalRow" dxfId="8"/>
            <tableStyleElement type="firstHeaderCell" dxfId="9"/>
            <tableStyleElement type="lastHeaderCell" dxfId="10"/>
            <tableStyleElement type="firstTotalCell" dxfId="11"/>
            <tableStyleElement type="lastTotalCell" dxfId="12"/>
            <tableStyleElement type="pageFieldLabels" dxfId="0"/>
            <tableStyleElement type="headerRow" dxfId="999"/>
            <tableStyleElement type="wholeTable" dxfId="1"/>
        </tableStyle></tableStyles></styleSheet>"#;
    let parsed = parse_table_styles(xml, &dxfs)
        .remove("AllRegions")
        .expect("parsed table style");

    for region in regions {
        assert!(
            parsed.definition.get(region).is_some(),
            "missing region {region:?}"
        );
    }
    assert_eq!(
        parsed
            .definition
            .get(TableStyleRegion::FirstColumnStripe)
            .map(|style| style.stripe_size),
        Some(3)
    );
    assert_eq!(
        parsed
            .definition
            .get(TableStyleRegion::FirstRowStripe)
            .map(|style| style.stripe_size),
        Some(2)
    );
    for kind in [
        StyleLossKind::UnsupportedProperty,
        StyleLossKind::MissingReference,
        StyleLossKind::LimitExceeded,
        StyleLossKind::UnresolvedColor,
    ] {
        assert!(
            parsed.losses.iter().any(|loss| loss.kind == kind),
            "missing typed loss {kind:?}: {:?}",
            parsed.losses
        );
    }
}

#[test]
fn xlsx_table_regions_compose_with_sheet_column_row_and_direct_cell_styles() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let styles = r#"<styleSheet>
            <fonts count="3"><font><name val="Base"/></font><font><b/></font><font><i/></font></fonts>
            <fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="solid"><fgColor rgb="FF636363"/></patternFill></fill></fills>
            <borders count="1"><border/></borders>
            <cellXfs count="4">
                <xf numFmtId="2" fontId="0" fillId="0" borderId="0"/>
                <xf numFmtId="2" fontId="1" fillId="0" borderId="0" applyFont="1"/>
                <xf numFmtId="2" fontId="2" fillId="0" borderId="0" applyFont="1"/>
                <xf numFmtId="2" fontId="0" fillId="1" borderId="0" applyFill="1"/>
            </cellXfs>
            <dxfs count="11">
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF0A0A0A"/></patternFill></fill></dxf>
                <dxf><font><b/><color rgb="FFFFFFFF"/></font><fill><patternFill patternType="solid"><fgColor rgb="FF141414"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF1E1E1E"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF282828"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF323232"/></patternFill></fill></dxf>
                <dxf><font><color rgb="FF3C3C3C"/></font></dxf>
                <dxf><font><i/></font></dxf>
                <dxf><font><color rgb="FF505050"/></font></dxf>
                <dxf><font><color rgb="FF5A5A5A"/></font></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF464646"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF484848"/></patternFill></fill></dxf>
            </dxfs>
            <tableStyles count="1"><tableStyle name="Layered" count="12">
                <tableStyleElement type="wholeTable" dxfId="0"/>
                <tableStyleElement type="headerRow" dxfId="1"/>
                <tableStyleElement type="totalRow" dxfId="2"/>
                <tableStyleElement type="firstRowStripe" size="2" dxfId="3"/>
                <tableStyleElement type="secondRowStripe" dxfId="4"/>
                <tableStyleElement type="firstColumn" dxfId="5"/>
                <tableStyleElement type="lastColumn" dxfId="6"/>
                <tableStyleElement type="firstHeaderCell" dxfId="7"/>
                <tableStyleElement type="lastTotalCell" dxfId="8"/>
                <tableStyleElement type="firstColumnStripe" dxfId="9"/>
                <tableStyleElement type="secondColumnStripe" dxfId="10"/>
                <tableStyleElement type="pageFieldLabels" dxfId="0"/>
            </tableStyle></tableStyles>
        </styleSheet>"#;
    let worksheet = r#"<worksheet><cols><col min="1" max="1" style="1"/></cols><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>H1</t></is></c><c r="B1" t="inlineStr"><is><t>H2</t></is></c><c r="C1" t="inlineStr"><is><t>H3</t></is></c></row>
            <row r="2" s="2" customFormat="1"><c r="A2"><v>1</v></c><c r="B2" s="3"><v>2</v></c><c r="C2"><v>3</v></c></row>
            <row r="3"><c r="A3"><v>4</v></c><c r="B3"><v>5</v></c><c r="C3"><v>6</v></c></row>
            <row r="4"><c r="A4"><v>7</v></c><c r="B4"><v>8</v></c><c r="C4"><v>9</v></c></row>
            <row r="5"><c r="A5"><v>10</v></c><c r="B5"><v>11</v></c><c r="C5"><v>12</v></c></row>
        </sheetData><tableParts count="1"><tablePart r:id="rIdTable"/></tableParts></worksheet>"#;
    let table = r#"<table id="1" name="LayeredTable" displayName="LayeredTable" ref="A1:C5" headerRowCount="1" totalsRowCount="1"><tableColumns count="3"><tableColumn id="1" name="H1"/><tableColumn id="2" name="H2"/><tableColumn id="3" name="H3"/></tableColumns><tableStyleInfo name="Layered" showFirstColumn="1" showLastColumn="1" showRowStripes="1" showColumnStripes="1"/></table>"#;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
        ),
        ("xl/styles.xml", styles),
        ("xl/worksheets/sheet1.xml", worksheet),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdTable" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
        ),
        ("xl/tables/table1.xml", table),
    ] {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    let workbook = Workbook::open(&zip.finish().unwrap().into_inner()).unwrap();
    let sheet = &workbook.sheets[0];
    let effective_fill = |row, col| {
        let style = sheet.resolved_cell_style(row, col)?;
        style
            .pattern_fill
            .and_then(|fill| fill.foreground.or(fill.background))
            .or(style.fill)
    };

    assert_eq!(effective_fill(0, 0), Some(Color::rgb(0x14, 0x14, 0x14)));
    assert_eq!(
        sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.font)
            .and_then(|font| font.color),
        Some(Color::rgb(0x50, 0x50, 0x50))
    );
    assert_eq!(
        effective_fill(1, 1),
        Some(Color::rgb(0x63, 0x63, 0x63)),
        "direct cell fill must win over both row and column banding"
    );
    let direct = sheet.resolved_cell_style(1, 1).expect("direct style");
    assert!(direct.font.as_ref().is_some_and(|font| font.italic));
    assert_eq!(direct.num_fmt.as_deref(), Some("0.00"));
    assert_eq!(effective_fill(2, 1), Some(Color::rgb(0x28, 0x28, 0x28)));
    assert_eq!(effective_fill(3, 1), Some(Color::rgb(0x32, 0x32, 0x32)));
    assert_eq!(effective_fill(4, 2), Some(Color::rgb(0x1E, 0x1E, 0x1E)));
    assert_eq!(
        sheet
            .resolved_cell_style(4, 2)
            .and_then(|style| style.font)
            .and_then(|font| font.color),
        Some(Color::rgb(0x5A, 0x5A, 0x5A))
    );
    assert!(sheet
        .style_losses()
        .iter()
        .any(|loss| { loss.kind == StyleLossKind::UnsupportedProperty && loss.occurrences == 1 }));
    assert_eq!(
        sheet.resolved_cell_style(2, 1),
        sheet.resolved_cell_style(2, 1)
    );
}

#[test]
fn range_parsing() {
    assert_eq!(parse_range("A1:C3"), Some((0, 0, 2, 2)));
    assert_eq!(parse_range("B2"), Some((1, 1, 1, 1))); // lone ref = 1×1
    assert_eq!(parse_range("A1:"), None);
    assert_eq!(parse_range("junk"), None);
}

#[test]
fn sheet_view_and_autofilter_metadata_is_parsed() {
    let xml = r#"<worksheet>
            <sheetViews>
                <sheetView showGridLines="0" showRowColHeaders="0" rightToLeft="1" zoomScale="125">
                    <pane xSplit="2" ySplit="1" state="frozen"/>
                </sheetView>
            </sheetViews>
            <sheetData/>
            <autoFilter ref="A1:C10"/>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.freeze, Some((1, 2)));
    assert_eq!(parsed.autofilter, Some((0, 0, 9, 2)));
    assert!(parsed.hide_gridlines);
    assert_eq!(parsed.show_headers, Some(false));
    assert!(parsed.right_to_left);
    assert_eq!(parsed.zoom, Some(125));
}

#[test]
fn sheet_view_metadata_uses_primary_view_only() {
    let xml = r#"<worksheet>
            <sheetViews>
                <sheetView workbookViewId="0" zoomScale="110"/>
                <sheetView workbookViewId="1" showGridLines="0" showRowColHeaders="0" rightToLeft="1" zoomScale="125">
                    <pane xSplit="2" ySplit="1" state="frozen"/>
                </sheetView>
            </sheetViews>
            <sheetData/>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.freeze, None);
    assert!(!parsed.hide_gridlines);
    assert_eq!(parsed.show_headers, None);
    assert!(!parsed.right_to_left);
    assert_eq!(parsed.zoom, Some(110));
}

#[test]
fn sheet_view_explicit_visible_headers_are_preserved() {
    let xml = r#"<worksheet>
            <sheetViews>
                <sheetView showRowColHeaders="1"/>
            </sheetViews>
            <sheetData/>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.show_headers, Some(true));
}

#[test]
fn page_setup_first_page_number_requires_use_flag() {
    for (attrs, expected) in [
        (r#"firstPageNumber="7""#, None),
        (r#"firstPageNumber="7" useFirstPageNumber="0""#, None),
        (r#"firstPageNumber="7" useFirstPageNumber="1""#, Some(7)),
    ] {
        let xml = format!(r#"<worksheet><sheetData/><pageSetup {attrs}/></worksheet>"#);
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            &xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(
            parsed
                .page_setup
                .as_ref()
                .and_then(|setup| setup.first_page_number),
            expected,
            "unexpected first_page_number for pageSetup attrs {attrs}"
        );
    }
}

#[test]
fn page_setup_pr_is_the_authoritative_fit_mode_switch() {
    for (sheet_pr, expected) in [
        ("", false),
        ("<sheetPr><pageSetUpPr/></sheetPr>", false),
        (r#"<sheetPr><pageSetUpPr fitToPage="0"/></sheetPr>"#, false),
        (
            r#"<sheetPr><pageSetUpPr fitToPage="false"/></sheetPr>"#,
            false,
        ),
        (r#"<sheetPr><pageSetUpPr fitToPage="1"/></sheetPr>"#, true),
        (
            r#"<sheetPr><pageSetUpPr fitToPage="true"/></sheetPr>"#,
            true,
        ),
    ] {
        let xml = format!(
            r#"<worksheet>{sheet_pr}<sheetData/><pageSetup scale="85" fitToWidth="1" fitToHeight="1"/></worksheet>"#
        );
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            &xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.print_metadata.fit_to_page(), Some(expected));
        let setup = parsed.page_setup.expect("pageSetup must be retained");
        assert_eq!(setup.scale, Some(85));
        assert_eq!(setup.fit_to_width, Some(1));
        assert_eq!(setup.fit_to_height, Some(1));
    }
}

#[test]
fn active_fit_retains_defaulted_and_zero_dimensions() {
    for (attrs, expected_width, expected_height) in [
        (r#"scale="85""#, None, None),
        (
            r#"scale="85" fitToWidth="1" fitToHeight="0""#,
            Some(1),
            Some(0),
        ),
        (
            r#"scale="85" fitToWidth="0" fitToHeight="0""#,
            Some(0),
            Some(0),
        ),
    ] {
        let xml = format!(
            r#"<worksheet><sheetPr><pageSetUpPr fitToPage="1"/></sheetPr><sheetData/><pageSetup {attrs}/></worksheet>"#
        );
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            &xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.print_metadata.fit_to_page(), Some(true));
        let setup = parsed.page_setup.expect("pageSetup must be retained");
        assert_eq!(setup.scale, Some(85));
        assert_eq!(setup.fit_to_width, expected_width);
        assert_eq!(setup.fit_to_height, expected_height);
    }
}

#[test]
fn fit_count_attributes_saturate_numeric_overflow_and_report_typed_losses() {
    for attribute in ["fitToWidth", "fitToHeight"] {
        for (value, expected, expected_loss) in [
            ("0", Some(0), None),
            ("65535", Some(u16::MAX), None),
            ("65536", Some(u16::MAX), Some(PrintLossKind::LimitExceeded)),
            (
                "4294967295",
                Some(u16::MAX),
                Some(PrintLossKind::LimitExceeded),
            ),
            (
                "not-a-count",
                None,
                Some(PrintLossKind::UnsupportedProperty),
            ),
        ] {
            let xml = format!(
                r#"<worksheet><sheetPr><pageSetUpPr fitToPage="1"/></sheetPr><sheetData/><pageSetup scale="85" {attribute}="{value}"/></worksheet>"#
            );
            let mut budget = crate::MAX_TEXT_BYTES;
            let parsed = parse_sheet(
                &xml,
                &[],
                &Styles::default(),
                &ThemeColors::default(),
                false,
                &mut budget,
            );

            assert_eq!(
                parsed.print_metadata.fit_to_page(),
                Some(true),
                "{attribute}={value}"
            );
            let setup = parsed.page_setup.expect("pageSetup must be retained");
            let retained = if attribute == "fitToWidth" {
                setup.fit_to_width
            } else {
                setup.fit_to_height
            };
            assert_eq!(retained, expected, "{attribute}={value}");
            let other = if attribute == "fitToWidth" {
                setup.fit_to_height
            } else {
                setup.fit_to_width
            };
            assert_eq!(other, None, "{attribute}={value}");
            match expected_loss {
                Some(kind) => {
                    assert_eq!(
                        parsed.print_metadata.fidelity(),
                        crate::PrintFidelity::Partial,
                        "{attribute}={value}"
                    );
                    assert_eq!(
                        parsed
                            .print_metadata
                            .losses()
                            .iter()
                            .find(|loss| loss.kind == kind)
                            .map(|loss| loss.occurrences),
                        Some(1),
                        "{attribute}={value}"
                    );
                }
                None => assert!(
                    parsed.print_metadata.losses().is_empty(),
                    "{attribute}={value}"
                ),
            }
        }
    }
}

#[test]
fn malformed_fit_mode_fails_closed_with_typed_loss() {
    let xml = r#"<worksheet><sheetPr><pageSetUpPr fitToPage="maybe"/></sheetPr>
            <sheetData/><pageSetup scale="85" fitToWidth="1" fitToHeight="1"/>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.print_metadata.fit_to_page(), Some(false));
    assert_eq!(
        parsed.print_metadata.fidelity(),
        crate::PrintFidelity::Partial
    );
    assert!(parsed
        .print_metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::UnsupportedProperty));
}

#[test]
fn first_header_footer_falls_back_to_page_setup_metadata() {
    let xml = r#"<worksheet>
            <sheetData/>
            <headerFooter>
                <firstHeader>&amp;CFirst page</firstHeader>
                <firstFooter>&amp;RFirst footer</firstFooter>
            </headerFooter>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    let page_setup = parsed.page_setup.expect("page setup metadata");
    assert_eq!(page_setup.header.as_deref(), Some("&CFirst page"));
    assert_eq!(page_setup.footer.as_deref(), Some("&RFirst footer"));
}

#[test]
fn even_header_footer_falls_back_to_page_setup_metadata() {
    let xml = r#"<worksheet>
            <sheetData/>
            <headerFooter>
                <evenHeader>&amp;LEven pages</evenHeader>
                <evenFooter>&amp;REven footer</evenFooter>
            </headerFooter>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    let page_setup = parsed.page_setup.expect("page setup metadata");
    assert_eq!(page_setup.header.as_deref(), Some("&LEven pages"));
    assert_eq!(page_setup.footer.as_deref(), Some("&REven footer"));
}

#[test]
fn odd_header_footer_overrides_first_even_fallback_metadata() {
    let xml = r#"<worksheet>
            <sheetData/>
            <headerFooter>
                <firstHeader>&amp;CFirst page</firstHeader>
                <evenHeader>&amp;LEven pages</evenHeader>
                <oddHeader>&amp;COdd pages</oddHeader>
                <firstFooter>&amp;RFirst footer</firstFooter>
                <evenFooter>&amp;REven footer</evenFooter>
                <oddFooter>&amp;COdd footer</oddFooter>
            </headerFooter>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    let page_setup = parsed.page_setup.expect("page setup metadata");
    assert_eq!(page_setup.header.as_deref(), Some("&COdd pages"));
    assert_eq!(page_setup.footer.as_deref(), Some("&COdd footer"));
}

#[test]
fn print_sidecar_retains_exact_ooxml_source_metadata() {
    let xml = r#"<worksheet>
            <sheetData/>
            <printOptions gridLines="0" headings="1" horizontalCentered="1" verticalCentered="0"/>
            <pageSetup pageOrder="overThenDown"/>
            <headerFooter differentOddEven="1" differentFirst="1" scaleWithDoc="0" alignWithMargins="1">
                <oddHeader>&amp;COdd</oddHeader><oddFooter>&amp;LOddF</oddFooter>
                <evenHeader>&amp;CEven</evenHeader><evenFooter>&amp;LEvenF</evenFooter>
                <firstHeader>&amp;CFirst</firstHeader><firstFooter>&amp;LFirstF</firstFooter>
            </headerFooter>
            <rowBreaks count="3" manualBreakCount="2">
                <brk id="20" min="0" max="16383" man="1"/>
                <brk id="5" min="0" max="16383" man="1"/>
                <brk id="8" min="0" max="16383" man="0"/>
            </rowBreaks>
            <colBreaks count="2" manualBreakCount="2">
                <brk id="7" min="0" max="1048575" man="1"/>
                <brk id="3" min="0" max="1048575" man="1"/>
            </colBreaks>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let mut parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );
    let names = [SheetDefinedName {
        local_sheet_id: 0,
        name: "_xlnm.Print_Area".to_string(),
        refers_to: "'Print Sheet'!$A$1:$B$2,'Print Sheet'!$D$4:$F$9".to_string(),
    }];
    apply_sheet_defined_names(
        &mut parsed.page_setup,
        &mut parsed.print_metadata,
        &mut parsed.autofilter,
        names.iter(),
    );

    let metadata = &parsed.print_metadata;
    assert_eq!(metadata.fidelity(), crate::PrintFidelity::Retained);
    assert_eq!(metadata.print_areas(), &[(0, 0, 1, 1), (3, 3, 8, 5)]);
    assert_eq!(metadata.manual_row_breaks(), &[5, 20]);
    assert_eq!(metadata.manual_col_breaks(), &[3, 7]);
    assert_eq!(metadata.page_order(), Some(PrintPageOrder::OverThenDown));
    assert_eq!(metadata.print_gridlines(), Some(false));
    assert_eq!(metadata.print_headings(), Some(true));
    assert_eq!(metadata.center_horizontally(), Some(true));
    assert_eq!(metadata.center_vertically(), Some(false));
    let header_footer = metadata.header_footer();
    assert_eq!(header_footer.odd_header(), Some("&COdd"));
    assert_eq!(header_footer.odd_footer(), Some("&LOddF"));
    assert_eq!(header_footer.even_header(), Some("&CEven"));
    assert_eq!(header_footer.even_footer(), Some("&LEvenF"));
    assert_eq!(header_footer.first_header(), Some("&CFirst"));
    assert_eq!(header_footer.first_footer(), Some("&LFirstF"));
    assert_eq!(header_footer.different_odd_even(), Some(true));
    assert_eq!(header_footer.different_first(), Some(true));
    assert_eq!(header_footer.scale_with_document(), Some(false));
    assert_eq!(header_footer.align_with_margins(), Some(true));
    assert_eq!(
        parsed
            .page_setup
            .as_ref()
            .and_then(|setup| setup.print_area),
        Some((0, 0, 1, 1))
    );
}

#[test]
fn malformed_ooxml_print_state_is_typed_not_flattened() {
    let xml = r#"<worksheet><sheetData/><pageSetup pageOrder="sideways"/>
            <rowBreaks><brk id="bad" man="1"/></rowBreaks>
            <headerFooter differentFirst="maybe"><firstHeader>first</firstHeader></headerFooter>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );
    assert_eq!(
        parsed.print_metadata.fidelity(),
        crate::PrintFidelity::Partial
    );
    assert!(parsed
        .print_metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::InvalidPageBreak));
    assert!(parsed
        .print_metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::MalformedHeaderFooter));
    assert!(parsed
        .print_metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::UnsupportedProperty));
}

#[test]
fn self_closing_ooxml_break_container_does_not_capture_stray_breaks() {
    let xml = r#"<worksheet><sheetData/><rowBreaks/><brk id="9" man="1"/></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );
    assert!(parsed.print_metadata.manual_row_breaks().is_empty());
    assert!(parsed.print_metadata.manual_col_breaks().is_empty());
}

#[test]
fn data_validations_metadata_is_parsed() {
    let xml = r#"<worksheet>
            <sheetData/>
            <dataValidations count="3">
                <dataValidation type="list" allowBlank="1" showInputMessage="1" sqref="A1 A3:A4" promptTitle="Pick" prompt="Choose one">
                    <formula1>"Yes,No"</formula1>
                </dataValidation>
                <dataValidation type="whole" operator="between" allowBlank="0" showErrorMessage="1" sqref="B1:B2" errorTitle="Bounds" error="1..9 only">
                    <formula1>1</formula1><formula2>9</formula2>
                </dataValidation>
                <dataValidation type="custom" sqref="C1"><formula1>ISNUMBER(C1)</formula1></dataValidation>
            </dataValidations>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.data_validations.len(), 4);
    assert_eq!(parsed.data_validations[0].sqref, (0, 0, 0, 0));
    assert_eq!(parsed.data_validations[1].sqref, (2, 0, 3, 0));
    assert_eq!(parsed.data_validations[0].kind, DvKind::List);
    assert_eq!(parsed.data_validations[0].formula1, "\"Yes,No\"");
    assert_eq!(
        parsed.data_validations[0].prompt.as_ref(),
        Some(&("Pick".to_string(), "Choose one".to_string()))
    );
    assert!(parsed.data_validations[0].show_input_message);
    assert!(!parsed.data_validations[0].show_error_message);
    let whole = &parsed.data_validations[2];
    assert_eq!(whole.kind, DvKind::Whole);
    assert_eq!(whole.operator, DvOp::Between);
    assert!(!whole.allow_blank);
    assert!(!whole.show_input_message);
    assert!(whole.show_error_message);
    assert_eq!(whole.formula1, "1");
    assert_eq!(whole.formula2.as_deref(), Some("9"));
    assert_eq!(
        whole.error.as_ref(),
        Some(&("Bounds".to_string(), "1..9 only".to_string()))
    );
    assert_eq!(parsed.data_validations[3].kind, DvKind::Custom);
    assert_eq!(parsed.data_validations[3].formula1, "ISNUMBER(C1)");
}

#[test]
fn data_validation_missing_allow_blank_defaults_false() {
    let xml = r#"<worksheet><sheetData/>
            <dataValidations count="1">
                <dataValidation type="whole" sqref="A1" promptTitle="Prompt" prompt="Text" errorTitle="Error" error="Text"><formula1>1</formula1></dataValidation>
            </dataValidations>
        </worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );

    assert_eq!(parsed.data_validations.len(), 1);
    assert!(!parsed.data_validations[0].allow_blank);
    assert!(!parsed.data_validations[0].show_input_message);
    assert!(!parsed.data_validations[0].show_error_message);
    assert_eq!(
        parsed.data_validations[0].prompt.as_ref(),
        Some(&("Prompt".to_string(), "Text".to_string()))
    );
    assert_eq!(
        parsed.data_validations[0].error.as_ref(),
        Some(&("Error".to_string(), "Text".to_string()))
    );
}

#[test]
fn shared_string_excludes_phonetic_ruby() {
    // `<rPh>` carries East Asian ruby (furigana) guide text, not part of the
    // displayed string — it must not be concatenated into the value.
    let xml = r#"<sst><si><t>東京</t><rPh sb="0" eb="2"><t>とうきょう</t></rPh></si></sst>"#;
    assert_eq!(shared_texts(xml), vec!["東京"]);
}

#[test]
fn formula_without_cached_value_is_surfaced() {
    // An uncalculated formula (`<f>` but no `<v>`) must still surface its
    // source as Cell::Formula, not be silently dropped.
    let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\"><f>SUM(B1:B2)</f></c></row></sheetData></worksheet>";
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;
    assert_eq!(cells.len(), 1);
    match &cells[0].value {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "SUM(B1:B2)");
            assert_eq!(**cached, Cell::Text(String::new()));
        }
        other => panic!("expected a formula cell, got {other:?}"),
    }
}

#[test]
fn self_closing_formula_keeps_cached_value() {
    // A self-closing `<f/>` (e.g. a shared-formula follower) has no formula
    // text and no End event; the following `<v>` must be read as the value,
    // not captured as formula text (which would surface an empty-cached
    // formula and swallow the 42).
    let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\"><f t=\"shared\" si=\"0\"/><v>42</v></c></row></sheetData></worksheet>";
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].value, Cell::Number(42.0));
}

#[test]
fn shift_formula_engine() {
    assert_eq!(shift_formula("A1+B1", 1, 0), "A2+B2");
    assert_eq!(shift_formula("$A$1+B1", 1, 1), "$A$1+C2");
    assert_eq!(shift_formula("SUM(A1:A3)", 2, 0), "SUM(A3:A5)");
    assert_eq!(shift_formula("LOG10(A1)", 1, 0), "LOG10(A2)"); // function, not a ref
    assert_eq!(shift_formula("\"A1\"&B2", 1, 0), "\"A1\"&B3"); // string literal untouched
    assert_eq!(shift_formula("$A1", 5, 9), "$A6"); // col absolute, row shifts
    assert_eq!(shift_formula("A1", 0, -1), "#REF!"); // shifted off-grid
    assert_eq!(shift_formula("Z9", 1, 1), "AA10"); // column carry
    assert_eq!(shift_formula("'My Sheet'!A1", 1, 0), "'My Sheet'!A2"); // sheet name kept
    assert_eq!(shift_formula("'A1'!B1", 1, 0), "'A1'!B2"); // ref inside '…' not shifted
    assert_eq!(shift_formula("XFE1+1", 1, 0), "XFE1+1"); // off-grid A1-shaped name kept
    assert_eq!(shift_formula("SUM(1:2)", 1, 0), "SUM(2:3)");
    assert_eq!(shift_formula("SUM($1:2)", 1, 0), "SUM($1:3)");
    assert_eq!(shift_formula("SUM(A:B)", 0, 1), "SUM(B:C)");
    assert_eq!(shift_formula("SUM($A:B)", 0, 1), "SUM($A:C)");
}

#[test]
fn shared_formula_follower_with_whitespace() {
    // A pretty-printed follower (whitespace between the self-closing `<f/>` and
    // `<v>`) must not capture that whitespace as formula text and be mis-registered
    // as a master.
    let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\"><f t=\"shared\" ref=\"A1:A2\" si=\"0\">B1*2</f><v>2</v></c></row>\
            <row r=\"2\"><c r=\"A2\"><f t=\"shared\" si=\"0\"/>\n            <v>4</v></c></row>\
            </sheetData></worksheet>";
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;
    match &cells[1].value {
        Cell::Formula { formula, .. } => assert_eq!(formula, "B2*2"),
        o => panic!("whitespace follower not reconstructed: {o:?}"),
    }
}

#[test]
fn shared_formula_follower_is_reconstructed() {
    // Master at A1 defines si=0; follower at A2 must surface the relative-shifted
    // formula (B1 -> B2), not a bare cached value.
    let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\"><f t=\"shared\" ref=\"A1:A2\" si=\"0\">B1*2</f><v>2</v></c></row>\
            <row r=\"2\"><c r=\"A2\"><f t=\"shared\" si=\"0\"/><v>4</v></c></row>\
            </sheetData></worksheet>";
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;
    assert_eq!(cells.len(), 2);
    match &cells[0].value {
        Cell::Formula { formula, .. } => assert_eq!(formula, "B1*2"),
        o => panic!("master not a formula: {o:?}"),
    }
    match &cells[1].value {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "B2*2");
            assert_eq!(**cached, Cell::Number(4.0));
        }
        o => panic!("follower not reconstructed: {o:?}"),
    }
}

#[test]
fn iso_date_cell_t_d() {
    // A `t="d"` ISO date cell (emitted by some non-Excel writers) must read as
    // a Date, not be dropped by the numeric fallback.
    let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\" t=\"d\"><v>2024-03-15</v></c></row></sheetData></worksheet>";
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].value, Cell::Date(45366.0));
}

#[test]
fn iso_date_cell_t_d_renders_datetime_and_time_only_values() {
    let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\" s=\"1\" t=\"d\"><v>2021-01-01T10:10:10</v></c>\
            <c r=\"A2\" s=\"2\" t=\"d\"><v>10:10:10</v></c>\
            </row></sheetData></worksheet>";
    let styles = Styles {
        xf_numfmt: vec![0, 22, 20],
        ..Default::default()
    };
    let mut budget = crate::MAX_TEXT_BYTES;
    let cells = parse_sheet(
        xml,
        &[],
        &styles,
        &ThemeColors::default(),
        false,
        &mut budget,
    )
    .cells;

    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0].text, "2021-01-01 10:10:10");
    assert_eq!(cells[1].text, "10:10:10");
}

#[test]
fn reads_merged_ranges_and_formula() {
    let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\"><f>SUM(B1:B2)</f><v>30</v></c>\
            <c r=\"B1\"><v>10</v></c></row>\
            <row r=\"2\"><c r=\"B2\"><v>20</v></c></row>\
            </sheetData>\
            <mergeCells count=\"1\"><mergeCell ref=\"A1:C1\"/></mergeCells>\
            </worksheet>";
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(
        xml,
        &[],
        &Styles::default(),
        &ThemeColors::default(),
        false,
        &mut budget,
    );
    let cells = parsed.cells;
    assert_eq!(parsed.merges, vec![(0, 0, 0, 2)]); // A1:C1

    // The formula cell exposes both the source text and the cached value.
    let a1 = cells.iter().find(|c| c.row == 0 && c.col == 0).unwrap();
    match &a1.value {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "SUM(B1:B2)");
            assert_eq!(**cached, Cell::Number(30.0));
        }
        other => panic!("expected a formula cell, got {other:?}"),
    }
    assert_eq!(a1.text, "30"); // display text is the cached value
}

#[test]
fn conditional_metadata_retains_priority_stop_full_dxf_and_losses() {
    let styles_xml = r#"<styleSheet>
            <dxfs count="1"><dxf>
                <font><b/><color rgb="FF112233"/><outline/></font>
                <fill><patternFill patternType="solid"><fgColor rgb="FF445566"/></patternFill></fill>
                <border><left style="thin"><color rgb="FF778899"/></left><diagonal style="thin"/></border>
                <numFmt numFmtId="166" formatCode="0.000"/>
                <alignment horizontal="center" wrapText="1" readingOrder="2"/>
                <protection locked="0" hidden="1"/>
            </dxf></dxfs>
        </styleSheet>"#;
    let theme = ThemeColors::default();
    let styles = parse_styles(styles_xml, &theme);
    let xml = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>2</v></c></row></sheetData>
            <conditionalFormatting sqref="A1">
                <cfRule type="cellIs" dxfId="0" priority="7" stopIfTrue="1" operator="greaterThan"><formula>1</formula></cfRule>
            </conditionalFormatting></worksheet>"#;
    let mut budget = crate::MAX_TEXT_BYTES;
    let parsed = parse_sheet(xml, &[], &styles, &theme, false, &mut budget);

    assert_eq!(parsed.cond_formats.len(), 1);
    assert_eq!(parsed.cond_format_metadata.len(), 1);
    let metadata = &parsed.cond_format_metadata[0];
    assert_eq!(metadata.priority, Some(7));
    assert!(metadata.stop_if_true);
    let dxf = metadata
        .differential_style
        .as_ref()
        .expect("retained differential style");
    assert_eq!(dxf.fill, Some(Color::rgb(0x44, 0x55, 0x66)));
    assert_eq!(
        dxf.font.as_ref().and_then(|font| font.color),
        Some(Color::rgb(0x11, 0x22, 0x33))
    );
    assert!(dxf.font.as_ref().is_some_and(|font| font.bold));
    assert_eq!(
        dxf.border.as_ref().map(|border| border.left),
        Some(BorderStyle::Thin)
    );
    assert_eq!(dxf.num_fmt.as_deref(), Some("0.000"));
    assert!(dxf.align.as_ref().is_some_and(|align| align.wrap));
    assert!(dxf
        .protection
        .as_ref()
        .is_some_and(|protection| protection.locked == Some(false) && protection.hidden));
    assert!(metadata
        .style_losses
        .iter()
        .any(|loss| { loss.kind == StyleLossKind::UnsupportedProperty && loss.occurrences >= 2 }));
}
#[test]
fn zero_print_title_rows_are_rejected_without_panicking() {
    assert_eq!(parse_repeat_rows("0:1"), None);
    assert_eq!(parse_repeat_rows("$0:$1"), None);
    assert_eq!(parse_repeat_rows("1:1048577"), None);
}
