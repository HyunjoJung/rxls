#![no_main]
//! Replay arbitrary bounded scene nodes through deterministic SVG serialization.

mod support;

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use rxls_render::{
    render_scene_svg, Fixed, LineNode, Rect, RectNode, Rgb, Scene, SceneNode, TextAnchor,
    TextBaseline, TextNode, TextStyle,
};

fn fixed(unstructured: &mut Unstructured<'_>) -> Fixed {
    Fixed::from_raw(
        unstructured
            .int_in_range(-2_000_000i64..=2_000_000)
            .unwrap_or(0),
    )
}

fn color(unstructured: &mut Unstructured<'_>) -> Rgb {
    Rgb::new(
        u8::arbitrary(unstructured).unwrap_or(0),
        u8::arbitrary(unstructured).unwrap_or(0),
        u8::arbitrary(unstructured).unwrap_or(0),
    )
}

fn rect(unstructured: &mut Unstructured<'_>) -> Rect {
    Rect {
        x: fixed(unstructured),
        y: fixed(unstructured),
        width: fixed(unstructured),
        height: fixed(unstructured),
    }
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(support::input(data));
    let mut scene = Scene {
        title: support::bounded_text(&mut unstructured, 512),
        width: Fixed::from_raw(unstructured.int_in_range(1i64..=4_194_304).unwrap_or(1)),
        height: Fixed::from_raw(unstructured.int_in_range(1i64..=4_194_304).unwrap_or(1)),
        background: color(&mut unstructured),
        nodes: Vec::new(),
    };
    let nodes = unstructured.int_in_range(0u16..=256).unwrap_or(0);
    for _ in 0..nodes {
        if unstructured.is_empty() {
            break;
        }
        scene
            .nodes
            .push(match unstructured.int_in_range(0u8..=2).unwrap_or(0) {
                0 => SceneNode::Rect(RectNode {
                    rect: rect(&mut unstructured),
                    fill: bool::arbitrary(&mut unstructured)
                        .unwrap_or(false)
                        .then(|| color(&mut unstructured)),
                    stroke: bool::arbitrary(&mut unstructured)
                        .unwrap_or(false)
                        .then(|| color(&mut unstructured)),
                    stroke_width: fixed(&mut unstructured),
                }),
                1 => SceneNode::Line(LineNode {
                    x1: fixed(&mut unstructured),
                    y1: fixed(&mut unstructured),
                    x2: fixed(&mut unstructured),
                    y2: fixed(&mut unstructured),
                    color: color(&mut unstructured),
                    width: fixed(&mut unstructured),
                }),
                _ => {
                    let bounds = rect(&mut unstructured);
                    SceneNode::Text(TextNode {
                        text: support::bounded_text(&mut unstructured, 512),
                        bounds,
                        clip_bounds: rect(&mut unstructured),
                        horizontal_padding: fixed(&mut unstructured),
                        style: TextStyle {
                            family: support::bounded_text(&mut unstructured, 128),
                            size: fixed(&mut unstructured),
                            color: color(&mut unstructured),
                            bold: bool::arbitrary(&mut unstructured).unwrap_or(false),
                            italic: bool::arbitrary(&mut unstructured).unwrap_or(false),
                            underline: bool::arbitrary(&mut unstructured).unwrap_or(false),
                            strikethrough: bool::arbitrary(&mut unstructured).unwrap_or(false),
                            anchor: match unstructured.int_in_range(0u8..=2).unwrap_or(0) {
                                0 => TextAnchor::Start,
                                1 => TextAnchor::Middle,
                                _ => TextAnchor::End,
                            },
                            baseline: match unstructured.int_in_range(0u8..=2).unwrap_or(0) {
                                0 => TextBaseline::Top,
                                1 => TextBaseline::Middle,
                                _ => TextBaseline::Bottom,
                            },
                            rotation_degrees: i16::arbitrary(&mut unstructured).unwrap_or(0),
                        },
                        hyperlink: bool::arbitrary(&mut unstructured)
                            .unwrap_or(false)
                            .then(|| support::bounded_text(&mut unstructured, 256)),
                    })
                }
            });
    }
    let limit = unstructured.int_in_range(0u64..=4 << 20).unwrap_or(4 << 20);
    let _ = render_scene_svg(&scene, limit);
});
