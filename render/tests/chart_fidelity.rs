use std::io::Write;

use rxls::{ChartMarkerSymbol, ChartSeriesStyleLossKind, DrawingObjectKind, Workbook};
use rxls_render::{
    render_sheet_svg, LimitKind, PathCommand, RenderError, RenderOptions, RenderRange,
    RenderSelection, Rgb, SceneNode, WarningCode,
};
use zip::write::SimpleFileOptions;

fn line_chart_workbook(series_style: &str) -> Workbook {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    let chart = format!(
        r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
          <c:chart><c:plotArea><c:lineChart><c:grouping val="standard"/><c:ser>
            <c:idx val="0"/><c:order val="0"/>{series_style}
            <c:tx><c:strRef><c:f>Host!$C$1</c:f><c:strCache><c:pt idx="0"><c:v>Revenue</c:v></c:pt></c:strCache></c:strRef></c:tx>
            <c:cat><c:strRef><c:f>Host!$A$1:$A$4</c:f><c:strCache><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt><c:pt idx="2"><c:v>Q3</c:v></c:pt><c:pt idx="3"><c:v>Q4</c:v></c:pt></c:strCache></c:strRef></c:cat>
            <c:val><c:numRef><c:f>Host!$B$1:$B$4</c:f><c:numCache><c:pt idx="0"><c:v>28</c:v></c:pt><c:pt idx="1"><c:v>41</c:v></c:pt><c:pt idx="2"><c:v>54</c:v></c:pt><c:pt idx="3"><c:v>67</c:v></c:pt></c:numCache></c:numRef></c:val>
          </c:ser><c:axId val="1"/><c:axId val="2"/></c:lineChart>
          <c:catAx><c:axId val="1"/><c:crossAx val="2"/></c:catAx>
          <c:valAx><c:axId val="2"/><c:crossAx val="1"/></c:valAx>
          </c:plotArea></c:chart>
        </c:chartSpace>"#
    );
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Host" r:id="rId1"/></sheets></workbook>"#
                .to_string(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#
                .to_string(),
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData>
              <row r="1"><c r="A1" t="inlineStr"><is><t>Q1</t></is></c><c r="B1"><v>28</v></c><c r="C1" t="inlineStr"><is><t>Revenue</t></is></c></row>
              <row r="2"><c r="A2" t="inlineStr"><is><t>Q2</t></is></c><c r="B2"><v>41</v></c></row>
              <row r="3"><c r="A3" t="inlineStr"><is><t>Q3</t></is></c><c r="B3"><v>54</v></c></row>
              <row r="4"><c r="A4" t="inlineStr"><is><t>Q4</t></is></c><c r="B4"><v>67</v></c></row>
            </sheetData><drawing r:id="rIdDraw"/></worksheet>"#
                .to_string(),
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#
                .to_string(),
        ),
        (
            "xl/drawings/drawing1.xml",
            r#"<wsDr><twoCellAnchor><from><col>1</col><row>0</row></from><to><col>9</col><row>12</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#
                .to_string(),
        ),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            r#"<Relationships><Relationship Id="rIdChart" Target="../charts/chart1.xml"/></Relationships>"#
                .to_string(),
        ),
        ("xl/charts/chart1.xml", chart),
    ];
    for (name, body) in parts {
        writer.start_file(name, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    Workbook::open(&writer.finish().unwrap().into_inner()).expect("minimal OOXML chart package")
}

fn chart_options() -> RenderOptions {
    RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 12, 9)),
        gridlines: false,
        ..RenderOptions::default()
    }
}

#[test]
fn imported_line_chart_renders_categories_nice_axis_and_circle_markers() {
    let workbook = line_chart_workbook(
        r#"<c:marker><c:symbol val="circle"/><c:size val="5"/></c:marker>
        <c:spPr><a:ln><a:solidFill><a:srgbClr val="336699"/></a:solidFill></a:ln></c:spPr>"#,
    );
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .unwrap_or_else(|| {
            panic!(
                "chart sidecar; charts={:?} metadata={:?}",
                workbook.sheets[0].charts(),
                workbook.sheets[0].drawing_metadata()
            )
        });
    assert_eq!(metadata.chart_series_styles.len(), 1);
    assert_eq!(
        metadata.chart_series_styles[0].marker,
        ChartMarkerSymbol::Circle
    );
    assert_eq!(metadata.chart_series_styles[0].marker_size, Some(5));
    assert_eq!(
        metadata.chart_series_styles[0]
            .line_color
            .map(|color| color.as_rgb()),
        Some([0x33, 0x66, 0x99])
    );

    let first = render_sheet_svg(&workbook, 0, &chart_options()).unwrap();
    let second = render_sheet_svg(&workbook, 0, &chart_options()).unwrap();
    assert_eq!(first, second, "chart scene and SVG must be deterministic");
    assert!(!first
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::ChartPlaceholder));
    let texts = first
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for category in ["Q1", "Q2", "Q3", "Q4"] {
        assert!(
            texts.contains(&category),
            "missing category label {category}"
        );
    }
    for tick in ["0", "10", "20", "30", "40", "50", "60", "70", "80"] {
        assert!(texts.contains(&tick), "missing nice-axis tick {tick}");
    }
    let major_gridlines = first
        .scene
        .nodes
        .iter()
        .filter(|node| {
            matches!(node, SceneNode::Line(line) if line.color == Rgb::new(217, 217, 217) && line.y1 == line.y2)
        })
        .count();
    assert_eq!(major_gridlines, 9);
    let circle_markers = first
        .scene
        .nodes
        .iter()
        .filter(|node| {
            matches!(node, SceneNode::Path(path)
                if path.fill == Some(Rgb::new(0x33, 0x66, 0x99))
                    && path.commands.iter().filter(|command| matches!(command, PathCommand::CubicTo { .. })).count() == 4)
        })
        .count();
    assert_eq!(circle_markers, 4);
    assert!(first.svg.contains("stroke=\"#336699\""));

    let marker_path_commands = first
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Path(path) => Some(path.commands.len() as u64),
            _ => None,
        })
        .sum::<u64>();
    assert_eq!(marker_path_commands, 24);
    let mut limited = chart_options();
    limited.limits.max_path_commands = marker_path_commands - 1;
    assert_eq!(
        render_sheet_svg(&workbook, 0, &limited),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::PathCommands,
            limit: marker_path_commands - 1,
            actual: marker_path_commands,
        })
    );
    let mut limited = chart_options();
    limited.limits.max_chart_points = 7;
    assert_eq!(
        render_sheet_svg(&workbook, 0, &limited),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::ChartPoints,
            limit: 7,
            actual: 8,
        })
    );
}

#[test]
fn unsupported_series_style_uses_typed_warning_without_painting_hidden_line() {
    let workbook = line_chart_workbook(
        r#"<c:marker><c:symbol val="picture"/><c:size val="255"/></c:marker>
        <c:spPr><a:ln><a:noFill/><a:prstDash val="dash"/></a:ln></c:spPr>"#,
    );
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .unwrap_or_else(|| {
            panic!(
                "chart sidecar; charts={:?} metadata={:?}",
                workbook.sheets[0].charts(),
                workbook.sheets[0].drawing_metadata()
            )
        });
    let style = &metadata.chart_series_styles[0];
    assert!(!style.line_visible);
    assert_eq!(style.marker, ChartMarkerSymbol::Automatic);
    assert_eq!(
        style.losses,
        [
            ChartSeriesStyleLossKind::UnsupportedMarkerSymbol,
            ChartSeriesStyleLossKind::InvalidMarkerSize,
            ChartSeriesStyleLossKind::UnsupportedLinePaint,
        ]
    );

    let output = render_sheet_svg(&workbook, 0, &chart_options()).unwrap();
    assert!(!output
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::ChartPlaceholder));
    assert!(output.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::ChartMetadataSimplified && warning.occurrences == 3
    }));
    assert!(!output.scene.nodes.iter().any(|node| {
        matches!(node, SceneNode::Line(line) if line.color == Rgb::new(68, 114, 196))
    }));
}
