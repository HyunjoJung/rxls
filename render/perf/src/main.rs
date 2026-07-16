#![forbid(unsafe_code)]

use std::env;
use std::process::ExitCode;

use rxls::{
    Border, BorderStyle, CellStyle, Chart, ChartKind, Color, Image, ImageFmt, PageSetup, Series,
    Workbook,
};
use rxls_render::{
    build_print_document, render_scene_svg, render_sheet_svg, FontPack, LimitKind, PrintLimits,
    PrintOptions, RenderError, RenderLimits, RenderOptions, RenderRange, RenderSelection, Scene,
    SceneNode,
};
use serde_json::json;
use sha2::{Digest, Sha256};

const CASES: &[&str] = &[
    "huge-sparse-sheet",
    "wrapped-cjk",
    "many-styles",
    "merge-grid",
    "hundreds-of-pages",
    "image-bomb-headers",
    "image-pixel-limits",
    "decoded-media-limits",
    "chart-point-limits",
];

const VALID_RGBA_PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x56, 0xc7, 0x2f, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[derive(Debug)]
struct Metrics {
    disposition: &'static str,
    limit_kind: Option<&'static str>,
    pages: u64,
    backend_commands: u64,
    output_bytes: u64,
    artifact_sha256: String,
}

impl Metrics {
    fn bounded_limit(kind: LimitKind) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"rxls-render-perf-limit-v1\0");
        digest.update(kind.code().as_bytes());
        Self {
            disposition: "bounded_limit",
            limit_kind: Some(kind.code()),
            pages: 0,
            backend_commands: 0,
            output_bytes: 0,
            artifact_sha256: hex_digest(digest.finalize()),
        }
    }
}

fn command_count(scene: &Scene) -> u64 {
    scene
        .nodes
        .iter()
        .map(|node| match node {
            SceneNode::Rect(_) | SceneNode::Image(_) | SceneNode::Text(_) => 1,
            SceneNode::Line(_) => 2,
            SceneNode::Path(node) => node.commands.len() as u64,
            SceneNode::GlyphRun(node) => {
                node.commands.len() as u64 + node.decorations.len() as u64 * 2
            }
        })
        .fold(0, u64::saturating_add)
}

fn rendered_scene(label: &str, scene: &Scene, svg: &[u8]) -> Metrics {
    let mut digest = Sha256::new();
    digest.update(b"rxls-render-perf-artifact-v1\0");
    digest.update(label.as_bytes());
    digest.update((svg.len() as u64).to_le_bytes());
    digest.update(svg);
    Metrics {
        disposition: "rendered",
        limit_kind: None,
        pages: 1,
        backend_commands: command_count(scene),
        output_bytes: svg.len() as u64,
        artifact_sha256: hex_digest(digest.finalize()),
    }
}

fn render_workbook(
    label: &str,
    workbook: &Workbook,
    options: &RenderOptions,
) -> Result<Metrics, String> {
    match render_sheet_svg(workbook, 0, options) {
        Ok(output) => Ok(rendered_scene(label, &output.scene, output.svg.as_bytes())),
        Err(RenderError::LimitExceeded { kind, .. }) => Ok(Metrics::bounded_limit(kind)),
        Err(error) => Err(format!("renderer rejected {label}: {error}")),
    }
}

fn render_workbook_with_outlines(
    label: &str,
    workbook: &Workbook,
    options: &RenderOptions,
) -> Result<Metrics, String> {
    match render_sheet_svg(workbook, 0, options) {
        Ok(output) => {
            if !output
                .scene
                .nodes
                .iter()
                .any(|node| matches!(node, SceneNode::GlyphRun(run) if !run.commands.is_empty()))
            {
                return Err(format!("{label} did not exercise verified glyph outlines"));
            }
            Ok(rendered_scene(label, &output.scene, output.svg.as_bytes()))
        }
        Err(RenderError::LimitExceeded { kind, .. }) => Ok(Metrics::bounded_limit(kind)),
        Err(error) => Err(format!("renderer rejected {label}: {error}")),
    }
}

fn verified_font_options() -> Result<RenderOptions, String> {
    let manifest = env::var_os("RXLS_RENDER_FONT_PACK_MANIFEST")
        .ok_or_else(|| "RXLS_RENDER_FONT_PACK_MANIFEST is required".to_string())?;
    let pack = FontPack::load_manifest(&manifest)
        .map_err(|error| format!("verified font pack rejected: {error}"))?;
    Ok(RenderOptions {
        default_font_family: pack.default_family().to_string(),
        font_pack: Some(pack),
        ..RenderOptions::default()
    })
}

fn huge_sparse_sheet() -> Result<Metrics, String> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("huge-sparse");
    sheet.write(0, 0, "origin");
    sheet.write(1_048_575, 16_383, "far-corner");
    let limit_kind = match render_sheet_svg(&workbook, 0, &RenderOptions::default()) {
        Err(RenderError::LimitExceeded { kind, .. }) => kind,
        Ok(_) => return Err("whole huge sparse sheet did not trip a renderer limit".to_string()),
        Err(error) => {
            return Err(format!(
                "whole huge sparse sheet failed unexpectedly: {error}"
            ))
        }
    };
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(1_048_575, 16_383, 1_048_575, 16_383)),
        gridlines: false,
        ..RenderOptions::default()
    };
    let mut metrics = render_workbook("huge-sparse-sheet", &workbook, &options)?;
    if metrics.disposition != "rendered" || metrics.pages != 1 {
        return Err("far-corner sparse window was not rendered independently".to_string());
    }
    let mut digest = Sha256::new();
    digest.update(b"rxls-render-perf-sparse-window-v2\0");
    digest.update(limit_kind.code().as_bytes());
    digest.update(metrics.artifact_sha256.as_bytes());
    metrics.artifact_sha256 = hex_digest(digest.finalize());
    Ok(metrics)
}

fn wrapped_cjk() -> Result<Metrics, String> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("wrapped-cjk");
    let style = CellStyle::new()
        .font_name("Liberation Sans")
        .size(11)
        .wrap();
    let text = "서울 공공주택 공급 일정과 신청 자격을 확인합니다. 日本語の折り返しと中文标点符号も同じ 셀에서 검증합니다.";
    for column in 0..8u16 {
        sheet.set_col_width(column, 12.0);
    }
    for row in 0..48u32 {
        sheet.set_row_height(row, 48.0);
        for column in 0..6u16 {
            sheet.write_styled(row, column, format!("{row}:{column} {text}"), &style);
        }
    }
    render_workbook_with_outlines("wrapped-cjk", &workbook, &verified_font_options()?)
}

fn many_styles() -> Result<Metrics, String> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("many-styles");
    for row in 0..64u32 {
        for column in 0..32u16 {
            let red = ((row * 29 + u32::from(column) * 11) & 0xff) as u8;
            let green = ((row * 7 + u32::from(column) * 31) & 0xff) as u8;
            let blue = ((row * 17 + u32::from(column) * 13) & 0xff) as u8;
            let border = match (row + u32::from(column)) % 4 {
                0 => BorderStyle::Thin,
                1 => BorderStyle::Medium,
                2 => BorderStyle::Thick,
                _ => BorderStyle::Double,
            };
            let style = CellStyle::new()
                .font_name(if row % 2 == 0 {
                    "Liberation Sans"
                } else {
                    "Carlito"
                })
                .size(8 + (row % 10) as u16)
                .color(Color::rgb(255 - red, 255 - green, 255 - blue))
                .fill(Color::rgb(red, green, blue))
                .border(
                    Border::new()
                        .with_all(border)
                        .with_color(Color::rgb(blue, red, green)),
                )
                .num_fmt(match column % 4 {
                    0 => "0.00",
                    1 => "#,##0",
                    2 => "0.0%",
                    _ => "[Red]-0.00;0.00",
                })
                .text_rotation(((column % 7) as i16 - 3) * 15);
            sheet.write_styled(row, column, f64::from(row * 32 + u32::from(column)), &style);
        }
    }
    render_workbook("many-styles", &workbook, &verified_font_options()?)
}

fn merge_grid() -> Result<Metrics, String> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("merge-grid");
    let style = CellStyle::new()
        .fill(Color::rgb(225, 235, 245))
        .border(Border::new().with_all(BorderStyle::Thin));
    for row in (0..512u32).step_by(2) {
        for column in (0..8u16).step_by(2) {
            sheet.write_styled(row, column, format!("merge-{row}-{column}"), &style);
            sheet.merge(row, column, row + 1, column + 1);
        }
    }
    render_workbook("merge-grid", &workbook, &verified_font_options()?)
}

fn hundreds_of_pages() -> Result<Metrics, String> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("hundreds-of-pages");
    for row in 0..420u32 {
        sheet.set_row_height(row, 240.0);
        sheet.write(row, 0, format!("page-row-{row}"));
    }
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 419, 0))
            .with_paper_size(9)
            .with_margins(0.7, 0.7, 0.75, 0.75, 0.3, 0.3),
    );
    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            render: verified_font_options()?,
            omit_sparse_pages: false,
            limits: PrintLimits {
                max_logical_pages: 512,
                max_pages: 512,
                max_total_scene_nodes: 1_000_000,
                max_backend_commands: 2_000_000,
                max_pdf_bytes: 128 << 20,
                max_raster_dimension: 8_192,
                max_raster_pixels: 32_000_000,
                max_png_bytes_per_page: 16 << 20,
            },
            ..PrintOptions::default()
        },
    )
    .map_err(|error| format!("pagination failed: {error}"))?;
    if document.pages.len() < 100 {
        return Err(format!(
            "hundreds-of-pages produced only {} pages",
            document.pages.len()
        ));
    }

    let mut digest = Sha256::new();
    digest.update(b"rxls-render-perf-pages-v1\0");
    let mut commands = 0_u64;
    let mut output_bytes = 0_u64;
    for page in &document.pages {
        let svg = render_scene_svg(&page.scene, 8 << 20)
            .map_err(|error| format!("page SVG failed: {error}"))?;
        commands = commands.saturating_add(command_count(&page.scene));
        output_bytes = output_bytes.saturating_add(svg.len() as u64);
        digest.update((svg.len() as u64).to_le_bytes());
        digest.update(svg.as_bytes());
    }
    Ok(Metrics {
        disposition: "rendered",
        limit_kind: None,
        pages: document.pages.len() as u64,
        backend_commands: commands,
        output_bytes,
        artifact_sha256: hex_digest(digest.finalize()),
    })
}

fn png_crc(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn push_png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    output.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(payload);
    let mut checked = Vec::with_capacity(4 + payload.len());
    checked.extend_from_slice(kind);
    checked.extend_from_slice(payload);
    output.extend_from_slice(&png_crc(&checked).to_be_bytes());
}

fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut output = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_png_chunk(&mut output, b"IHDR", &ihdr);
    push_png_chunk(&mut output, b"IDAT", &[]);
    push_png_chunk(&mut output, b"IEND", &[]);
    output
}

fn image_bomb_headers() -> Result<Metrics, String> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("image-bomb");
    sheet.write(0, 0, "oversized image header");
    sheet.add_image(Image::new(png_header(u32::MAX, 1), ImageFmt::Png, (0, 1)).with_to((8, 8)));
    let options = RenderOptions {
        limits: RenderLimits {
            max_image_dimension: 4_096,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    };
    let metrics = render_workbook("image-bomb-headers", &workbook, &options)?;
    if metrics.limit_kind != Some("image_dimension") {
        return Err("image bomb did not trip the decoded dimension limit".to_string());
    }
    Ok(metrics)
}

fn image_pixel_limits() -> Result<Metrics, String> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("image-pixels");
    sheet.add_image(Image::new(png_header(4_096, 4_096), ImageFmt::Png, (0, 0)).with_to((8, 8)));
    let options = RenderOptions {
        limits: RenderLimits {
            max_image_dimension: 4_096,
            max_image_pixels: 1_000_000,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    };
    let metrics = render_workbook("image-pixel-limits", &workbook, &options)?;
    if metrics.limit_kind != Some("image_pixels") {
        return Err("image header did not trip the decoded pixel limit".to_string());
    }
    Ok(metrics)
}

fn decoded_media_limits() -> Result<Metrics, String> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("decoded-media");
    sheet.add_image(Image::new(VALID_RGBA_PNG_1X1, ImageFmt::Png, (0, 0)).with_to((1, 1)));
    let options = RenderOptions {
        limits: RenderLimits {
            max_decoded_media_bytes: 3,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    };
    let metrics = render_workbook("decoded-media-limits", &workbook, &options)?;
    if metrics.limit_kind != Some("decoded_media_bytes") {
        return Err("valid PNG did not trip the decoded RGBA byte limit".to_string());
    }
    Ok(metrics)
}

fn chart_point_limits() -> Result<Metrics, String> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("chart-points");
    for row in 0..64u32 {
        sheet.write(row, 0, row);
    }
    let mut chart = Chart::new(ChartKind::Line, (0, 2), (20, 12));
    for index in 0..64u16 {
        chart = chart.add_series(
            Series::new("'chart-points'!$A$1:$A$1048576")
                .with_categories("'chart-points'!$B$1:$B$1048576")
                .with_name(format!("series-{index}")),
        );
    }
    sheet.add_chart(chart);
    let options = RenderOptions {
        limits: RenderLimits {
            max_chart_series: 128,
            max_chart_points: 1_024,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    };
    let metrics = render_workbook("chart-point-limits", &workbook, &options)?;
    if metrics.limit_kind != Some("chart_points") {
        return Err("hostile A1 chart did not trip the chart point limit".to_string());
    }
    Ok(metrics)
}

fn run_case(case: &str) -> Result<Metrics, String> {
    match case {
        "huge-sparse-sheet" => huge_sparse_sheet(),
        "wrapped-cjk" => wrapped_cjk(),
        "many-styles" => many_styles(),
        "merge-grid" => merge_grid(),
        "hundreds-of-pages" => hundreds_of_pages(),
        "image-bomb-headers" => image_bomb_headers(),
        "image-pixel-limits" => image_pixel_limits(),
        "decoded-media-limits" => decoded_media_limits(),
        "chart-point-limits" => chart_point_limits(),
        _ => Err(format!("unknown performance case: {case}")),
    }
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments == ["--list"] {
        for case in CASES {
            println!("{case}");
        }
        return ExitCode::SUCCESS;
    }
    if arguments == ["--version"] {
        println!("rxls-render-perf 0.1.0");
        return ExitCode::SUCCESS;
    }
    let [case] = arguments.as_slice() else {
        eprintln!("usage: rxls-render-perf <case>|--list|--version");
        return ExitCode::from(2);
    };
    match run_case(case) {
        Ok(metrics) => {
            println!(
                "{}",
                json!({
                    "artifact_sha256": metrics.artifact_sha256,
                    "backend_commands": metrics.backend_commands,
                    "case": case,
                    "disposition": metrics.disposition,
                    "limit_kind": metrics.limit_kind,
                    "output_bytes": metrics.output_bytes,
                    "pages": metrics.pages,
                    "schema": "rxls.render-performance-driver.v1",
                })
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("rxls-render-perf: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_names_are_unique_and_stable() {
        let mut sorted = CASES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), CASES.len());
        assert_eq!(CASES.len(), 9);
    }

    #[test]
    fn resource_bomb_cases_hit_typed_limits() {
        assert_eq!(huge_sparse_sheet().unwrap().disposition, "rendered");
        assert_eq!(
            image_bomb_headers().unwrap().limit_kind,
            Some("image_dimension")
        );
        assert_eq!(
            image_pixel_limits().unwrap().limit_kind,
            Some("image_pixels")
        );
        assert_eq!(
            decoded_media_limits().unwrap().limit_kind,
            Some("decoded_media_bytes")
        );
        assert_eq!(
            chart_point_limits().unwrap().limit_kind,
            Some("chart_points")
        );
    }
}
