#![no_main]
//! Replay bounded vector commands through SVG, PDF, and PNG backends.

mod support;

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use rxls::Workbook;
use rxls_render::{
    build_print_document, render_print_document_pdf, render_print_document_png_pages,
    render_scene_svg, Fixed, GlyphRunNode, LineNode, PathCommand, PrintLimits, PrintOptions, Rect,
    RectNode, Rgb, Scene, SceneNode,
};

fn point(unstructured: &mut Unstructured<'_>) -> (Fixed, Fixed) {
    (
        Fixed::from_raw(unstructured.int_in_range(-65_536i64..=589_824).unwrap_or(0)),
        Fixed::from_raw(unstructured.int_in_range(-65_536i64..=589_824).unwrap_or(0)),
    )
}

fn color(unstructured: &mut Unstructured<'_>) -> Rgb {
    Rgb::new(
        u8::arbitrary(unstructured).unwrap_or(0),
        u8::arbitrary(unstructured).unwrap_or(0),
        u8::arbitrary(unstructured).unwrap_or(0),
    )
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(support::input(data));
    let mut commands = Vec::new();
    let count = unstructured.int_in_range(0u16..=1_024).unwrap_or(0);
    for _ in 0..count {
        commands.push(match unstructured.int_in_range(0u8..=4).unwrap_or(0) {
            0 => {
                let (x, y) = point(&mut unstructured);
                PathCommand::MoveTo { x, y }
            }
            1 => {
                let (x, y) = point(&mut unstructured);
                PathCommand::LineTo { x, y }
            }
            2 => {
                let (control_x, control_y) = point(&mut unstructured);
                let (x, y) = point(&mut unstructured);
                PathCommand::QuadraticTo {
                    control_x,
                    control_y,
                    x,
                    y,
                }
            }
            3 => {
                let (control1_x, control1_y) = point(&mut unstructured);
                let (control2_x, control2_y) = point(&mut unstructured);
                let (x, y) = point(&mut unstructured);
                PathCommand::CubicTo {
                    control1_x,
                    control1_y,
                    control2_x,
                    control2_y,
                    x,
                    y,
                }
            }
            _ => PathCommand::Close,
        });
    }
    let canvas = Fixed::from_pixels(64);
    let clip = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: canvas,
        height: canvas,
    };
    let scene = Scene {
        title: support::bounded_text(&mut unstructured, 128),
        width: canvas,
        height: canvas,
        background: color(&mut unstructured),
        nodes: vec![
            SceneNode::Rect(RectNode {
                rect: clip,
                fill: Some(color(&mut unstructured)),
                stroke: Some(color(&mut unstructured)),
                stroke_width: Fixed::from_pixels(1),
            }),
            SceneNode::GlyphRun(GlyphRunNode {
                text: support::bounded_text(&mut unstructured, 256),
                clip_bounds: clip,
                commands,
                clusters: Vec::new(),
                paints: Vec::new(),
                decorations: vec![LineNode {
                    x1: Fixed::ZERO,
                    y1: Fixed::from_pixels(32),
                    x2: canvas,
                    y2: Fixed::from_pixels(32),
                    color: color(&mut unstructured),
                    width: Fixed::from_pixels(1),
                }],
                color: color(&mut unstructured),
                rotation_degrees: i16::arbitrary(&mut unstructured).unwrap_or(0),
                pivot_x: Fixed::from_pixels(32),
                pivot_y: Fixed::from_pixels(32),
                hyperlink: bool::arbitrary(&mut unstructured)
                    .unwrap_or(false)
                    .then(|| support::bounded_text(&mut unstructured, 128)),
            }),
        ],
    };

    let _ = render_scene_svg(&scene, 4 << 20);
    let mut workbook = Workbook::new();
    workbook.add_sheet("backend").write(0, 0, "x");
    let options = PrintOptions {
        limits: PrintLimits {
            max_logical_pages: 2,
            max_pages: 2,
            max_total_scene_nodes: 4_096,
            max_backend_commands: 4_096,
            max_pdf_bytes: 4 << 20,
            max_raster_dimension: 256,
            max_raster_pixels: 65_536,
            max_png_bytes_per_page: 1 << 20,
        },
        ..PrintOptions::default()
    };
    if let Ok(mut document) = build_print_document(&workbook, 0, &options) {
        if let Some(page) = document.pages.first_mut() {
            page.scene = scene;
        }
        let _ = render_print_document_pdf(&document);
        let _ = render_print_document_png_pages(&document, 36);
    }
});
