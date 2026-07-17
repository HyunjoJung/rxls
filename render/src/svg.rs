//! Deterministic bounded SVG serialization.

use crate::error::{LimitKind, RenderError};
use crate::scene::{
    backend_image_trace, backend_text_trace, format_fixed, BackendGeometryTrace, BackendGlyphTrace,
    BackendGlyphTraceBuilder, BackendNodeTrace, BackendPathTrace, BackendPathTraceBuilder, Fixed,
    GlyphRunNode, ImageNode, LineNode, PathCommand, PathNode, Rect, RectNode, Rgb, Scene,
    SceneNode, TextAnchor, TextBaseline, TextNode,
};

const MAX_CLIP_GROUP_DEPTH: usize = 64;

/// Serialize a backend-neutral scene as deterministic SVG.
pub fn render_scene_svg(scene: &Scene, max_output_bytes: u64) -> Result<String, RenderError> {
    render_scene_svg_impl(scene, max_output_bytes, None)
}

#[cfg(test)]
pub(crate) fn render_scene_svg_with_trace(
    scene: &Scene,
    max_output_bytes: u64,
) -> Result<(String, BackendGeometryTrace), RenderError> {
    let mut trace = BackendGeometryTrace::new(scene);
    let svg = render_scene_svg_impl(scene, max_output_bytes, Some(&mut trace))?;
    Ok((svg, trace))
}

fn render_scene_svg_impl(
    scene: &Scene,
    max_output_bytes: u64,
    trace: Option<&mut BackendGeometryTrace>,
) -> Result<String, RenderError> {
    let mut out = BoundedString::new(max_output_bytes);
    out.push("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n")?;
    out.push("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"")?;
    out.push(&format_fixed(scene.width))?;
    out.push("\" height=\"")?;
    out.push(&format_fixed(scene.height))?;
    out.push("\" viewBox=\"0 0 ")?;
    out.push(&format_fixed(scene.width))?;
    out.push(" ")?;
    out.push(&format_fixed(scene.height))?;
    out.push("\" role=\"img\" overflow=\"hidden\">\n<title>")?;
    push_xml_escaped(&mut out, &scene.title, false)?;
    out.push("</title>\n")?;

    let mut clip_bounds = Vec::new();
    collect_clip_bounds(&scene.nodes, 0, &mut clip_bounds)?;
    if !clip_bounds.is_empty() {
        out.push("<defs>\n")?;
        for (clip_index, bounds) in clip_bounds.into_iter().enumerate() {
            out.push("<clipPath id=\"clip-")?;
            out.push(&clip_index.to_string())?;
            out.push("\"><rect")?;
            push_rect_geometry(&mut out, bounds)?;
            out.push("/></clipPath>\n")?;
        }
        out.push("</defs>\n")?;
    }

    out.push("<rect width=\"100%\" height=\"100%\" fill=\"")?;
    push_rgb(&mut out, scene.background)?;
    out.push("\"/>\n")?;

    let mut clip_index = 0_usize;
    push_scene_nodes(&mut out, &scene.nodes, &mut clip_index, trace, 0)?;
    out.push("</svg>\n")?;
    Ok(out.finish())
}

fn collect_clip_bounds(
    nodes: &[SceneNode],
    depth: usize,
    output: &mut Vec<Rect>,
) -> Result<(), RenderError> {
    for node in nodes {
        match node {
            SceneNode::ClipGroup(group) => {
                if depth >= MAX_CLIP_GROUP_DEPTH {
                    return Err(RenderError::Backend {
                        reason: "svg_clip_group_depth",
                    });
                }
                output.push(group.clip);
                collect_clip_bounds(&group.nodes, depth + 1, output)?;
            }
            SceneNode::Text(text) => output.push(text.clip_bounds),
            SceneNode::GlyphRun(glyphs) => output.push(glyphs.clip_bounds),
            SceneNode::Rect(_) | SceneNode::Line(_) | SceneNode::Path(_) | SceneNode::Image(_) => {}
        }
    }
    Ok(())
}

fn push_scene_nodes(
    out: &mut BoundedString,
    nodes: &[SceneNode],
    clip_index: &mut usize,
    mut trace: Option<&mut BackendGeometryTrace>,
    depth: usize,
) -> Result<(), RenderError> {
    for node in nodes {
        match node {
            SceneNode::ClipGroup(group) => {
                if depth >= MAX_CLIP_GROUP_DEPTH {
                    return Err(RenderError::Backend {
                        reason: "svg_clip_group_depth",
                    });
                }
                let group_clip_index = *clip_index;
                *clip_index = (*clip_index)
                    .checked_add(1)
                    .ok_or(RenderError::CoordinateOverflow)?;
                out.push("<g clip-path=\"url(#clip-")?;
                out.push(&group_clip_index.to_string())?;
                out.push(")\">\n")?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::ClipStart(group.clip));
                }
                push_scene_nodes(
                    out,
                    &group.nodes,
                    clip_index,
                    trace.as_deref_mut(),
                    depth + 1,
                )?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::ClipEnd);
                }
                out.push("</g>\n")?;
            }
            SceneNode::Rect(rect) => {
                push_rect(out, rect)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Rect(rect.clone()));
                }
            }
            SceneNode::Line(line) => {
                push_line(out, line)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Line(line.clone()));
                }
            }
            SceneNode::Path(path) => {
                let path_trace = push_path_node(out, path, trace.is_some())?;
                if let (Some(trace), Some(path_trace)) = (trace.as_deref_mut(), path_trace) {
                    trace.push(BackendNodeTrace::Path(path_trace));
                }
            }
            SceneNode::Image(image) => {
                push_image(out, image)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Image(backend_image_trace(image)));
                }
            }
            SceneNode::Text(text) => {
                push_text(out, text, *clip_index)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Text(backend_text_trace(
                        text,
                        text.hyperlink.as_deref(),
                    )));
                }
                *clip_index = (*clip_index)
                    .checked_add(1)
                    .ok_or(RenderError::CoordinateOverflow)?;
            }
            SceneNode::GlyphRun(glyphs) => {
                let glyph_trace = push_glyph_run(out, glyphs, *clip_index, trace.is_some())?;
                if let (Some(trace), Some(glyph_trace)) = (trace.as_deref_mut(), glyph_trace) {
                    trace.push(BackendNodeTrace::Glyph(glyph_trace));
                }
                *clip_index = (*clip_index)
                    .checked_add(1)
                    .ok_or(RenderError::CoordinateOverflow)?;
            }
        }
    }
    Ok(())
}

fn push_path_node(
    out: &mut BoundedString,
    node: &PathNode,
    tracing: bool,
) -> Result<Option<BackendPathTrace>, RenderError> {
    let mut trace = tracing.then(|| BackendPathTraceBuilder::new(node));
    out.push("<path d=\"")?;
    push_path_commands(out, &node.commands, |index, command| {
        if let Some(trace) = trace.as_mut() {
            trace.record(index, command).map_err(trace_error)?;
        }
        Ok(())
    })?;
    out.push("\" fill=\"")?;
    match node.fill {
        Some(color) => push_rgb(out, color)?,
        None => out.push("none")?,
    }
    out.push("\"")?;
    if let Some(stroke) = node.stroke {
        out.push(" stroke=\"")?;
        push_rgb(out, stroke)?;
        out.push("\" stroke-width=\"")?;
        out.push(&format_fixed(node.stroke_width))?;
        out.push("\"")?;
    }
    out.push("/>\n")?;
    trace
        .map(BackendPathTraceBuilder::finish)
        .transpose()
        .map_err(trace_error)
}

fn push_image(out: &mut BoundedString, node: &ImageNode) -> Result<(), RenderError> {
    let png = encode_rgba_png(node)?;
    let center_x = Fixed::from_raw(
        node.rect
            .x
            .raw()
            .checked_add(node.rect.width.raw() / 2)
            .ok_or(RenderError::CoordinateOverflow)?,
    );
    let center_y = Fixed::from_raw(
        node.rect
            .y
            .raw()
            .checked_add(node.rect.height.raw() / 2)
            .ok_or(RenderError::CoordinateOverflow)?,
    );
    out.push("<g role=\"img\"")?;
    if let Some(alt) = node.alt_text.as_deref() {
        out.push(" aria-label=\"")?;
        push_xml_escaped(out, alt, true)?;
        out.push("\"")?;
    }
    if node.rotation_mdeg != 0 {
        out.push(" transform=\"rotate(")?;
        out.push(&format_millidegrees(node.rotation_mdeg))?;
        out.push(" ")?;
        out.push(&format_fixed(center_x))?;
        out.push(" ")?;
        out.push(&format_fixed(center_y))?;
        out.push(")\"")?;
    }
    out.push("><image")?;
    push_rect_geometry(out, node.rect)?;
    out.push(" preserveAspectRatio=\"none\" href=\"data:image/png;base64,")?;
    push_base64(out, &png)?;
    out.push("\"/></g>\n")
}

fn encode_rgba_png(node: &ImageNode) -> Result<Vec<u8>, RenderError> {
    let expected = u64::from(node.pixel_width)
        .checked_mul(u64::from(node.pixel_height))
        .and_then(|value| value.checked_mul(4))
        .ok_or(RenderError::CoordinateOverflow)?;
    if expected != node.rgba.len() as u64 {
        return Err(RenderError::Backend {
            reason: "invalid_image_rgba_length",
        });
    }
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, node.pixel_width, node.pixel_height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|_| RenderError::Backend {
            reason: "svg_image_png_header",
        })?;
        writer
            .write_image_data(&node.rgba)
            .map_err(|_| RenderError::Backend {
                reason: "svg_image_png_encoding",
            })?;
    }
    Ok(encoded)
}

fn push_base64(out: &mut BoundedString, bytes: &[u8]) -> Result<(), RenderError> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let encoded = [
            TABLE[(first >> 2) as usize],
            TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize],
            if chunk.len() > 1 {
                TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize]
            } else {
                b'='
            },
            if chunk.len() > 2 {
                TABLE[(third & 0x3f) as usize]
            } else {
                b'='
            },
        ];
        out.push(std::str::from_utf8(&encoded).expect("base64 output is ASCII"))?;
    }
    Ok(())
}

fn format_millidegrees(value: i32) -> String {
    let negative = value < 0;
    let magnitude = value.unsigned_abs();
    let whole = magnitude / 1_000;
    let fraction = magnitude % 1_000;
    let mut output = if negative {
        format!("-{whole}")
    } else {
        whole.to_string()
    };
    if fraction != 0 {
        output.push('.');
        output.push_str(&format!("{fraction:03}"));
        while output.ends_with('0') {
            output.pop();
        }
    }
    output
}

fn push_glyph_run(
    out: &mut BoundedString,
    node: &GlyphRunNode,
    clip_index: usize,
    tracing: bool,
) -> Result<Option<BackendGlyphTrace>, RenderError> {
    if !node.metadata_is_valid() {
        return Err(RenderError::Backend {
            reason: "invalid_glyph_metadata",
        });
    }
    let mut trace = tracing.then(|| BackendGlyphTraceBuilder::new(node));
    if let Some(target) = node.hyperlink.as_deref() {
        out.push("<a href=\"")?;
        push_xml_escaped(out, target, true)?;
        out.push("\">")?;
        if let Some(trace) = trace.as_mut() {
            trace
                .record_link(node.clip_bounds, target)
                .map_err(trace_error)?;
        }
    }
    let visible_label = visible_glyph_label(node)?;
    out.push("<g role=\"text\" aria-label=\"")?;
    push_xml_escaped(out, &node.text, true)?;
    out.push("\" data-rxls-visible-label=\"")?;
    push_xml_escaped(out, &visible_label, true)?;
    out.push("\" fill=\"")?;
    push_rgb(out, node.color)?;
    out.push("\" clip-path=\"url(#clip-")?;
    out.push(&clip_index.to_string())?;
    out.push(")\"")?;
    if let Some(trace) = trace.as_mut() {
        trace.record_clip(node.clip_bounds).map_err(trace_error)?;
    }
    if node.rotation_degrees != 0 {
        out.push(" transform=\"rotate(")?;
        out.push(&node.rotation_degrees.to_string())?;
        out.push(" ")?;
        out.push(&format_fixed(node.pivot_x))?;
        out.push(" ")?;
        out.push(&format_fixed(node.pivot_y))?;
        out.push(")\"")?;
    }
    out.push(">")?;
    for paint in &node.paints {
        let start = usize::try_from(paint.command_start).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let end = usize::try_from(paint.command_end).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let commands = node.commands.get(start..end).ok_or(RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        out.push("<path d=\"")?;
        push_path_commands(out, commands, |offset, command| {
            if let Some(trace) = trace.as_mut() {
                trace
                    .record_command(paint.command_start + offset as u64, command, paint.color)
                    .map_err(trace_error)?;
            }
            Ok(())
        })?;
        out.push("\" fill=\"")?;
        push_rgb(out, paint.color)?;
        out.push("\"/>")?;
    }
    for decoration in &node.decorations {
        push_line(out, decoration)?;
        if let Some(trace) = trace.as_mut() {
            trace.record_decoration(decoration).map_err(trace_error)?;
        }
    }
    out.push("</g>")?;
    if node.hyperlink.is_some() {
        out.push("</a>")?;
    }
    out.push("\n")?;
    trace
        .map(BackendGlyphTraceBuilder::finish)
        .transpose()
        .map_err(trace_error)
}

/// Return the logical source text whose glyph outlines have non-empty overlap
/// with the cell clip. The full source remains in `aria-label`; this bounded
/// derivative lets parity tooling compare what can actually be painted.
fn visible_glyph_label(node: &GlyphRunNode) -> Result<String, RenderError> {
    let clip_right = node
        .clip_bounds
        .x
        .raw()
        .checked_add(node.clip_bounds.width.raw())
        .ok_or(RenderError::CoordinateOverflow)?;
    let clip_bottom = node
        .clip_bounds
        .y
        .raw()
        .checked_add(node.clip_bounds.height.raw())
        .ok_or(RenderError::CoordinateOverflow)?;
    let clip = (
        node.clip_bounds.x.raw(),
        node.clip_bounds.y.raw(),
        clip_right,
        clip_bottom,
    );
    if clip.2 <= clip.0 || clip.3 <= clip.1 {
        return Ok(String::new());
    }

    let mut ranges = Vec::new();
    for cluster in &node.clusters {
        let start = usize::try_from(cluster.command_start).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let end = usize::try_from(cluster.command_end).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let commands = node.commands.get(start..end).ok_or(RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        if glyph_commands_intersect_clip(commands, clip) {
            ranges.push((cluster.source_start, cluster.source_end));
        }
    }
    if ranges.is_empty() {
        return Ok(String::new());
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut().filter(|last| start <= last.1) {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }

    let first = merged[0].0;
    let last = merged.last().expect("visible ranges are non-empty").1;
    let mut visible = String::with_capacity(node.text.len());
    let mut range_index = 0_usize;
    let mut omitted_between_visible_ranges = false;
    for (byte_start, character) in node.text.char_indices() {
        let byte_start = byte_start as u64;
        let byte_end = byte_start + character.len_utf8() as u64;
        while range_index < merged.len() && merged[range_index].1 <= byte_start {
            range_index += 1;
        }
        let selected = range_index < merged.len()
            && merged[range_index].0 < byte_end
            && byte_start < merged[range_index].1;
        if selected {
            if omitted_between_visible_ranges
                && !visible.ends_with(char::is_whitespace)
                && !character.is_whitespace()
            {
                visible.push(' ');
            }
            visible.push(character);
            omitted_between_visible_ranges = false;
        } else if byte_end > first && byte_start < last {
            omitted_between_visible_ranges = true;
        }
    }
    Ok(visible)
}

fn glyph_commands_intersect_clip(commands: &[PathCommand], clip: (i64, i64, i64, i64)) -> bool {
    let mut current: Option<(i64, i64)> = None;
    let mut contour_start: Option<(i64, i64)> = None;
    let mut bounds: Option<(i64, i64, i64, i64)> = None;
    let include = |bounds: &mut Option<(i64, i64, i64, i64)>, x: i64, y: i64| {
        if let Some((min_x, min_y, max_x, max_y)) = bounds.as_mut() {
            *min_x = (*min_x).min(x);
            *min_y = (*min_y).min(y);
            *max_x = (*max_x).max(x);
            *max_y = (*max_y).max(y);
        } else {
            *bounds = Some((x, y, x, y));
        }
    };
    for command in commands {
        match *command {
            PathCommand::MoveTo { x, y } => {
                current = Some((x.raw(), y.raw()));
                contour_start = current;
            }
            PathCommand::LineTo { x, y } => {
                if let Some((current_x, current_y)) = current {
                    include(&mut bounds, current_x, current_y);
                }
                include(&mut bounds, x.raw(), y.raw());
                current = Some((x.raw(), y.raw()));
            }
            PathCommand::QuadraticTo {
                control_x,
                control_y,
                x,
                y,
            } => {
                if let Some((current_x, current_y)) = current {
                    include(&mut bounds, current_x, current_y);
                }
                include(&mut bounds, control_x.raw(), control_y.raw());
                include(&mut bounds, x.raw(), y.raw());
                current = Some((x.raw(), y.raw()));
            }
            PathCommand::CubicTo {
                control1_x,
                control1_y,
                control2_x,
                control2_y,
                x,
                y,
            } => {
                if let Some((current_x, current_y)) = current {
                    include(&mut bounds, current_x, current_y);
                }
                include(&mut bounds, control1_x.raw(), control1_y.raw());
                include(&mut bounds, control2_x.raw(), control2_y.raw());
                include(&mut bounds, x.raw(), y.raw());
                current = Some((x.raw(), y.raw()));
            }
            PathCommand::Close => {
                if let (Some((current_x, current_y)), Some((start_x, start_y))) =
                    (current, contour_start)
                {
                    include(&mut bounds, current_x, current_y);
                    include(&mut bounds, start_x, start_y);
                    current = contour_start;
                }
            }
        }
    }
    bounds.is_some_and(|(min_x, min_y, max_x, max_y)| {
        max_x > clip.0 && min_x < clip.2 && max_y > clip.1 && min_y < clip.3
    })
}

fn push_path_commands<F>(
    out: &mut BoundedString,
    commands: &[PathCommand],
    mut record: F,
) -> Result<(), RenderError>
where
    F: FnMut(usize, PathCommand) -> Result<(), RenderError>,
{
    for (index, command) in commands.iter().enumerate() {
        if index != 0 {
            out.push(" ")?;
        }
        match *command {
            PathCommand::MoveTo { x, y } => push_path_point(out, "M", x, y)?,
            PathCommand::LineTo { x, y } => push_path_point(out, "L", x, y)?,
            PathCommand::QuadraticTo {
                control_x,
                control_y,
                x,
                y,
            } => {
                push_path_point(out, "Q", control_x, control_y)?;
                out.push(" ")?;
                push_path_point(out, "", x, y)?;
            }
            PathCommand::CubicTo {
                control1_x,
                control1_y,
                control2_x,
                control2_y,
                x,
                y,
            } => {
                push_path_point(out, "C", control1_x, control1_y)?;
                out.push(" ")?;
                push_path_point(out, "", control2_x, control2_y)?;
                out.push(" ")?;
                push_path_point(out, "", x, y)?;
            }
            PathCommand::Close => out.push("Z")?,
        }
        record(index, *command)?;
    }
    Ok(())
}

fn trace_error(reason: &'static str) -> RenderError {
    RenderError::Backend { reason }
}

fn push_path_point(
    out: &mut BoundedString,
    prefix: &str,
    x: Fixed,
    y: Fixed,
) -> Result<(), RenderError> {
    out.push(prefix)?;
    out.push(&format_fixed(x))?;
    out.push(" ")?;
    out.push(&format_fixed(y))
}

fn push_rect(out: &mut BoundedString, node: &RectNode) -> Result<(), RenderError> {
    out.push("<rect")?;
    push_rect_geometry(out, node.rect)?;
    out.push(" fill=\"")?;
    match node.fill {
        Some(color) => push_rgb(out, color)?,
        None => out.push("none")?,
    }
    out.push("\"")?;
    if let Some(stroke) = node.stroke {
        out.push(" stroke=\"")?;
        push_rgb(out, stroke)?;
        out.push("\" stroke-width=\"")?;
        out.push(&format_fixed(node.stroke_width))?;
        out.push("\"")?;
    }
    out.push("/>\n")
}

fn push_rect_geometry(out: &mut BoundedString, rect: Rect) -> Result<(), RenderError> {
    out.push(" x=\"")?;
    out.push(&format_fixed(rect.x))?;
    out.push("\" y=\"")?;
    out.push(&format_fixed(rect.y))?;
    out.push("\" width=\"")?;
    out.push(&format_fixed(rect.width))?;
    out.push("\" height=\"")?;
    out.push(&format_fixed(rect.height))?;
    out.push("\"")
}

fn push_line(out: &mut BoundedString, node: &LineNode) -> Result<(), RenderError> {
    out.push("<line x1=\"")?;
    out.push(&format_fixed(node.x1))?;
    out.push("\" y1=\"")?;
    out.push(&format_fixed(node.y1))?;
    out.push("\" x2=\"")?;
    out.push(&format_fixed(node.x2))?;
    out.push("\" y2=\"")?;
    out.push(&format_fixed(node.y2))?;
    out.push("\" stroke=\"")?;
    push_rgb(out, node.color)?;
    out.push("\" stroke-width=\"")?;
    out.push(&format_fixed(node.width))?;
    out.push("\" stroke-linecap=\"butt\"/>\n")
}

fn push_text(
    out: &mut BoundedString,
    node: &TextNode,
    clip_index: usize,
) -> Result<(), RenderError> {
    let (x, anchor) = text_x(node)?;
    let (y, baseline) = text_y(node)?;
    if let Some(target) = node.hyperlink.as_deref() {
        out.push("<a href=\"")?;
        push_xml_escaped(out, target, true)?;
        out.push("\">")?;
    }
    out.push("<text x=\"")?;
    out.push(&format_fixed(x))?;
    out.push("\" y=\"")?;
    out.push(&format_fixed(y))?;
    out.push("\" font-family=\"")?;
    push_xml_escaped(out, &node.style.family, true)?;
    out.push("\" font-size=\"")?;
    out.push(&format_fixed(node.style.size))?;
    out.push("\" fill=\"")?;
    push_rgb(out, node.style.color)?;
    out.push("\" text-anchor=\"")?;
    out.push(anchor)?;
    out.push("\" dominant-baseline=\"")?;
    out.push(baseline)?;
    out.push("\"")?;
    if node.style.bold {
        out.push(" font-weight=\"700\"")?;
    }
    if node.style.italic {
        out.push(" font-style=\"italic\"")?;
    }
    if node.style.underline || node.style.strikethrough {
        out.push(" text-decoration=\"")?;
        if node.style.underline {
            out.push("underline")?;
        }
        if node.style.underline && node.style.strikethrough {
            out.push(" ")?;
        }
        if node.style.strikethrough {
            out.push("line-through")?;
        }
        out.push("\"")?;
    }
    if node.style.rotation_degrees != 0 {
        out.push(" transform=\"rotate(")?;
        out.push(&node.style.rotation_degrees.to_string())?;
        out.push(" ")?;
        out.push(&format_fixed(x))?;
        out.push(" ")?;
        out.push(&format_fixed(y))?;
        out.push(")\"")?;
    }
    out.push(" clip-path=\"url(#clip-")?;
    out.push(&clip_index.to_string())?;
    out.push(")\" xml:space=\"preserve\">")?;
    push_xml_escaped(out, &node.text, false)?;
    out.push("</text>")?;
    if node.hyperlink.is_some() {
        out.push("</a>")?;
    }
    out.push("\n")
}

fn text_x(node: &TextNode) -> Result<(Fixed, &'static str), RenderError> {
    let right = node
        .bounds
        .x
        .checked_add(node.bounds.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let middle = Fixed::from_raw(
        node.bounds
            .x
            .raw()
            .checked_add(node.bounds.width.raw() / 2)
            .ok_or(RenderError::CoordinateOverflow)?,
    );
    match node.style.anchor {
        TextAnchor::Start => Ok((
            node.bounds
                .x
                .checked_add(node.horizontal_padding)
                .unwrap_or(node.bounds.x),
            "start",
        )),
        TextAnchor::Middle => Ok((middle, "middle")),
        TextAnchor::End => Ok((
            right.checked_sub(node.horizontal_padding).unwrap_or(right),
            "end",
        )),
    }
}

fn text_y(node: &TextNode) -> Result<(Fixed, &'static str), RenderError> {
    let bottom = node
        .bounds
        .y
        .checked_add(node.bounds.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let middle = Fixed::from_raw(
        node.bounds
            .y
            .raw()
            .checked_add(node.bounds.height.raw() / 2)
            .ok_or(RenderError::CoordinateOverflow)?,
    );
    match node.style.baseline {
        TextBaseline::Top => Ok((node.bounds.y, "hanging")),
        TextBaseline::Middle => Ok((middle, "central")),
        TextBaseline::Bottom => Ok((bottom, "text-after-edge")),
    }
}

fn push_rgb(out: &mut BoundedString, color: Rgb) -> Result<(), RenderError> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let bytes = [
        b'#',
        HEX[(color.red >> 4) as usize],
        HEX[(color.red & 0x0f) as usize],
        HEX[(color.green >> 4) as usize],
        HEX[(color.green & 0x0f) as usize],
        HEX[(color.blue >> 4) as usize],
        HEX[(color.blue & 0x0f) as usize],
    ];
    // SAFETY is unnecessary: the byte set is statically ASCII.
    let value = std::str::from_utf8(&bytes).expect("hex output is ASCII");
    out.push(value)
}

fn push_xml_escaped(
    out: &mut BoundedString,
    value: &str,
    attribute: bool,
) -> Result<(), RenderError> {
    for ch in value.chars() {
        match ch {
            '&' => out.push("&amp;")?,
            '<' => out.push("&lt;")?,
            '>' => out.push("&gt;")?,
            '"' if attribute => out.push("&quot;")?,
            '\'' if attribute => out.push("&apos;")?,
            ch if !is_valid_xml_char(ch) => out.push("\u{fffd}")?,
            ch => {
                let mut buffer = [0_u8; 4];
                out.push(ch.encode_utf8(&mut buffer))?;
            }
        }
    }
    Ok(())
}

fn is_valid_xml_char(ch: char) -> bool {
    matches!(ch, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&ch)
        || ('\u{E000}'..='\u{FFFD}').contains(&ch)
        || ('\u{10000}'..='\u{10FFFF}').contains(&ch)
}

struct BoundedString {
    value: String,
    limit: u64,
}

impl BoundedString {
    fn new(limit: u64) -> Self {
        Self {
            value: String::new(),
            limit,
        }
    }

    fn push(&mut self, value: &str) -> Result<(), RenderError> {
        let actual = (self.value.len() as u64)
            .checked_add(value.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        if actual > self.limit {
            return Err(RenderError::LimitExceeded {
                kind: LimitKind::OutputBytes,
                limit: self.limit,
                actual,
            });
        }
        self.value.push_str(value);
        Ok(())
    }

    fn finish(self) -> String {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::scene::{GlyphCluster, GlyphPaint};

    fn rectangle_commands(left: i64, right: i64) -> Vec<PathCommand> {
        vec![
            PathCommand::MoveTo {
                x: Fixed::from_raw(left),
                y: Fixed::from_raw(0),
            },
            PathCommand::LineTo {
                x: Fixed::from_raw(right),
                y: Fixed::from_raw(0),
            },
            PathCommand::LineTo {
                x: Fixed::from_raw(right),
                y: Fixed::from_raw(10),
            },
            PathCommand::LineTo {
                x: Fixed::from_raw(left),
                y: Fixed::from_raw(10),
            },
            PathCommand::Close,
        ]
    }

    fn glyph_node(
        text: &str,
        clip_left: i64,
        clip_right: i64,
        commands: Vec<PathCommand>,
        clusters: Vec<GlyphCluster>,
    ) -> GlyphRunNode {
        let command_end = commands.len() as u64;
        GlyphRunNode {
            text: text.to_string(),
            clip_bounds: Rect {
                x: Fixed::from_raw(clip_left),
                y: Fixed::from_raw(0),
                width: Fixed::from_raw(clip_right - clip_left),
                height: Fixed::from_raw(10),
            },
            commands,
            clusters,
            paints: vec![GlyphPaint {
                command_start: 0,
                command_end,
                color: Rgb::BLACK,
            }],
            decorations: Vec::new(),
            color: Rgb::BLACK,
            rotation_degrees: 0,
            pivot_x: Fixed::ZERO,
            pivot_y: Fixed::ZERO,
            hyperlink: None,
        }
    }

    #[test]
    fn rgba_images_are_self_contained_accessible_and_rotated() {
        let scene = Scene {
            title: "image".to_string(),
            width: Fixed::from_pixels(20),
            height: Fixed::from_pixels(20),
            background: Rgb::WHITE,
            nodes: vec![SceneNode::Image(ImageNode {
                rect: Rect {
                    x: Fixed::from_pixels(2),
                    y: Fixed::from_pixels(4),
                    width: Fixed::from_pixels(8),
                    height: Fixed::from_pixels(10),
                },
                pixel_width: 1,
                pixel_height: 1,
                rgba: Arc::from([255, 0, 0, 128]),
                rotation_mdeg: 45_000,
                alt_text: Some("A <logo>".to_string()),
            })],
        };
        let first = render_scene_svg(&scene, 1 << 20).unwrap();
        let second = render_scene_svg(&scene, 1 << 20).unwrap();
        assert_eq!(first, second);
        assert!(first.contains("aria-label=\"A &lt;logo&gt;\""));
        assert!(first.contains("transform=\"rotate(45 6 9)\""));
        assert!(first.contains("href=\"data:image/png;base64,iVBORw0KGgo"));
        assert!(!first.contains("file:"));
        assert!(!first.contains("href=\"http"));
    }

    #[test]
    fn glyph_visible_label_excludes_clipped_clusters_and_preserves_accessibility() {
        let mut commands = rectangle_commands(0, 5);
        commands.extend(rectangle_commands(6, 10));
        commands.extend(rectangle_commands(11, 16));
        let node = glyph_node(
            "alpha beta gamma",
            0,
            10,
            commands,
            vec![
                GlyphCluster {
                    source_start: 0,
                    source_end: 5,
                    command_start: 0,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 5,
                    source_end: 6,
                    command_start: 5,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 6,
                    source_end: 10,
                    command_start: 5,
                    command_end: 10,
                },
                GlyphCluster {
                    source_start: 10,
                    source_end: 11,
                    command_start: 10,
                    command_end: 10,
                },
                GlyphCluster {
                    source_start: 11,
                    source_end: 16,
                    command_start: 10,
                    command_end: 15,
                },
            ],
        );
        assert!(node.metadata_is_valid());
        assert_eq!(visible_glyph_label(&node).unwrap(), "alpha beta");

        let scene = Scene {
            title: "clipped label".to_string(),
            width: Fixed::from_raw(20),
            height: Fixed::from_raw(20),
            background: Rgb::WHITE,
            nodes: vec![SceneNode::GlyphRun(node)],
        };
        let svg = render_scene_svg(&scene, 1 << 20).unwrap();
        assert!(svg.contains("aria-label=\"alpha beta gamma\""));
        assert!(svg.contains("data-rxls-visible-label=\"alpha beta\""));
    }

    #[test]
    fn glyph_visible_label_is_logical_for_visual_order_and_separates_gaps() {
        let mut commands = rectangle_commands(0, 3);
        commands.extend(rectangle_commands(4, 7));
        let visual_suffix = glyph_node(
            "abcXYZ",
            0,
            3,
            commands.clone(),
            vec![
                GlyphCluster {
                    source_start: 3,
                    source_end: 6,
                    command_start: 0,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 0,
                    source_end: 3,
                    command_start: 5,
                    command_end: 10,
                },
            ],
        );
        assert_eq!(visible_glyph_label(&visual_suffix).unwrap(), "XYZ");

        let discontiguous = glyph_node(
            "oneHIDDENtwo",
            0,
            7,
            commands,
            vec![
                GlyphCluster {
                    source_start: 0,
                    source_end: 3,
                    command_start: 0,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 9,
                    source_end: 12,
                    command_start: 5,
                    command_end: 10,
                },
            ],
        );
        assert_eq!(visible_glyph_label(&discontiguous).unwrap(), "one two");
    }

    #[test]
    fn glyph_visible_label_requires_painted_area_inside_nonempty_clip() {
        let move_only = glyph_node(
            "x",
            0,
            10,
            vec![PathCommand::MoveTo {
                x: Fixed::from_raw(5),
                y: Fixed::from_raw(5),
            }],
            vec![GlyphCluster {
                source_start: 0,
                source_end: 1,
                command_start: 0,
                command_end: 1,
            }],
        );
        assert_eq!(visible_glyph_label(&move_only).unwrap(), "");

        let outside = glyph_node(
            "x",
            20,
            30,
            rectangle_commands(0, 10),
            vec![GlyphCluster {
                source_start: 0,
                source_end: 1,
                command_start: 0,
                command_end: 5,
            }],
        );
        assert_eq!(visible_glyph_label(&outside).unwrap(), "");
    }
}
