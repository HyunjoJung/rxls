use std::io::Write;

use rxls::{
    ChartFrameFill, ChartMarkerSymbol, ChartSeriesStyleLossKind, Color, DrawingObjectKind, Workbook,
};
use rxls_render::{
    build_print_document, render_print_document_pdf, render_print_page_png, render_sheet_svg,
    Fixed, LimitKind, PathCommand, PrintOptions, RenderError, RenderOptions, RenderRange,
    RenderSelection, Rgb, SceneNode, WarningCode,
};
use zip::write::SimpleFileOptions;

fn line_chart_workbook(series_style: &str) -> Workbook {
    line_chart_workbook_with_metadata(series_style, "", "<c:majorGridlines/>", "")
}

fn line_chart_workbook_with_metadata(
    series_style: &str,
    category_axis_style: &str,
    value_axis_style: &str,
    chart_space_style: &str,
) -> Workbook {
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
          <c:catAx><c:axId val="1"/>{category_axis_style}<c:crossAx val="2"/></c:catAx>
          <c:valAx><c:axId val="2"/>{value_axis_style}<c:crossAx val="1"/></c:valAx>
          </c:plotArea></c:chart>{chart_space_style}
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
            r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#
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

fn scatter_chart_workbook(x_axis_style: &str, y_axis_style: &str) -> Workbook {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    let chart = format!(
        r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
          <c:chart><c:plotArea><c:scatterChart><c:scatterStyle val="marker"/><c:ser>
            <c:idx val="0"/><c:order val="0"/>
            <c:xVal><c:numRef><c:f>Host!$A$1:$A$4</c:f></c:numRef></c:xVal>
            <c:yVal><c:numRef><c:f>Host!$B$1:$B$4</c:f></c:numRef></c:yVal>
          </c:ser><c:axId val="1"/><c:axId val="2"/></c:scatterChart>
          <c:valAx><c:axId val="1"/>{x_axis_style}<c:crossAx val="2"/></c:valAx>
          <c:valAx><c:axId val="2"/>{y_axis_style}<c:crossAx val="1"/></c:valAx>
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
              <row r="1"><c r="A1"><v>10</v></c><c r="B1"><v>120</v></c></row>
              <row r="2"><c r="A2"><v>20</v></c><c r="B2"><v>220</v></c></row>
              <row r="3"><c r="A3"><v>30</v></c><c r="B3"><v>320</v></c></row>
              <row r="4"><c r="A4"><v>40</v></c><c r="B4"><v>420</v></c></row>
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
            r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#
                .to_string(),
        ),
        ("xl/charts/chart1.xml", chart),
    ];
    for (name, body) in parts {
        writer.start_file(name, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    Workbook::open(&writer.finish().unwrap().into_inner())
        .expect("minimal OOXML scatter chart package")
}

fn chart_options() -> RenderOptions {
    RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 12, 9)),
        gridlines: false,
        ..RenderOptions::default()
    }
}

#[test]
fn imported_missing_theme_chart_labels_use_calc_latin_point_defaults() {
    const CALC_LATIN_FALLBACK: &str = "Liberation Sans";
    const CALLER_FALLBACK: &str = "Noto Sans CJK KR";
    const CHART_TEXT_SIZE: Fixed = Fixed::from_raw(13_653);

    let workbook = line_chart_workbook("");
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("chart sidecar");
    assert_eq!(
        metadata.chart_default_latin_font_family.as_deref(),
        Some(CALC_LATIN_FALLBACK)
    );

    let mut options = chart_options();
    options.default_font_family = CALLER_FALLBACK.to_string();
    let output = render_sheet_svg(&workbook, 0, &options).expect("render missing-theme chart");

    for category in ["Q1", "Q2", "Q3", "Q4"] {
        let text = output
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Text(text)
                    if text.text == category && text.style.family == CALC_LATIN_FALLBACK =>
                {
                    Some(text)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing Calc-styled chart category {category}"));
        assert_eq!(text.style.size, CHART_TEXT_SIZE, "{category}");
        assert!(!text.style.bold, "{category}");
    }

    assert!(
        output.scene.nodes.iter().any(|node| match node {
            SceneNode::Text(text) => {
                text.text == "Q1"
                    && text.style.family == CALLER_FALLBACK
                    && text.style.size == options.default_font_size
                    && !text.style.bold
            }
            _ => false,
        }),
        "worksheet text must retain the caller fallback"
    );
}

#[test]
fn imported_implicit_chart_space_fill_is_transparent() {
    let workbook = line_chart_workbook("");
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("chart sidecar");
    assert_eq!(metadata.chart_frame_fill, ChartFrameFill::Automatic);
    assert!(metadata.chart_default_latin_font_family.is_some());
    assert!(metadata.chart_frame_style_losses.is_empty());

    let output = render_sheet_svg(&workbook, 0, &chart_options()).unwrap();
    assert!(output.scene.nodes.iter().any(|node| {
        matches!(
            node,
            SceneNode::Rect(frame)
                if frame.fill.is_none()
                    && frame.stroke == Some(Rgb::new(127, 127, 127))
                    && frame.stroke_width == Fixed::from_pixels(1)
        )
    }));
}

#[test]
fn imported_line_chart_renders_categories_nice_axis_and_circle_markers() {
    let workbook = line_chart_workbook(
        r#"<c:marker><c:symbol val="circle"/><c:size val="5"/></c:marker>
        <c:spPr><a:ln w="38100"><a:solidFill><a:srgbClr val="336699"/></a:solidFill></a:ln></c:spPr>"#,
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
    assert_eq!(metadata.chart_series_styles[0].line_width_emu, Some(38_100));

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
    let series_lines = first
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(line) if line.color == Rgb::new(0x33, 0x66, 0x99) => Some(line),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(series_lines.len(), 3);
    assert!(series_lines
        .iter()
        .all(|line| line.width == Fixed::from_pixels(4)));
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
    assert!(first.svg.contains("stroke-width=\"4\""));

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
fn imported_line_chart_cross_between_moves_series_into_category_bands() {
    let style = r#"<c:spPr><a:ln w="38100"><a:solidFill><a:srgbClr val="336699"/></a:solidFill></a:ln></c:spPr>"#;
    let between =
        line_chart_workbook_with_metadata(style, "", r#"<c:crossBetween val="between"/>"#, "");
    let mid_cat =
        line_chart_workbook_with_metadata(style, "", r#"<c:crossBetween val="midCat"/>"#, "");
    assert_eq!(
        between.sheets[0]
            .drawing_metadata()
            .iter()
            .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
            .expect("between chart sidecar")
            .chart_category_axis_shifted,
        Some(true)
    );
    assert_eq!(
        mid_cat.sheets[0]
            .drawing_metadata()
            .iter()
            .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
            .expect("midCat chart sidecar")
            .chart_category_axis_shifted,
        Some(false)
    );

    let x_positions = |workbook: &Workbook| {
        let output = render_sheet_svg(workbook, 0, &chart_options()).unwrap();
        let mut positions = output
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Line(line) if line.color == Rgb::new(0x33, 0x66, 0x99) => {
                    Some([line.x1.raw(), line.x2.raw()])
                }
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        positions.sort_unstable();
        positions.dedup();
        positions
    };
    let shifted = x_positions(&between);
    let endpoints = x_positions(&mid_cat);
    assert_eq!(shifted.len(), 4);
    assert_eq!(endpoints.len(), 4);
    assert!(shifted[0] > endpoints[0]);
    assert!(shifted[3] < endpoints[3]);

    let between_output = render_sheet_svg(&between, 0, &chart_options()).unwrap();
    let frame = between_output
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Rect(frame)
                if frame.stroke == Some(Rgb::new(127, 127, 127))
                    && frame.stroke_width == Fixed::from_pixels(1) =>
            {
                Some(frame.rect)
            }
            _ => None,
        })
        .expect("imported chart frame");
    let horizontal_axis = between_output
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Line(line)
                if line.color == Rgb::BLACK && line.y1 == line.y2 && line.x2 > line.x1 =>
            {
                Some(line.clone())
            }
            _ => None,
        })
        .expect("imported chart horizontal axis");
    let vertical_axis = between_output
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Line(line)
                if line.color == Rgb::BLACK && line.x1 == line.x2 && line.y2 > line.y1 =>
            {
                Some(line.clone())
            }
            _ => None,
        })
        .expect("imported chart vertical axis");
    assert_eq!(
        Fixed::from_raw(frame.x.raw() + frame.width.raw() - horizontal_axis.x2.raw()),
        Fixed::from_pixels(8),
        "Calc keeps the final shifted category band inside the frame padding"
    );
    assert_eq!(
        Fixed::from_raw(vertical_axis.y1.raw() - frame.y.raw()),
        Fixed::from_pixels(8),
        "Calc keeps the shifted value-axis top inside the frame padding"
    );
}

#[test]
fn imported_scatter_chart_replays_first_value_axis_major_gridlines_on_x_axis() {
    let workbook = scatter_chart_workbook("<c:majorGridlines/>", "");
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("scatter chart sidecar");
    assert_eq!(metadata.chart_category_major_gridlines, Some(true));
    assert_eq!(metadata.chart_value_major_gridlines, Some(false));

    let output = render_sheet_svg(&workbook, 0, &chart_options()).unwrap();
    let vertical_major_gridlines = output
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(line)
                if line.color == Rgb::new(217, 217, 217) && line.x1 == line.x2 =>
            {
                Some(line)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(vertical_major_gridlines.len(), 7);
    assert!(vertical_major_gridlines
        .windows(2)
        .all(|lines| lines[0].x1 < lines[1].x1));
    assert!(vertical_major_gridlines
        .iter()
        .all(|line| line.y1 < line.y2));
    let plot_x_bounds = output
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Line(line)
                if line.color == Rgb::BLACK && line.y1 == line.y2 && line.x1 < line.x2 =>
            {
                Some((line.x1, line.x2))
            }
            _ => None,
        })
        .expect("scatter plot horizontal axis");
    assert!(vertical_major_gridlines
        .iter()
        .all(|line| line.x1 >= plot_x_bounds.0 && line.x1 <= plot_x_bounds.1));
    assert!(!output
        .scene
        .nodes
        .iter()
        .any(|node| matches!(node, SceneNode::Line(line)
            if line.color == Rgb::new(217, 217, 217) && line.y1 == line.y2)));

    let without_gridlines = scatter_chart_workbook("", "");
    let metadata = without_gridlines.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("scatter chart sidecar");
    assert_eq!(metadata.chart_category_major_gridlines, Some(false));
    assert_eq!(metadata.chart_value_major_gridlines, Some(false));
    let output = render_sheet_svg(&without_gridlines, 0, &chart_options()).unwrap();
    assert!(!output.scene.nodes.iter().any(
        |node| matches!(node, SceneNode::Line(line) if line.color == Rgb::new(217, 217, 217))
    ));
}

#[test]
fn category_chart_major_gridline_replay_remains_on_the_category_axis() {
    let workbook = line_chart_workbook_with_metadata("", "<c:majorGridlines/>", "", "");
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("line chart sidecar");
    assert_eq!(metadata.chart_category_major_gridlines, Some(true));
    assert_eq!(metadata.chart_value_major_gridlines, Some(false));

    let output = render_sheet_svg(&workbook, 0, &chart_options()).unwrap();
    let vertical_major_gridlines = output
        .scene
        .nodes
        .iter()
        .filter(|node| {
            matches!(node, SceneNode::Line(line)
                if line.color == Rgb::new(217, 217, 217) && line.x1 == line.x2)
        })
        .count();
    assert_eq!(vertical_major_gridlines, 4);
    assert!(!output
        .scene
        .nodes
        .iter()
        .any(|node| matches!(node, SceneNode::Line(line)
            if line.color == Rgb::new(217, 217, 217) && line.y1 == line.y2)));
}

#[test]
fn explicit_no_fill_and_absent_major_gridlines_do_not_invent_chart_paint() {
    let workbook = line_chart_workbook_with_metadata(
        r#"<c:spPr><a:ln w="19050"><a:solidFill><a:srgbClr val="AA00CC"/></a:solidFill></a:ln></c:spPr>"#,
        "",
        "",
        "<c:spPr><a:noFill/></c:spPr>",
    );
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("chart sidecar");
    assert_eq!(metadata.chart_frame_fill, ChartFrameFill::NoFill);
    assert_eq!(metadata.chart_category_major_gridlines, Some(false));
    assert_eq!(metadata.chart_value_major_gridlines, Some(false));

    let output = render_sheet_svg(&workbook, 0, &chart_options()).unwrap();
    assert!(!output.scene.nodes.iter().any(
        |node| matches!(node, SceneNode::Line(line) if line.color == Rgb::new(217, 217, 217))
    ));
    assert!(output.scene.nodes.iter().any(|node| {
        matches!(
            node,
            SceneNode::Rect(frame)
                if frame.fill.is_none()
                    && frame.stroke == Some(Rgb::new(127, 127, 127))
                    && frame.stroke_width == Fixed::from_pixels(1)
        )
    }));
    let retained_lines = output
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(line) if line.color == Rgb::new(0xAA, 0x00, 0xCC) => Some(line),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(retained_lines.len(), 3);
    assert!(retained_lines
        .iter()
        .all(|line| line.width == Fixed::from_pixels(2)));
}

#[test]
fn explicit_solid_chart_space_fill_is_rendered_exactly() {
    let workbook = line_chart_workbook_with_metadata(
        "",
        "",
        "",
        r#"<c:spPr><a:solidFill><a:srgbClr val="102030"/></a:solidFill></c:spPr>"#,
    );
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("chart sidecar");
    assert_eq!(
        metadata.chart_frame_fill,
        ChartFrameFill::Solid(Color::rgb(0x10, 0x20, 0x30))
    );

    let output = render_sheet_svg(&workbook, 0, &chart_options()).unwrap();
    assert!(output.scene.nodes.iter().any(|node| {
        matches!(
            node,
            SceneNode::Rect(frame)
                if frame.fill == Some(Rgb::new(0x10, 0x20, 0x30))
                    && frame.stroke == Some(Rgb::new(127, 127, 127))
                    && frame.stroke_width == Fixed::from_pixels(1)
        )
    }));
    assert!(output.svg.contains("fill=\"#102030\""));
}

#[test]
fn explicit_zero_emu_series_width_is_one_visible_hairline_across_backends() {
    let workbook = line_chart_workbook_with_metadata(
        r#"<c:marker><c:symbol val="none"/></c:marker>
        <c:spPr><a:ln w="0"><a:solidFill><a:srgbClr val="AA00CC"/></a:solidFill></a:ln></c:spPr>"#,
        "",
        "",
        "<c:spPr><a:noFill/></c:spPr>",
    );
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("chart sidecar");
    assert_eq!(metadata.chart_series_styles[0].line_width_emu, Some(0));

    let output = render_sheet_svg(&workbook, 0, &chart_options()).unwrap();
    let series_lines = output
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(line) if line.color == Rgb::new(0xAA, 0x00, 0xCC) => Some(line),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(series_lines.len(), 3);
    assert!(series_lines
        .iter()
        .all(|line| line.width == Fixed::from_pixels(1)));
    assert_eq!(
        output
            .svg
            .matches("stroke=\"#AA00CC\" stroke-width=\"1\"")
            .count(),
        3
    );
    assert!(!output.svg.contains("stroke=\"#AA00CC\" stroke-width=\"0\""));

    let mut document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            render: chart_options(),
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(document.pages.len(), 1);
    assert_eq!(
        document.pages[0]
            .scene
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node,
                    SceneNode::Line(line)
                        if line.color == Rgb::new(0xAA, 0x00, 0xCC)
                            && line.width == Fixed::from_pixels(1)
                )
            })
            .count(),
        3
    );
    document.pages[0]
        .scene
        .nodes
        .retain(|node| !matches!(node, SceneNode::Text(_)));

    let pdf = render_print_document_pdf(&document).unwrap();
    let pdf_text = String::from_utf8_lossy(&pdf);
    assert_eq!(pdf_text.matches("0.666667 0 0.8 RG\n1 w 0 J").count(), 3);
    assert!(!pdf_text.contains("0.666667 0 0.8 RG\n0 w 0 J"));

    let png = render_print_page_png(&document.pages[0], 96, &document).unwrap();
    let raster = tiny_skia::Pixmap::decode_png(&png).unwrap();
    assert!(raster.pixels().iter().any(|pixel| {
        pixel.red().saturating_sub(pixel.green()) > 30
            && pixel.blue().saturating_sub(pixel.green()) > 40
    }));
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
