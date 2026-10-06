//! Public contracts for bounded compressed worksheet viewport geometry.

use std::io::Write as _;

use rxls::{CellStyle, HAlign, Image, ImageFmt, VAlign, Workbook};
use rxls_render::{
    prepare_viewport, render_sheet_svg, render_viewport_tile, Fixed, LimitKind, Rect, RenderError,
    RenderOptions, RenderRange, RenderSelection, SceneNode, TextNode, ViewportError,
    ViewportLimits,
};
use zip::write::SimpleFileOptions;

fn options(range: RenderRange) -> RenderOptions {
    RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        ..RenderOptions::default()
    }
}

fn rect(x: i64, y: i64, width: i64, height: i64) -> Rect {
    Rect {
        x: Fixed::from_pixels(x),
        y: Fixed::from_pixels(y),
        width: Fixed::from_pixels(width),
        height: Fixed::from_pixels(height),
    }
}

fn text<'a>(nodes: &'a [SceneNode], value: &str) -> Option<&'a TextNode> {
    for node in nodes {
        match node {
            SceneNode::Text(node) if node.text == value => return Some(node),
            SceneNode::ClipGroup(group) => {
                if let Some(found) = text(&group.nodes, value) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

#[test]
fn merged_text_uses_one_complete_box_before_both_tile_clips() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("merged");
    sheet.set_default_row_height(24.0);
    sheet.set_col_width(0, 10.0);
    sheet.set_col_width(1, 20.0);
    sheet.merge(0, 0, 0, 1);
    sheet.write_styled(
        0,
        0,
        "MERGED",
        &CellStyle::new()
            .align(HAlign::Center)
            .valign(VAlign::Middle),
    );
    let range = RenderRange::new(0, 0, 0, 1);
    let options = options(range);
    let before = render_sheet_svg(&workbook, 0, &options).unwrap();
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    assert_eq!(prepared.columns().extent(), Fixed::from_pixels(214));
    for (x, width, namespace) in [(0, 72, 1), (72, 142, 2)] {
        let tile = render_viewport_tile(&prepared, rect(x, 0, width, 32), namespace)
            .unwrap()
            .unwrap();
        assert_eq!(tile.source_range, range);
        let label = text(&tile.scene.nodes, "MERGED").unwrap();
        assert_eq!(label.bounds.width, Fixed::from_pixels(214));
        assert_eq!(
            label.bounds.x.raw() + label.bounds.width.raw() / 2 + tile.logical_rect.x.raw(),
            Fixed::from_pixels(107).raw()
        );
        assert_eq!(tile.scene.width, Fixed::from_pixels(width));
        let SceneNode::ClipGroup(group) = &tile.scene.nodes[0] else {
            panic!("missing final clip");
        };
        assert_eq!(group.clip, rect(0, 0, width, 32));
    }
    assert_eq!(render_sheet_svg(&workbook, 0, &options).unwrap(), before);
    for (col, center) in [(0, 36), (1, 71)] {
        let legacy = render_sheet_svg(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, col, 0, col)),
                ..options.clone()
            },
        )
        .unwrap();
        let label = text(&legacy.scene.nodes, "MERGED").unwrap();
        assert_eq!(
            label.bounds.x.raw() + label.bounds.width.raw() / 2,
            Fixed::from_pixels(center).raw()
        );
    }
}

#[test]
fn hidden_tracks_contribute_zero_and_do_not_shift_the_next_tile() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("hidden");
    sheet.set_default_row_height(15.0);
    sheet.set_col_width(0, 10.0);
    sheet.set_col_width(1, 20.0);
    sheet.set_col_width(2, 30.0);
    sheet.hide_row(1);
    sheet.hide_column(1);
    sheet.write(0, 0, "TOP");
    sheet.write(2, 2, "BOTTOM");
    let range = RenderRange::new(0, 0, 2, 2);
    let options = options(range);
    let whole = render_sheet_svg(&workbook, 0, &options).unwrap();
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    assert_eq!(prepared.rows().extent(), Fixed::from_pixels(40));
    assert_eq!(
        prepared.rows().track(1).unwrap(),
        (Fixed::from_pixels(20), Fixed::ZERO)
    );
    assert_eq!(prepared.rows().track(2).unwrap().0, Fixed::from_pixels(20));
    assert_eq!(prepared.columns().extent(), Fixed::from_pixels(284));
    assert_eq!(
        prepared.columns().track(1).unwrap(),
        (Fixed::from_pixels(72), Fixed::ZERO)
    );
    assert_eq!(
        prepared.columns().track(2).unwrap().0,
        Fixed::from_pixels(72)
    );
    assert_eq!(
        prepared
            .columns()
            .source_interval(Fixed::from_pixels(72), Fixed::from_pixels(284))
            .unwrap(),
        Some((2, 2))
    );
    let lower = render_viewport_tile(&prepared, rect(72, 20, 212, 20), 3)
        .unwrap()
        .unwrap();
    assert_eq!(lower.logical_rect, rect(72, 20, 212, 20));
    assert_eq!(lower.source_range, RenderRange::new(2, 0, 2, 2));
    assert_eq!(
        text(&lower.scene.nodes, "BOTTOM").unwrap().bounds.x,
        Fixed::ZERO
    );
    assert_eq!(
        text(&lower.scene.nodes, "BOTTOM").unwrap().bounds.y.raw() + lower.logical_rect.y.raw(),
        text(&whole.scene.nodes, "BOTTOM").unwrap().bounds.y.raw()
    );
    let legacy = render_sheet_svg(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(1, 1, 1, 1)),
            ..options
        },
    )
    .unwrap();
    assert_eq!(
        (legacy.scene.width, legacy.scene.height),
        (Fixed::from_pixels(1), Fixed::from_pixels(1))
    );
}

#[test]
fn all_hidden_logical_sheet_has_no_fake_paint_tile() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("empty-visible");
    sheet.hide_row(0);
    sheet.hide_column(0);
    sheet.write(0, 0, "hidden");
    let range = RenderRange::new(0, 0, 0, 0);
    let options = options(range);
    let before = render_sheet_svg(&workbook, 0, &options).unwrap();
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    assert_eq!(
        (prepared.rows().extent(), prepared.columns().extent()),
        (Fixed::ZERO, Fixed::ZERO)
    );
    assert!(render_viewport_tile(&prepared, rect(0, 0, 100, 100), 0)
        .unwrap()
        .is_none());
    assert_eq!(render_sheet_svg(&workbook, 0, &options).unwrap(), before);
}

#[test]
fn over_budget_rows_prepare_as_runs_and_render_one_existing_budget_band() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("large");
    sheet.set_default_row_height(15.0);
    sheet.write_number(0, 0, 1);
    sheet.write_number(5_999, 63, 2);
    let range = RenderRange::new(0, 0, 5_999, 63);
    let options = options(range);
    assert!(matches!(
        render_sheet_svg(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Rows,
            limit: 4096,
            actual: 6000,
        })
    ));
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    assert_eq!(prepared.rows().runs().len(), 1);
    assert_eq!(prepared.columns().runs().len(), 1);
    assert_eq!(prepared.coordinate_visits(), 66);
    assert!(prepared.geometry_bytes() < 2048);
    let tile = render_viewport_tile(&prepared, rect(0, 60_000, 512, 400), 4)
        .unwrap()
        .unwrap();
    assert_eq!(tile.source_range, RenderRange::new(3000, 0, 3019, 63));
    assert_eq!(tile.report.rows_considered, 20);
    assert_eq!(tile.report.cells_considered, 20 * 64);
    assert_eq!(tile.scene.height, Fixed::from_pixels(400));
}

#[test]
fn viewport_caps_fail_before_publishing_partial_geometry() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("limits");
    sheet.set_default_row_height(15.0);
    sheet.write(1, 0, "data");
    let range = RenderRange::new(0, 0, 1, 0);
    let options = options(range);
    for (resource, limits) in [
        (
            "coordinate_visits",
            ViewportLimits {
                max_coordinate_visits: 1,
                ..ViewportLimits::default()
            },
        ),
        (
            "axis_runs",
            ViewportLimits {
                max_axis_runs: 1,
                ..ViewportLimits::default()
            },
        ),
        (
            "geometry_bytes",
            ViewportLimits {
                max_geometry_bytes: 0,
                ..ViewportLimits::default()
            },
        ),
        (
            "logical_dimension_raw",
            ViewportLimits {
                max_logical_dimension_raw: 20 * 1024,
                ..ViewportLimits::default()
            },
        ),
        (
            "options_bytes",
            ViewportLimits {
                max_options_bytes: 1,
                ..ViewportLimits::default()
            },
        ),
    ] {
        assert!(
            matches!(prepare_viewport(&workbook, 0, range, &options, limits),
            Err(ViewportError::Limit { resource: actual, .. }) if actual == resource)
        );
    }
    let good = prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    assert_eq!(good.rows().extent(), Fixed::from_pixels(40));
}

#[test]
fn huge_merge_closure_and_partial_source_range_are_typed_rejections() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("large-merge");
    sheet.set_default_row_height(15.0);
    sheet.merge(0, 0, 9_999, 0);
    sheet.write(0, 0, "MERGED");
    let range = RenderRange::new(0, 0, 9_999, 0);
    let options = options(range);
    assert!(matches!(
        prepare_viewport(
            &workbook,
            0,
            RenderRange::new(0, 0, 0, 0),
            &options,
            ViewportLimits::default()
        ),
        Err(ViewportError::Unsupported {
            reason: "partial_merge_source_range"
        })
    ));
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    assert!(matches!(
        render_viewport_tile(&prepared, rect(0, 0, 64, 20), 0),
        Err(ViewportError::Render(RenderError::LimitExceeded {
            kind: LimitKind::Rows,
            limit: 4096,
            actual: 10_000
        }))
    ));
}

#[test]
fn tile_namespace_changes_only_internal_ids_and_output_budget_still_applies() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("namespaces");
    sheet.write(0, 0, "url(#clip-9) is cell data");
    let range = RenderRange::new(0, 0, 0, 0);
    let options = options(range);
    let before = render_sheet_svg(&workbook, 0, &options).unwrap();
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    let first = render_viewport_tile(&prepared, rect(0, 0, 64, 20), 1)
        .unwrap()
        .unwrap();
    let second = render_viewport_tile(&prepared, rect(0, 0, 64, 20), 2)
        .unwrap()
        .unwrap();
    assert!(first.svg.contains("url(#clip-9) is cell data"));
    assert!(first.svg.contains("id=\"vp-1-clip-0\""));
    assert!(first.svg.contains("url(#vp-1-clip-0)"));
    assert!(second.svg.contains("id=\"vp-2-clip-0\""));
    assert!(!first.svg.contains("vp-2-clip-"));
    assert_eq!(render_sheet_svg(&workbook, 0, &options).unwrap(), before);
    let namespace_cap = first.svg.len() as u64;
    let tiny = RenderOptions {
        limits: rxls_render::RenderLimits {
            max_output_bytes: namespace_cap,
            ..options.limits.clone()
        },
        ..options.clone()
    };
    let prepared = prepare_viewport(&workbook, 0, range, &tiny, ViewportLimits::default()).unwrap();
    assert!(matches!(
        render_viewport_tile(&prepared, rect(0, 0, 64, 20), u64::MAX),
        Err(ViewportError::Render(RenderError::LimitExceeded {
            kind: LimitKind::OutputBytes,
            limit,
            actual,
        })) if limit == namespace_cap && actual > limit
    ));
}

#[test]
fn imported_default_hidden_rows_form_zero_runs_with_visible_exceptions() {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (path, xml) in [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="hidden-default" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetFormatPr defaultRowHeight="15" zeroHeight="1"/><sheetData><row r="2"><c r="A2"><v>1</v></c></row></sheetData></worksheet>"#,
        ),
    ] {
        writer
            .start_file(path, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(xml.as_bytes()).unwrap();
    }
    let workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
    assert!(workbook.sheets[0].default_hidden_row_exceptions().is_some());
    let range = RenderRange::new(0, 0, 100_000, 0);
    let options = options(range);
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    assert_eq!(prepared.rows().runs().len(), 3);
    assert_eq!(prepared.rows().extent(), Fixed::from_pixels(20));
    assert_eq!(
        prepared
            .rows()
            .source_interval(Fixed::ZERO, Fixed::from_pixels(20))
            .unwrap(),
        Some((1, 1))
    );
    assert_eq!(
        prepared.rows().track(100_000).unwrap(),
        (Fixed::from_pixels(20), Fixed::ZERO)
    );
    assert!(prepared.geometry_bytes() < 2048);
    assert!(prepared.coordinate_visits() <= 10);
}

#[test]
fn drawing_geometry_is_explicitly_unsupported_in_this_slice() {
    let mut workbook = Workbook::new();
    workbook
        .add_sheet("drawing")
        .add_image(Image::new(vec![0], ImageFmt::Png, (0, 0)));
    let range = RenderRange::new(0, 0, 0, 0);
    let options = options(range);
    assert!(matches!(
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()),
        Err(ViewportError::Unsupported {
            reason: "drawing_geometry_not_prepared"
        })
    ));
}

#[test]
fn rtl_tiles_keep_the_whole_row_reflection_phase() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("rtl");
    sheet.set_right_to_left(true);
    sheet.set_default_row_height(15.0);
    sheet.set_col_width(0, 10.0);
    sheet.set_col_width(1, 20.0);
    sheet.write(0, 0, "A");
    sheet.write(0, 1, "B");
    let range = RenderRange::new(0, 0, 0, 1);
    let options = options(range);
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    for (value, x, width, namespace) in [("B", 0, 142, 1), ("A", 142, 72, 2)] {
        let tile = render_viewport_tile(&prepared, rect(x, 0, width, 20), namespace)
            .unwrap()
            .unwrap();
        let label = text(&tile.scene.nodes, value).unwrap();
        assert_eq!(label.bounds.x, Fixed::ZERO);
        assert_eq!(label.bounds.width, Fixed::from_pixels(width));
    }
}

#[test]
fn vertical_merge_closes_row_band_before_layout_and_translation() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("vertical-merge");
    sheet.set_default_row_height(15.0);
    sheet.merge(0, 0, 2, 0);
    sheet.write_styled(0, 0, "MERGED", &CellStyle::new().valign(VAlign::Middle));
    let range = RenderRange::new(0, 0, 2, 0);
    let options = options(range);
    let whole = render_sheet_svg(&workbook, 0, &options).unwrap();
    let expected = text(&whole.scene.nodes, "MERGED").unwrap();
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    for (y, namespace) in [(0, 1), (20, 2), (40, 3)] {
        let tile = render_viewport_tile(&prepared, rect(0, y, 64, 20), namespace)
            .unwrap()
            .unwrap();
        assert_eq!(tile.source_range, range);
        let label = text(&tile.scene.nodes, "MERGED").unwrap();
        assert_eq!(label.bounds.height, expected.bounds.height);
        assert_eq!(
            label.bounds.y.raw() + tile.logical_rect.y.raw(),
            expected.bounds.y.raw()
        );
        assert_eq!(
            label.bounds.y.raw() + label.bounds.height.raw() / 2 + tile.logical_rect.y.raw(),
            Fixed::from_pixels(30).raw()
        );
    }
}
