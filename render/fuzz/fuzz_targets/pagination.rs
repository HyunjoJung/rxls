#![no_main]
//! Exercise print-area selection, scaling, repeated titles, merges, and page maps.

mod support;

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use rxls::PageSetup;
use rxls_render::{build_print_document, render_scene_svg, PrintLimits, PrintOptions};

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(support::input(data));
    let mut workbook = support::workbook(&mut unstructured);
    let (first_row, first_col) = support::coordinate(&mut unstructured, 31, 15);
    let (last_row, last_col) = support::coordinate(&mut unstructured, 63, 31);
    let mut setup = PageSetup::new()
        .with_print_area((
            first_row.min(last_row),
            first_col.min(last_col),
            first_row.max(last_row),
            first_col.max(last_col),
        ))
        .with_repeat_rows(
            unstructured.int_in_range(0u32..=4).unwrap_or(0),
            unstructured.int_in_range(0u32..=8).unwrap_or(0),
        )
        .with_repeat_cols(
            unstructured.int_in_range(0u16..=2).unwrap_or(0),
            unstructured.int_in_range(0u16..=4).unwrap_or(0),
        )
        .with_paper_size(u16::arbitrary(&mut unstructured).unwrap_or(9))
        .with_scale(u16::arbitrary(&mut unstructured).unwrap_or(100))
        .with_margins(
            f64::arbitrary(&mut unstructured).unwrap_or(0.7),
            f64::arbitrary(&mut unstructured).unwrap_or(0.7),
            f64::arbitrary(&mut unstructured).unwrap_or(0.75),
            f64::arbitrary(&mut unstructured).unwrap_or(0.75),
            f64::arbitrary(&mut unstructured).unwrap_or(0.3),
            f64::arbitrary(&mut unstructured).unwrap_or(0.3),
        )
        .with_header(support::bounded_text(&mut unstructured, 256))
        .with_footer(support::bounded_text(&mut unstructured, 256));
    if bool::arbitrary(&mut unstructured).unwrap_or(false) {
        setup = setup.with_landscape();
    }
    if bool::arbitrary(&mut unstructured).unwrap_or(false) {
        setup = setup.with_fit_to_pages(
            unstructured.int_in_range(0u16..=16).unwrap_or(1),
            unstructured.int_in_range(0u16..=16).unwrap_or(1),
        );
    }
    workbook.sheets[0].set_page_setup(setup);

    let options = PrintOptions {
        render: support::render_options(&mut unstructured),
        omit_sparse_pages: bool::arbitrary(&mut unstructured).unwrap_or(true),
        single_page_sheets: bool::arbitrary(&mut unstructured).unwrap_or(false),
        limits: PrintLimits {
            max_logical_pages: 256,
            max_pages: 128,
            max_total_scene_nodes: 262_144,
            max_backend_commands: 524_288,
            max_pdf_bytes: 8 << 20,
            max_raster_dimension: 2_048,
            max_raster_pixels: 4_194_304,
            max_png_bytes_per_page: 4 << 20,
        },
    };
    if let Ok(document) = build_print_document(&workbook, 0, &options) {
        let _ = document.report.to_json();
        for page in document.pages.iter().take(8) {
            let _ = render_scene_svg(&page.scene, 2 << 20);
        }
    }
});
