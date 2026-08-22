#![no_main]
//! Exercise hostile chart and sparkline A1 references through public workbook APIs.

mod support;

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use rxls::{Chart, ChartKind, Series, Sparkline, SparklineKind, Workbook};
use rxls_render::{build_scene, render_scene_svg, RenderLimits, RenderOptions};

fn range(unstructured: &mut Unstructured<'_>) -> String {
    const TOKENS: &[&str] = &[
        "A1:A10",
        "$A$1:$XFD$1048576",
        "Sheet1!A:A",
        "Sheet1!1:1048576",
        "'quoted sheet'!$B$2:$B$999999",
        "[external.xlsx]Sheet1!A1:A10",
        "#REF!",
        "A0",
        "XFE1",
        "A1048577",
        "A1:A1 A2:A2",
        "Sheet1!A1,Sheet1!B2",
    ];
    let mut output = TOKENS[unstructured
        .int_in_range(0usize..=TOKENS.len() - 1)
        .unwrap_or(0)]
    .to_string();
    if bool::arbitrary(unstructured).unwrap_or(false) {
        output.push_str(&support::bounded_text(unstructured, 512));
    }
    output
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(support::input(data));
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Sheet1");
    for row in 0..64u32 {
        sheet.write(row, 0, f64::from(row));
        sheet.write(row, 1, f64::from(row.saturating_mul(row)));
    }
    let kind = match unstructured.int_in_range(0u8..=7).unwrap_or(0) {
        0 => ChartKind::Bar,
        1 => ChartKind::Line,
        2 => ChartKind::Pie,
        3 => ChartKind::Scatter,
        4 => ChartKind::Area,
        5 => ChartKind::Doughnut,
        6 => ChartKind::Radar,
        _ => ChartKind::Bubble,
    };
    let mut chart = Chart::new(kind, (0, 3), (20, 12));
    let series_count = unstructured.int_in_range(0u8..=32).unwrap_or(0);
    for _ in 0..series_count {
        chart = chart.add_series(
            Series::new(range(&mut unstructured))
                .with_categories(range(&mut unstructured))
                .with_bubble_sizes(range(&mut unstructured))
                .with_name(support::bounded_text(&mut unstructured, 128)),
        );
    }
    chart = chart.add_series(
        Series::new("Sheet1!$A$1:$A$1048576")
            .with_categories("Sheet1!$B$1:$B$1048576")
            .with_name("full-grid-limit"),
    );
    sheet.add_chart(chart);
    sheet.add_sparkline(Sparkline::new((0, 2), range(&mut unstructured)).with_kind(
        match unstructured.int_in_range(0u8..=2).unwrap_or(0) {
            0 => SparklineKind::Line,
            1 => SparklineKind::Column,
            _ => SparklineKind::WinLoss,
        },
    ));

    let options = RenderOptions {
        limits: RenderLimits {
            max_chart_series: 64,
            max_chart_points: 1_024,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    };
    if let Ok(build) = build_scene(&workbook, 0, &options) {
        let _ = render_scene_svg(&build.scene, 4 << 20);
    }
    let encoded = workbook.to_xlsx();
    if encoded.len() <= 4 << 20 {
        if let Ok(reopened) = Workbook::open(&encoded) {
            let _ = build_scene(&reopened, 0, &options);
        }
    }
});
