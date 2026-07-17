use std::{fmt::Write as _, io::Write};

use rxls::{PageSetup, Workbook};
use rxls_render::{
    build_print_document, build_print_page, build_scene, prepare_print_document, Fixed, LimitKind,
    PrintLimits, PrintOptions, Rect, RenderError, RenderLimits, RenderOptions, RenderRange,
    RenderSelection, SceneNode,
};
use zip::write::SimpleFileOptions;

const COLUMN_PIXELS: i64 = 597;
const ROW_PIXELS: i64 = 600;

#[derive(Clone, Copy)]
struct PageImage {
    x_inches: f64,
    y_inches: f64,
    width_inches: f64,
    height_inches: f64,
}

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

fn imported_page_images(images: &[PageImage]) -> Workbook {
    let mut frames = String::new();
    for (index, image) in images.iter().enumerate() {
        write!(
            frames,
            r#"<draw:frame draw:name="Page image {index}" text:anchor-type="page" svg:x="{}in" svg:y="{}in" svg:width="{}in" svg:height="{}in"><draw:image xlink:href="Pictures/page.png"/></draw:frame>"#,
            image.x_inches, image.y_inches, image.width_inches, image.height_inches,
        )
        .expect("write page-image frame");
    }
    let content = format!(
        r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="Page images"><table:table-column table:number-columns-repeated="3"/><table:shapes>{frames}</table:shapes><table:table-row table:number-rows-repeated="3"><table:table-cell table:number-columns-repeated="3"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#,
    );
    let png = solid_rgba_png();
    let mut workbook = Workbook::open(&zip_parts(&[
        (
            "mimetype",
            b"application/vnd.oasis.opendocument.spreadsheet",
        ),
        ("content.xml", content.as_bytes()),
        ("Pictures/page.png", &png),
    ]))
    .expect("minimal ODS page image fixture");

    let sheet = &mut workbook.sheets[0];
    for column in 0..3 {
        // 85 characters at the deterministic 7 px digit width plus the Calc
        // import allowance is exactly 597 CSS pixels.
        sheet.set_col_width(column, 85.0);
    }
    for row in 0..3 {
        // 450 points at 96 dpi is exactly 600 CSS pixels.
        sheet.set_row_height(row, 450.0);
    }
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 2, 2))
            .with_paper_size(1)
            .with_scale(100),
    );
    workbook
}

fn spanning_image_workbook() -> Workbook {
    imported_page_images(&[PageImage {
        x_inches: 5.5,
        y_inches: 5.5,
        width_inches: 1.5,
        height_inches: 1.5,
    }])
}

fn collect_image_clips(nodes: &[SceneNode], output: &mut Vec<(Rect, Rect)>) {
    for node in nodes {
        if let SceneNode::ClipGroup(group) = node {
            for child in &group.nodes {
                if let SceneNode::Image(image) = child {
                    output.push((group.clip, image.rect));
                }
            }
            collect_image_clips(&group.nodes, output);
        }
    }
}

fn image_clips(nodes: &[SceneNode]) -> Vec<(Rect, Rect)> {
    let mut output = Vec::new();
    collect_image_clips(nodes, &mut output);
    output
}

fn pixels(raw: i64) -> i64 {
    raw / 1_024
}

#[test]
fn explicit_ranges_and_print_tiles_keep_full_geometry_inside_local_clips() {
    let workbook = spanning_image_workbook();
    let expected_offsets = [
        ((0, 0), (528, 528)),
        ((0, 1), (-69, 528)),
        ((1, 0), (528, -72)),
        ((1, 1), (-69, -72)),
    ];

    for ((row, column), (expected_x, expected_y)) in expected_offsets {
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(row, column, row, column)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert_eq!(build.scene.width, Fixed::from_pixels(COLUMN_PIXELS));
        assert_eq!(build.scene.height, Fixed::from_pixels(ROW_PIXELS));
        let clips = image_clips(&build.scene.nodes);
        assert_eq!(clips.len(), 1, "range ({row}, {column})");
        let (clip, image) = clips[0];
        assert_eq!(
            clip,
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(COLUMN_PIXELS),
                height: Fixed::from_pixels(ROW_PIXELS),
            }
        );
        assert_eq!(pixels(image.x.raw()), expected_x);
        assert_eq!(pixels(image.y.raw()), expected_y);
        assert_eq!(image.width, Fixed::from_pixels(144));
        assert_eq!(image.height, Fixed::from_pixels(144));
    }

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            omit_sparse_pages: false,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(document.report.logical_pages, 9);
    assert_eq!(document.pages.len(), 9);
    for page in &document.pages {
        let clips = image_clips(&page.scene.nodes);
        let expected = expected_offsets.iter().find(|((row, column), _)| {
            usize::from(*column) == page.map.horizontal_index
                && usize::try_from(*row).unwrap() == page.map.vertical_index
        });
        match expected {
            Some((_, (expected_x, expected_y))) => {
                assert_eq!(clips.len(), 1, "page map: {:?}", page.map);
                let (clip, image) = clips[0];
                assert_eq!(clip.width, Fixed::from_pixels(COLUMN_PIXELS));
                assert_eq!(clip.height, Fixed::from_pixels(ROW_PIXELS));
                assert_eq!(pixels(image.x.raw() - clip.x.raw()), *expected_x);
                assert_eq!(pixels(image.y.raw() - clip.y.raw()), *expected_y);
                assert_eq!(image.width, Fixed::from_pixels(144));
                assert_eq!(image.height, Fixed::from_pixels(144));
            }
            None => assert!(clips.is_empty(), "page map: {:?}", page.map),
        }
    }
}

#[test]
fn sparse_page_omission_retains_every_drawing_only_intersection() {
    let document =
        build_print_document(&spanning_image_workbook(), 0, &PrintOptions::default()).unwrap();

    assert_eq!(document.report.logical_pages, 9);
    assert_eq!(document.report.sparse_pages_omitted, 5);
    assert_eq!(document.pages.len(), 4);
    assert_eq!(
        document
            .pages
            .iter()
            .map(|page| (page.map.horizontal_index, page.map.vertical_index))
            .collect::<Vec<_>>(),
        vec![(0, 0), (0, 1), (1, 0), (1, 1)]
    );
    assert!(document
        .pages
        .iter()
        .all(|page| image_clips(&page.scene.nodes).len() == 1));
}

#[test]
fn default_used_print_range_reaches_absolute_only_drawing_geometry() {
    let mut workbook = imported_page_images(&[PageImage {
        // Entirely beyond A1, crossing the second horizontal and vertical
        // page boundaries. No cell value or explicit print area establishes
        // this extent; the absolute drawing must do so itself.
        x_inches: 11.5,
        y_inches: 11.5,
        width_inches: 1.5,
        height_inches: 1.5,
    }]);
    workbook.sheets[0].set_page_setup(PageSetup::new().with_paper_size(1).with_scale(100));

    let document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
    assert_eq!(document.report.logical_pages, 9);
    assert_eq!(document.report.sparse_pages_omitted, 5);
    assert_eq!(
        document
            .pages
            .iter()
            .map(|page| (page.map.horizontal_index, page.map.vertical_index))
            .collect::<Vec<_>>(),
        vec![(1, 1), (1, 2), (2, 1), (2, 2)]
    );
    assert!(document
        .pages
        .iter()
        .all(|page| image_clips(&page.scene.nodes).len() == 1));
}

#[test]
fn repeated_titles_retain_corner_and_row_objects_without_cross_column_duplication() {
    let mut workbook = imported_page_images(&[
        PageImage {
            // Inside the repeated row and the first body-column viewport.
            x_inches: 6.75,
            y_inches: 0.125,
            width_inches: 0.5,
            height_inches: 0.5,
        },
        PageImage {
            // Inside the repeated row/column corner.
            x_inches: 0.125,
            y_inches: 0.125,
            width_inches: 0.25,
            height_inches: 0.25,
        },
    ]);
    workbook.sheets[0].set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 2, 2))
            .with_repeat_rows(0, 0)
            .with_repeat_cols(0, 0)
            .with_paper_size(1)
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
    assert_eq!(document.report.logical_pages, 4);
    assert_eq!(document.pages.len(), 4);

    for page in &document.pages {
        let clips = image_clips(&page.scene.nodes);
        let row_title_images = clips
            .iter()
            .filter(|(_, image)| image.width == Fixed::from_pixels(48))
            .count();
        let corner_images = clips
            .iter()
            .filter(|(_, image)| image.width == Fixed::from_pixels(24))
            .count();
        assert_eq!(corner_images, 1, "page map: {:?}", page.map);
        assert_eq!(
            row_title_images,
            usize::from(page.map.horizontal_index == 0),
            "page map: {:?}",
            page.map
        );
    }
    assert_eq!(
        document
            .pages
            .iter()
            .filter(|page| {
                page.map.horizontal_index == 0
                    && page.map.vertical_index == 1
                    && image_clips(&page.scene.nodes)
                        .iter()
                        .any(|(_, image)| image.width == Fixed::from_pixels(48))
            })
            .count(),
        1,
        "the repeated-row object must recur on vertical page two"
    );
}

#[test]
fn decoded_media_limit_is_aggregated_across_retained_fragments() {
    let options = PrintOptions {
        render: RenderOptions {
            limits: RenderLimits {
                // Each retained page owns one decoded 1x1 RGBA image (4 bytes),
                // so three pages fit and the fourth reaches an exact 16 bytes.
                max_decoded_media_bytes: 12,
                ..RenderLimits::default()
            },
            ..RenderOptions::default()
        },
        ..PrintOptions::default()
    };
    let mut exact = options.clone();
    exact.render.limits.max_decoded_media_bytes = 16;
    assert_eq!(
        build_print_document(&spanning_image_workbook(), 0, &exact)
            .unwrap()
            .pages
            .len(),
        4
    );
    assert_eq!(
        build_print_document(&spanning_image_workbook(), 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::DecodedMediaBytes,
            limit: 12,
            actual: 16,
        })
    );
}

#[test]
fn streamed_pages_each_receive_a_fresh_decoded_media_budget() {
    let workbook = spanning_image_workbook();
    let options = PrintOptions {
        render: RenderOptions {
            limits: RenderLimits {
                max_decoded_media_bytes: 4,
                ..RenderLimits::default()
            },
            ..RenderOptions::default()
        },
        ..PrintOptions::default()
    };
    let prepared = prepare_print_document(&workbook, 0, &options).unwrap();

    for page_index in [0, 1] {
        let page = build_print_page(&workbook, &prepared, page_index).unwrap();
        assert_eq!(image_clips(&page.scene.nodes).len(), 1);
    }
    assert_eq!(
        build_print_document(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::DecodedMediaBytes,
            limit: 4,
            actual: 8,
        })
    );
}

#[test]
fn single_page_scene_node_limits_count_clip_group_descendants() {
    let workbook = spanning_image_workbook();
    let mut options = PrintOptions {
        render: RenderOptions {
            gridlines: false,
            ..RenderOptions::default()
        },
        single_page_sheets: true,
        limits: PrintLimits {
            max_total_scene_nodes: 2,
            ..PrintLimits::default()
        },
        ..PrintOptions::default()
    };
    let mut prepared = prepare_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(prepared.report.source.scene_nodes, 2);

    prepared.limits.max_total_scene_nodes = 1;
    assert_eq!(
        build_print_page(&workbook, &prepared, 0),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::PageSceneNodes,
            limit: 1,
            actual: 2,
        })
    );

    options.limits.max_total_scene_nodes = 1;
    assert_eq!(
        prepare_print_document(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::PageSceneNodes,
            limit: 1,
            actual: 2,
        })
    );
}

#[test]
fn streamed_pages_aggregate_decoded_media_across_repeat_blocks() {
    let mut workbook = imported_page_images(&[PageImage {
        x_inches: 0.125,
        y_inches: 5.5,
        width_inches: 0.25,
        height_inches: 1.5,
    }]);
    workbook.sheets[0].set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 2, 0))
            .with_repeat_rows(0, 0)
            .with_paper_size(1)
            .with_scale(100),
    );
    let options = PrintOptions {
        render: RenderOptions {
            limits: RenderLimits {
                // The source range and each block decode 4 bytes independently,
                // but page one retains both the repeated-row and body fragments.
                max_decoded_media_bytes: 7,
                ..RenderLimits::default()
            },
            ..RenderOptions::default()
        },
        omit_sparse_pages: false,
        ..PrintOptions::default()
    };
    let mut exact = options.clone();
    exact.render.limits.max_decoded_media_bytes = 8;
    let exact_prepared = prepare_print_document(&workbook, 0, &exact).unwrap();
    assert_eq!(
        image_clips(
            &build_print_page(&workbook, &exact_prepared, 0)
                .unwrap()
                .scene
                .nodes
        )
        .len(),
        2
    );
    let prepared = prepare_print_document(&workbook, 0, &options).unwrap();
    assert_eq!(
        build_print_page(&workbook, &prepared, 0),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::DecodedMediaBytes,
            limit: 7,
            actual: 8,
        })
    );
}
