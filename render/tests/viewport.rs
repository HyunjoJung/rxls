//! Public contracts for bounded compressed worksheet viewport geometry.

use std::io::Write as _;
use std::sync::Arc;

use rxls::{CellStyle, HAlign, Image, ImageFmt, VAlign, Workbook};
use rxls_render::{
    prepare_viewport, render_sheet_svg, render_viewport_tile, Fixed, LimitKind,
    OwnedPreparedViewport, Rect, RenderError, RenderOptions, RenderRange, RenderSelection,
    SceneNode, TextNode, ViewportError, ViewportLimits,
};
use zip::write::SimpleFileOptions;

fn add(left: Fixed, right: Fixed) -> Fixed {
    Fixed::from_raw(left.raw().checked_add(right.raw()).unwrap())
}

fn subtract(left: Fixed, right: Fixed) -> Fixed {
    Fixed::from_raw(left.raw().checked_sub(right.raw()).unwrap())
}

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
        assert_eq!(tile.report.visible_rows, 1);
        assert_eq!(tile.report.visible_columns, 1);
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
    assert_eq!(lower.source_range, RenderRange::new(2, 2, 2, 2));
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
    assert_eq!(prepared.coordinate_visits(), 69);
    assert!(prepared.geometry_bytes() < 2048);
    let tile = render_viewport_tile(&prepared, rect(0, 60_000, 512, 400), 4)
        .unwrap()
        .unwrap();
    assert_eq!(tile.source_range, RenderRange::new(3000, 0, 3019, 7));
    assert_eq!(tile.report.rows_considered, 20);
    assert_eq!(tile.report.cells_considered, 20 * 8);
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
fn huge_merge_paints_complete_box_but_partial_source_range_remains_rejected() {
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
    let tile = render_viewport_tile(&prepared, rect(0, 0, 64, 20), 0)
        .unwrap()
        .unwrap();
    assert_eq!(tile.report.visible_rows, 1);
    assert_eq!(tile.report.merged_regions, 1);
    assert_eq!(
        text(&tile.scene.nodes, "MERGED").unwrap().bounds.height,
        Fixed::from_pixels(200_000)
    );
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
        assert_eq!(tile.report.visible_rows, 1);
        assert_eq!(tile.report.visible_columns, 1);
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

#[test]
fn huge_explicit_sparse_range_renders_small_window_without_source_product() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("sparse-large");
    sheet.set_default_row_height(15.0);
    sheet.write_number(0, 0, 1);
    sheet.write(100_000, 16_383, "FAR");
    let range = RenderRange::new(0, 0, 100_000, 16_383);
    let options = options(range);
    assert!(matches!(
        render_sheet_svg(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Rows,
            ..
        })
    ));
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    let (x, width) = prepared.columns().track(16_383).unwrap();
    let (y, height) = prepared.rows().track(100_000).unwrap();
    let request = Rect {
        x,
        y,
        width,
        height,
    };
    let tile = render_viewport_tile(&prepared, request, 100)
        .unwrap()
        .unwrap();
    assert_eq!(
        tile.source_range,
        RenderRange::new(100_000, 16_383, 100_000, 16_383)
    );
    assert_eq!(
        (tile.report.visible_rows, tile.report.visible_columns),
        (1, 1)
    );
    assert_eq!(tile.report.cells_considered, 1);
    let bounded_options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(100_000, 16_383, 100_000, 16_383)),
        ..options.clone()
    };
    let bounded = render_sheet_svg(&workbook, 0, &bounded_options).unwrap();
    assert_eq!(
        text(&tile.scene.nodes, "FAR").unwrap(),
        text(&bounded.scene.nodes, "FAR").unwrap()
    );
    assert_eq!(
        render_viewport_tile(&prepared, request, 100)
            .unwrap()
            .unwrap(),
        tile
    );
}

#[test]
fn offscreen_spill_owner_and_global_blocker_keep_original_layout() {
    for rtl in [false, true] {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("spill");
        sheet.set_right_to_left(rtl);
        sheet.set_default_row_height(15.0);
        let label = "spill words ".repeat(500);
        sheet.write_styled(
            0,
            0,
            label.as_str(),
            &CellStyle::new().align(if rtl { HAlign::Right } else { HAlign::Left }),
        );
        let range = RenderRange::new(0, 0, 0, 999);
        let options = options(range);
        let request = {
            let prepared =
                prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
            let (logical_x, width) = prepared.columns().track(400).unwrap();
            let x = if rtl {
                subtract(subtract(prepared.columns().extent(), logical_x), width)
            } else {
                logical_x
            };
            let request = Rect {
                x,
                y: Fixed::ZERO,
                width,
                height: Fixed::from_pixels(20),
            };
            let tile = render_viewport_tile(&prepared, request, 101)
                .unwrap()
                .unwrap();
            let owner = text(&tile.scene.nodes, &label).unwrap();
            let (owner_x, owner_width) = prepared.columns().track(0).unwrap();
            let expected_x = if rtl {
                subtract(subtract(prepared.columns().extent(), owner_x), owner_width)
            } else {
                owner_x
            };
            assert_eq!(add(owner.bounds.x, request.x), expected_x);
            assert_eq!(owner.bounds.width, owner_width);
            assert_eq!(owner.clip_bounds.width, prepared.columns().extent());
            request
        };
        // A populated cell between the source and window ends its global spill.
        workbook.sheets[0].write_number(0, 200, 7);
        let prepared =
            prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
        let tile = render_viewport_tile(&prepared, request, 102)
            .unwrap()
            .unwrap();
        assert!(text(&tile.scene.nodes, &label).is_none());
    }
}

#[test]
fn styled_blank_does_not_block_spill_but_merge_does() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("blockers");
    sheet.set_default_row_height(15.0);
    sheet.write(0, 0, "long long long long long long long long");
    sheet.write_blank_styled(0, 1, &CellStyle::new().fill(rxls::Color::rgb(1, 2, 3)));
    let range = RenderRange::new(0, 0, 0, 3);
    let options = options(range);
    let request = {
        let prepared =
            prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
        let (x, width) = prepared.columns().track(2).unwrap();
        let request = Rect {
            x,
            y: Fixed::ZERO,
            width,
            height: Fixed::from_pixels(20),
        };
        let tile = render_viewport_tile(&prepared, request, 103)
            .unwrap()
            .unwrap();
        assert!(text(&tile.scene.nodes, "long long long long long long long long").is_some());
        request
    };
    workbook.sheets[0].merge(0, 1, 0, 1);
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    let tile = render_viewport_tile(&prepared, request, 104)
        .unwrap()
        .unwrap();
    assert!(text(&tile.scene.nodes, "long long long long long long long long").is_none());
}

#[test]
fn giant_complete_merge_keeps_one_box_in_middle_and_last_tiles() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("giant");
    sheet.set_default_row_height(15.0);
    sheet.merge(0, 0, 100_000, 16_383);
    sheet.write_styled(
        0,
        0,
        "GIANT",
        &CellStyle::new()
            .align(HAlign::Center)
            .valign(VAlign::Middle),
    );
    let range = RenderRange::new(0, 0, 100_000, 16_383);
    let options = options(range);
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    for (row, col, namespace) in [(50_000, 8_192, 105), (100_000, 16_383, 106)] {
        let (x, width) = prepared.columns().track(col).unwrap();
        let (y, height) = prepared.rows().track(row).unwrap();
        let tile = render_viewport_tile(
            &prepared,
            Rect {
                x,
                y,
                width,
                height,
            },
            namespace,
        )
        .unwrap()
        .unwrap();
        assert_eq!(tile.report.rendered_regions, 1);
        let label = text(&tile.scene.nodes, "GIANT").unwrap();
        assert_eq!(add(label.bounds.x, x), Fixed::ZERO);
        assert_eq!(add(label.bounds.y, y), Fixed::ZERO);
        assert_eq!(label.bounds.width, prepared.columns().extent());
        assert_eq!(label.bounds.height, prepared.rows().extent());
    }
}

#[test]
fn used_matches_legacy_styled_blank_hidden_and_active_merge_extent() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("used");
    sheet.set_default_row_height(15.0);
    sheet.write(2, 3, "used");
    sheet.write_blank_styled(7, 8, &CellStyle::new().fill(rxls::Color::rgb(3, 4, 5)));
    sheet.write_blank_styled(50, 50, &CellStyle::new().bold());
    sheet.merge(1, 2, 4, 5);
    sheet.merge(10, 10, 20, 20); // Detached empty merge.
    sheet.hide_row(7);
    sheet.hide_column(8);
    let options = RenderOptions {
        selection: RenderSelection::Used,
        ..RenderOptions::default()
    };
    let legacy = render_sheet_svg(&workbook, 0, &options).unwrap();
    let prepared =
        rxls_render::prepare_used_viewport(&workbook, 0, &options, ViewportLimits::default())
            .unwrap()
            .unwrap();
    assert_eq!(prepared.source_range(), legacy.report.range);
    assert_eq!(prepared.source_range(), RenderRange::new(1, 2, 7, 8));
    assert_eq!(prepared.rows().extent(), Fixed::from_pixels(120));
    let report = prepared.preparation_report().clone();
    let tile = render_viewport_tile(&prepared, rect(0, 0, 128, 80), 107)
        .unwrap()
        .unwrap();
    // Interior content activates the merge extent, but only its anchor paints.
    assert!(text(&legacy.scene.nodes, "used").is_none());
    assert!(text(&tile.scene.nodes, "used").is_none());
    assert_eq!(prepared.preparation_report(), &report);
    assert!(prepared.preparation_report().coordinate_visits > 0);

    workbook.sheets[0].write(1, 2, "anchor");
    let legacy_anchor = render_sheet_svg(&workbook, 0, &options).unwrap();
    let prepared_anchor =
        rxls_render::prepare_used_viewport(&workbook, 0, &options, ViewportLimits::default())
            .unwrap()
            .unwrap();
    assert_eq!(prepared_anchor.source_range(), legacy_anchor.report.range);
    assert_eq!(prepared_anchor.source_range(), RenderRange::new(1, 2, 7, 8));
    let tile_anchor = render_viewport_tile(&prepared_anchor, rect(0, 0, 128, 80), 108)
        .unwrap()
        .unwrap();
    assert_eq!(
        text(&tile_anchor.scene.nodes, "anchor").unwrap(),
        text(&legacy_anchor.scene.nodes, "anchor").unwrap()
    );
}

#[test]
fn empty_used_ignores_style_only_axes_and_detached_merges() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("empty");
    sheet.merge(0, 0, 100_000, 10_000);
    sheet.write_blank_styled(9, 9, &CellStyle::new().bold());
    let options = RenderOptions::default();
    assert!(
        rxls_render::prepare_used_viewport(&workbook, 0, &options, ViewportLimits::default())
            .unwrap()
            .is_none()
    );
}

#[test]
fn compressed_default_warning_counts_all_logical_rows_once() {
    let mut workbook = Workbook::new();
    workbook
        .add_sheet("fallback")
        .set_default_row_height(f32::NAN);
    let range = RenderRange::new(0, 0, 99_999, 0);
    let options = options(range);
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    let warnings = &prepared.preparation_report().warnings;
    let geometry = warnings
        .iter()
        .find(|warning| warning.code == rxls_render::WarningCode::InvalidGeometryFallback)
        .unwrap();
    assert_eq!(geometry.occurrences, 100_000);
    assert_eq!(
        geometry.first_cell,
        Some(rxls_render::CellCoordinate { row: 0, col: 0 })
    );
    assert!(prepared.coordinate_visits() < 10);
    let report = prepared.preparation_report().clone();
    render_viewport_tile(&prepared, rect(0, 500, 64, 20), 108)
        .unwrap()
        .unwrap();
    assert_eq!(prepared.preparation_report(), &report);
}

#[test]
fn sparse_tile_text_cap_and_source_preflight_are_typed_and_repeatable() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("caps");
    sheet.set_default_row_height(15.0);
    sheet.write(0, 0, "long source");
    let range = RenderRange::new(0, 0, 0, 999);
    let mut options = options(range);
    options.limits.max_text_bytes = 3;
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    let first = render_viewport_tile(&prepared, rect(100, 0, 64, 20), 109).unwrap_err();
    assert!(matches!(
        first,
        ViewportError::Render(RenderError::LimitExceeded {
            kind: LimitKind::TextBytes,
            ..
        })
    ));
    assert_eq!(
        render_viewport_tile(&prepared, rect(100, 0, 64, 20), 109).unwrap_err(),
        first
    );
    drop(prepared);
    workbook.sheets[0].write_number(0, 0, 1);
    workbook.sheets[0].write_number(0, 0, 2); // Raw duplicate counts before index compaction.
    options.limits.max_cells = 1;
    assert!(matches!(
        prepare_viewport(
            &workbook,
            0,
            RenderRange::new(0, 0, 0, 0),
            &options,
            ViewportLimits::default()
        ),
        Err(ViewportError::Render(RenderError::LimitExceeded {
            kind: LimitKind::Cells,
            actual: 2,
            ..
        }))
    ));
}

#[test]
fn used_ignores_hostile_dimension_and_jumps_long_default_hidden_gap() {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    for (path, xml) in [
        (
            "[Content_Types].xml",
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="hidden-gap" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><dimension ref="A1:XFD1048576"/><sheetFormatPr defaultRowHeight="15" zeroHeight="1"/><sheetData><row r="2"><c r="A2"><v>1</v></c></row><row r="100001"><c r="A100001"><v>2</v></c></row></sheetData></worksheet>"#,
        ),
    ] {
        writer
            .start_file(path, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(xml.as_bytes()).unwrap();
    }
    let workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
    let options = RenderOptions::default();
    let prepared =
        rxls_render::prepare_used_viewport(&workbook, 0, &options, ViewportLimits::default())
            .unwrap()
            .unwrap();
    assert_eq!(prepared.source_range(), RenderRange::new(1, 0, 100_000, 0));
    assert_eq!(prepared.rows().extent(), Fixed::from_pixels(40));
    let tile = render_viewport_tile(&prepared, rect(0, 0, 64, 40), 114)
        .unwrap()
        .unwrap();
    assert_eq!(tile.report.visible_rows, 2);
    assert_eq!(tile.report.cells_considered, 2);
    assert!(tile.coordinate_visits < 100);
    assert!(text(&tile.scene.nodes, "1").is_some());
    assert!(text(&tile.scene.nodes, "2").is_some());
}

#[test]
fn conditional_scale_retains_values_outside_the_sparse_tile() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("conditional");
    sheet.set_default_row_height(15.0);
    sheet.write_number(0, 0, 0);
    sheet.write_number(50_000, 0, 50);
    sheet.write_number(100_000, 0, 100);
    sheet.add_conditional_format(rxls::CondFormat::new(
        (0, 0, 100_000, 0),
        rxls::CfRule::color_scale2(rxls::Color::rgb(255, 0, 0), rxls::Color::rgb(0, 255, 0)),
    ));
    let range = RenderRange::new(0, 0, 100_000, 999);
    let options = options(range);
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    let (y, height) = prepared.rows().track(50_000).unwrap();
    let (_, width) = prepared.columns().track(0).unwrap();
    let tile = render_viewport_tile(
        &prepared,
        Rect {
            x: Fixed::ZERO,
            y,
            width,
            height,
        },
        115,
    )
    .unwrap()
    .unwrap();
    let SceneNode::ClipGroup(group) = &tile.scene.nodes[0] else {
        panic!("missing tile clip");
    };
    assert!(group.nodes.iter().any(|node| matches!(node,
        SceneNode::Rect(rectangle) if rectangle.fill == Some(rxls_render::Rgb::new(128, 128, 0)))));
}

fn lines(nodes: &[SceneNode]) -> Vec<&rxls_render::LineNode> {
    let mut found = Vec::new();
    for node in nodes {
        match node {
            SceneNode::Line(line) => found.push(line),
            SceneNode::ClipGroup(group) => found.extend(lines(&group.nodes)),
            _ => {}
        }
    }
    found
}

#[test]
fn boundary_halo_keeps_four_sides_and_corner_border_claims() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("halo");
    sheet.set_default_row_height(15.0);
    let thick = rxls::FormatBorder::Thick;
    let colors = [(1, 2, 3), (4, 5, 6), (7, 8, 9), (10, 11, 12), (13, 14, 15)];
    sheet.write_blank_styled(
        1,
        0,
        &CellStyle::new()
            .border_right(thick)
            .border_right_color(rxls::Color::rgb(1, 2, 3)),
    );
    sheet.write_blank_styled(
        1,
        2,
        &CellStyle::new()
            .border_left(thick)
            .border_left_color(rxls::Color::rgb(4, 5, 6)),
    );
    sheet.write_blank_styled(
        0,
        1,
        &CellStyle::new()
            .border_bottom(thick)
            .border_bottom_color(rxls::Color::rgb(7, 8, 9)),
    );
    sheet.write_blank_styled(
        2,
        1,
        &CellStyle::new()
            .border_top(thick)
            .border_top_color(rxls::Color::rgb(10, 11, 12)),
    );
    sheet.write_blank_styled(
        0,
        0,
        &CellStyle::new()
            .border_bottom(thick)
            .border_bottom_color(rxls::Color::rgb(13, 14, 15)),
    );
    let range = RenderRange::new(0, 0, 2, 2);
    let options = options(range);
    let whole = render_sheet_svg(&workbook, 0, &options).unwrap();
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    let (x, width) = prepared.columns().track(1).unwrap();
    let (y, height) = prepared.rows().track(1).unwrap();
    let tile = render_viewport_tile(
        &prepared,
        Rect {
            x,
            y,
            width,
            height,
        },
        116,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (tile.halo_rows, tile.halo_columns, tile.halo_cells),
        (2, 2, 8)
    );
    let actual = lines(&tile.scene.nodes);
    for (r, g, b) in colors {
        let color = rxls_render::Rgb::new(r, g, b);
        let expected = lines(&whole.scene.nodes)
            .into_iter()
            .find(|line| line.color == color)
            .unwrap();
        assert!(actual.iter().any(|line| line.color == color
            && line.width == expected.width
            && line.x1 == subtract(expected.x1, x)
            && line.y1 == subtract(expected.y1, y)
            && line.x2 == subtract(expected.x2, x)
            && line.y2 == subtract(expected.y2, y)));
    }
}

#[test]
fn halo_border_neighbor_jumps_hidden_column_gap() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("halo-hidden");
    sheet.set_default_row_height(15.0);
    for col in 1..10 {
        sheet.hide_column(col);
    }
    sheet.write_blank_styled(
        1,
        0,
        &CellStyle::new()
            .border_right(rxls::FormatBorder::Thick)
            .border_right_color(rxls::Color::rgb(91, 92, 93)),
    );
    let range = RenderRange::new(0, 0, 2, 11);
    let options = options(range);
    let prepared =
        prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
    let (x, width) = prepared.columns().track(10).unwrap();
    let (y, height) = prepared.rows().track(1).unwrap();
    let tile = render_viewport_tile(
        &prepared,
        Rect {
            x,
            y,
            width,
            height,
        },
        117,
    )
    .unwrap()
    .unwrap();
    assert_eq!(tile.report.visible_columns, 1);
    assert_eq!(tile.halo_columns, 2);
    assert!(lines(&tile.scene.nodes).iter().any(|line| line.color
        == rxls_render::Rgb::new(91, 92, 93)
        && line.x1 == Fixed::ZERO
        && line.x2 == Fixed::ZERO
        && line.width == Fixed::from_pixels(3)));
}

#[test]
fn distant_raw_rows_affect_preparation_but_not_visible_tile_query_work() {
    let mut near = Workbook::new();
    near.add_sheet("amortized").set_default_row_height(15.0);
    near.sheets[0].write_number(0, 0, 11);
    let mut far = Workbook::new();
    far.add_sheet("amortized").set_default_row_height(15.0);
    far.sheets[0].write_number(0, 0, 11);
    for row in 5_000..10_000 {
        far.sheets[0].write_number(row, 999, 7);
    }
    let range = RenderRange::new(0, 0, 10_000, 999);
    let options = options(range);
    let near = prepare_viewport(&near, 0, range, &options, ViewportLimits::default()).unwrap();
    let far = prepare_viewport(&far, 0, range, &options, ViewportLimits::default()).unwrap();
    assert_eq!(near.preparation_report().source_raw_cells, 1);
    assert_eq!(far.preparation_report().source_raw_cells, 5_001);
    assert!(far.coordinate_visits() > near.coordinate_visits());
    assert!(
        far.preparation_report().source_index_build_peak_bytes
            > near.preparation_report().source_index_build_peak_bytes
    );
    let request = rect(0, 0, 64, 20);
    let small = render_viewport_tile(&near, request, 200).unwrap().unwrap();
    let large = render_viewport_tile(&far, request, 200).unwrap().unwrap();
    assert_eq!(large.coordinate_visits, small.coordinate_visits);
    assert_eq!(large.geometry_bytes, small.geometry_bytes);
    assert_eq!(large.svg, small.svg);
    assert_eq!(large.scene, small.scene);
    assert_eq!(
        large,
        render_viewport_tile(&far, request, 200).unwrap().unwrap()
    );
}

#[test]
fn populated_hidden_gap_does_not_add_visible_row_query_work() {
    let mut near = Workbook::new();
    let mut far = Workbook::new();
    for workbook in [&mut near, &mut far] {
        let sheet = workbook.add_sheet("hidden-query");
        sheet.set_default_row_height(15.0);
        for row in 1..=5_000 {
            sheet.hide_row(row);
        }
        sheet.write_number(0, 0, 11);
        sheet.write_number(5_001, 0, 22);
    }
    for row in 1..=5_000 {
        far.sheets[0].write_number(row, 0, 7);
    }
    let range = RenderRange::new(0, 0, 5_001, 0);
    let options = options(range);
    let near = prepare_viewport(&near, 0, range, &options, ViewportLimits::default()).unwrap();
    let far = prepare_viewport(&far, 0, range, &options, ViewportLimits::default()).unwrap();
    let request = rect(0, 0, 64, 40);
    let small = render_viewport_tile(&near, request, 202).unwrap().unwrap();
    let large = render_viewport_tile(&far, request, 202).unwrap().unwrap();
    assert_eq!(large.report.visible_rows, 2);
    assert_eq!(large.coordinate_visits, small.coordinate_visits);
    assert_eq!(large.geometry_bytes, small.geometry_bytes);
    assert_eq!(large.svg, small.svg);
}

fn index_bound_hyperlink_fixture() -> Workbook {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (path, xml) in [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="index-bound" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><hyperlinks><hyperlink ref="A1" r:id="hl1"/><hyperlink ref="B1" r:id="hl1"/></hyperlinks></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="hl1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/" TargetMode="External"/></Relationships>"#,
        ),
    ] {
        writer
            .start_file(path, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(xml.as_bytes()).unwrap();
    }
    let mut workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
    assert_eq!(workbook.sheets[0].cells().count(), 1);
    assert_eq!(workbook.sheets[0].hyperlinks().len(), 2);
    workbook.sheets[0].write_blank_styled(
        10,
        10,
        &CellStyle::new().fill(rxls::Color::rgb(10, 20, 30)),
    );
    workbook
}

#[test]
fn raw_and_hyperlink_bounds_precede_index_peak_and_used_blank_style_resolution() {
    let range = RenderRange::new(0, 0, 0, 0);
    let mut options = options(range);
    options.limits.max_cells = 2;
    let limits = ViewportLimits {
        max_geometry_bytes: 0,
        ..ViewportLimits::default()
    };
    let mut raw = Workbook::new();
    let sheet = raw.add_sheet("raw-bound");
    sheet.set_default_row_height(15.0);
    for _ in 0..3 {
        sheet.write_number(0, 0, 1);
    }
    sheet.write_blank_styled(10, 10, &CellStyle::new().fill(rxls::Color::rgb(1, 2, 3)));
    let hyperlinks = index_bound_hyperlink_fixture();
    for workbook in [&raw, &hyperlinks] {
        assert!(matches!(
            prepare_viewport(workbook, 0, range, &options, limits),
            Err(ViewportError::Render(RenderError::LimitExceeded {
                kind: LimitKind::Cells,
                actual: 3,
                ..
            }))
        ));
        assert!(matches!(
            rxls_render::prepare_used_viewport(workbook, 0, &options, limits),
            Err(ViewportError::Render(RenderError::LimitExceeded {
                kind: LimitKind::Cells,
                actual: 3,
                ..
            }))
        ));
    }
    options.limits.max_cells = 3;
    assert!(
        matches!(prepare_viewport(&hyperlinks, 0, range, &options, limits),
        Err(ViewportError::Limit { resource: "geometry_bytes", actual, .. }) if actual > 0)
    );
}

fn tiny_imported_border_fixture(border_style: &str) -> Workbook {
    let styles = format!(
        r##"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:style style:name="TinyRow" style:family="table-row"><style:table-row-properties style:row-height="0.075pt" style:use-optimal-row-height="false"/></style:style><style:style style:name="TinyColumn" style:family="table-column"><style:table-column-properties style:column-width="0.075pt"/></style:style><style:style style:name="Right" style:family="table-cell"><style:table-cell-properties fo:border-right="2.25pt {border_style} #990011"/></style:style><style:style style:name="Bottom" style:family="table-cell"><style:table-cell-properties fo:border-bottom="2.25pt {border_style} #119900"/></style:style></office:styles></office:document-styles>"##
    );
    let mut content = String::from(
        r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="tiny-border"><table:table-column table:style-name="TinyColumn" table:number-columns-repeated="13"/>"#,
    );
    for row in 0..13 {
        content.push_str(r#"<table:table-row table:style-name="TinyRow">"#);
        for col in 0..13 {
            if row == 10 && col == 0 {
                content.push_str(r#"<table:table-cell table:style-name="Right"/>"#);
            } else if row == 0 && col == 10 {
                content.push_str(r#"<table:table-cell table:style-name="Bottom"/>"#);
            } else {
                content.push_str("<table:table-cell/>");
            }
        }
        content.push_str("</table:table-row>");
    }
    content.push_str("</table:table></office:spreadsheet></office:body></office:document-content>");
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (path, bytes) in [
        ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
        ("styles.xml", styles.as_str()),
        ("content.xml", content.as_str()),
    ] {
        writer
            .start_file(path, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes.as_bytes()).unwrap();
    }
    Workbook::open(&writer.finish().unwrap().into_inner()).unwrap()
}

#[test]
fn thick_and_double_border_outset_reaches_beyond_one_narrow_imported_track() {
    for border_style in ["solid", "double"] {
        for rtl in [false, true] {
            let mut workbook = tiny_imported_border_fixture(border_style);
            workbook.sheets[0].set_right_to_left(rtl);
            assert!(workbook.sheets[0].physical_column_widths().contains_key(&0));
            let range = RenderRange::new(0, 0, 12, 12);
            let options = options(range);
            let whole = render_sheet_svg(&workbook, 0, &options).unwrap();
            let prepared =
                prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
            let (logical_x, width) = prepared.columns().track(10).unwrap();
            let x = if rtl {
                subtract(subtract(prepared.columns().extent(), logical_x), width)
            } else {
                logical_x
            };
            let (y, height) = prepared.rows().track(10).unwrap();
            assert!(width > Fixed::ZERO && width < Fixed::from_pixels(1));
            assert!(height > Fixed::ZERO && height < Fixed::from_pixels(1));
            let tile = render_viewport_tile(
                &prepared,
                Rect {
                    x,
                    y,
                    width,
                    height,
                },
                201,
            )
            .unwrap()
            .unwrap();
            assert!(tile.halo_columns > 2);
            assert!(tile.halo_rows > 2);
            assert_eq!((tile.scene.width, tile.scene.height), (width, height));
            let actual = lines(&tile.scene.nodes);
            for color in [
                rxls_render::Rgb::new(153, 0, 17),
                rxls_render::Rgb::new(17, 153, 0),
            ] {
                let expected = lines(&whole.scene.nodes)
                    .into_iter()
                    .filter(|line| line.color == color)
                    .collect::<Vec<_>>();
                assert!(!expected.is_empty());
                for expected in expected {
                    assert!(actual.iter().any(|line| line.color == color
                        && line.width == expected.width
                        && line.x1 == subtract(expected.x1, x)
                        && line.y1 == subtract(expected.y1, y)
                        && line.x2 == subtract(expected.x2, x)
                        && line.y2 == subtract(expected.y2, y)));
                }
            }
        }
    }
}

#[test]
fn owned_preparation_survives_caller_handles_and_reuses_exact_geometry() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("retained");
    sheet.set_default_row_height(15.0);
    sheet.write(0, 0, "retained label");
    sheet.write_number(5_000, 10, 22);
    let workbook = Arc::new(workbook);
    let range = RenderRange::new(0, 0, 5_000, 10);
    let options = Arc::new(options(range));
    let expected = {
        let borrowed =
            prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
        render_viewport_tile(&borrowed, rect(0, 0, 128, 40), 301)
            .unwrap()
            .unwrap()
    };
    let owner = OwnedPreparedViewport::prepare(
        Arc::clone(&workbook),
        0,
        range,
        Arc::clone(&options),
        ViewportLimits::default(),
    )
    .unwrap();
    assert!(Arc::ptr_eq(owner.workbook(), &workbook));
    assert!(Arc::ptr_eq(owner.options(), &options));
    assert_eq!(owner.sheet_index(), 0);
    assert_eq!(Arc::strong_count(&workbook), 2);
    assert_eq!(Arc::strong_count(&options), 2);
    let workbook_weak = Arc::downgrade(&workbook);
    let options_weak = Arc::downgrade(&options);
    let report = owner.preparation_report().clone();
    let rows_pointer = owner.rows().runs().as_ptr();
    let columns_pointer = owner.columns().runs().as_ptr();
    drop(workbook);
    drop(options);
    assert_eq!(workbook_weak.strong_count(), 1);
    assert_eq!(options_weak.strong_count(), 1);
    for _ in 0..3 {
        let facade = owner.as_prepared().unwrap();
        assert_eq!(facade.rows().runs().as_ptr(), rows_pointer);
        assert_eq!(facade.columns().runs().as_ptr(), columns_pointer);
        assert_eq!(facade.preparation_report(), &report);
        assert_eq!(
            render_viewport_tile(&facade, rect(0, 0, 128, 40), 301)
                .unwrap()
                .unwrap(),
            expected
        );
        assert_eq!(workbook_weak.strong_count(), 1);
        assert_eq!(options_weak.strong_count(), 1);
    }
    assert_eq!(owner.preparation_report(), &report);
    drop(owner);
    assert!(workbook_weak.upgrade().is_none());
    assert!(options_weak.upgrade().is_none());
}

#[test]
fn owned_preparation_keeps_its_original_snapshot_after_caller_edits() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("snapshot").set_default_row_height(15.0);
    workbook.sheets[0].write(0, 0, "original");
    let mut workbook = Arc::new(workbook);
    let range = RenderRange::new(0, 0, 0, 1);
    let mut options = Arc::new(options(range));
    let owner = OwnedPreparedViewport::prepare(
        Arc::clone(&workbook),
        0,
        range,
        Arc::clone(&options),
        ViewportLimits::default(),
    )
    .unwrap();
    let before = render_viewport_tile(&owner.as_prepared().unwrap(), rect(0, 0, 128, 20), 302)
        .unwrap()
        .unwrap();
    Arc::make_mut(&mut workbook).sheets[0].write(0, 0, "replacement");
    Arc::make_mut(&mut options).horizontal_padding = Fixed::from_pixels(10);
    assert!(!Arc::ptr_eq(owner.workbook(), &workbook));
    assert!(!Arc::ptr_eq(owner.options(), &options));
    assert_eq!(
        render_viewport_tile(&owner.as_prepared().unwrap(), rect(0, 0, 128, 20), 302)
            .unwrap()
            .unwrap(),
        before
    );
    let replacement =
        OwnedPreparedViewport::prepare(workbook, 0, range, options, ViewportLimits::default())
            .unwrap();
    let after = render_viewport_tile(
        &replacement.as_prepared().unwrap(),
        rect(0, 0, 128, 20),
        302,
    )
    .unwrap()
    .unwrap();
    assert!(text(&before.scene.nodes, "original").is_some());
    assert!(text(&after.scene.nodes, "replacement").is_some());
    assert_ne!(before.svg, after.svg);
}

#[test]
fn owned_used_empty_and_invalid_sheet_publish_no_retained_owner() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("empty").set_default_row_height(15.0);
    workbook.sheets[0].write_blank_styled(9, 9, &CellStyle::new().bold());
    let workbook = Arc::new(workbook);
    let options = Arc::new(RenderOptions::default());
    assert!(OwnedPreparedViewport::prepare_used(
        Arc::clone(&workbook),
        0,
        Arc::clone(&options),
        ViewportLimits::default(),
    )
    .unwrap()
    .is_none());
    let expected = ViewportError::Render(RenderError::SheetIndexOutOfRange {
        requested: 1,
        sheet_count: 1,
    });
    assert_eq!(
        OwnedPreparedViewport::prepare(
            Arc::clone(&workbook),
            1,
            RenderRange::new(0, 0, 0, 0),
            Arc::clone(&options),
            ViewportLimits::default(),
        )
        .unwrap_err(),
        expected
    );
    assert_eq!(
        OwnedPreparedViewport::prepare_used(
            Arc::clone(&workbook),
            1,
            Arc::clone(&options),
            ViewportLimits::default(),
        )
        .unwrap_err(),
        expected
    );
    assert_eq!(Arc::strong_count(&workbook), 1);
    assert_eq!(Arc::strong_count(&options), 1);
}

#[test]
fn failed_owned_factory_preserves_prior_owner_and_input_arc_counts() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("atomic").set_default_row_height(15.0);
    workbook.sheets[0].write_number(0, 0, 77);
    let workbook = Arc::new(workbook);
    let range = RenderRange::new(0, 0, 0, 0);
    let options = Arc::new(options(range));
    let owner = OwnedPreparedViewport::prepare(
        Arc::clone(&workbook),
        0,
        range,
        Arc::clone(&options),
        ViewportLimits::default(),
    )
    .unwrap();
    let before = render_viewport_tile(&owner.as_prepared().unwrap(), rect(0, 0, 64, 20), 303)
        .unwrap()
        .unwrap();
    let report = owner.preparation_report().clone();
    let limits = ViewportLimits {
        max_coordinate_visits: 0,
        ..ViewportLimits::default()
    };
    let expected = prepare_viewport(&workbook, 0, range, &options, limits).unwrap_err();
    assert_eq!(
        OwnedPreparedViewport::prepare(
            Arc::clone(&workbook),
            0,
            range,
            Arc::clone(&options),
            limits,
        )
        .unwrap_err(),
        expected
    );
    assert_eq!(OwnedPreparedViewport::prepare_used(
        Arc::clone(&workbook), 0, Arc::clone(&options), limits,
    ).unwrap_err(), expected);
    assert_eq!(Arc::strong_count(&workbook), 2);
    assert_eq!(Arc::strong_count(&options), 2);
    assert_eq!(owner.preparation_report(), &report);
    assert_eq!(
        render_viewport_tile(&owner.as_prepared().unwrap(), rect(0, 0, 64, 20), 303)
            .unwrap()
            .unwrap(),
        before
    );
}

#[test]
fn owned_used_factory_matches_borrowed_extent_report_and_tile() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("used-owner");
    sheet.set_default_row_height(15.0);
    sheet.write(2, 3, "merge anchor");
    sheet.merge(2, 3, 4, 5);
    sheet.write_blank_styled(7, 8, &CellStyle::new().fill(rxls::Color::rgb(7, 8, 9)));
    sheet.hide_row(7);
    let workbook = Arc::new(workbook);
    let options = Arc::new(RenderOptions {
        selection: RenderSelection::Used,
        ..RenderOptions::default()
    });
    let borrowed =
        rxls_render::prepare_used_viewport(&workbook, 0, &options, ViewportLimits::default())
            .unwrap()
            .unwrap();
    let owner = OwnedPreparedViewport::prepare_used(
        Arc::clone(&workbook),
        0,
        Arc::clone(&options),
        ViewportLimits::default(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(owner.source_range(), borrowed.source_range());
    assert_eq!(owner.preparation_report(), borrowed.preparation_report());
    assert_eq!(owner.rows(), borrowed.rows());
    assert_eq!(owner.columns(), borrowed.columns());
    assert_eq!(
        render_viewport_tile(&owner.as_prepared().unwrap(), rect(0, 0, 128, 60), 304)
            .unwrap()
            .unwrap(),
        render_viewport_tile(&borrowed, rect(0, 0, 128, 60), 304)
            .unwrap()
            .unwrap()
    );
}
