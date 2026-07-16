use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rxls::{
    Border, BorderStyle, CellStyle, Color, Font, HAlign, StyleFidelity, StyleLossKind, TextRun,
    VAlign, Workbook,
};
use rxls_render::{
    render_sheet_svg, LimitKind, RenderError, RenderLimits, RenderOptions, RenderRange,
    RenderSelection, WarningCode, MAX_WORKSHEET_COLUMN, MAX_WORKSHEET_ROW,
};

fn styled_workbook() -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("서울 & <주택>");
    let style = CellStyle::new()
        .font_name("맑은 \"고딕\" & Sans")
        .size(12)
        .color(Color::rgb(255, 255, 255))
        .bold()
        .fill(Color::rgb(31, 78, 121))
        .align(HAlign::Center)
        .valign(VAlign::Middle)
        .border(
            Border::new()
                .with_all(BorderStyle::Thin)
                .with_color(Color::rgb(192, 0, 0)),
        );
    sheet.write_styled(0, 0, "입주 <공고> & \"확정\"", &style);
    sheet.merge(0, 0, 0, 2);
    sheet.write_number(2, 2, 42);
    sheet.hide_column(1);
    sheet.hide_row(1);
    workbook
}

fn styled_options() -> RenderOptions {
    RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 2, 2)),
        gridlines: false,
        ..RenderOptions::default()
    }
}

#[test]
fn korean_merges_hidden_axes_styles_and_xml_escaping_are_exact() {
    let output = render_sheet_svg(&styled_workbook(), 0, &styled_options()).unwrap();
    assert_eq!(output.svg, EXPECTED_STYLED_SVG);
    assert_eq!(
        output.report.to_json(),
        "{\"schema_version\":2,\"sheet_index\":0,\"sheet_name\":\"서울 & <주택>\",\"range\":{\"first_row\":0,\"first_col\":0,\"last_row\":2,\"last_col\":2},\"rows_considered\":3,\"columns_considered\":3,\"cells_considered\":9,\"visible_rows\":2,\"visible_columns\":2,\"rendered_regions\":3,\"hidden_rows_skipped\":1,\"hidden_columns_skipped\":1,\"merged_regions\":1,\"text_bytes\":28,\"glyphs\":16,\"scene_nodes\":7,\"svg_bytes\":1297,\"font_pack_sha256\":null,\"font_faces\":[],\"warnings\":[{\"code\":\"approximate_text_metrics\",\"occurrences\":2,\"first_cell\":{\"row\":0,\"col\":0}}]}"
    );
    assert!(output.svg.contains("서울 &amp; &lt;주택&gt;"));
    assert!(output.svg.contains("입주 &lt;공고&gt; &amp; \"확정\""));
    assert!(output.svg.contains("맑은 &quot;고딕&quot; &amp; Sans"));
}

#[test]
fn every_expansion_surface_fails_with_a_typed_hostile_limit_error() {
    let workbook = styled_workbook();

    let mut options = styled_options();
    options.limits.max_cells = 8;
    assert_eq!(
        render_sheet_svg(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Cells,
            limit: 8,
            actual: 9,
        })
    );

    let mut options = styled_options();
    options.limits.max_text_bytes = 1;
    assert!(matches!(
        render_sheet_svg(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TextBytes,
            limit: 1,
            ..
        })
    ));

    let mut options = styled_options();
    options.limits.max_glyphs = 1;
    assert!(matches!(
        render_sheet_svg(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Glyphs,
            limit: 1,
            ..
        })
    ));

    let mut options = styled_options();
    options.limits.max_scene_nodes = 1;
    assert!(matches!(
        render_sheet_svg(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::SceneNodes,
            limit: 1,
            ..
        })
    ));

    let mut options = styled_options();
    options.limits.max_dimension_raw = 1;
    assert!(matches!(
        render_sheet_svg(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Dimension,
            limit: 1,
            ..
        })
    ));

    let mut options = styled_options();
    options.limits.max_output_bytes = 64;
    assert!(matches!(
        render_sheet_svg(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::OutputBytes,
            limit: 64,
            ..
        })
    ));
}

#[test]
fn every_render_limit_accepts_its_exact_boundary() {
    let workbook = styled_workbook();
    let baseline = render_sheet_svg(&workbook, 0, &styled_options()).unwrap();
    let mut options = styled_options();
    options.limits.max_rows = baseline.report.rows_considered;
    options.limits.max_columns = baseline.report.columns_considered;
    options.limits.max_cells = baseline.report.cells_considered;
    options.limits.max_text_bytes = baseline.report.text_bytes;
    options.limits.max_glyphs = baseline.report.glyphs;
    options.limits.max_scene_nodes = baseline.report.scene_nodes;
    options.limits.max_dimension_raw =
        u64::try_from(baseline.scene.width.raw().max(baseline.scene.height.raw())).unwrap();
    options.limits.max_output_bytes = baseline.report.svg_bytes;
    assert_eq!(render_sheet_svg(&workbook, 0, &options).unwrap(), baseline);
}

#[test]
fn selection_grid_and_default_limits_are_exact() {
    assert_eq!(
        RenderLimits::default(),
        RenderLimits {
            max_rows: 4_096,
            max_columns: 512,
            max_cells: 250_000,
            max_conditional_rules: 4_096,
            max_conditional_evaluations: 1_000_000,
            max_drawing_objects: 4_096,
            max_media_bytes: 64 << 20,
            max_image_dimension: 16_384,
            max_image_pixels: 100_000_000,
            max_decoded_media_bytes: 256 << 20,
            max_chart_series: 256,
            max_chart_points: 1_000_000,
            max_text_bytes: 16 << 20,
            max_glyphs: 2_000_000,
            max_text_runs: 1_000_000,
            max_text_lines: 500_000,
            max_path_commands: 8_000_000,
            max_scene_nodes: 4_000_000,
            max_dimension_raw: 10_000_000 * 1_024,
            max_output_bytes: 64 << 20,
        }
    );

    let workbook = styled_workbook();
    let mut options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(2, 2, 1, 1)),
        ..RenderOptions::default()
    };
    assert!(matches!(
        render_sheet_svg(&workbook, 0, &options),
        Err(RenderError::InvalidRange { .. })
    ));

    options.selection = RenderSelection::Range(RenderRange::new(
        MAX_WORKSHEET_ROW,
        MAX_WORKSHEET_COLUMN,
        MAX_WORKSHEET_ROW,
        MAX_WORKSHEET_COLUMN + 1,
    ));
    assert!(matches!(
        render_sheet_svg(&workbook, 0, &options),
        Err(RenderError::RangeOutsideGrid { .. })
    ));
}

#[test]
fn hyperlinks_are_allowlisted_and_unsafe_schemes_are_dropped() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("links");
    sheet.write_url(0, 0, "https://example.com/?a=1&b=2", "safe");
    sheet.write_url(1, 0, "javascript:alert(1)", "unsafe");

    let output = render_sheet_svg(&workbook, 0, &RenderOptions::default()).unwrap();
    assert!(output
        .svg
        .contains("<a href=\"https://example.com/?a=1&amp;b=2\"><text"));
    assert!(!output.svg.contains("javascript:"));
    assert_eq!(
        output
            .report
            .warnings
            .iter()
            .find(|warning| warning.code == WarningCode::UnsafeHyperlinkDropped)
            .map(|warning| warning.occurrences),
        Some(1)
    );
}

#[test]
fn sparse_rich_and_hostile_xml_cells_are_bounded_and_explicit() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("sparse");
    sheet.write(0, 0, "A");
    sheet.write_rich(
        2,
        2,
        [
            TextRun::new("한", Font::default()),
            TextRun::new("글", Font::default().bold()),
        ],
    );
    sheet.write(1, 1, "bad\u{1}xml");

    let output = render_sheet_svg(&workbook, 0, &RenderOptions::default()).unwrap();
    assert_eq!(output.report.cells_considered, 9);
    assert_eq!(output.report.rendered_regions, 9);
    assert!(!output.svg.as_bytes().contains(&1));
    assert!(output.svg.contains("bad�xml"));
    for code in [
        WarningCode::RichTextFlattened,
        WarningCode::InvalidXmlCharacterReplaced,
    ] {
        assert!(output
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == code && warning.occurrences == 1));
    }
}

#[test]
fn repeated_render_is_byte_for_byte_and_report_deterministic() {
    let workbook = styled_workbook();
    let options = styled_options();
    let expected = render_sheet_svg(&workbook, 0, &options).unwrap();
    for _ in 0..16 {
        assert_eq!(render_sheet_svg(&workbook, 0, &options).unwrap(), expected);
    }
    assert_eq!(
        expected.report.warnings[0].code,
        WarningCode::ApproximateTextMetrics
    );
}

#[test]
fn bundle_cli_emits_ordered_hashed_artifacts_deterministically() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest_dir.join("../tests/fixtures/xls/korean-cp949-biff5.xls");
    let first = unique_temp_dir("first");
    let second = unique_temp_dir("second");

    for output_dir in [&first, &second] {
        let result = Command::new(env!("CARGO_BIN_EXE_rxls-render"))
            .arg("bundle")
            .arg(&fixture)
            .arg("--output-dir")
            .arg(output_dir)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    let first_svg = fs::read(first.join("sheet-0000.svg")).unwrap();
    let second_svg = fs::read(second.join("sheet-0000.svg")).unwrap();
    let first_manifest = fs::read(first.join("render-manifest.json")).unwrap();
    let second_manifest = fs::read(second.join("render-manifest.json")).unwrap();
    assert_eq!(first_svg, second_svg);
    assert_eq!(first_manifest, second_manifest);
    let manifest = String::from_utf8(first_manifest).unwrap();
    assert!(manifest.starts_with("{\"schema\":\"rxls.render.bundle.v1\""));
    assert!(manifest.contains("\"file\":\"sheet-0000.svg\""));
    assert!(manifest.contains("\"visibility\":\"visible\""));
    assert!(manifest.contains("\"sha256\":\""));
    assert!(manifest.contains("\"scene\":{\"sha256\":\""));
    assert!(manifest.contains("\"font_pack_sha256\":null"));

    fs::remove_dir_all(first).unwrap();
    fs::remove_dir_all(second).unwrap();
}

#[test]
fn bundle_cli_validates_the_font_pack_before_creating_output() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest_dir.join("../tests/fixtures/xls/reader-basic.xls");
    let output_dir = unique_temp_dir("invalid-font-pack-output");
    let missing_manifest = unique_temp_dir("missing-font-pack").join("manifest.json");
    let result = Command::new(env!("CARGO_BIN_EXE_rxls-render"))
        .arg("bundle")
        .arg(fixture)
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--font-pack-manifest")
        .arg(missing_manifest)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("font-pack I/O failed"));
    assert!(!output_dir.exists());
}

#[test]
fn bundle_cli_rolls_back_on_a_late_render_limit_failure() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest_dir.join("../tests/fixtures/xls/reader-basic.xls");
    let output_dir = unique_temp_dir("rollback");
    let parent = output_dir.parent().unwrap().to_path_buf();
    let result = Command::new(env!("CARGO_BIN_EXE_rxls-render"))
        .arg("bundle")
        .arg(&fixture)
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--range")
        .arg("A1:XFD4096")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("columns limit exceeded"));
    assert!(!output_dir.exists());
    let stage_prefix = format!(
        ".{}.rxls-render-stage-",
        output_dir.file_name().unwrap().to_string_lossy()
    );
    assert!(parent
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .starts_with(&stage_prefix)));
}

#[test]
fn bundle_cli_opens_every_supported_fixture_format() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixtures = [
        "../tests/fixtures/xls/reader-basic.xls",
        "../tests/fixtures/xlsx/reader-structural.xlsx",
        "../tests/fixtures/xlsb/reader-basic.xlsb",
        "../tests/fixtures/ods/repeated-hidden.ods",
    ];
    for (index, fixture) in fixtures.iter().enumerate() {
        let output_dir = unique_temp_dir(&format!("format-{index}"));
        let result = Command::new(env!("CARGO_BIN_EXE_rxls-render"))
            .arg("bundle")
            .arg(manifest_dir.join(fixture))
            .arg("--output-dir")
            .arg(&output_dir)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{fixture}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(output_dir.join("render-manifest.json").is_file());
        fs::remove_dir_all(output_dir).unwrap();
    }
}

#[test]
fn imported_style_fidelity_is_never_silently_flattened() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixtures = [
        (
            "../tests/fixtures/xls/reader-basic.xls",
            StyleFidelity::Partial,
        ),
        (
            "../tests/fixtures/xlsx/reader-structural.xlsx",
            StyleFidelity::Partial,
        ),
        (
            "../tests/fixtures/xlsb/reader-basic.xlsb",
            StyleFidelity::Partial,
        ),
        (
            "../tests/fixtures/ods/repeated-hidden.ods",
            StyleFidelity::Retained,
        ),
    ];
    for (fixture, expected_fidelity) in fixtures {
        let workbook = Workbook::open(&fs::read(manifest_dir.join(fixture)).unwrap()).unwrap();
        assert_eq!(workbook.sheets[0].style_fidelity(), expected_fidelity);
        if fixture.ends_with("reader-basic.xlsb") {
            let losses = workbook.sheets[0].style_losses();
            assert_eq!(losses.len(), 1);
            assert_eq!(losses[0].kind, StyleLossKind::MissingReference);
            assert_eq!(losses[0].occurrences, 2);
        }
        let output = render_sheet_svg(&workbook, 0, &RenderOptions::default()).unwrap();
        let style_warnings = output.report.warnings.iter().filter(|warning| {
            matches!(
                warning.code,
                WarningCode::SourceStylesPartial | WarningCode::SourceStylesUnavailable
            )
        });
        match expected_fidelity {
            StyleFidelity::Partial => assert_eq!(style_warnings.count(), 1, "{fixture}"),
            StyleFidelity::Unavailable => assert!(style_warnings
                .into_iter()
                .any(|warning| warning.code == WarningCode::SourceStylesUnavailable)),
            StyleFidelity::Retained | StyleFidelity::Authored => {
                assert_eq!(style_warnings.count(), 0, "{fixture}")
            }
            _ => panic!("unexpected style-fidelity expectation for {fixture}"),
        }
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rxls-render-test-{}-{label}-{nonce}",
        std::process::id()
    ))
}

const EXPECTED_STYLED_SVG: &str = r###"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="128" height="40" viewBox="0 0 128 40" role="img">
<title>서울 &amp; &lt;주택&gt;</title>
<defs>
<clipPath id="clip-0"><rect x="0" y="0" width="128" height="20"/></clipPath>
<clipPath id="clip-1"><rect x="64" y="20" width="64" height="20"/></clipPath>
</defs>
<rect width="100%" height="100%" fill="#FFFFFF"/>
<rect x="0" y="0" width="128" height="20" fill="#1F4E79"/>
<text x="64" y="10" font-family="맑은 &quot;고딕&quot; &amp; Sans" font-size="16" fill="#FFFFFF" text-anchor="middle" dominant-baseline="central" font-weight="700" clip-path="url(#clip-0)" xml:space="preserve">입주 &lt;공고&gt; &amp; "확정"</text>
<text x="125" y="30" font-family="Liberation Sans" font-size="14.6669921875" fill="#000000" text-anchor="end" dominant-baseline="central" clip-path="url(#clip-1)" xml:space="preserve">42</text>
<line x1="0" y1="0" x2="0" y2="20" stroke="#C00000" stroke-width="1" stroke-linecap="butt"/>
<line x1="128" y1="0" x2="128" y2="20" stroke="#C00000" stroke-width="1" stroke-linecap="butt"/>
<line x1="0" y1="0" x2="128" y2="0" stroke="#C00000" stroke-width="1" stroke-linecap="butt"/>
<line x1="0" y1="20" x2="128" y2="20" stroke="#C00000" stroke-width="1" stroke-linecap="butt"/>
</svg>
"###;
