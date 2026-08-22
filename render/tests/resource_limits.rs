use rxls::{Chart, ChartKind, Image, ImageFmt, Series, Sparkline, Workbook};
use rxls_render::{
    build_scene, LimitKind, RenderError, RenderOptions, RenderRange, RenderSelection, SceneNode,
    WarningCode,
};

fn solid_rgba_png(width: u32, height: u32) -> Vec<u8> {
    let pixels = usize::try_from(u64::from(width) * u64::from(height)).unwrap();
    let rgba = [23, 67, 149, 191].repeat(pixels);
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

fn drawing_options() -> RenderOptions {
    RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 12, 10)),
        gridlines: false,
        ..RenderOptions::default()
    }
}

fn assert_no_placeholder(build: &rxls_render::SceneBuild, warning: WarningCode) {
    assert!(
        build
            .report
            .warnings
            .iter()
            .all(|candidate| candidate.code != warning),
        "unexpected {warning:?} warning: {:?}",
        build.report.warnings
    );
}

#[test]
fn drawing_object_limit_accepts_exact_count_and_rejects_one_less() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("drawings");
    sheet.write_number(0, 0, 1);
    sheet.write_number(1, 0, 2);
    sheet.add_image(Image::new(solid_rgba_png(1, 1), ImageFmt::Png, (0, 1)));
    sheet.add_chart(
        Chart::new(ChartKind::Line, (0, 2), (10, 8)).add_series(Series::new("drawings!$A$1:$A$2")),
    );
    sheet.add_sparkline(Sparkline::new((0, 9), "drawings!$A$1:$A$2"));

    let mut exact = drawing_options();
    exact.limits.max_drawing_objects = 3;
    build_scene(&workbook, 0, &exact)
        .expect("three drawing objects fit an inclusive limit of three");

    let mut below = exact;
    below.limits.max_drawing_objects = 2;
    assert_eq!(
        build_scene(&workbook, 0, &below),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::DrawingObjects,
            limit: 2,
            actual: 3,
        })
    );
}

#[test]
fn image_dimension_limit_accepts_exact_extent_and_rejects_one_less() {
    let mut workbook = Workbook::new();
    workbook
        .add_sheet("image")
        .add_image(Image::new(solid_rgba_png(3, 2), ImageFmt::Png, (0, 0)));

    let mut exact = drawing_options();
    exact.limits.max_image_dimension = 3;
    let build = build_scene(&workbook, 0, &exact)
        .expect("a three-pixel maximum extent fits an inclusive limit of three");
    assert!(build
        .scene
        .nodes
        .iter()
        .any(|node| matches!(node, SceneNode::Image(image) if image.pixel_width == 3 && image.pixel_height == 2)));
    assert_no_placeholder(&build, WarningCode::ImagePlaceholder);

    let mut below = exact;
    below.limits.max_image_dimension = 2;
    assert_eq!(
        build_scene(&workbook, 0, &below),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::ImageDimension,
            limit: 2,
            actual: 3,
        })
    );
}

#[test]
fn image_pixel_limit_accepts_exact_area_and_rejects_one_less() {
    let mut workbook = Workbook::new();
    workbook
        .add_sheet("image")
        .add_image(Image::new(solid_rgba_png(3, 2), ImageFmt::Png, (0, 0)));

    let mut exact = drawing_options();
    exact.limits.max_image_pixels = 6;
    let build = build_scene(&workbook, 0, &exact)
        .expect("six decoded pixels fit an inclusive limit of six");
    assert!(build
        .scene
        .nodes
        .iter()
        .any(|node| matches!(node, SceneNode::Image(image) if image.rgba.len() == 24)));
    assert_no_placeholder(&build, WarningCode::ImagePlaceholder);

    let mut below = exact;
    below.limits.max_image_pixels = 5;
    assert_eq!(
        build_scene(&workbook, 0, &below),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::ImagePixels,
            limit: 5,
            actual: 6,
        })
    );
}

#[test]
fn decoded_media_byte_limit_accepts_exact_aggregate_and_rejects_one_less() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("images");
    sheet.add_image(Image::new(solid_rgba_png(1, 1), ImageFmt::Png, (0, 0)));
    sheet.add_image(Image::new(solid_rgba_png(1, 1), ImageFmt::Png, (0, 1)));

    let mut exact = drawing_options();
    exact.limits.max_decoded_media_bytes = 8;
    let build = build_scene(&workbook, 0, &exact)
        .expect("two four-byte RGBA images fit an inclusive aggregate limit of eight");
    assert_eq!(
        build
            .scene
            .nodes
            .iter()
            .filter(|node| matches!(node, SceneNode::Image(_)))
            .count(),
        2
    );
    assert_no_placeholder(&build, WarningCode::ImagePlaceholder);

    let mut below = exact;
    below.limits.max_decoded_media_bytes = 7;
    assert_eq!(
        build_scene(&workbook, 0, &below),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::DecodedMediaBytes,
            limit: 7,
            actual: 8,
        })
    );
}

#[test]
fn chart_series_limit_accepts_exact_count_and_rejects_one_less() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("series");
    sheet.write_number(0, 0, 1);
    sheet.write_number(1, 0, 2);
    sheet.write_number(0, 1, 3);
    sheet.write_number(1, 1, 4);
    sheet.add_chart(Chart::new(ChartKind::Line, (0, 2), (10, 8)).with_series([
        Series::new("series!$A$1:$A$2"),
        Series::new("series!$B$1:$B$2"),
    ]));

    let mut exact = drawing_options();
    exact.limits.max_chart_series = 2;
    let build =
        build_scene(&workbook, 0, &exact).expect("two chart series fit an inclusive limit of two");
    assert_no_placeholder(&build, WarningCode::ChartPlaceholder);

    let mut below = exact;
    below.limits.max_chart_series = 1;
    assert_eq!(
        build_scene(&workbook, 0, &below),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::ChartSeries,
            limit: 1,
            actual: 2,
        })
    );
}

#[test]
fn chart_point_limit_accepts_exact_sources_and_rejects_one_less() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("points");
    sheet.write_number(0, 0, 1);
    sheet.write_number(1, 0, 2);
    sheet.write(0, 1, "A");
    sheet.write(1, 1, "B");
    sheet.add_chart(
        Chart::new(ChartKind::Line, (0, 2), (10, 8))
            .add_series(Series::new("points!$A$1:$A$2").with_categories("points!$B$1:$B$2")),
    );
    sheet.add_sparkline(Sparkline::new((0, 9), "points!$A$1:$A$2"));

    let mut exact = drawing_options();
    exact.limits.max_chart_points = 6;
    let build = build_scene(&workbook, 0, &exact)
        .expect("two values, two labels, and two sparkline values fit a limit of six");
    assert_no_placeholder(&build, WarningCode::ChartPlaceholder);
    assert_no_placeholder(&build, WarningCode::SparklinePlaceholder);

    let mut below = exact;
    below.limits.max_chart_points = 5;
    assert_eq!(
        build_scene(&workbook, 0, &below),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::ChartPoints,
            limit: 5,
            actual: 6,
        })
    );
}
