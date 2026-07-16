//! Bounded deterministic per-page PNG rasterization.

use tiny_skia::{
    FillRule, FilterQuality, IntSize, Mask, Paint, Path, PathBuilder, Pixmap, PixmapPaint,
    Rect as SkRect, Stroke, Transform,
};

use crate::error::{LimitKind, RenderError};
use crate::print::{PrintDocument, PrintPage};
use crate::scene::{
    backend_image_trace, BackendGeometryTrace, BackendGlyphTraceBuilder, BackendNodeTrace,
    BackendPathTraceBuilder, Fixed, ImageNode, PathCommand, PathNode, Rect, Rgb, SceneNode,
    FIXED_UNITS_PER_PIXEL,
};

/// Rasterize one print page at an explicit integer DPI.
///
/// The page is preflighted before pixel allocation and encoded independently,
/// so callers never need to retain a document-sized raster surface. Text must
/// already be a shaped glyph-outline node; pass a verified font pack while
/// building the print document.
pub fn render_print_page_png(
    page: &PrintPage,
    dpi: u32,
    document: &PrintDocument,
) -> Result<Vec<u8>, RenderError> {
    render_print_page_png_impl(page, dpi, document, None)
}

#[cfg(test)]
pub(crate) fn render_print_page_png_with_trace(
    page: &PrintPage,
    dpi: u32,
    document: &PrintDocument,
) -> Result<(Vec<u8>, BackendGeometryTrace), RenderError> {
    let mut trace = BackendGeometryTrace::new(&page.scene);
    let png = render_print_page_png_impl(page, dpi, document, Some(&mut trace))?;
    Ok((png, trace))
}

fn render_print_page_png_impl(
    page: &PrintPage,
    dpi: u32,
    document: &PrintDocument,
    mut trace: Option<&mut BackendGeometryTrace>,
) -> Result<Vec<u8>, RenderError> {
    if !(36..=1_200).contains(&dpi) {
        return Err(RenderError::Backend {
            reason: "png_dpi_out_of_range",
        });
    }
    let width = raster_dimension(page.scene.width, dpi)?;
    let height = raster_dimension(page.scene.height, dpi)?;
    enforce(
        LimitKind::RasterDimension,
        u64::from(document.limits.max_raster_dimension),
        u64::from(width.max(height)),
    )?;
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::RasterPixels,
        document.limits.max_raster_pixels,
        pixels,
    )?;
    let mut commands = 0_u64;
    for node in &page.scene.nodes {
        commands = commands
            .checked_add(match node {
                SceneNode::Rect(_) => 1,
                SceneNode::Line(_) => 2,
                SceneNode::Path(node) => node.commands.len() as u64,
                SceneNode::Image(_) => 1,
                SceneNode::Text(_) => {
                    return Err(RenderError::Backend {
                        reason: "png_requires_outlined_text",
                    });
                }
                SceneNode::GlyphRun(node) => {
                    if !node.metadata_is_valid() {
                        return Err(RenderError::Backend {
                            reason: "invalid_glyph_metadata",
                        });
                    }
                    node.commands.len() as u64 + node.decorations.len() as u64 * 2
                }
            })
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    enforce(
        LimitKind::BackendCommands,
        document.limits.max_backend_commands,
        commands,
    )?;

    let mut pixmap = Pixmap::new(width, height).ok_or(RenderError::Backend {
        reason: "png_surface_allocation",
    })?;
    pixmap.fill(color(page.scene.background));
    let scale = dpi as f32 / 96.0;
    let base_transform = Transform::from_scale(scale, scale);
    for node in &page.scene.nodes {
        match node {
            SceneNode::Rect(node) => {
                let Some(rect) = sk_rect(node.rect) else {
                    continue;
                };
                if let Some(fill) = node.fill {
                    let paint = solid_paint(fill, false);
                    pixmap.fill_rect(rect, &paint, base_transform, None);
                }
                if let Some(stroke_color) = node.stroke {
                    let path = rect_path(node.rect)?;
                    let paint = solid_paint(stroke_color, false);
                    let stroke = Stroke {
                        width: fixed_f32(node.stroke_width),
                        ..Stroke::default()
                    };
                    pixmap.stroke_path(&path, &paint, &stroke, base_transform, None);
                }
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Rect(node.clone()));
                }
            }
            SceneNode::Line(node) => {
                let mut builder = PathBuilder::new();
                builder.move_to(fixed_f32(node.x1), fixed_f32(node.y1));
                builder.line_to(fixed_f32(node.x2), fixed_f32(node.y2));
                let Some(path) = builder.finish() else {
                    continue;
                };
                let paint = solid_paint(node.color, true);
                let stroke = Stroke {
                    width: fixed_f32(node.width),
                    ..Stroke::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, base_transform, None);
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Line(node.clone()));
                }
            }
            SceneNode::Path(node) => {
                let mut path_trace = trace.is_some().then(|| BackendPathTraceBuilder::new(node));
                draw_path_node(&mut pixmap, node, base_transform, path_trace.as_mut())?;
                if let (Some(trace), Some(path_trace)) = (trace.as_deref_mut(), path_trace) {
                    trace.push(BackendNodeTrace::Path(
                        path_trace.finish().map_err(trace_error)?,
                    ));
                }
            }
            SceneNode::Image(node) => {
                draw_image_node(&mut pixmap, node, dpi)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Image(backend_image_trace(node)));
                }
            }
            SceneNode::Text(_) => unreachable!("preflight rejects approximate text"),
            SceneNode::GlyphRun(node) => {
                let mut glyph_trace = trace.is_some().then(|| BackendGlyphTraceBuilder::new(node));
                let mask = clip_mask(width, height, node.clip_bounds, base_transform)?;
                if let Some(trace) = glyph_trace.as_mut() {
                    trace.record_clip(node.clip_bounds).map_err(trace_error)?;
                    // PNG cannot serialize an interactive annotation, but it
                    // explicitly acknowledges the same bounded link rectangle
                    // while replaying the node.
                    if let Some(target) = node.hyperlink.as_deref() {
                        trace
                            .record_link(node.clip_bounds, target)
                            .map_err(trace_error)?;
                    }
                }
                let transform =
                    glyph_transform(scale, node.rotation_degrees, node.pivot_x, node.pivot_y);
                for span in &node.paints {
                    let start =
                        usize::try_from(span.command_start).map_err(|_| RenderError::Backend {
                            reason: "invalid_glyph_metadata",
                        })?;
                    let end =
                        usize::try_from(span.command_end).map_err(|_| RenderError::Backend {
                            reason: "invalid_glyph_metadata",
                        })?;
                    let commands = node.commands.get(start..end).ok_or(RenderError::Backend {
                        reason: "invalid_glyph_metadata",
                    })?;
                    if let Some(path) = scene_path_with_trace(commands, |offset, command| {
                        if let Some(trace) = glyph_trace.as_mut() {
                            trace
                                .record_command(
                                    span.command_start + offset as u64,
                                    command,
                                    span.color,
                                )
                                .map_err(trace_error)?;
                        }
                        Ok(())
                    })? {
                        let paint = solid_paint(span.color, true);
                        pixmap.fill_path(&path, &paint, FillRule::Winding, transform, Some(&mask));
                    }
                }
                for line in &node.decorations {
                    let mut builder = PathBuilder::new();
                    builder.move_to(fixed_f32(line.x1), fixed_f32(line.y1));
                    builder.line_to(fixed_f32(line.x2), fixed_f32(line.y2));
                    if let Some(path) = builder.finish() {
                        let decoration_paint = solid_paint(line.color, true);
                        let stroke = Stroke {
                            width: fixed_f32(line.width),
                            ..Stroke::default()
                        };
                        pixmap.stroke_path(
                            &path,
                            &decoration_paint,
                            &stroke,
                            transform,
                            Some(&mask),
                        );
                        if let Some(trace) = glyph_trace.as_mut() {
                            trace.record_decoration(line).map_err(trace_error)?;
                        }
                    }
                }
                if let (Some(trace), Some(glyph_trace)) = (trace.as_deref_mut(), glyph_trace) {
                    trace.push(BackendNodeTrace::Glyph(
                        glyph_trace.finish().map_err(trace_error)?,
                    ));
                }
            }
        }
    }
    let png = pixmap.encode_png().map_err(|_| RenderError::Backend {
        reason: "png_encoding",
    })?;
    enforce(
        LimitKind::PngBytes,
        document.limits.max_png_bytes_per_page,
        png.len() as u64,
    )?;
    Ok(png)
}

fn draw_path_node(
    pixmap: &mut Pixmap,
    node: &PathNode,
    transform: Transform,
    mut trace: Option<&mut BackendPathTraceBuilder<'_>>,
) -> Result<(), RenderError> {
    let Some(path) = scene_path_with_trace(&node.commands, |index, command| {
        if let Some(trace) = trace.as_deref_mut() {
            trace.record(index, command).map_err(trace_error)?;
        }
        Ok(())
    })?
    else {
        return Ok(());
    };
    if let Some(fill) = node.fill {
        let paint = solid_paint(fill, true);
        pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
    }
    if let Some(stroke_color) = node.stroke {
        let paint = solid_paint(stroke_color, true);
        let stroke = Stroke {
            width: fixed_f32(node.stroke_width),
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, transform, None);
    }
    Ok(())
}

fn draw_image_node(pixmap: &mut Pixmap, node: &ImageNode, dpi: u32) -> Result<(), RenderError> {
    let expected = u64::from(node.pixel_width)
        .checked_mul(u64::from(node.pixel_height))
        .and_then(|value| value.checked_mul(4))
        .ok_or(RenderError::CoordinateOverflow)?;
    if expected != node.rgba.len() as u64 {
        return Err(RenderError::Backend {
            reason: "invalid_image_rgba_length",
        });
    }
    let mut premultiplied = Vec::with_capacity(node.rgba.len());
    for rgba in node.rgba.chunks_exact(4) {
        let alpha = u16::from(rgba[3]);
        for channel in &rgba[..3] {
            premultiplied.push(((u16::from(*channel) * alpha + 127) / 255) as u8);
        }
        premultiplied.push(rgba[3]);
    }
    let size =
        IntSize::from_wh(node.pixel_width, node.pixel_height).ok_or(RenderError::Backend {
            reason: "invalid_image_dimensions",
        })?;
    let source = Pixmap::from_vec(premultiplied, size).ok_or(RenderError::Backend {
        reason: "image_pixmap_allocation",
    })?;
    let dpi_scale = dpi as f64 / 96.0;
    let source_width = f64::from(node.pixel_width);
    let source_height = f64::from(node.pixel_height);
    let scale_x = fixed_f64(node.rect.width) / source_width;
    let scale_y = fixed_f64(node.rect.height) / source_height;
    let left = fixed_f64(node.rect.x);
    let top = fixed_f64(node.rect.y);
    let center_x = left + fixed_f64(node.rect.width) / 2.0;
    let center_y = top + fixed_f64(node.rect.height) / 2.0;
    let radians = f64::from(node.rotation_mdeg) * std::f64::consts::PI / 180_000.0;
    let cosine = radians.cos();
    let sine = radians.sin();
    let translate_x = center_x + cosine * (left - center_x) - sine * (top - center_y);
    let translate_y = center_y + sine * (left - center_x) + cosine * (top - center_y);
    let transform = Transform::from_row(
        (cosine * scale_x * dpi_scale) as f32,
        (sine * scale_x * dpi_scale) as f32,
        (-sine * scale_y * dpi_scale) as f32,
        (cosine * scale_y * dpi_scale) as f32,
        (translate_x * dpi_scale) as f32,
        (translate_y * dpi_scale) as f32,
    );
    let paint = PixmapPaint {
        quality: FilterQuality::Bilinear,
        ..PixmapPaint::default()
    };
    pixmap.draw_pixmap(0, 0, source.as_ref(), &paint, transform, None);
    Ok(())
}

fn fixed_f64(value: Fixed) -> f64 {
    value.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64
}

/// Rasterize every page independently at one explicit DPI.
pub fn render_print_document_png_pages(
    document: &PrintDocument,
    dpi: u32,
) -> Result<Vec<Vec<u8>>, RenderError> {
    document
        .pages
        .iter()
        .map(|page| render_print_page_png(page, dpi, document))
        .collect()
}

fn scene_path_with_trace<F>(
    commands: &[PathCommand],
    mut record: F,
) -> Result<Option<Path>, RenderError>
where
    F: FnMut(usize, PathCommand) -> Result<(), RenderError>,
{
    if commands.is_empty() {
        return Ok(None);
    }
    let mut builder = PathBuilder::new();
    let mut has_current = false;
    for (index, command) in commands.iter().enumerate() {
        match *command {
            PathCommand::MoveTo { x, y } => {
                builder.move_to(fixed_f32(x), fixed_f32(y));
                has_current = true;
            }
            PathCommand::LineTo { x, y } => {
                if !has_current {
                    return Err(RenderError::Backend {
                        reason: "png_path_without_move",
                    });
                }
                builder.line_to(fixed_f32(x), fixed_f32(y));
            }
            PathCommand::QuadraticTo {
                control_x,
                control_y,
                x,
                y,
            } => {
                if !has_current {
                    return Err(RenderError::Backend {
                        reason: "png_path_without_move",
                    });
                }
                builder.quad_to(
                    fixed_f32(control_x),
                    fixed_f32(control_y),
                    fixed_f32(x),
                    fixed_f32(y),
                );
            }
            PathCommand::CubicTo {
                control1_x,
                control1_y,
                control2_x,
                control2_y,
                x,
                y,
            } => {
                if !has_current {
                    return Err(RenderError::Backend {
                        reason: "png_path_without_move",
                    });
                }
                builder.cubic_to(
                    fixed_f32(control1_x),
                    fixed_f32(control1_y),
                    fixed_f32(control2_x),
                    fixed_f32(control2_y),
                    fixed_f32(x),
                    fixed_f32(y),
                );
            }
            PathCommand::Close => builder.close(),
        }
        record(index, *command)?;
    }
    Ok(builder.finish())
}

fn trace_error(reason: &'static str) -> RenderError {
    RenderError::Backend { reason }
}

fn rect_path(rect: Rect) -> Result<Path, RenderError> {
    let mut builder = PathBuilder::new();
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    builder.move_to(fixed_f32(rect.x), fixed_f32(rect.y));
    builder.line_to(fixed_f32(right), fixed_f32(rect.y));
    builder.line_to(fixed_f32(right), fixed_f32(bottom));
    builder.line_to(fixed_f32(rect.x), fixed_f32(bottom));
    builder.close();
    builder.finish().ok_or(RenderError::Backend {
        reason: "png_rectangle_path",
    })
}

fn clip_mask(
    width: u32,
    height: u32,
    rect: Rect,
    transform: Transform,
) -> Result<Mask, RenderError> {
    let mut mask = Mask::new(width, height).ok_or(RenderError::Backend {
        reason: "png_clip_allocation",
    })?;
    let path = rect_path(rect)?;
    mask.fill_path(&path, FillRule::Winding, false, transform);
    Ok(mask)
}

fn glyph_transform(scale: f32, degrees: i16, pivot_x: Fixed, pivot_y: Fixed) -> Transform {
    if degrees == 0 {
        return Transform::from_scale(scale, scale);
    }
    let radians = f32::from(degrees).to_radians();
    let cosine = radians.cos();
    let sine = radians.sin();
    let x = fixed_f32(pivot_x);
    let y = fixed_f32(pivot_y);
    let tx = x - cosine * x + sine * y;
    let ty = y - sine * x - cosine * y;
    Transform::from_row(
        scale * cosine,
        scale * sine,
        -scale * sine,
        scale * cosine,
        scale * tx,
        scale * ty,
    )
}

fn sk_rect(rect: Rect) -> Option<SkRect> {
    SkRect::from_xywh(
        fixed_f32(rect.x),
        fixed_f32(rect.y),
        fixed_f32(rect.width),
        fixed_f32(rect.height),
    )
}

fn color(rgb: Rgb) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(rgb.red, rgb.green, rgb.blue, 255)
}

fn solid_paint(rgb: Rgb, anti_alias: bool) -> Paint<'static> {
    let mut paint = Paint {
        anti_alias,
        ..Paint::default()
    };
    paint.set_color(color(rgb));
    paint
}

fn fixed_f32(value: Fixed) -> f32 {
    value.raw() as f32 / FIXED_UNITS_PER_PIXEL as f32
}

fn raster_dimension(value: Fixed, dpi: u32) -> Result<u32, RenderError> {
    if value.raw() <= 0 {
        return Err(RenderError::Backend {
            reason: "png_empty_dimension",
        });
    }
    let numerator = i128::from(value.raw())
        .checked_mul(i128::from(dpi))
        .ok_or(RenderError::CoordinateOverflow)?;
    let denominator = i128::from(FIXED_UNITS_PER_PIXEL * 96);
    let pixels = numerator
        .checked_add(denominator - 1)
        .and_then(|number| number.checked_div(denominator))
        .ok_or(RenderError::CoordinateOverflow)?;
    u32::try_from(pixels).map_err(|_| RenderError::CoordinateOverflow)
}

fn enforce(kind: LimitKind, limit: u64, actual: u64) -> Result<(), RenderError> {
    if actual > limit {
        Err(RenderError::LimitExceeded {
            kind,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn raster_dimensions_round_up_exactly() {
        assert_eq!(raster_dimension(Fixed::from_pixels(96), 300).unwrap(), 300);
        assert_eq!(raster_dimension(Fixed::from_raw(1), 96).unwrap(), 1);
    }

    #[test]
    fn rgba_image_alpha_composites_into_raster_surface() {
        let mut pixmap = Pixmap::new(4, 4).unwrap();
        pixmap.fill(tiny_skia::Color::WHITE);
        draw_image_node(
            &mut pixmap,
            &ImageNode {
                rect: Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(4),
                    height: Fixed::from_pixels(4),
                },
                pixel_width: 1,
                pixel_height: 1,
                rgba: Arc::from([255, 0, 0, 128]),
                rotation_mdeg: 0,
                alt_text: None,
            },
            96,
        )
        .unwrap();
        let pixel = pixmap.pixel(2, 2).unwrap();
        assert_eq!(pixel.alpha(), 255);
        assert_eq!(pixel.red(), 255);
        assert!((126..=128).contains(&pixel.green()));
        assert!((126..=128).contains(&pixel.blue()));
    }
}
