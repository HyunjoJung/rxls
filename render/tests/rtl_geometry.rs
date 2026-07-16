use std::io::Write;

use rxls::{Chart, ChartKind, Image, ImageFmt, PageSetup, Series, Workbook};
use rxls_render::{
    build_print_document, build_scene, Fixed, PrintOptions, Rect, RenderOptions, RenderRange,
    RenderSelection, Rgb, Scene, SceneNode, TextAnchor, TextNode, WarningCode,
};
use zip::write::SimpleFileOptions;

fn render_range(workbook: &Workbook, range: RenderRange) -> rxls_render::SceneBuild {
    build_scene(
        workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap()
}

fn text_node<'a>(scene: &'a Scene, text: &str) -> &'a TextNode {
    scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Text(node) if node.text == text => Some(node),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing text node {text:?}"))
}

fn assert_horizontal_reflection(ltr: Rect, rtl: Rect, canvas_width: Fixed) {
    assert_eq!(rtl.y, ltr.y);
    assert_eq!(rtl.width, ltr.width);
    assert_eq!(rtl.height, ltr.height);
    assert_eq!(
        rtl.x.raw(),
        canvas_width.raw() - ltr.x.raw() - ltr.width.raw()
    );
}

fn largest_rect_with_fill(scene: &Scene, fill: Rgb) -> Rect {
    scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Rect(node) if node.fill == Some(fill) => Some(node.rect),
            _ => None,
        })
        .max_by_key(|rect| i128::from(rect.width.raw()) * i128::from(rect.height.raw()))
        .unwrap_or_else(|| panic!("missing rectangle with fill {fill:?}"))
}

fn axis_workbook(right_to_left: bool) -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("axis");
    sheet.set_right_to_left(right_to_left);
    sheet.set_col_width(1, 5.0);
    sheet.set_col_width(2, 11.0);
    sheet.set_col_width(3, 19.0);
    sheet.set_col_width(4, 8.0);
    sheet.write(0, 1, "column-B");
    sheet.write(0, 2, "hidden-C");
    sheet.write(0, 3, "column-D");
    sheet.write(0, 4, "column-E");
    sheet.hide_column(2);
    workbook
}

#[test]
fn rtl_reflects_unequal_explicit_columns_without_reordering_logical_selection() {
    let range = RenderRange::new(0, 1, 0, 4);
    let ltr = render_range(&axis_workbook(false), range);
    let rtl = render_range(&axis_workbook(true), range);

    assert_eq!(ltr.report.range, range);
    assert_eq!(rtl.report.range, range);
    assert_eq!(ltr.scene.width, rtl.scene.width);
    assert_eq!(ltr.report.hidden_columns_skipped, 1);
    assert_eq!(rtl.report.hidden_columns_skipped, 1);
    assert!(rtl
        .scene
        .nodes
        .iter()
        .all(|node| { !matches!(node, SceneNode::Text(node) if node.text == "hidden-C") }));

    for text in ["column-B", "column-D", "column-E"] {
        assert_horizontal_reflection(
            text_node(&ltr.scene, text).bounds,
            text_node(&rtl.scene, text).bounds,
            rtl.scene.width,
        );
    }
    assert!(
        text_node(&rtl.scene, "column-E").bounds.x < text_node(&rtl.scene, "column-D").bounds.x
    );
    assert!(
        text_node(&rtl.scene, "column-D").bounds.x < text_node(&rtl.scene, "column-B").bounds.x
    );
}

fn partial_merge_workbook(right_to_left: bool) -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("merge");
    sheet.set_right_to_left(right_to_left);
    for (column, width) in [(0, 7.0), (1, 13.0), (2, 21.0), (3, 9.0)] {
        sheet.set_col_width(column, width);
    }
    sheet.write(0, 0, "partial-merge");
    sheet.write(0, 3, "outside");
    sheet.merge(0, 0, 0, 2);
    workbook
}

#[test]
fn rtl_reflects_partially_selected_merges_from_the_visual_minimum_edge() {
    let range = RenderRange::new(0, 1, 0, 3);
    let ltr = render_range(&partial_merge_workbook(false), range);
    let rtl = render_range(&partial_merge_workbook(true), range);
    let ltr_merge = text_node(&ltr.scene, "partial-merge").bounds;
    let rtl_merge = text_node(&rtl.scene, "partial-merge").bounds;

    assert_horizontal_reflection(ltr_merge, rtl_merge, rtl.scene.width);
    assert!(ltr_merge.width > text_node(&ltr.scene, "outside").bounds.width);
    assert!(rtl.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::MergeAnchorOutsideVisibleRange && warning.occurrences == 1
    }));
}

fn overflow_workbook(right_to_left: bool) -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("overflow");
    sheet.set_right_to_left(right_to_left);
    for (column, width) in [(0, 6.0), (1, 8.0), (2, 10.0), (3, 12.0), (4, 14.0)] {
        sheet.set_col_width(column, width);
    }
    sheet.write(0, 0, "rtl-blocker");
    sheet.write(0, 2, "overflow-through-empty");
    sheet.write(0, 4, "ltr-blocker");
    workbook
}

#[test]
fn rtl_ltr_text_overflow_starts_at_the_visual_left_and_stops_at_blockers() {
    let range = RenderRange::new(0, 0, 0, 4);
    let ltr = render_range(&overflow_workbook(false), range);
    let rtl = render_range(&overflow_workbook(true), range);
    let ltr_text = text_node(&ltr.scene, "overflow-through-empty");
    let rtl_text = text_node(&rtl.scene, "overflow-through-empty");
    let ltr_blocker = text_node(&ltr.scene, "ltr-blocker");
    let rtl_blocker = text_node(&rtl.scene, "rtl-blocker");

    assert_horizontal_reflection(ltr_text.bounds, rtl_text.bounds, rtl.scene.width);
    assert_eq!(
        ltr_text.clip_bounds.x.raw() + ltr_text.clip_bounds.width.raw(),
        ltr_blocker.bounds.x.raw()
    );
    assert_eq!(rtl_text.clip_bounds.x.raw(), rtl_text.bounds.x.raw());
    assert_eq!(
        rtl_text.clip_bounds.x.raw() + rtl_text.clip_bounds.width.raw(),
        rtl_blocker.bounds.x.raw()
    );
}

#[test]
fn rtl_general_alignment_follows_the_first_strong_character_and_cell_type() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("general-alignment");
    sheet.set_right_to_left(true);
    sheet.write(0, 0, "Latin");
    sheet.write(1, 0, "中文");
    sheet.write(2, 0, "עברית");
    sheet.write(3, 0, "العربية");
    sheet.write_number(4, 0, 42);

    let build = render_range(&workbook, RenderRange::new(0, 0, 4, 0));
    for text in ["Latin", "中文"] {
        assert_eq!(
            text_node(&build.scene, text).style.anchor,
            TextAnchor::Start
        );
    }
    for text in ["עברית", "العربية", "42"] {
        assert_eq!(text_node(&build.scene, text).style.anchor, TextAnchor::End);
    }
}

fn solid_rgba_png(width: u32, height: u32) -> Vec<u8> {
    let pixels = usize::try_from(u64::from(width) * u64::from(height)).unwrap();
    let rgba = [17, 89, 143, 211].repeat(pixels);
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&rgba).unwrap();
    }
    output
}

fn drawing_workbook(right_to_left: bool) -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("drawings");
    sheet.set_right_to_left(right_to_left);
    for (column, width) in [
        (0, 7.0),
        (1, 11.0),
        (2, 16.0),
        (3, 9.0),
        (4, 20.0),
        (5, 8.0),
    ] {
        sheet.set_col_width(column, width);
    }
    sheet.write_number(0, 5, 2);
    sheet.write_number(1, 5, 5);
    sheet.add_image(Image::new(solid_rgba_png(2, 2), ImageFmt::Png, (0, 0)).with_to((2, 1)));
    sheet.add_chart(
        Chart::new(ChartKind::Line, (0, 2), (8, 5))
            .with_title("anchor-chart")
            .add_series(Series::new("drawings!$F$1:$F$2")),
    );
    workbook
}

fn cell_anchored_shape_workbook(right_to_left: bool) -> Workbook {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let sheet_view = if right_to_left {
        r#"<sheetViews><sheetView rightToLeft="1"/></sheetViews>"#
    } else {
        r#"<sheetViews><sheetView/></sheetViews>"#
    };
    let worksheet =
        format!(r#"<worksheet>{sheet_view}<sheetData/><drawing r:id="rIdDraw"/></worksheet>"#);
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Shapes" r:id="rId1"/></sheets></workbook>"#
                .to_string(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#
                .to_string(),
        ),
        ("xl/worksheets/sheet1.xml", worksheet),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#
                .to_string(),
        ),
        (
            "xl/drawings/drawing1.xml",
            r#"<wsDr><twoCellAnchor><from><col>1</col><row>0</row></from><to><col>4</col><row>3</row></to><sp><nvSpPr><cNvPr id="1" name="Callout"/></nvSpPr></sp></twoCellAnchor></wsDr>"#
                .to_string(),
        ),
    ];
    for (name, body) in parts {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    Workbook::open(&writer.finish().unwrap().into_inner()).unwrap()
}

#[test]
fn rtl_reflects_final_image_chart_and_shape_anchor_rectangles() {
    let range = RenderRange::new(0, 0, 8, 5);
    let ltr = render_range(&drawing_workbook(false), range);
    let rtl = render_range(&drawing_workbook(true), range);
    let ltr_image = ltr
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Image(node) => Some(node.rect),
            _ => None,
        })
        .expect("decoded image");
    let rtl_image = rtl
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Image(node) => Some(node.rect),
            _ => None,
        })
        .expect("decoded image");
    assert_horizontal_reflection(ltr_image, rtl_image, rtl.scene.width);

    let ltr_chart = largest_rect_with_fill(&ltr.scene, Rgb::WHITE);
    let rtl_chart = largest_rect_with_fill(&rtl.scene, Rgb::WHITE);
    assert_horizontal_reflection(ltr_chart, rtl_chart, rtl.scene.width);

    let shape_range = RenderRange::new(0, 0, 3, 4);
    let ltr_shape = render_range(&cell_anchored_shape_workbook(false), shape_range);
    let rtl_shape = render_range(&cell_anchored_shape_workbook(true), shape_range);
    assert!(rtl_shape.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::ShapePlaceholder && warning.occurrences == 1
    }));
    assert_horizontal_reflection(
        largest_rect_with_fill(&ltr_shape.scene, Rgb::new(221, 235, 247)),
        largest_rect_with_fill(&rtl_shape.scene, Rgb::new(221, 235, 247)),
        rtl_shape.scene.width,
    );
}

fn print_workbook(right_to_left: bool) -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("print-rtl");
    sheet.set_right_to_left(right_to_left);
    for (column, width) in [(0, 5.0), (1, 20.0), (2, 20.0), (3, 50.0)] {
        sheet.set_col_width(column, width);
    }
    for (column, text) in [(0, "cell-A"), (1, "cell-B"), (2, "cell-C"), (3, "cell-D")] {
        sheet.write(0, column, text);
    }
    sheet.set_print_headings();
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 0, 3))
            .with_repeat_cols(0, 0)
            .with_scale(100),
    );
    workbook
}

fn manual_break_workbook(right_to_left: bool) -> Workbook {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let workbook = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Breaks" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm.Print_Area" localSheetId="0">'Breaks'!$A$1:$D$1</definedName><definedName name="_xlnm.Print_Titles" localSheetId="0">'Breaks'!$A:$A</definedName></definedNames></workbook>"#;
    let relationships = r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;
    let right_to_left = if right_to_left { "1" } else { "0" };
    let worksheet = format!(
        r#"<worksheet><sheetViews><sheetView rightToLeft="{right_to_left}"/></sheetViews><cols><col min="1" max="1" width="5" customWidth="1"/><col min="2" max="3" width="20" customWidth="1"/><col min="4" max="4" width="50" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>cell-A</t></is></c><c r="B1" t="inlineStr"><is><t>cell-B</t></is></c><c r="C1" t="inlineStr"><is><t>cell-C</t></is></c><c r="D1" t="inlineStr"><is><t>cell-D</t></is></c></row></sheetData><printOptions headings="1"/><pageSetup paperSize="1" scale="100"/><colBreaks count="1" manualBreakCount="1"><brk id="3" min="0" max="1048575" man="1"/></colBreaks></worksheet>"#
    );
    for (name, body) in [
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", relationships),
        ("xl/worksheets/sheet1.xml", worksheet.as_str()),
    ] {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    Workbook::open(&writer.finish().unwrap().into_inner()).unwrap()
}

#[test]
fn rtl_print_keeps_logical_pages_but_paints_body_repeat_and_heading_rightward() {
    let options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    let ltr = build_print_document(&print_workbook(false), 0, &options).unwrap();
    let rtl = build_print_document(&print_workbook(true), 0, &options).unwrap();

    assert_eq!(rtl.report.pages, ltr.report.pages);
    assert_eq!(rtl.pages.len(), 2);
    assert_eq!(rtl.pages[0].map.horizontal_index, 0);
    assert_eq!(rtl.pages[0].map.body_range, RenderRange::new(0, 1, 0, 2));
    assert_eq!(rtl.pages[1].map.horizontal_index, 1);
    assert_eq!(rtl.pages[1].map.body_range, RenderRange::new(0, 3, 0, 3));
    assert_eq!(rtl.pages[0].map.repeat_cols, Some((0, 0)));

    let first = &rtl.pages[0].scene;
    let cell_c = text_node(first, "cell-C").bounds.x;
    let cell_b = text_node(first, "cell-B").bounds.x;
    let cell_a = text_node(first, "cell-A").bounds.x;
    let row_heading = text_node(first, "1").bounds.x;
    assert!(cell_c < cell_b && cell_b < cell_a && cell_a < row_heading);

    let heading_c = text_node(first, "C").bounds.x;
    let heading_b = text_node(first, "B").bounds.x;
    let heading_a = text_node(first, "A").bounds.x;
    assert!(heading_c < heading_b && heading_b < heading_a);

    let second = &rtl.pages[1].scene;
    assert!(text_node(second, "cell-D").bounds.x < text_node(second, "cell-A").bounds.x);
    assert!(text_node(second, "D").bounds.x < text_node(second, "A").bounds.x);
}

#[test]
fn rtl_manual_column_breaks_keep_their_logical_page_map_positions() {
    let options = PrintOptions {
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    let ltr = build_print_document(&manual_break_workbook(false), 0, &options).unwrap();
    let rtl = build_print_document(&manual_break_workbook(true), 0, &options).unwrap();

    assert_eq!(rtl.report.manual_col_breaks, [3]);
    assert_eq!(rtl.report.pages, ltr.report.pages);
    assert_eq!(rtl.pages.len(), 2);
    assert_eq!(rtl.pages[0].map.body_range, RenderRange::new(0, 1, 0, 2));
    assert!(!rtl.pages[0].map.manual_col_break_before);
    assert_eq!(rtl.pages[1].map.body_range, RenderRange::new(0, 3, 0, 3));
    assert!(rtl.pages[1].map.manual_col_break_before);
    assert!(
        text_node(&rtl.pages[0].scene, "cell-C").bounds.x
            < text_node(&rtl.pages[0].scene, "cell-B").bounds.x
    );
}
