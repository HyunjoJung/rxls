use super::*;

#[test]
fn authored_charts_do_not_gain_imported_font_provenance() {
    let mut sheet = Sheet::new("authored");
    sheet.add_chart(Chart::new(ChartKind::Line, (0, 0), (10, 5)));

    assert_eq!(sheet.charts().len(), 1);
    assert!(sheet.drawing_metadata().is_empty());
    assert!(DrawingMetadata::default()
        .chart_default_latin_font_family
        .is_none());
}

#[test]
fn decimal_ratio_parser_preserves_normal_and_scientific_values_exactly() {
    assert_eq!(parse_decimal_ratio_u64("8.43"), Some((843, 100)));
    assert_eq!(parse_decimal_ratio_u64("  +1.2345E1  "), Some((2469, 200)));
    assert_eq!(parse_decimal_ratio_u64(".5e1"), Some((5, 1)));
    assert_eq!(parse_decimal_ratio_u64("00010.00"), Some((10, 1)));
    assert_eq!(parse_decimal_ratio_u64("0.000"), Some((0, 1)));
    assert_eq!(
        parse_decimal_ratio_u64("18446744073709551615"),
        Some((u64::MAX, 1))
    );
}

#[test]
fn decimal_ratio_parser_rejects_invalid_or_unrepresentable_values() {
    for value in [
        "",
        " ",
        "+",
        "-1",
        ".",
        "1.2.3",
        "1e",
        "1e2e3",
        "NaN",
        "inf",
        "18446744073709551616",
        "1e-20",
        "1e2147483648",
    ] {
        assert_eq!(
            parse_decimal_ratio_u64(value),
            None,
            "unexpected exact ratio for {value:?}"
        );
    }
}

#[test]
fn decimal_scaled_parser_requires_an_integral_scaled_result() {
    assert_eq!(parse_decimal_scaled_u32("12.85", 20), Some(257));
    assert_eq!(parse_decimal_scaled_u32("1.2345e2", 20), Some(2_469));
    assert_eq!(parse_decimal_scaled_u32("8.43", 256), None);
    assert_eq!(parse_decimal_scaled_u32("4294967295", 1), Some(u32::MAX));
    assert_eq!(parse_decimal_scaled_u32("4294967296", 1), None);
}

#[test]
fn authored_row_height_is_always_a_manual_override() {
    let mut sheet = Sheet::new("manual heights");
    sheet.automatic_row_height_candidates.insert(2);

    assert!(!sheet.row_height_is_manual(2));
    sheet.set_row_height(2, 12.0);

    assert!(sheet.row_height_is_manual(2));
    assert!(!sheet.automatic_row_height_candidates.contains(&2));
    assert!(!sheet.row_height_is_manual(3));
}

#[test]
fn authored_default_row_height_is_always_a_manual_override() {
    let mut sheet = Sheet::new("manual default height");
    sheet.default_row_height = Some(15.0);
    sheet.automatic_default_row_height_candidate = true;

    assert!(!sheet.default_row_height_is_manual());
    sheet.set_default_row_height(18.0);

    assert!(sheet.default_row_height_is_manual());
    assert!(!sheet.automatic_default_row_height_candidate);
}

#[test]
fn authored_row_heights_invalidate_default_hidden_source_state() {
    let mut sheet = Sheet::new("hidden defaults");
    sheet.default_hidden_row_exceptions = Some(BTreeSet::from([1]));

    sheet.set_row_height(2, 12.0);
    assert_eq!(
        sheet.default_hidden_row_exceptions(),
        Some(&BTreeSet::from([1, 2]))
    );

    sheet.hide_row(3);
    sheet.set_row_height(3, 12.0);
    assert_eq!(
        sheet.default_hidden_row_exceptions(),
        Some(&BTreeSet::from([1, 2])),
        "an explicitly hidden row must remain hidden"
    );

    sheet.set_default_row_height(14.0);
    assert_eq!(sheet.default_hidden_row_exceptions(), None);
}

#[test]
fn render_style_range_sweep_is_exact_deduplicated_and_coordinate_ordered() {
    let mut sheet = Sheet::new("style identity");
    for row in 0..128_u32 {
        for col in 0..64_u16 {
            sheet
                .direct_cell_formats
                .insert((row, col), CellStyleOverlay::default());
        }
    }
    let mut ranges = Vec::<RenderStyleRange>::new();
    for index in 0..1_024_u32 {
        let first_row = index.wrapping_mul(37) % 120;
        let first_col = (index.wrapping_mul(53) % 60) as u16;
        ranges.push((
            first_row,
            first_col,
            (first_row + index % 11).min(127),
            (first_col + (index % 7) as u16).min(63),
        ));
    }
    ranges.extend([(10, 10, 40, 40), (10, 10, 40, 40), (9, 2, 8, 3)]);

    let expected = sheet
        .direct_cell_formats
        .keys()
        .copied()
        .filter(|&(row, col)| {
            ranges.iter().any(|range| {
                range.0 <= range.2
                    && range.1 <= range.3
                    && row >= range.0
                    && row <= range.2
                    && col >= range.1
                    && col <= range.3
            })
        })
        .collect::<Vec<_>>();
    let index = RenderStyleRangeIndex::new(&ranges);
    let mut actual = Vec::new();
    let stats = index
        .for_each_relevant_direct_cell_format(&sheet.direct_cell_formats, |row, col, _| {
            actual.push((row, col))
        });

    assert_eq!(actual, expected);
    assert!(actual.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(stats.selected_entries, expected.len() as u64);
    assert_eq!(
        sheet.render_style_sidecar_entry_count(&ranges),
        expected.len() as u64
    );
}

#[test]
fn render_style_sidecar_v2_is_union_canonical_in_global_coordinate_order() {
    let mut sheet = Sheet::new("canonical sidecar");
    for row in 0..32_u32 {
        for col in 0..40_u16 {
            sheet.direct_cell_formats.insert(
                (row, col),
                CellStyleOverlay {
                    style: CellStyle::new().background_color([
                        row as u8,
                        col as u8,
                        row.wrapping_add(u32::from(col)) as u8,
                    ]),
                    replace_fill: true,
                    ..CellStyleOverlay::default()
                },
            );
        }
    }
    let one_rectangle = [(10, 20, 19, 29)];
    let permuted_overlapping_union = [
        (12, 22, 17, 27),
        (10, 25, 19, 29),
        (10, 20, 19, 24),
        (10, 20, 19, 24),
    ];

    assert_eq!(
        sheet.render_style_sidecar_entry_count(&one_rectangle),
        sheet.render_style_sidecar_entry_count(&permuted_overlapping_union)
    );
    assert_eq!(
        format!("{:?}", sheet.render_style_sidecar_identity(&one_rectangle)),
        format!(
            "{:?}",
            sheet.render_style_sidecar_identity(&permuted_overlapping_union)
        ),
        "v2 identity must depend on the selected coordinate union, not range order or overlap"
    );
}

#[test]
fn render_style_range_sweep_visits_each_of_250k_overlays_once() {
    const FORMAT_COUNT: u32 = 250_000;
    const RANGE_COUNT: u32 = 1_024;

    let mut formats = BTreeMap::new();
    for row in 0..FORMAT_COUNT {
        formats.insert((row, u16::MAX), CellStyleOverlay::default());
    }
    let ranges = (0..RANGE_COUNT)
        .map(|index| {
            let first_row = index.wrapping_mul(193) % 200_000;
            (
                first_row,
                (index % 127) as u16,
                (first_row + 40_000 + index % 101).min(FORMAT_COUNT - 1),
                255,
            )
        })
        .collect::<Vec<_>>();
    let index = RenderStyleRangeIndex::new(&ranges);
    let (selected, stats) = index.relevant_direct_cell_format_count(&formats);

    assert_eq!(selected, 0);
    assert_eq!(stats.direct_entries_visited, u64::from(FORMAT_COUNT));
    assert_eq!(stats.membership_queries, u64::from(FORMAT_COUNT));
    assert_eq!(stats.selected_entries, 0);
    assert!(
        stats.row_events_applied <= u64::from(RANGE_COUNT) * 2,
        "row events must be processed once, not once per direct format"
    );
}

#[test]
fn formatted_returns_display_text_while_cell_returns_typed_value() {
    let mut s = Sheet::new("s");
    s.write(0, 0, "공고명");
    s.write(0, 1, 1000_i64);
    s.write(0, 2, 0.5);
    s.write(0, 3, true);

    // formatted() yields the rendered display string used by to_text()...
    assert_eq!(s.formatted(0, 0), Some("공고명"));
    assert_eq!(s.formatted(0, 1), Some("1000"));
    assert_eq!(s.formatted(0, 2), Some("0.5"));
    assert_eq!(s.formatted(0, 3), Some("TRUE"));

    // ...while cell() yields the typed value, not a string.
    assert_eq!(s.cell(0, 0), Some(&Cell::Text("공고명".to_string())));
    assert_eq!(s.cell(0, 1), Some(&Cell::Number(1000.0)));
    assert_eq!(s.cell(0, 2), Some(&Cell::Number(0.5)));
    assert_eq!(s.cell(0, 3), Some(&Cell::Bool(true)));

    // An empty cell has no display text.
    assert_eq!(s.formatted(9, 9), None);
}

#[test]
fn formatted_is_last_write_wins_like_cell() {
    let mut s = Sheet::new("s");
    s.write(0, 0, 1_i64);
    s.write(0, 0, 2_i64);
    assert_eq!(s.formatted(0, 0), Some("2"));
    assert_eq!(s.cell(0, 0), Some(&Cell::Number(2.0)));
}

#[test]
fn authored_number_formats_drive_retained_display_text() {
    let mut sheet = Sheet::new("formatted");
    sheet.write_with_format(
        0,
        0,
        1_234.5,
        &Format::new().set_num_format("[$₩-412]#,##0.00"),
    );
    sheet.write_with_format(
        0,
        1,
        -2.0,
        &Format::new().set_num_format("0;[Red](0);\"없음\";\"값: \"@"),
    );
    sheet.write_with_format(
        0,
        2,
        "한글",
        &Format::new().set_num_format("0;[Red](0);0;\"값: \"@"),
    );
    sheet.write_datetime_with_format(
        0,
        3,
        45_366.0,
        &Format::new().set_num_format("yyyy\"년\" m\"월\" d\"일\""),
    );

    assert_eq!(sheet.formatted(0, 0), Some("₩1,234.50"));
    assert_eq!(sheet.formatted(0, 1), Some("(2)"));
    assert_eq!(sheet.formatted(0, 2), Some("값: 한글"));
    assert_eq!(sheet.formatted(0, 3), Some("2024년 3월 15일"));
}

#[test]
fn inherited_authored_number_format_precedence_refreshes_existing_cells() {
    let mut sheet = Sheet::new("inherited");
    sheet.write(2, 3, 1.25);
    sheet.set_default_format(&Format::new().set_num_format("0.0"));
    assert_eq!(sheet.formatted(2, 3), Some("1.3"));

    sheet.set_col_format(3, &Format::new().set_num_format("0.00"));
    assert_eq!(sheet.formatted(2, 3), Some("1.25"));

    sheet.set_row_format(2, &Format::new().set_num_format("0.000"));
    assert_eq!(sheet.formatted(2, 3), Some("1.250"));

    sheet.write_with_format(2, 3, 0.5, &Format::new().set_num_format("0.0%"));
    assert_eq!(sheet.formatted(2, 3), Some("50.0%"));
}

#[test]
fn workbook_created_authored_sheets_inherit_the_1904_display_epoch() {
    let mut workbook = Workbook::new();
    workbook.date1904 = true;
    let sheet = workbook.add_sheet("mac-date");
    sheet.write_datetime_with_format(0, 0, 0.0, &Format::new().set_num_format("yyyy-mm-dd"));
    assert_eq!(sheet.formatted(0, 0), Some("1904-01-01"));
}

#[test]
fn display_cells_are_ordered_deduplicated_and_carry_render_metadata() {
    let mut sheet = Sheet::new("render");
    sheet.write(1, 1, "old");
    sheet.write_url_with_text_and_format(
        1,
        1,
        "https://example.com",
        "새 값",
        &Format::new().bold(),
    );
    sheet.write(0, 2, 3_i64);

    let cells = sheet.display_cells().collect::<Vec<_>>();
    assert_eq!(cells.len(), 2);
    assert_eq!(
        (cells[0].row, cells[0].col, cells[0].formatted),
        (0, 2, "3")
    );
    assert_eq!((cells[1].row, cells[1].col), (1, 1));
    assert_eq!(cells[1].formatted, "새 값");
    assert!(cells[1]
        .explicit_style
        .and_then(|style| style.font.as_ref())
        .is_some_and(|font| font.bold));
    assert_eq!(cells[1].hyperlink, Some("https://example.com"));
}

#[test]
fn display_cell_views_preserve_lww_ordering_and_authoring_invalidation() {
    let mut sheet = Sheet::new("indexed");
    sheet.write(0, 9, "outside-left-row");
    sheet.write(1, 1, "old");
    sheet.write(1, 1, "inside");
    sheet.write(1, 9, "outside-middle-row");
    sheet.write(2, 2, "inside-too");
    sheet.write(3, 0, "outside-right-row");

    // Whole-sheet access initializes the lazy point/display index.
    assert_eq!(sheet.display_cells().count(), 5);
    assert!(sheet.display_cell_index.get().is_some());
    let cells = sheet
        .display_cells_in_range(1, 1, 2, 2)
        .map(|cell| (cell.row, cell.col, cell.formatted.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        cells,
        vec![
            (1, 1, "inside".to_string()),
            (2, 2, "inside-too".to_string())
        ]
    );
    // A later write must invalidate the whole-sheet index, replace the
    // effective coordinate, and introduce new coordinates.
    sheet.write(1, 1, "new");
    sheet.write(2, 1, "new-coordinate");
    assert!(sheet.display_cell_index.get().is_none());
    let cells = sheet
        .display_cells_in_range(1, 1, 2, 2)
        .map(|cell| (cell.row, cell.col, cell.formatted.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        cells,
        vec![
            (1, 1, "new".to_string()),
            (2, 1, "new-coordinate".to_string()),
            (2, 2, "inside-too".to_string())
        ]
    );
    assert!(sheet.display_cells_in_range(2, 2, 1, 1).next().is_none());
    assert_eq!(
        sheet
            .display_cells_in_range(1, 9, 1, 9)
            .map(|cell| cell.formatted)
            .collect::<Vec<_>>(),
        ["outside-middle-row"]
    );
}

#[test]
fn compact_display_index_bounds_large_sparse_range_and_point_queries() {
    const UNRELATED_CELL_COUNT: u32 = 100_000;

    let mut sheet = Sheet::new("bounded-range");
    sheet.write(0, 0, "old");
    sheet.write(0, 0, "selected");
    sheet
        .read_hyperlinks
        .push((0, 0, "https://example.com/old".to_string()));
    sheet
        .read_hyperlinks
        .push((0, 0, "https://example.com/selected".to_string()));
    sheet.cells.reserve(UNRELATED_CELL_COUNT as usize);
    for row in 1..=UNRELATED_CELL_COUNT {
        sheet.cells.push(CellEntry {
            row,
            col: 1,
            value: Cell::Number(f64::from(row)),
            text: row.to_string(),
            style: None,
            xlsx_font_size_pt: None,
            hyperlink: None,
        });
    }

    assert!(sheet.display_cell_index.get().is_none());
    let selected = sheet.display_cells_in_range(0, 0, 0, 0).collect::<Vec<_>>();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].formatted, "selected");
    assert_eq!(selected[0].hyperlink, Some("https://example.com/selected"));
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("selected".to_string())));

    let index = sheet
        .display_cell_index
        .get()
        .expect("range traversal initializes the compact shared index");
    assert_eq!(index.cells.len(), UNRELATED_CELL_COUNT as usize + 1);
    assert_eq!(index.cells.capacity(), index.cells.len());
    assert_eq!(index.read_hyperlinks.len(), 1);
    assert_eq!(
        std::mem::size_of::<DisplayCellIndexEntry>(),
        std::mem::size_of::<u64>() + std::mem::size_of::<usize>()
    );
    assert!(
        std::mem::size_of_val(index.cells.as_slice()) <= (UNRELATED_CELL_COUNT as usize + 1) * 16
    );

    // Repeated point/range access now performs binary search over this
    // compact vector rather than rescanning all unrelated source records.
    for _ in 0..1_000 {
        assert_eq!(
            sheet
                .display_cells_in_range(0, 0, 0, 0)
                .map(|cell| cell.formatted)
                .collect::<Vec<_>>(),
            ["selected"]
        );
        assert_eq!(sheet.formatted(0, 0), Some("selected"));
    }
}

#[test]
fn resolved_style_and_visual_dimensions_match_writer_precedence() {
    let mut sheet = Sheet::new("render");
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Authored);
    assert_eq!(
        Sheet::default().style_fidelity(),
        StyleFidelity::Unavailable
    );
    sheet.set_default_format(&Format::new().background_color([1, 2, 3]));
    sheet.set_col_format(2, &Format::new().bold());
    sheet.set_row_format(3, &Format::new().italic());
    sheet.write_with_format(3, 2, "value", &Format::new().color([4, 5, 6]));
    sheet.write_blank_with_format(8, 9, &Format::new().underline());
    sheet.merge(10, 10, 11, 12);
    sheet.set_default_row_height(18.0);
    sheet.set_default_col_width(9.5);

    let style = sheet.resolved_cell_style(3, 2).expect("resolved style");
    let font = style.font.expect("merged font");
    assert!(font.bold);
    assert!(font.italic);
    assert_eq!(font.color, Some(Color::rgb(4, 5, 6)));
    assert_eq!(style.fill, Some(Color::rgb(1, 2, 3)));
    assert_eq!(sheet.default_row_height(), Some(18.0));
    assert!(!sheet.has_implicit_ooxml_row_height());
    assert_eq!(sheet.default_column_width(), Some(9.5));
    assert_eq!(sheet.visual_dimensions(), Some((3, 2, 11, 12)));
}

#[test]
fn authored_default_column_width_clears_imported_ooxml_provenance() {
    let mut sheet = Sheet::new("geometry");
    sheet.ooxml_implicit_col_width = OoxmlImplicitColumnWidth::ApplicationDefault;
    sheet.ooxml_defaulted_base_col_width = true;
    sheet.imported_default_column_axis_measure =
        Some(ImportedAxisMeasure::DigitBaseWidth256(8 * 256));

    sheet.set_default_col_width(9.5);

    assert_eq!(sheet.default_column_width(), Some(9.5));
    assert_eq!(sheet.implicit_ooxml_column_width(), None);
    assert!(!sheet.ooxml_uses_defaulted_base_column_width());
    assert_eq!(sheet.imported_default_column_axis_measure(), None);
}

#[test]
fn table_regions_merge_in_fixed_order_between_inherited_and_direct_styles() {
    let mut sheet = Sheet::new("table-cascade");
    sheet.set_default_format(&Format::new().set_num_format("0.0"));
    sheet.set_col_format(0, &Format::new().bold());
    sheet.set_row_format(2, &Format::new().italic());
    sheet.tables.push(Table::new(
        (1, 0, 6, 2),
        "Sales",
        ["first", "middle", "last"],
    ));

    let mut definition = TableStyleDefinition::default();
    definition.insert(
        TableStyleRegion::WholeTable,
        CellStyle::new().underline(),
        1,
    );
    definition.insert(
        TableStyleRegion::FirstColumnStripe,
        CellStyle::new().background_color([20, 20, 20]),
        1,
    );
    definition.insert(
        TableStyleRegion::FirstRowStripe,
        CellStyle::new().background_color([30, 30, 30]),
        2,
    );
    definition.insert(
        TableStyleRegion::SecondRowStripe,
        CellStyle::new().background_color([40, 40, 40]),
        1,
    );
    definition.insert(
        TableStyleRegion::FirstColumn,
        CellStyle::new().color([50, 50, 50]),
        1,
    );
    definition.insert(
        TableStyleRegion::LastColumn,
        CellStyle::new().strikethrough(),
        1,
    );
    definition.insert(
        TableStyleRegion::HeaderRow,
        CellStyle::new().background_color([60, 60, 60]),
        1,
    );
    definition.insert(
        TableStyleRegion::TotalRow,
        CellStyle::new().background_color([70, 70, 70]),
        1,
    );
    definition.insert(
        TableStyleRegion::FirstHeaderCell,
        CellStyle::new().color([80, 80, 80]),
        1,
    );
    definition.insert(
        TableStyleRegion::LastTotalCell,
        CellStyle::new().color([90, 90, 90]),
        1,
    );
    sheet.table_region_formats.insert(
        "Sales".to_string(),
        TableStyleApplication {
            definition,
            header_row: true,
            totals_row: true,
            show_first_column: true,
            show_last_column: true,
            show_row_stripes: true,
            show_column_stripes: true,
        },
    );
    sheet.write_with_format(
        2,
        0,
        1.25,
        &Format::new()
            .background_color([100, 100, 100])
            .color([110, 110, 110]),
    );

    let header = sheet.resolved_cell_style(1, 0).expect("header style");
    assert_eq!(header.fill, Some(Color::rgb(60, 60, 60)));
    assert_eq!(
        header.font.as_ref().and_then(|font| font.color),
        Some(Color::rgb(80, 80, 80))
    );

    let direct = sheet.resolved_cell_style(2, 0).expect("direct style");
    assert_eq!(direct.fill, Some(Color::rgb(100, 100, 100)));
    let font = direct.font.as_ref().expect("merged font");
    assert!(font.bold);
    assert!(font.italic);
    assert!(font.underline);
    assert_eq!(font.color, Some(Color::rgb(110, 110, 110)));
    assert_eq!(direct.num_fmt.as_deref(), Some("0.0"));

    assert_eq!(
        sheet.resolved_cell_style(3, 1).and_then(|style| style.fill),
        Some(Color::rgb(30, 30, 30)),
        "two-row first stripe must cover the second body row"
    );
    assert_eq!(
        sheet.resolved_cell_style(4, 1).and_then(|style| style.fill),
        Some(Color::rgb(40, 40, 40)),
        "the second stripe follows the first stripe's declared size"
    );
    let total = sheet.resolved_cell_style(6, 2).expect("total style");
    assert_eq!(total.fill, Some(Color::rgb(70, 70, 70)));
    assert_eq!(
        total.font.as_ref().and_then(|font| font.color),
        Some(Color::rgb(90, 90, 90))
    );
    assert!(total.font.as_ref().is_some_and(|font| font.strikethrough));

    assert_eq!(
        sheet.resolved_cell_style(3, 1),
        sheet.resolved_cell_style(3, 1),
        "resolution must not depend on map iteration order"
    );
}

#[test]
fn xlsx_cell_font_provenance_does_not_cross_inherited_font_layers() {
    fn source_sheet() -> Sheet {
        let style = CellStyle::new().font_name("Source").set_font_size(14);
        let mut sheet = Sheet::new("provenance");
        sheet.default_format = Some(style.clone());
        sheet.cells.push(CellEntry {
            row: 0,
            col: 0,
            value: Cell::Text("value".to_string()),
            text: "value".to_string(),
            style: Some(style),
            xlsx_font_size_pt: Some(14),
            hyperlink: None,
        });
        sheet
    }

    let unshadowed = source_sheet();
    assert_eq!(unshadowed.verified_xlsx_cell_font_size_pt(0, 0), Some(14));

    let rounded_collision = CellStyle::new().font_name("Source").set_font_size(14);
    let mut row = source_sheet();
    row.row_formats.insert(0, rounded_collision.clone());
    assert_eq!(row.verified_xlsx_cell_font_size_pt(0, 0), None);

    let mut column = source_sheet();
    column.col_formats.insert(0, rounded_collision.clone());
    assert_eq!(column.verified_xlsx_cell_font_size_pt(0, 0), None);

    let mut table = source_sheet();
    table.tables.push(Table::new((0, 0, 1, 0), "T", ["value"]));
    let mut definition = TableStyleDefinition::default();
    definition.insert(TableStyleRegion::WholeTable, rounded_collision.clone(), 1);
    table.table_region_formats.insert(
        "T".to_string(),
        TableStyleApplication {
            definition,
            ..TableStyleApplication::default()
        },
    );
    assert_eq!(table.verified_xlsx_cell_font_size_pt(0, 0), None);

    let mut non_replacing_direct = source_sheet();
    non_replacing_direct
        .direct_cell_formats
        .insert((0, 0), CellStyleOverlay::default());
    assert_eq!(
        non_replacing_direct.verified_xlsx_cell_font_size_pt(0, 0),
        None
    );

    let mut verified_default = source_sheet();
    verified_default.xlsx_normal_font_size_pt = Some(14);
    verified_default
        .direct_cell_formats
        .insert((0, 0), CellStyleOverlay::default());
    assert_eq!(
        verified_default.verified_xlsx_cell_font_size_pt(0, 0),
        Some(14)
    );

    let mut direct = row;
    direct.direct_cell_formats.insert(
        (0, 0),
        CellStyleOverlay {
            style: rounded_collision,
            replace_font: true,
            ..CellStyleOverlay::default()
        },
    );
    assert_eq!(direct.verified_xlsx_cell_font_size_pt(0, 0), Some(14));
}

#[test]
fn imported_cell_xf_components_replace_inherited_and_table_properties() {
    let mut sheet = Sheet {
        style_fidelity: StyleFidelity::Partial,
        ..Sheet::default()
    };
    sheet.col_formats.insert(
        0,
        CellStyle {
            font: Some(Font::default().bold()),
            border: Some(Border {
                left: BorderStyle::Thin,
                ..Border::default()
            }),
            num_fmt: Some("0.00".to_string()),
            align: Some(Alignment {
                wrap: true,
                ..Alignment::default()
            }),
            ..CellStyle::default()
        },
    );
    sheet.row_formats.insert(
        2,
        CellStyle {
            font: Some(Font::default().italic()),
            ..CellStyle::default()
        },
    );
    sheet.tables.push(Table::new((1, 0, 3, 0), "T", ["value"]));
    let mut definition = TableStyleDefinition::default();
    definition.insert(
        TableStyleRegion::WholeTable,
        CellStyle::new().background_color([12, 34, 56]),
        1,
    );
    sheet.table_region_formats.insert(
        "T".to_string(),
        TableStyleApplication {
            definition,
            header_row: true,
            ..TableStyleApplication::default()
        },
    );
    sheet.direct_cell_formats.insert(
        (1, 0),
        CellStyleOverlay {
            style: CellStyle {
                font: Some(Font::default()),
                border: Some(Border::default()),
                num_fmt: None,
                align: Some(Alignment::default()),
                ..CellStyle::default()
            },
            replace_font: true,
            replace_border: true,
            replace_num_fmt: true,
            replace_alignment: true,
            ..CellStyleOverlay::default()
        },
    );

    let direct = sheet.resolved_cell_style(1, 0).expect("direct style");
    assert_eq!(direct.fill, Some(Color::rgb(12, 34, 56)));
    assert!(direct.font.as_ref().is_some_and(|font| !font.bold));
    assert_eq!(direct.border, Some(Border::default()));
    assert_eq!(direct.num_fmt, None);
    assert_eq!(direct.align, Some(Alignment::default()));

    let row = sheet.resolved_cell_style(2, 0).expect("row style");
    assert!(row.font.as_ref().is_some_and(|font| font.italic));
    assert!(row.font.as_ref().is_some_and(|font| !font.bold));
    assert_eq!(row.num_fmt, None, "row XF replaces the column XF");
    assert_eq!(row.fill, Some(Color::rgb(12, 34, 56)));
}

// Regression tests: HTML gap-fill column alignment and CSV delimiter
// validation.

#[test]
fn to_html_fills_unwritten_gap_in_middle_of_row_so_columns_stay_aligned() {
    let mut s = Sheet::new("s");
    s.write(0, 0, "Name");
    s.write(0, 1, "Age");
    s.write(0, 2, "City");
    // col1 is deliberately never written on the data row.
    s.write(1, 0, "Alice");
    s.write(1, 2, "Seattle");

    let html = s.to_html();
    let data_row = html
        .split("</tr>")
        .find(|row| row.contains("Alice"))
        .expect("data row present");
    let tds: Vec<&str> = data_row.matches("<td").collect();
    assert_eq!(
        tds.len(),
        3,
        "expected exactly 3 <td> in the data row, got: {data_row}"
    );
    assert_eq!(
        data_row, "<tr><td>Alice</td><td></td><td>Seattle</td>",
        "Seattle must land in the 3rd <td>, not shift into the 2nd \
             because the unwritten col1 was skipped entirely"
    );
}

#[test]
fn to_html_merge_anchor_without_cell_entry_still_emits_td() {
    let mut s = Sheet::new("s");
    s.merge(0, 0, 0, 1);
    // Only the covered cell (0,1) is written; the anchor (0,0) never is.
    s.write(0, 1, "stray");

    let html = s.to_html();
    assert_eq!(
        html, "<table><tr><td colspan=\"2\"></td></tr></table>",
        "the merge anchor must render an empty <td colspan=\"2\"> instead \
             of vanishing (and the covered cell's stray text must stay \
             hidden, matching real merge semantics)"
    );
}

#[test]
fn sheet_to_csv_with_delimiter_normalizes_quote_delimiter_to_comma() {
    let mut s = Sheet::new("s");
    s.write(0, 0, "has \"quote\" inside");
    s.write(0, 1, "plain");

    // '"' as a delimiter is inherently ambiguous (field separator and
    // quoted-field boundary collide); Sheet::to_csv_with_delimiter can't
    // signal failure via its String return type, so it must normalize
    // to the default ',' instead of emitting the ambiguous output that
    // treating '"' literally as the delimiter would produce.
    let out = s.to_csv_with_delimiter('"');
    assert_eq!(
        out,
        s.to_csv(),
        "invalid '\"' delimiter should fall back to ','"
    );
    assert!(
        !out.contains("\"\"\"\""),
        "must not emit the ambiguous quadruple-quote output: {out}"
    );
}

#[test]
fn workbook_to_csv_with_delimiter_rejects_quote_delimiter() {
    let mut wb = Workbook::new();
    {
        let s = wb.add_sheet("CSV");
        s.write(0, 0, "has \"quote\" inside");
    }

    assert_eq!(
        wb.to_csv_with_delimiter(0, '"'),
        None,
        "'\"' is not a valid delimiter and should be rejected like an invalid sheet index"
    );
}

#[test]
fn print_metadata_is_bounded_deduplicated_and_unicode_safe() {
    let mut metadata = PrintMetadata::default();
    metadata.push_print_area((0, 0, 9, 9));
    metadata.push_print_area((0, 0, 9, 9));
    metadata.push_print_area((10, 4, 3, 5));
    for row in 0..=1_026 {
        metadata.push_manual_row_break(row);
    }
    metadata.set_header_footer(HeaderFooterKind::OddHeader, "한".repeat(3_000));

    assert_eq!(metadata.print_areas(), &[(0, 0, 9, 9)]);
    assert_eq!(metadata.manual_row_breaks().len(), 1_026);
    assert_eq!(metadata.manual_row_breaks()[0], 0);
    assert_eq!(metadata.manual_row_breaks()[1_025], 1_025);
    let header = metadata.header_footer().odd_header().expect("header");
    assert!(header.len() <= MAX_HEADER_FOOTER_BYTES);
    assert!(std::str::from_utf8(header.as_bytes()).is_ok());
    assert_eq!(metadata.fidelity(), PrintFidelity::Partial);
    assert!(metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::InvalidPrintArea));
    assert!(metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::LimitExceeded));
}

#[test]
fn authored_page_setup_populates_compatible_print_sidecar() {
    let mut sheet = Sheet::new("Print");
    sheet.set_print_gridlines();
    sheet.set_print_headings();
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((2, 1, 8, 4))
            .with_center_horizontally(true)
            .with_header("&CTitle")
            .with_footer("&P/&N"),
    );

    let metadata = sheet.print_metadata();
    assert_eq!(metadata.fidelity(), PrintFidelity::Authored);
    assert_eq!(metadata.print_areas(), &[(2, 1, 8, 4)]);
    assert_eq!(metadata.print_gridlines(), Some(true));
    assert_eq!(metadata.print_headings(), Some(true));
    assert_eq!(metadata.fit_to_page(), Some(false));
    assert_eq!(metadata.center_horizontally(), Some(true));
    assert_eq!(metadata.header_footer().odd_header(), Some("&CTitle"));
    assert_eq!(metadata.header_footer().odd_footer(), Some("&P/&N"));

    let mut fitted = Sheet::new("Fit");
    fitted.set_page_setup(PageSetup::new().with_scale(85).with_fit_to_pages(0, 0));
    assert_eq!(fitted.print_metadata().fit_to_page(), Some(true));
}
