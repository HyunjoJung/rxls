use std::io::Write as _;

use rxls::{Chart, ChartKind, PageSetup, Series, Workbook};
use rxls_render::{
    build_print_document, build_print_page, prepare_print_document, render_print_page_png, Fixed,
    PrintOptions, Rect, RenderError, Rgb, Scene, SceneNode, FIXED_UNITS_PER_PIXEL,
};
use zip::write::SimpleFileOptions;

const COLUMN_PIXELS: i64 = 597;
const ROW_PIXELS: i64 = 600;

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

fn striped_rgba_png() -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, 4, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255, 128, 0, 0, 255,
                0, 128, 0, 255, 0, 0, 128, 255, 128, 128, 0, 255,
            ])
            .unwrap();
    }
    output
}

fn tall_rgba_png() -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, 64, 300);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[17, 89, 143, 255].repeat(64 * 300))
            .unwrap();
    }
    output
}

fn five_stripe_rgba_png() -> Vec<u8> {
    let colors = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
        [255, 0, 255, 255],
    ];
    let mut pixels = Vec::with_capacity(10 * 2 * 4);
    for _ in 0..2 {
        for color in colors {
            pixels.extend_from_slice(&color);
            pixels.extend_from_slice(&color);
        }
    }
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, 10, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&pixels).unwrap();
    }
    output
}

fn partial_area_image_workbook(
    right_to_left: bool,
    print_area: (u32, u16, u32, u16),
    manual_column_tiles: bool,
) -> Workbook {
    let print_area_a1 = match print_area {
        (0, 0, 0, 1) => "$A$1:$B$1",
        (0, 2, 0, 4) => "$C$1:$E$1",
        (0, 2, 0, 5) => "$C$1:$F$1",
        _ => panic!("unsupported partial-area fixture range"),
    };
    let right_to_left = if right_to_left { "1" } else { "0" };
    let column_breaks = if manual_column_tiles {
        r#"<colBreaks count="3" manualBreakCount="3"><brk id="3" min="0" max="1048575" man="1"/><brk id="4" min="0" max="1048575" man="1"/><brk id="5" min="0" max="1048575" man="1"/></colBreaks>"#
    } else {
        ""
    };
    let worksheet = format!(
        r#"<worksheet><sheetViews><sheetView rightToLeft="{right_to_left}"/></sheetViews><sheetData/><pageSetup paperSize="1" scale="100"/>{column_breaks}<drawing r:id="rIdDraw"/></worksheet>"#
    );
    let workbook_xml = format!(
        r#"<workbook><sheets><sheet name="Partial" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm.Print_Area" localSheetId="0">'Partial'!{print_area_a1}</definedName></definedNames></workbook>"#
    );
    // The authored destination is A1:E1, inset 20 px from the A edge. A
    // print area may retain only its left side, right side, or middle.
    let drawing = r#"<wsDr><twoCellAnchor><from><col>0</col><colOff>190500</colOff><row>0</row><rowOff>0</rowOff></from><to><col>5</col><colOff>0</colOff><row>1</row><rowOff>0</rowOff></to><pic><nvPicPr><cNvPr id="1" name="partial print-area stripes"/></nvPicPr><blipFill><blip r:embed="rIdImage"/></blipFill></pic></twoCellAnchor></wsDr>"#;
    let image = five_stripe_rgba_png();
    let mut workbook = Workbook::open(&zip_parts(&[
        ("xl/workbook.xml", workbook_xml.as_bytes()),
        (
            "xl/_rels/workbook.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            br#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
        ),
        ("xl/drawings/drawing1.xml", drawing.as_bytes()),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            br#"<Relationships><Relationship Id="rIdImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/></Relationships>"#,
        ),
        ("xl/media/image1.png", &image),
    ]))
    .expect("minimal partial-area cell-anchored image fixture");
    let sheet = &mut workbook.sheets[0];
    for column in 0..6 {
        sheet.set_col_width(column, 14.0);
    }
    sheet.set_row_height(0, 75.0);
    workbook
}

fn decoded_png_rgba(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().unwrap();
    let mut bytes = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut bytes).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgba);
    bytes.truncate(info.buffer_size());
    (info.width, info.height, bytes)
}

fn rgba_at(width: u32, pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let offset = (u64::from(y) * u64::from(width) + u64::from(x)) as usize * 4;
    pixels[offset..offset + 4].try_into().unwrap()
}

fn offset_cropped_image_workbook(right_to_left: bool) -> Workbook {
    let right_to_left = if right_to_left { "1" } else { "0" };
    let worksheet = format!(
        r#"<worksheet><sheetViews><sheetView rightToLeft="{right_to_left}"/></sheetViews><sheetData/><drawing r:id="rIdDraw"/></worksheet>"#
    );
    let drawing = r#"<wsDr><twoCellAnchor><from><col>0</col><colOff>914400</colOff><row>0</row><rowOff>952500</rowOff></from><to><col>2</col><colOff>457200</colOff><row>2</row><rowOff>476250</rowOff></to><pic><nvPicPr><cNvPr id="1" name="spanning crop"/></nvPicPr><blipFill><blip r:embed="rIdImage"/><srcRect l="25000"/></blipFill></pic></twoCellAnchor></wsDr>"#;
    let image = striped_rgba_png();
    let mut workbook = Workbook::open(&zip_parts(&[
        (
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="Tiles" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            br#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
        ),
        ("xl/drawings/drawing1.xml", drawing.as_bytes()),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            br#"<Relationships><Relationship Id="rIdImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/></Relationships>"#,
        ),
        ("xl/media/image1.png", &image),
    ]))
    .expect("minimal cell-anchored image fixture");
    configure_three_by_three_print_grid(&mut workbook);
    workbook
}

fn chart_workbook(right_to_left: bool) -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Tiles");
    sheet.set_right_to_left(right_to_left);
    for row in 0..3 {
        sheet.write_number(row, 2, f64::from(row + 1));
    }
    sheet.add_chart(
        Chart::new(ChartKind::Line, (0, 0), (2, 2))
            .with_title("one-global-chart")
            .add_series(Series::new("Tiles!$C$1:$C$3")),
    );
    configure_three_by_three_print_grid(&mut workbook);
    workbook
}

fn partial_area_chart_workbook() -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("PartialChart");
    for column in 0..5 {
        sheet.set_col_width(column, 20.0);
    }
    for row in 0..5 {
        sheet.set_row_height(row, 30.0);
    }
    for row in 10..13 {
        sheet.write_number(row, 9, f64::from(row + 1));
    }
    sheet.add_chart(
        Chart::new(ChartKind::Line, (0, 0), (4, 4))
            .with_title("partial-data-chart")
            .add_series(Series::new("PartialChart!$J$11:$J$13")),
    );
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((1, 1, 3, 3))
            .with_paper_size(1)
            .with_scale(100),
    );
    workbook
}

fn rotated_image_workbook(right_to_left: bool) -> Workbook {
    let right_to_left = if right_to_left { "1" } else { "0" };
    let workbook = br#"<workbook><sheets><sheet name="Rotated" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm.Print_Area" localSheetId="0">'Rotated'!$A$1:$C$2</definedName></definedNames></workbook>"#;
    let worksheet = format!(
        r#"<worksheet><sheetViews><sheetView rightToLeft="{right_to_left}"/></sheetViews><sheetData/><pageSetup paperSize="1" scale="100"/><rowBreaks count="1" manualBreakCount="1"><brk id="1" min="0" max="16383" man="1"/></rowBreaks><colBreaks count="2" manualBreakCount="2"><brk id="1" min="0" max="1048575" man="1"/><brk id="2" min="0" max="1048575" man="1"/></colBreaks><drawing r:id="rIdDraw"/></worksheet>"#
    );
    // A1:B2 is the retained two-cell anchor, while editAs=oneCell gives the
    // object an exact 64x300 px destination. At x=50 px, its 45-degree paint
    // bounds reach beyond B into the C print tile.
    let drawing = r#"<wsDr><twoCellAnchor editAs="oneCell"><from><col>0</col><colOff>476250</colOff><row>0</row><rowOff>0</rowOff></from><to><col>1</col><colOff>0</colOff><row>1</row><rowOff>0</rowOff></to><pic><nvPicPr><cNvPr id="1" name="rotated continuation"/></nvPicPr><blipFill><blip r:embed="rIdImage"/></blipFill><spPr><xfrm rot="2700000"><ext cx="609600" cy="2857500"/></xfrm></spPr></pic></twoCellAnchor></wsDr>"#;
    let image = tall_rgba_png();
    let mut workbook = Workbook::open(&zip_parts(&[
        ("xl/workbook.xml", workbook),
        (
            "xl/_rels/workbook.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            br#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
        ),
        ("xl/drawings/drawing1.xml", drawing.as_bytes()),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            br#"<Relationships><Relationship Id="rIdImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/></Relationships>"#,
        ),
        ("xl/media/image1.png", &image),
    ]))
    .expect("minimal rotated cell-anchored image fixture");
    let sheet = &mut workbook.sheets[0];
    for column in 0..3 {
        sheet.set_col_width(column, 14.0);
    }
    for row in 0..2 {
        sheet.set_row_height(row, 112.5);
    }
    workbook
}

fn configure_three_by_three_print_grid(workbook: &mut Workbook) {
    let sheet = &mut workbook.sheets[0];
    for column in 0..3 {
        sheet.set_col_width(column, 85.0);
    }
    for row in 0..3 {
        sheet.set_row_height(row, 450.0);
    }
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 2, 2))
            .with_paper_size(1)
            .with_scale(100),
    );
}

fn first_image(scene: &Scene) -> (Rect, u32, u32, Vec<u8>) {
    fn find(nodes: &[SceneNode]) -> Option<(Rect, u32, u32, Vec<u8>)> {
        nodes.iter().find_map(|node| match node {
            SceneNode::ClipGroup(group) => find(&group.nodes),
            SceneNode::Image(image) => Some((
                image.rect,
                image.pixel_width,
                image.pixel_height,
                image.rgba.to_vec(),
            )),
            _ => None,
        })
    }
    find(&scene.nodes).expect("decoded image continuation")
}

struct RotatedImageEvidence {
    rect: Rect,
    rotation_mdeg: i32,
    pixel_width: u32,
    pixel_height: u32,
    rgba: Vec<u8>,
    clips: Vec<Rect>,
}

fn rotated_image_evidence(scene: &Scene) -> RotatedImageEvidence {
    fn find(nodes: &[SceneNode], clips: &mut Vec<Rect>) -> Option<RotatedImageEvidence> {
        for node in nodes {
            match node {
                SceneNode::ClipGroup(group) => {
                    clips.push(group.clip);
                    if let Some(evidence) = find(&group.nodes, clips) {
                        return Some(evidence);
                    }
                    clips.pop();
                }
                SceneNode::Image(image) => {
                    return Some(RotatedImageEvidence {
                        rect: image.rect,
                        rotation_mdeg: image.rotation_mdeg,
                        pixel_width: image.pixel_width,
                        pixel_height: image.pixel_height,
                        rgba: image.rgba.to_vec(),
                        clips: clips.clone(),
                    });
                }
                _ => {}
            }
        }
        None
    }
    find(&scene.nodes, &mut Vec::new()).expect("rotated image continuation")
}

fn chart_nodes(scene: &Scene) -> &[SceneNode] {
    fn find(nodes: &[SceneNode]) -> Option<&[SceneNode]> {
        for node in nodes {
            let SceneNode::ClipGroup(group) = node else {
                continue;
            };
            if let Some(nodes) = find(&group.nodes) {
                return Some(nodes);
            }
            if group.nodes.iter().any(
                |node| matches!(node, SceneNode::Text(text) if text.text == "one-global-chart"),
            ) {
                return Some(&group.nodes);
            }
        }
        None
    }
    find(&scene.nodes).expect("clipped chart object")
}

fn largest_white_rect(nodes: &[SceneNode]) -> Rect {
    fn collect(nodes: &[SceneNode], output: &mut Vec<Rect>) {
        for node in nodes {
            match node {
                SceneNode::ClipGroup(group) => collect(&group.nodes, output),
                SceneNode::Rect(rect) if rect.fill == Some(Rgb::WHITE) => {
                    output.push(rect.rect);
                }
                _ => {}
            }
        }
    }
    let mut rects = Vec::new();
    collect(nodes, &mut rects);
    rects
        .into_iter()
        .max_by_key(|rect| i128::from(rect.width.raw()) * i128::from(rect.height.raw()))
        .expect("chart frame")
}

fn chart_title(nodes: &[SceneNode]) -> Rect {
    fn find(nodes: &[SceneNode]) -> Option<Rect> {
        nodes.iter().find_map(|node| match node {
            SceneNode::ClipGroup(group) => find(&group.nodes),
            SceneNode::Text(text) if text.text == "one-global-chart" => Some(text.bounds),
            _ => None,
        })
    }
    find(nodes).expect("chart title")
}

fn chart_line_signature(nodes: &[SceneNode], origin: Rect) -> Vec<(i64, i64, i64, i64)> {
    fn collect(nodes: &[SceneNode], output: &mut Vec<(i64, i64, i64, i64)>) {
        for node in nodes {
            match node {
                SceneNode::ClipGroup(group) => collect(&group.nodes, output),
                SceneNode::Line(line) => {
                    output.push((line.x1.raw(), line.y1.raw(), line.x2.raw(), line.y2.raw()))
                }
                _ => {}
            }
        }
    }
    let mut lines = Vec::new();
    collect(nodes, &mut lines);
    lines
        .into_iter()
        .map(|(x1, y1, x2, y2)| {
            (
                x1 - origin.x.raw(),
                y1 - origin.y.raw(),
                x2 - origin.x.raw(),
                y2 - origin.y.raw(),
            )
        })
        .collect()
}

#[test]
fn offset_and_cropped_image_keeps_one_source_mapping_across_all_print_tiles() {
    for right_to_left in [false, true] {
        let document = build_print_document(
            &offset_cropped_image_workbook(right_to_left),
            0,
            &PrintOptions {
                omit_sparse_pages: false,
                ..PrintOptions::default()
            },
        )
        .unwrap();
        assert_eq!(document.pages.len(), 9);

        let expected_width = Fixed::from_pixels(COLUMN_PIXELS * 2 + 48 - 96);
        let expected_height = Fixed::from_pixels(ROW_PIXELS * 2 + 50 - 100);
        let mut decoded_identity = None;
        for page in &document.pages {
            let (rect, pixel_width, pixel_height, rgba) = first_image(&page.scene);
            assert_eq!(rect.width, expected_width, "{:?}", page.map);
            assert_eq!(rect.height, expected_height, "{:?}", page.map);
            let expected_x = if right_to_left {
                [-645, -48, 549][page.map.horizontal_index]
            } else {
                [96, -501, -1_098][page.map.horizontal_index]
            };
            let expected_y = [100, -500, -1_100][page.map.vertical_index];
            assert_eq!(
                rect.x.raw() - document.report.content_rect.x.raw(),
                Fixed::from_pixels(expected_x).raw(),
                "{:?}",
                page.map
            );
            assert_eq!(
                rect.y.raw() - document.report.content_rect.y.raw(),
                Fixed::from_pixels(expected_y).raw(),
                "{:?}",
                page.map
            );
            let identity = (pixel_width, pixel_height, rgba);
            assert_eq!(
                decoded_identity.get_or_insert_with(|| identity.clone()),
                &identity,
                "every page must clip the same decoded/cropped source"
            );
        }
        let (pixel_width, pixel_height, _) = decoded_identity.expect("image identity");
        assert_eq!((pixel_width, pixel_height), (3, 2));
    }
}

#[test]
fn chart_title_and_data_paths_use_one_global_geometry_across_row_and_column_breaks() {
    for right_to_left in [false, true] {
        let document = build_print_document(
            &chart_workbook(right_to_left),
            0,
            &PrintOptions {
                omit_sparse_pages: false,
                ..PrintOptions::default()
            },
        )
        .unwrap();
        let mut reference_signature = None;
        for page in document
            .pages
            .iter()
            .filter(|page| page.map.horizontal_index < 2 && page.map.vertical_index < 2)
        {
            let nodes = chart_nodes(&page.scene);
            let frame = largest_white_rect(nodes);
            assert_eq!(frame.width, Fixed::from_pixels(1_194), "{:?}", page.map);
            assert_eq!(frame.height, Fixed::from_pixels(1_200), "{:?}", page.map);
            let title = chart_title(nodes);
            assert_eq!(title.x, frame.x);
            assert_eq!(title.y, frame.y);
            assert_eq!(title.width, frame.width);
            let signature = chart_line_signature(nodes, frame);
            assert!(!signature.is_empty());
            assert_eq!(
                reference_signature.get_or_insert_with(|| signature.clone()),
                &signature,
                "chart data geometry changed in tile {:?}",
                page.map
            );

            let expected_x = if right_to_left {
                [-597, 0][page.map.horizontal_index]
            } else {
                [0, -597][page.map.horizontal_index]
            };
            let expected_y = [0, -600][page.map.vertical_index];
            assert_eq!(
                frame.x.raw() - document.report.content_rect.x.raw(),
                Fixed::from_pixels(expected_x).raw(),
                "{:?}",
                page.map
            );
            assert_eq!(
                frame.y.raw() - document.report.content_rect.y.raw(),
                Fixed::from_pixels(expected_y).raw(),
                "{:?}",
                page.map
            );
        }
    }
}

#[test]
fn rotated_paint_beyond_to_cell_retains_full_ltr_and_rtl_continuations() {
    for right_to_left in [false, true] {
        let document = build_print_document(
            &rotated_image_workbook(right_to_left),
            0,
            &PrintOptions::default(),
        )
        .unwrap();
        assert_eq!(document.report.logical_pages, 6);
        assert_eq!(document.report.sparse_pages_omitted, 0);
        assert_eq!(document.pages.len(), 6);

        let expected_global_pivot_x = if right_to_left { 218 } else { 82 };
        let mut payload_identity = None;
        for page in &document.pages {
            let evidence = rotated_image_evidence(&page.scene);
            assert_eq!(evidence.rotation_mdeg, 45_000, "{:?}", page.map);
            assert_eq!(
                evidence.rect.width,
                Fixed::from_pixels(64),
                "{:?}",
                page.map
            );
            assert_eq!(
                evidence.rect.height,
                Fixed::from_pixels(300),
                "{:?}",
                page.map
            );
            assert!(
                evidence.clips.iter().any(|clip| {
                    clip.width == Fixed::from_pixels(100) && clip.height == Fixed::from_pixels(150)
                }),
                "missing tile clip for {:?}: {:?}",
                page.map,
                evidence.clips
            );

            let tile_x = if right_to_left {
                [200, 100, 0][page.map.horizontal_index]
            } else {
                [0, 100, 200][page.map.horizontal_index]
            };
            let tile_y = [0, 150][page.map.vertical_index];
            let pivot_x = evidence.rect.x.raw() - document.report.content_rect.x.raw()
                + Fixed::from_pixels(tile_x + 32).raw();
            let pivot_y = evidence.rect.y.raw() - document.report.content_rect.y.raw()
                + Fixed::from_pixels(tile_y + 150).raw();
            assert_eq!(
                pivot_x,
                Fixed::from_pixels(expected_global_pivot_x).raw(),
                "{:?}",
                page.map
            );
            assert_eq!(pivot_y, Fixed::from_pixels(150).raw(), "{:?}", page.map);

            let identity = (evidence.pixel_width, evidence.pixel_height, evidence.rgba);
            assert_eq!(
                payload_identity.get_or_insert_with(|| identity.clone()),
                &identity,
                "tile {:?} changed the full decoded payload",
                page.map
            );
        }
        let (pixel_width, pixel_height, rgba) = payload_identity.expect("payload identity");
        assert_eq!((pixel_width, pixel_height), (64, 300));
        assert_eq!(rgba.len(), 64 * 300 * 4);
    }
}

#[test]
fn authored_print_area_clips_one_full_offset_image_without_rescaling_source_stripes() {
    let cases = [
        ((0, 0, 0, 1), [68_u32, 164], [[255, 0, 0], [0, 255, 0]]),
        ((0, 2, 0, 4), [84_u32, 276], [[0, 0, 255], [255, 0, 255]]),
    ];
    for right_to_left in [false, true] {
        for (print_area, sample_x, expected_ltr) in cases {
            let document = build_print_document(
                &partial_area_image_workbook(right_to_left, print_area, false),
                0,
                &PrintOptions::default(),
            )
            .unwrap();
            assert_eq!(document.report.logical_pages, 1);
            assert_eq!(document.report.sparse_pages_omitted, 0);
            assert_eq!(document.pages.len(), 1);

            let (rect, pixel_width, pixel_height, rgba) = first_image(&document.pages[0].scene);
            assert_eq!(rect.width, Fixed::from_pixels(480));
            assert_eq!(rect.height, Fixed::from_pixels(100));
            assert_eq!((pixel_width, pixel_height), (10, 2));
            assert_eq!(rgba, five_stripe_rgba_png_decoded());

            let expected_x = match (right_to_left, print_area.1) {
                (false, 0) => 20,
                (false, 2) => -180,
                (true, 0) => -300,
                (true, 2) => 0,
                _ => unreachable!(),
            };
            assert_eq!(
                rect.x.raw() - document.report.content_rect.x.raw(),
                Fixed::from_pixels(expected_x).raw()
            );
            assert_eq!(
                rect.y.raw() - document.report.content_rect.y.raw(),
                Fixed::ZERO.raw()
            );

            // LibreOffice's LTR oracle shows only the left source stripes for
            // A:B and only the right source stripes for C:E. Raster samples
            // catch any regression that clips and then re-scales the payload.
            if !right_to_left {
                let encoded = render_print_page_png(&document.pages[0], 96, &document).unwrap();
                let (width, _, pixels) = decoded_png_rgba(&encoded);
                let content_x = document.report.content_rect.x.raw() / FIXED_UNITS_PER_PIXEL;
                let content_y = document.report.content_rect.y.raw() / FIXED_UNITS_PER_PIXEL;
                for (x, expected) in sample_x.into_iter().zip(expected_ltr) {
                    let actual = rgba_at(
                        width,
                        &pixels,
                        u32::try_from(content_x).unwrap() + x,
                        u32::try_from(content_y + 50).unwrap(),
                    );
                    assert!(
                        actual[..3]
                            .iter()
                            .zip(expected)
                            .all(|(&actual, expected)| actual.abs_diff(expected) <= 3),
                        "sample {x} was {actual:?}, expected {expected:?}"
                    );
                    assert_eq!(actual[3], 255);
                }
            }
        }
    }
}

#[test]
fn sparse_partial_area_tiles_stop_at_the_authored_image_edge_in_ltr_and_rtl() {
    for right_to_left in [false, true] {
        let document = build_print_document(
            &partial_area_image_workbook(right_to_left, (0, 2, 0, 5), true),
            0,
            &PrintOptions::default(),
        )
        .unwrap();
        assert_eq!(document.report.logical_pages, 4);
        assert_eq!(document.report.sparse_pages_omitted, 1);
        assert_eq!(document.pages.len(), 3);
        assert_eq!(
            document
                .pages
                .iter()
                .map(|page| page.map.body_range.first_col)
                .collect::<Vec<_>>(),
            [2, 3, 4]
        );
        for page in &document.pages {
            let (rect, pixel_width, pixel_height, rgba) = first_image(&page.scene);
            assert_eq!(rect.width, Fixed::from_pixels(480), "{:?}", page.map);
            assert_eq!(rect.height, Fixed::from_pixels(100), "{:?}", page.map);
            assert_eq!((pixel_width, pixel_height), (10, 2));
            assert_eq!(rgba, five_stripe_rgba_png_decoded());
        }
    }
}

#[test]
fn partial_area_chart_sources_outside_prepared_geometry_invalidate_streamed_pages() {
    let mut workbook = partial_area_chart_workbook();
    let prepared = prepare_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
    assert_eq!(prepared.report.pages.len(), 1);
    build_print_page(&workbook, &prepared, 0).unwrap();

    workbook.sheets[0].write_number(10, 9, 99.0);
    assert!(matches!(
        build_print_page(&workbook, &prepared, 0),
        Err(RenderError::Backend {
            reason: "prepared_print_source_changed"
        })
    ));
}

fn five_stripe_rgba_png_decoded() -> Vec<u8> {
    let (_, _, rgba) = decoded_png_rgba(&five_stripe_rgba_png());
    rgba
}
