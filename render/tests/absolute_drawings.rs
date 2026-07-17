use std::io::Write;

use rxls::{DrawingAnchorBehavior, DrawingObjectKind, Image, ImageFmt, Workbook};
use rxls_render::{
    build_scene, Fixed, LimitKind, Rect, RenderError, RenderLimits, RenderOptions, RenderRange,
    RenderSelection, Rgb, Scene, SceneNode, WarningCode,
};
use zip::write::SimpleFileOptions;

fn zip_parts(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for &(path, body) in parts {
        writer
            .start_file(path, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(body).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn solid_rgba_png() -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[17, 89, 143, 255]).unwrap();
    }
    output
}

fn imported_ooxml_absolute_chart(right_to_left: bool) -> Workbook {
    let sheet_view = if right_to_left {
        r#"<sheetViews><sheetView rightToLeft="1"/></sheetViews>"#
    } else {
        r#"<sheetViews><sheetView/></sheetViews>"#
    };
    let worksheet = format!(
        r#"<worksheet>{sheet_view}<sheetData><row r="1" hidden="1"/></sheetData><drawing r:id="rIdDraw"/></worksheet>"#
    );
    let drawing = r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:absoluteAnchor><xdr:pos x="95250" y="47625"/><xdr:ext cx="1524000" cy="952500"/><xdr:graphicFrame><a:graphic><a:graphicData><c:chart r:id="rIdChart"/></a:graphicData></a:graphic></xdr:graphicFrame></xdr:absoluteAnchor></xdr:wsDr>"#;
    let parts = [
        (
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="Absolute" r:id="rId1"/></sheets></workbook>"#
                .as_slice(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#
                .as_slice(),
        ),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            br#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#
                .as_slice(),
        ),
        ("xl/drawings/drawing1.xml", drawing.as_bytes()),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            br#"<Relationships><Relationship Id="rIdChart" Target="../charts/chart1.xml"/></Relationships>"#
                .as_slice(),
        ),
        (
            "xl/charts/chart1.xml",
            br#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea><c:lineChart/></c:plotArea></c:chart></c:chartSpace>"#
                .as_slice(),
        ),
    ];
    Workbook::open(&zip_parts(&parts)).expect("minimal OOXML absolute chart")
}

fn imported_ods_page_image(frame_geometry: &str, hidden_column: bool) -> Workbook {
    let column = if hidden_column {
        r#"<table:table-column table:visibility="collapse"/>"#
    } else {
        "<table:table-column/>"
    };
    let content = format!(
        r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="Page image">{column}<table:shapes><draw:frame draw:name="Page logo" text:anchor-type="page" {frame_geometry}><draw:image xlink:href="Pictures/page.png"/></draw:frame></table:shapes><table:table-row><table:table-cell/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
    );
    let png = solid_rgba_png();
    Workbook::open(&zip_parts(&[
        (
            "mimetype",
            b"application/vnd.oasis.opendocument.spreadsheet",
        ),
        ("content.xml", content.as_bytes()),
        ("Pictures/page.png", &png),
    ]))
    .expect("minimal ODS page image")
}

fn imported_ods_absolute_image() -> Workbook {
    imported_ods_page_image(
        r#"svg:x="0.25in" svg:y="0.125in" svg:width="1in" svg:height="0.5in""#,
        true,
    )
}

fn imported_ods_rotated_absolute_image() -> Workbook {
    imported_ods_page_image(
        r#"svg:x="0in" svg:y="0.46875in" svg:width="1.0416666666666667in" svg:height="0.10416666666666667in" draw:transform="rotate(1.5707963267948966)""#,
        false,
    )
}

fn used_options() -> RenderOptions {
    RenderOptions {
        selection: RenderSelection::Used,
        gridlines: false,
        ..RenderOptions::default()
    }
}

fn explicit_a1_options() -> RenderOptions {
    RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        gridlines: false,
        ..RenderOptions::default()
    }
}

fn largest_white_rect(scene: &Scene) -> Rect {
    fn collect(nodes: &[SceneNode], rects: &mut Vec<Rect>) {
        for node in nodes {
            match node {
                SceneNode::ClipGroup(group) => collect(&group.nodes, rects),
                SceneNode::Rect(node) if node.fill == Some(Rgb::WHITE) => rects.push(node.rect),
                _ => {}
            }
        }
    }
    let mut rects = Vec::new();
    collect(&scene.nodes, &mut rects);
    rects
        .iter()
        .copied()
        .max_by_key(|rect| i128::from(rect.width.raw()) * i128::from(rect.height.raw()))
        .expect("chart frame")
}

fn first_image(nodes: &[SceneNode]) -> Option<&rxls_render::ImageNode> {
    nodes.iter().find_map(|node| match node {
        SceneNode::ClipGroup(group) => first_image(&group.nodes),
        SceneNode::Image(image) => Some(image),
        _ => None,
    })
}

fn first_clip_group(nodes: &[SceneNode]) -> Option<&rxls_render::ClipGroupNode> {
    nodes.iter().find_map(|node| match node {
        SceneNode::ClipGroup(group) => Some(group),
        _ => None,
    })
}

fn first_text(nodes: &[SceneNode]) -> Option<&rxls_render::TextNode> {
    nodes.iter().find_map(|node| match node {
        SceneNode::ClipGroup(group) => first_text(&group.nodes),
        SceneNode::Text(text) => Some(text),
        _ => None,
    })
}

#[test]
fn imported_ooxml_absolute_chart_survives_hidden_rows_and_reflects_in_rtl() {
    let options = used_options();
    let ltr_workbook = imported_ooxml_absolute_chart(false);
    let metadata = ltr_workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .unwrap_or_else(|| {
            panic!(
                "chart metadata; charts={:?} metadata={:?} losses={:?}",
                ltr_workbook.sheets[0].charts(),
                ltr_workbook.sheets[0].drawing_metadata(),
                ltr_workbook.sheets[0].style_losses(),
            )
        });
    assert_eq!(metadata.behavior, DrawingAnchorBehavior::Absolute);
    assert_eq!(metadata.from_cell, None);
    assert_eq!(metadata.from_offset_emu, Some((95_250, 47_625)));
    assert_eq!(metadata.absolute_size_emu, Some((1_524_000, 952_500)));

    let ltr = build_scene(&ltr_workbook, 0, &options).unwrap();
    assert_eq!(ltr.report.visible_rows, 0);
    assert_eq!(ltr.report.visible_columns, 1);
    assert_eq!(ltr.report.hidden_rows_skipped, 1);
    assert_eq!(ltr.scene.width, Fixed::from_pixels(170));
    assert_eq!(ltr.scene.height, Fixed::from_pixels(105));
    let ltr_rect = largest_white_rect(&ltr.scene);
    assert_eq!(
        ltr_rect,
        Rect {
            x: Fixed::from_pixels(10),
            y: Fixed::from_pixels(5),
            width: Fixed::from_pixels(160),
            height: Fixed::from_pixels(100),
        }
    );

    let rtl = build_scene(&imported_ooxml_absolute_chart(true), 0, &options).unwrap();
    assert_eq!(rtl.scene.width, ltr.scene.width);
    assert_eq!(rtl.scene.height, ltr.scene.height);
    let rtl_rect = largest_white_rect(&rtl.scene);
    assert_eq!(rtl_rect.y, ltr_rect.y);
    assert_eq!(rtl_rect.width, ltr_rect.width);
    assert_eq!(rtl_rect.height, ltr_rect.height);
    assert_eq!(
        rtl_rect.x.raw(),
        rtl.scene.width.raw() - ltr_rect.x.raw() - ltr_rect.width.raw()
    );
}

#[test]
fn imported_ods_absolute_image_survives_hidden_columns() {
    let workbook = imported_ods_absolute_image();
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Image)
        .expect("image metadata");
    assert_eq!(metadata.behavior, DrawingAnchorBehavior::Absolute);
    assert_eq!(metadata.from_cell, None);

    let build = build_scene(&workbook, 0, &used_options()).unwrap();
    assert_eq!(build.report.visible_rows, 1);
    assert_eq!(build.report.visible_columns, 0);
    assert_eq!(build.report.hidden_columns_skipped, 1);
    assert_eq!(build.scene.width, Fixed::from_pixels(120));
    assert_eq!(build.scene.height, Fixed::from_pixels(60));
    let image = first_image(&build.scene.nodes).expect("decoded page image");
    assert_eq!(
        image.rect,
        Rect {
            x: Fixed::from_pixels(24),
            y: Fixed::from_pixels(12),
            width: Fixed::from_pixels(96),
            height: Fixed::from_pixels(48),
        }
    );
}

#[test]
fn rotated_absolute_images_expand_used_bounds_and_intersect_by_painted_geometry() {
    let workbook = imported_ods_rotated_absolute_image();
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Image)
        .expect("rotated image metadata");
    assert_eq!(metadata.rotation_mdeg, Some(90_000));

    let used = build_scene(&workbook, 0, &used_options()).unwrap();
    assert_eq!(used.scene.width, Fixed::from_pixels(64));
    assert_eq!(used.scene.height, Fixed::from_pixels(100));
    let image = first_image(&used.scene.nodes).expect("rotated image in Used scene");
    assert_eq!(image.rotation_mdeg, 90_000);
    assert_eq!(image.rect.y, Fixed::from_pixels(45));
    assert_eq!(image.rect.width, Fixed::from_pixels(100));
    assert_eq!(image.rect.height, Fixed::from_pixels(10));

    // The unrotated destination starts below A1's 20px viewport. Its rotated
    // paint reaches y=0, so an explicit A1 tile must still retain and clip it.
    let explicit = build_scene(&workbook, 0, &explicit_a1_options()).unwrap();
    assert_eq!(explicit.scene.height, Fixed::from_pixels(20));
    assert!(first_clip_group(&explicit.scene.nodes).is_some());
    assert!(first_image(&explicit.scene.nodes).is_some());
}

#[test]
fn imported_absolute_bounds_still_obey_the_dimension_limit() {
    let workbook = imported_ooxml_absolute_chart(false);
    let options = RenderOptions {
        limits: RenderLimits {
            max_dimension_raw: Fixed::from_pixels(170).raw() as u64 - 1,
            ..RenderLimits::default()
        },
        ..used_options()
    };
    assert_eq!(
        build_scene(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Dimension,
            limit: 174_079,
            actual: 174_080,
        })
    );
}

#[test]
fn cell_anchored_images_keep_hidden_axis_semantics() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("cell anchored");
    sheet.add_image(Image::new(solid_rgba_png(), ImageFmt::Png, (0, 0)));
    sheet.hide_row(0);

    let build = build_scene(&workbook, 0, &explicit_a1_options()).unwrap();
    assert_eq!(build.scene.width, Fixed::from_pixels(64));
    assert_eq!(build.scene.height, Fixed::from_pixels(1));
    assert!(!build
        .scene
        .nodes
        .iter()
        .any(|node| matches!(node, SceneNode::Image(_))));
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::DrawingAnchorUnavailable && warning.occurrences == 1
    }));
}

#[test]
fn explicit_range_translates_full_absolute_geometry_and_silently_omits_nonintersection() {
    let workbook = imported_ods_page_image(
        r#"svg:x="0.25in" svg:y="0.125in" svg:width="1in" svg:height="0.5in""#,
        false,
    );
    let intersecting = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 1, 0, 1)),
        gridlines: false,
        ..RenderOptions::default()
    };
    let build = build_scene(&workbook, 0, &intersecting).unwrap();
    assert_eq!(build.scene.width, Fixed::from_pixels(64));
    assert_eq!(build.scene.height, Fixed::from_pixels(20));
    let group = first_clip_group(&build.scene.nodes).expect("absolute object clip");
    assert_eq!(
        group.clip,
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(64),
            height: Fixed::from_pixels(20),
        }
    );
    assert_eq!(
        first_image(&group.nodes).expect("full image geometry").rect,
        Rect {
            x: Fixed::from_pixels(-40),
            y: Fixed::from_pixels(12),
            width: Fixed::from_pixels(96),
            height: Fixed::from_pixels(48),
        }
    );
    assert_eq!(build.report.scene_nodes, 2);

    let outside = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 2, 0, 2)),
        ..intersecting
    };
    let build = build_scene(&workbook, 0, &outside).unwrap();
    assert!(first_clip_group(&build.scene.nodes).is_none());
    assert!(!build
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::DrawingAnchorUnavailable));
}

#[test]
fn malformed_absolute_geometry_is_unavailable_instead_of_falling_back_to_a1() {
    for geometry in [
        r#"svg:y="0.125in" svg:width="1in" svg:height="0.5in""#,
        r#"svg:x="0.25in" svg:y="0.125in" svg:width="1in""#,
        r#"svg:x="0.25in" svg:y="0.125in" svg:width="0in" svg:height="0.5in""#,
        r#"svg:x="0.25in" svg:y="0.125in" svg:width="1in" svg:height="0in""#,
    ] {
        let workbook = imported_ods_page_image(geometry, false);
        let build = build_scene(&workbook, 0, &used_options()).unwrap();
        assert!(first_image(&build.scene.nodes).is_none(), "{geometry}");
        assert!(first_clip_group(&build.scene.nodes).is_none(), "{geometry}");
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::DrawingAnchorUnavailable && warning.occurrences == 1
        }));
    }
}

#[test]
fn unavailable_or_fully_negative_absolute_objects_do_not_expand_used_range_to_a1() {
    let cases = [
        (
            r#"svg:y="0.125in" svg:width="1in" svg:height="0.5in""#,
            true,
        ),
        (
            r#"svg:x="0.25in" svg:y="0.125in" svg:width="0in" svg:height="0.5in""#,
            true,
        ),
        (
            r#"svg:x="-2in" svg:y="0.125in" svg:width="1in" svg:height="0.5in""#,
            false,
        ),
    ];
    for (geometry, unavailable) in cases {
        let mut workbook = imported_ods_page_image(geometry, false);
        workbook.sheets[0].write_string(100, 10, "far cell");
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Used,
                gridlines: false,
                limits: RenderLimits {
                    max_rows: 1,
                    max_columns: 1,
                    max_cells: 1,
                    ..RenderLimits::default()
                },
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert_eq!(build.report.range, RenderRange::new(100, 10, 100, 10));
        assert_eq!(first_image(&build.scene.nodes), None);
        assert_eq!(
            build
                .report
                .warnings
                .iter()
                .any(|warning| warning.code == WarningCode::DrawingAnchorUnavailable),
            unavailable,
            "{geometry}"
        );
    }
}

#[test]
fn empty_used_selection_still_warns_for_unavailable_absolute_geometry() {
    let workbook = imported_ods_page_image(
        r#"svg:y="0.125in" svg:width="1in" svg:height="0.5in""#,
        false,
    );
    let build = build_scene(&workbook, 0, &used_options()).unwrap();
    assert!(build.scene.nodes.is_empty());
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::DrawingAnchorUnavailable && warning.occurrences == 1
    }));
}

#[test]
fn rtl_reflects_cells_and_absolute_drawings_around_the_same_expanded_canvas() {
    let mut ltr_workbook = imported_ods_page_image(
        r#"svg:x="0.25in" svg:y="0.125in" svg:width="1in" svg:height="0.5in""#,
        false,
    );
    ltr_workbook.sheets[0].write_string(0, 0, "cell");
    let mut rtl_workbook = imported_ods_page_image(
        r#"svg:x="0.25in" svg:y="0.125in" svg:width="1in" svg:height="0.5in""#,
        false,
    );
    rtl_workbook.sheets[0].write_string(0, 0, "cell");
    rtl_workbook.sheets[0].set_right_to_left(true);

    let ltr = build_scene(&ltr_workbook, 0, &used_options()).unwrap();
    let rtl = build_scene(&rtl_workbook, 0, &used_options()).unwrap();
    assert_eq!(ltr.scene.width, Fixed::from_pixels(120));
    assert_eq!(rtl.scene.width, ltr.scene.width);
    let ltr_cell = first_text(&ltr.scene.nodes).expect("LTR cell").bounds;
    let rtl_cell = first_text(&rtl.scene.nodes).expect("RTL cell").bounds;
    assert_eq!(ltr_cell.x, Fixed::ZERO);
    assert_eq!(rtl_cell.x, Fixed::from_pixels(56));
    assert_eq!(rtl_cell.width, ltr_cell.width);

    let ltr_image = first_image(&ltr.scene.nodes).expect("LTR image").rect;
    let rtl_image = first_image(&rtl.scene.nodes).expect("RTL image").rect;
    assert_eq!(ltr_image.x, Fixed::from_pixels(24));
    assert_eq!(rtl_image.x, Fixed::ZERO);
    assert_eq!(rtl_image.width, ltr_image.width);
    assert_eq!(
        rtl_image.x.raw(),
        rtl.scene.width.raw() - ltr_image.x.raw() - ltr_image.width.raw()
    );
}
