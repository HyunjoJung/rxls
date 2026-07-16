use rxls::{Border, BorderStyle, CellStyle, Chart, ChartKind, Color, Format, Sparkline, Workbook};
use rxls_render::{
    build_print_document, build_scene, Fixed, LimitKind, PrintOptions, RenderError, RenderLimits,
    RenderOptions, RenderRange, RenderSelection,
};

#[test]
fn used_range_retains_only_visible_blank_paint() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("blank paint");
    sheet.write_blank_styled(40, 40, &CellStyle::new().font_name("Invisible Font"));
    sheet.write_blank_styled(2, 3, &CellStyle::new().fill(Color::rgb(1, 2, 3)));
    sheet.write_blank_styled(
        8,
        9,
        &CellStyle::new().border(Border::new().with_bottom(BorderStyle::Thin)),
    );
    sheet.set_row_format(11, &Format::new().fill(Color::rgb(4, 5, 6)));
    sheet.write_blank_styled(11, 12, &CellStyle::new());

    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert_eq!(build.report.range, RenderRange::new(2, 3, 11, 12));
    assert_eq!(build.scene.width, Fixed::from_pixels(640));
    assert_eq!(build.scene.height, Fixed::from_pixels(200));
}

#[test]
fn used_range_activates_only_merges_that_intersect_retained_cell_content() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("merges");
    sheet.write(0, 0, "first");
    sheet.write(5, 5, "merged anchor");
    sheet.write(10, 10, "last");
    sheet.merge(2, 2, 4, 4); // Detached, despite lying inside the used rectangle.
    sheet.merge(5, 5, 7, 8); // Activated by its value-bearing anchor.
    sheet.merge(100, 1, 205, 13); // Detached and unable to expand the canvas.

    let options = RenderOptions {
        gridlines: false,
        ..RenderOptions::default()
    };
    let first = build_scene(&workbook, 0, &options).unwrap();
    let second = build_scene(&workbook, 0, &options).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.report.range, RenderRange::new(0, 0, 10, 10));
    assert_eq!(first.report.merged_regions, 1);
    assert_eq!(first.scene.width, Fixed::from_pixels(704));
    assert_eq!(first.scene.height, Fixed::from_pixels(220));

    let mut painted_merge = Workbook::new();
    let sheet = painted_merge.add_sheet("painted merge");
    sheet.write_blank_styled(3, 4, &CellStyle::new().fill(Color::rgb(7, 8, 9)));
    sheet.merge(3, 4, 6, 7);
    let build = build_scene(&painted_merge, 0, &options).unwrap();
    assert_eq!(build.report.range, RenderRange::new(3, 4, 6, 7));
    assert_eq!(build.report.merged_regions, 1);
}

#[test]
fn used_range_includes_chart_and_sparkline_anchors_without_cells() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("drawings");
    sheet.add_chart(Chart::new(ChartKind::Line, (10, 2), (20, 8)));
    sheet.add_sparkline(Sparkline::new((3, 1), "A1:A2"));

    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert_eq!(build.report.range, RenderRange::new(3, 1, 20, 8));
    assert_eq!(build.scene.width, Fixed::from_pixels(512));
    assert_eq!(build.scene.height, Fixed::from_pixels(360));
}

#[test]
fn empty_used_selection_is_one_pixel_but_explicit_ranges_are_unchanged() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("empty");
    sheet.write_blank_styled(300, 20, &CellStyle::new().font_name("metadata only"));
    sheet.merge(100, 2, 200, 9);

    let used = RenderOptions::default();
    let expected = build_scene(&workbook, 0, &used).unwrap();
    assert_eq!(expected.scene.width, Fixed::from_pixels(1));
    assert_eq!(expected.scene.height, Fixed::from_pixels(1));
    assert!(expected.scene.nodes.is_empty());
    assert_eq!(expected.report.range, RenderRange::new(0, 0, 0, 0));
    assert_eq!(expected.report.rows_considered, 1);
    assert_eq!(expected.report.columns_considered, 1);
    assert_eq!(expected.report.cells_considered, 1);
    assert_eq!(expected.report.rendered_regions, 0);
    for _ in 0..16 {
        assert_eq!(build_scene(&workbook, 0, &used).unwrap(), expected);
    }

    let exact_limit = RenderOptions {
        limits: RenderLimits {
            max_dimension_raw: 1_024,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    };
    assert_eq!(build_scene(&workbook, 0, &exact_limit).unwrap(), expected);
    let too_small = RenderOptions {
        limits: RenderLimits {
            max_dimension_raw: 1_023,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    };
    assert_eq!(
        build_scene(&workbook, 0, &too_small),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Dimension,
            limit: 1_023,
            actual: 1_024,
        })
    );

    let explicit = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        ..RenderOptions::default()
    };
    let explicit_build = build_scene(&workbook, 0, &explicit).unwrap();
    assert_eq!(explicit_build.scene.width, Fixed::from_pixels(64));
    assert_eq!(explicit_build.scene.height, Fixed::from_pixels(20));
    assert_eq!(explicit_build.report.rendered_regions, 1);

    let print = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(print.pages.len(), 1);
    assert_eq!(print.pages[0].scene.width, Fixed::from_pixels(1));
    assert_eq!(print.pages[0].scene.height, Fixed::from_pixels(1));

    let explicit_print = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            render: explicit,
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(explicit_print.pages[0].scene.width, Fixed::from_pixels(64));
    assert_eq!(explicit_print.pages[0].scene.height, Fixed::from_pixels(20));
}

#[test]
fn empty_used_selection_honors_only_a_valid_sheet_default_width() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("default width");
    sheet.set_default_col_width(10.0);
    sheet.set_col_width(0, 20.0);

    let expected = build_scene(&workbook, 0, &RenderOptions::default()).unwrap();
    assert_eq!(expected.scene.width, Fixed::from_pixels(72));
    assert_eq!(expected.scene.height, Fixed::from_pixels(1));
    assert!(expected.scene.nodes.is_empty());
    for _ in 0..16 {
        assert_eq!(
            build_scene(&workbook, 0, &RenderOptions::default()).unwrap(),
            expected
        );
    }

    let too_small = RenderOptions {
        limits: RenderLimits {
            max_dimension_raw: 72 * 1_024 - 1,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    };
    assert_eq!(
        build_scene(&workbook, 0, &too_small),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Dimension,
            limit: 72 * 1_024 - 1,
            actual: 72 * 1_024,
        })
    );

    let print = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(print.pages[0].scene.width, Fixed::from_pixels(72));
    assert_eq!(print.pages[0].scene.height, Fixed::from_pixels(1));

    let mut explicit_only = Workbook::new();
    explicit_only
        .add_sheet("explicit width only")
        .set_col_width(0, 20.0);
    let used = build_scene(&explicit_only, 0, &RenderOptions::default()).unwrap();
    assert_eq!(used.scene.width, Fixed::from_pixels(1));
    assert_eq!(used.scene.height, Fixed::from_pixels(1));
    let explicit = build_scene(
        &explicit_only,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert_eq!(explicit.scene.width, Fixed::from_pixels(142));
    assert_eq!(explicit.scene.height, Fixed::from_pixels(20));

    let mut invalid_default = Workbook::new();
    let sheet = invalid_default.add_sheet("invalid default");
    sheet.set_default_col_width(0.0);
    sheet.set_col_width(0, 20.0);
    let invalid = build_scene(&invalid_default, 0, &RenderOptions::default()).unwrap();
    assert_eq!(invalid.scene.width, Fixed::from_pixels(1));
    assert_eq!(invalid.scene.height, Fixed::from_pixels(1));
    assert_eq!(
        invalid.report.warnings[0].code,
        rxls_render::WarningCode::InvalidGeometryFallback
    );

    let mut full_grid_declarations = Workbook::new();
    let sheet = full_grid_declarations.add_sheet("BIFF full grid");
    sheet.set_default_col_width(10.0);
    for col in 0_u16..256 {
        sheet.set_col_width(col, 20.0);
    }
    let full_grid = build_scene(&full_grid_declarations, 0, &RenderOptions::default()).unwrap();
    assert_eq!(full_grid.scene.width, Fixed::from_pixels(1));
    assert_eq!(full_grid.scene.height, Fixed::from_pixels(1));

    let mut partial_grid_declarations = Workbook::new();
    let sheet = partial_grid_declarations.add_sheet("partial grid");
    sheet.set_default_col_width(10.0);
    for col in 0_u16..255 {
        sheet.set_col_width(col, 20.0);
    }
    let partial_grid =
        build_scene(&partial_grid_declarations, 0, &RenderOptions::default()).unwrap();
    assert_eq!(partial_grid.scene.width, Fixed::from_pixels(72));
    assert_eq!(partial_grid.scene.height, Fixed::from_pixels(1));
}
