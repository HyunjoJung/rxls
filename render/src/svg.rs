//! Deterministic bounded SVG serialization.

use crate::error::{LimitKind, RenderError};
use crate::scene::{
    backend_image_trace, backend_text_trace, format_fixed, BackendGeometryTrace, BackendGlyphTrace,
    BackendGlyphTraceBuilder, BackendNodeTrace, BackendPathTrace, BackendPathTraceBuilder, Fixed,
    GlyphRunNode, GlyphSemanticLayout, ImageNode, LineNode, PathCommand, PathNode, Rect, RectNode,
    Rgb, Scene, SceneNode, TextAnchor, TextBaseline, TextNode, FIXED_UNITS_PER_PIXEL,
};
use unicode_script::{Script, UnicodeScript};

const MAX_CLIP_GROUP_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SvgAuthoredClipScope {
    OutsideContent,
    Content,
    Body,
    Drawing,
    Object,
}

#[derive(Debug, Clone, Copy)]
struct SvgSemanticClipContext {
    outer_clip: Option<Rect>,
    page_clip: Rect,
    authored_scope: SvgAuthoredClipScope,
    body_clip: Option<Rect>,
    body_right_to_left: bool,
}

#[derive(Debug, Clone, Copy)]
struct SvgGlyphSemanticClip {
    effective: Option<Rect>,
    outer: Option<Rect>,
    retain_selected_groups_unbounded: bool,
    retain_selected_line_unbounded: bool,
    retain_outer_intersecting_words: bool,
    require_interior_horizontal_center: bool,
    use_nominal_horizontal_center: bool,
}

impl SvgSemanticClipContext {
    fn new(page_bounds: Rect) -> Self {
        Self {
            outer_clip: Some(page_bounds),
            page_clip: page_bounds,
            authored_scope: SvgAuthoredClipScope::OutsideContent,
            body_clip: None,
            body_right_to_left: false,
        }
    }

    fn descend(self, group: &crate::scene::ClipGroupNode) -> Result<Self, RenderError> {
        let clip = group.clip;
        let enters_content = self.authored_scope == SvgAuthoredClipScope::OutsideContent
            && is_authored_content_wrapper(group);
        let enters_body = self.authored_scope == SvgAuthoredClipScope::Content;
        let enters_drawing = self.authored_scope == SvgAuthoredClipScope::Body;
        let enters_object = self.authored_scope == SvgAuthoredClipScope::Drawing;
        let outer_clip = if enters_body {
            // Worksheet cells may retain complete source groups across the
            // replay viewport, so this clip remains paint-only.
            self.outer_clip
        } else if enters_drawing {
            // Calc clips a replayed drawing's text program to the physical
            // page and its own object, not to worksheet content margins.
            Some(self.page_clip)
        } else {
            match self.outer_clip {
                Some(outer_clip) => intersect_rects(outer_clip, clip)?,
                None => None,
            }
        };
        Ok(Self {
            outer_clip,
            page_clip: self.page_clip,
            authored_scope: if enters_content {
                SvgAuthoredClipScope::Content
            } else if enters_body {
                SvgAuthoredClipScope::Body
            } else if enters_drawing {
                SvgAuthoredClipScope::Drawing
            } else if enters_object {
                SvgAuthoredClipScope::Object
            } else {
                self.authored_scope
            },
            body_clip: if enters_body {
                Some(clip)
            } else {
                self.body_clip
            },
            body_right_to_left: if enters_body {
                authored_body_is_right_to_left(group)
            } else {
                self.body_right_to_left
            },
        })
    }

    fn glyph_clips(self, node: &GlyphRunNode) -> Result<SvgGlyphSemanticClip, RenderError> {
        let owner_clip = node.clip_bounds;
        let effective_clip = match self.outer_clip {
            Some(outer_clip) => intersect_rects(outer_clip, owner_clip)?,
            None => None,
        };
        // A mirrored worksheet does not make an embedded Latin run RTL. Calc
        // retains non-Latin source groups wholesale, but clips Latin groups at
        // the physical page edge one complete intersecting word at a time.
        let uses_non_latin_script = text_uses_non_latin_script(&node.text);
        let retain_selected_groups_unbounded = self.authored_scope == SvgAuthoredClipScope::Body
            && self.body_right_to_left
            && uses_non_latin_script;
        let retain_selected_line_unbounded = retain_selected_groups_unbounded
            && node.semantic_retention_groups().len() > 1
            && node.semantic_line_records().is_some()
            && node.text.chars().any(|character| {
                matches!(
                    unicode_bidi::bidi_class(character),
                    unicode_bidi::BidiClass::R | unicode_bidi::BidiClass::AL
                )
            });
        let retain_outer_intersecting_words = self.authored_scope == SvgAuthoredClipScope::Body
            && self.body_right_to_left
            && !uses_non_latin_script;
        let owner_at_body_right = self.body_clip.is_some_and(|body| {
            rect_right(owner_clip).is_some() && rect_right(owner_clip) == rect_right(body)
        });
        let broaden_body_semantics = self.authored_scope == SvgAuthoredClipScope::Body
            && (self.body_right_to_left
                || text_uses_non_latin_script(&node.text)
                || owner_at_body_right);
        let outer_clip = if self.authored_scope == SvgAuthoredClipScope::Body
            && !self.body_right_to_left
            && !uses_non_latin_script
            && !retains_complete_source(node)
        {
            effective_clip
        } else if retain_outer_intersecting_words {
            Some(self.page_clip)
        } else {
            match self.authored_scope {
                SvgAuthoredClipScope::Body if broaden_body_semantics => self.outer_clip,
                SvgAuthoredClipScope::Body
                | SvgAuthoredClipScope::Drawing
                | SvgAuthoredClipScope::Object => effective_clip,
                SvgAuthoredClipScope::OutsideContent | SvgAuthoredClipScope::Content => {
                    self.outer_clip
                }
            }
        };
        Ok(SvgGlyphSemanticClip {
            effective: effective_clip,
            outer: outer_clip,
            retain_selected_groups_unbounded,
            retain_selected_line_unbounded,
            retain_outer_intersecting_words,
            require_interior_horizontal_center: self.authored_scope == SvgAuthoredClipScope::Body
                && !self.body_right_to_left
                && !uses_non_latin_script
                && (!retains_complete_source(node) || owner_clip.width > Fixed::from_pixels(64)),
            use_nominal_horizontal_center: owner_at_body_right,
        })
    }
}

fn rect_right(rect: Rect) -> Option<Fixed> {
    rect.x.checked_add(rect.width)
}

fn text_uses_non_latin_script(text: &str) -> bool {
    text.chars().any(|character| {
        !matches!(
            character.script(),
            Script::Common | Script::Inherited | Script::Latin | Script::Unknown
        )
    })
}

fn retains_complete_source(node: &GlyphRunNode) -> bool {
    let Ok(source_end) = u64::try_from(node.text.len()) else {
        return false;
    };
    node.semantic_retention_groups()
        .iter()
        .any(|group| group.source_start == 0 && group.source_end == source_end)
}

fn authored_body_is_right_to_left(group: &crate::scene::ClipGroupNode) -> bool {
    let mut previous = None::<Rect>;
    for node in &group.nodes {
        let SceneNode::GlyphRun(run) = node else {
            continue;
        };
        let bounds = run.clip_bounds;
        if let Some(left) = previous {
            if bounds.y == left.y && bounds.height == left.height && bounds.x != left.x {
                return bounds.x < left.x;
            }
        }
        previous = Some(bounds);
    }
    false
}

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

    let page_bounds = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: scene.width,
        height: scene.height,
    };
    let mut clip_index = 0_usize;
    push_scene_nodes(
        &mut out,
        &scene.nodes,
        &mut clip_index,
        trace,
        0,
        Some(page_bounds),
        SvgSemanticClipContext::new(page_bounds),
    )?;
    out.push("</svg>\n")?;
    Ok(out.finish())
}

fn is_authored_content_wrapper(group: &crate::scene::ClipGroupNode) -> bool {
    !group.nodes.is_empty()
        && group
            .nodes
            .iter()
            .all(|node| matches!(node, SceneNode::ClipGroup(_)))
        && group.nodes.iter().any(authored_body_replay)
}

fn authored_body_replay(node: &SceneNode) -> bool {
    let SceneNode::ClipGroup(group) = node else {
        return false;
    };
    contains_retained_layout_semantics(&group.nodes)
}

fn contains_retained_layout_semantics(nodes: &[SceneNode]) -> bool {
    nodes.iter().any(|node| match node {
        SceneNode::ClipGroup(group) => contains_retained_layout_semantics(&group.nodes),
        SceneNode::GlyphRun(run) => {
            run.semantic_line_records().is_some() && !run.semantic_retention_groups().is_empty()
        }
        SceneNode::Rect(_)
        | SceneNode::Line(_)
        | SceneNode::Path(_)
        | SceneNode::Image(_)
        | SceneNode::Text(_) => false,
    })
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
    active_clip: Option<Rect>,
    semantic_clip: SvgSemanticClipContext,
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
                let effective_group_clip = match active_clip {
                    Some(active_clip) => intersect_rects(active_clip, group.clip)?,
                    None => None,
                };
                let nested_semantic_clip = semantic_clip.descend(group)?;
                push_scene_nodes(
                    out,
                    &group.nodes,
                    clip_index,
                    trace.as_deref_mut(),
                    depth + 1,
                    effective_group_clip,
                    nested_semantic_clip,
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
                let glyph_clip = semantic_clip.glyph_clips(glyphs)?;
                let glyph_trace =
                    push_glyph_run(out, glyphs, *clip_index, trace.is_some(), glyph_clip)?;
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
    semantic_clip: SvgGlyphSemanticClip,
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
    let visible_label = visible_glyph_label_in_clip(node, semantic_clip)?;
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

#[cfg(test)]
fn visible_glyph_label(node: &GlyphRunNode) -> Result<String, RenderError> {
    visible_glyph_label_in_clip(
        node,
        SvgGlyphSemanticClip {
            effective: Some(node.clip_bounds),
            outer: Some(node.clip_bounds),
            retain_selected_groups_unbounded: false,
            retain_selected_line_unbounded: false,
            retain_outer_intersecting_words: false,
            require_interior_horizontal_center: false,
            use_nominal_horizontal_center: false,
        },
    )
}

/// Return the logical source text whose nominal layout remains visible through
/// every effective clip. The full source remains in `aria-label`; this bounded
/// derivative lets parity tooling compare prepared semantics without deriving
/// source visibility from outline ink.
fn visible_glyph_label_in_clip(
    node: &GlyphRunNode,
    semantic_clip: SvgGlyphSemanticClip,
) -> Result<String, RenderError> {
    let Some(effective_clip) = semantic_clip.effective else {
        return Ok(String::new());
    };
    if semantic_clip.outer.is_none() {
        return Ok(String::new());
    }
    let clip_right = effective_clip
        .x
        .raw()
        .checked_add(effective_clip.width.raw())
        .ok_or(RenderError::CoordinateOverflow)?;
    let clip_bottom = effective_clip
        .y
        .raw()
        .checked_add(effective_clip.height.raw())
        .ok_or(RenderError::CoordinateOverflow)?;
    let clip = (
        effective_clip.x.raw(),
        effective_clip.y.raw(),
        clip_right,
        clip_bottom,
    );
    if clip.2 <= clip.0 || clip.3 <= clip.1 {
        return Ok(String::new());
    }

    let semantic_layout = node.semantic_text_layout();
    let (mut cluster_visible, authoritative_layout) = match semantic_layout
        .as_ref()
        .and_then(|layout| nominal_layout_cluster_visibility(node, layout, semantic_clip))
    {
        Some(visible) => (visible, true),
        None => {
            let mut visible = Vec::with_capacity(node.clusters.len());
            for (cluster_index, cluster) in node.clusters.iter().enumerate() {
                let start =
                    usize::try_from(cluster.command_start).map_err(|_| RenderError::Backend {
                        reason: "invalid_glyph_metadata",
                    })?;
                let end =
                    usize::try_from(cluster.command_end).map_err(|_| RenderError::Backend {
                        reason: "invalid_glyph_metadata",
                    })?;
                let commands = node.commands.get(start..end).ok_or(RenderError::Backend {
                    reason: "invalid_glyph_metadata",
                })?;
                let semantic_clip_is_cell_clip = effective_clip == node.clip_bounds;
                let nominal_baseline_visible = node.rotation_degrees != 0
                    || !semantic_clip_is_cell_clip
                    || node.cluster_metrics.is_empty()
                    || node
                        .cluster_metrics
                        .get(cluster_index)
                        .is_some_and(|metrics| {
                            metrics.baseline_y.raw() >= clip.1 && metrics.baseline_y.raw() < clip.3
                        });
                visible.push(
                    nominal_baseline_visible && glyph_commands_intersect_clip(commands, clip),
                );
            }
            (visible, false)
        }
    };
    if !authoritative_layout {
        node.expand_semantic_visibility(&mut cluster_visible);
    }
    let mut ranges = Vec::new();
    for (cluster, visible) in node.clusters.iter().zip(cluster_visible) {
        if visible {
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

#[derive(Clone, Copy)]
struct SvgSemanticTransform {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl SvgSemanticTransform {
    fn rotation(degrees: i16, pivot_x: Fixed, pivot_y: Fixed) -> Self {
        let radians = f64::from(degrees).to_radians();
        let cosine = radians.cos();
        let sine = radians.sin();
        let x = fixed_as_f64(pivot_x);
        let y = fixed_as_f64(pivot_y);
        Self {
            a: cosine,
            b: sine,
            c: -sine,
            d: cosine,
            e: x - cosine * x + sine * y,
            f: y - sine * x - cosine * y,
        }
    }

    fn point(self, x: Fixed, y: Fixed) -> [f64; 2] {
        let x = fixed_as_f64(x);
        let y = fixed_as_f64(y);
        [
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        ]
    }
}

fn fixed_as_f64(value: Fixed) -> f64 {
    value.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64
}

fn nominal_layout_cluster_visibility(
    node: &GlyphRunNode,
    layout: &GlyphSemanticLayout,
    semantic_clip: SvgGlyphSemanticClip,
) -> Option<Vec<bool>> {
    if node.cluster_metrics.len() != node.clusters.len() {
        return None;
    }
    let effective_clip = semantic_clip.effective?;
    let semantic_outer_clip = semantic_clip.outer?;
    let retain_selected_groups_unbounded = semantic_clip.retain_selected_groups_unbounded;
    let retain_selected_line_unbounded = semantic_clip.retain_selected_line_unbounded;
    let retain_outer_intersecting_words = semantic_clip.retain_outer_intersecting_words;
    let require_interior_horizontal_center = semantic_clip.require_interior_horizontal_center;
    let use_nominal_horizontal_center = semantic_clip.use_nominal_horizontal_center;
    let transform =
        SvgSemanticTransform::rotation(node.rotation_degrees, node.pivot_x, node.pivot_y);
    let require_baseline =
        node.rotation_degrees.rem_euclid(360) == 0 && effective_clip == node.clip_bounds;
    let mut visible = vec![false; node.clusters.len()];
    let mut selected_word_clusters = vec![false; node.clusters.len()];
    let mut retention_triggers = vec![false; node.clusters.len()];
    for line in &layout.lines {
        let line_visible = line.nominal_bounds.is_some_and(|bounds| {
            semantic_box_and_baselines_intersect_clip(
                bounds,
                &line.baselines,
                effective_clip,
                transform,
                require_baseline,
            )
        });
        if !line_visible {
            continue;
        }
        for word in &line.words {
            let word_visible = word.nominal_bounds.is_some_and(|bounds| {
                semantic_box_and_baselines_intersect_clip(
                    bounds,
                    &word.baselines,
                    effective_clip,
                    transform,
                    require_baseline,
                )
            });
            if !word_visible {
                continue;
            }
            for &index in &word.cluster_indices {
                selected_word_clusters[index] = true;
                let metrics = *node.cluster_metrics.get(index)?;
                let bounds = node.nominal_cluster_bounds(index)?;
                let intersects = semantic_cluster_is_visible(
                    node,
                    index,
                    bounds,
                    &[metrics.baseline_y],
                    effective_clip,
                    transform,
                    require_baseline,
                    require_interior_horizontal_center,
                    use_nominal_horizontal_center,
                );
                retention_triggers[index] = intersects;
                visible[index] = intersects;
            }
        }
    }
    node.expand_semantic_visibility_from(&mut visible, &retention_triggers);
    if retain_selected_line_unbounded {
        for line in &layout.lines {
            let selected = line.words.iter().any(|word| {
                word.cluster_indices
                    .iter()
                    .any(|&index| visible[index] || selected_word_clusters[index])
            });
            if selected {
                for word in &line.words {
                    for &index in &word.cluster_indices {
                        visible[index] = true;
                    }
                }
            }
        }
    }
    if !retain_selected_groups_unbounded && !retain_outer_intersecting_words {
        for (index, cluster_visible) in visible.iter_mut().enumerate() {
            if !*cluster_visible {
                continue;
            }
            let cluster = *node.clusters.get(index)?;
            let start = usize::try_from(cluster.source_start).ok()?;
            let end = usize::try_from(cluster.source_end).ok()?;
            *cluster_visible = layout
                .lines
                .iter()
                .any(|line| line.source.start <= start && end <= line.source.end);
        }
    }
    if retain_selected_groups_unbounded {
        return Some(visible);
    }
    if retain_outer_intersecting_words {
        let retained_groups = visible;
        let mut outer_visible = vec![false; node.clusters.len()];
        for line in &layout.lines {
            for word in &line.words {
                if !word
                    .cluster_indices
                    .iter()
                    .any(|&index| retained_groups[index] || selected_word_clusters[index])
                {
                    continue;
                }
                let intersects = word.nominal_bounds.is_some_and(|bounds| {
                    semantic_box_and_baselines_intersect_clip(
                        bounds,
                        &word.baselines,
                        semantic_outer_clip,
                        transform,
                        false,
                    )
                });
                if intersects {
                    for &index in &word.cluster_indices {
                        outer_visible[index] =
                            retained_groups[index] || selected_word_clusters[index];
                    }
                }
            }
        }
        return Some(outer_visible);
    }
    for index in 0..node.clusters.len() {
        if !visible[index] && !selected_word_clusters[index] {
            continue;
        }
        let metrics = *node.cluster_metrics.get(index)?;
        visible[index] = node.nominal_cluster_bounds(index).is_some_and(|bounds| {
            semantic_cluster_is_visible(
                node,
                index,
                bounds,
                &[metrics.baseline_y],
                semantic_outer_clip,
                transform,
                false,
                require_interior_horizontal_center,
                use_nominal_horizontal_center,
            )
        });
    }
    Some(visible)
}

#[allow(clippy::too_many_arguments)]
fn semantic_cluster_is_visible(
    node: &GlyphRunNode,
    cluster_index: usize,
    bounds: Rect,
    baselines: &[Fixed],
    clip: Rect,
    transform: SvgSemanticTransform,
    require_baseline: bool,
    require_interior_horizontal_center: bool,
    use_nominal_horizontal_center: bool,
) -> bool {
    semantic_box_and_baselines_intersect_clip(bounds, baselines, clip, transform, require_baseline)
        && (!require_interior_horizontal_center
            || glyph_cluster_has_interior_horizontal_center(
                node,
                cluster_index,
                bounds,
                clip,
                transform,
                use_nominal_horizontal_center,
            ))
}

fn semantic_box_and_baselines_intersect_clip(
    bounds: Rect,
    baselines: &[Fixed],
    clip: Rect,
    transform: SvgSemanticTransform,
    require_baseline: bool,
) -> bool {
    transformed_rect_intersects_clip(bounds, clip, transform)
        && (!require_baseline
            || baselines.iter().copied().any(|baseline| {
                transformed_baseline_intersects_clip_value(baseline, bounds, clip, transform)
            }))
}

fn transformed_baseline_intersects_clip_value(
    baseline_y: Fixed,
    bounds: Rect,
    clip: Rect,
    transform: SvgSemanticTransform,
) -> bool {
    let Some(bounds_right) = bounds.x.checked_add(bounds.width) else {
        return false;
    };
    let left = Fixed::from_raw(bounds.x.raw().min(bounds_right.raw()));
    let right = Fixed::from_raw(bounds.x.raw().max(bounds_right.raw()));
    let width = right
        .checked_sub(left)
        .map(|width| width.max(Fixed::from_raw(1)))
        .unwrap_or(Fixed::from_raw(1));
    transformed_rect_intersects_clip(
        Rect {
            x: left,
            y: baseline_y,
            width,
            height: Fixed::from_raw(1),
        },
        clip,
        transform,
    )
}

fn transformed_rect_intersects_clip(
    rect: Rect,
    clip: Rect,
    transform: SvgSemanticTransform,
) -> bool {
    if rect.width <= Fixed::ZERO
        || rect.height <= Fixed::ZERO
        || clip.width <= Fixed::ZERO
        || clip.height <= Fixed::ZERO
    {
        return false;
    }
    let Some(rect_right) = rect.x.checked_add(rect.width) else {
        return false;
    };
    let Some(rect_bottom) = rect.y.checked_add(rect.height) else {
        return false;
    };
    let Some(clip_right) = clip.x.checked_add(clip.width) else {
        return false;
    };
    let Some(clip_bottom) = clip.y.checked_add(clip.height) else {
        return false;
    };
    let transformed = [
        transform.point(rect.x, rect.y),
        transform.point(rect_right, rect.y),
        transform.point(rect_right, rect_bottom),
        transform.point(rect.x, rect_bottom),
    ];
    let clip = [
        [fixed_as_f64(clip.x), fixed_as_f64(clip.y)],
        [fixed_as_f64(clip_right), fixed_as_f64(clip.y)],
        [fixed_as_f64(clip_right), fixed_as_f64(clip_bottom)],
        [fixed_as_f64(clip.x), fixed_as_f64(clip_bottom)],
    ];
    semantic_quadrilaterals_overlap(&transformed, &clip)
}

fn glyph_cluster_has_interior_horizontal_center(
    node: &GlyphRunNode,
    cluster_index: usize,
    nominal_bounds: Rect,
    clip: Rect,
    transform: SvgSemanticTransform,
    use_nominal_bounds: bool,
) -> bool {
    let ink_bounds = (!use_nominal_bounds)
        .then(|| {
            node.clusters
                .get(cluster_index)
                .and_then(|cluster| {
                    let start = usize::try_from(cluster.command_start).ok()?;
                    let end = usize::try_from(cluster.command_end).ok()?;
                    glyph_command_bounds(node.commands.get(start..end)?)
                })
                .and_then(|(min_x, min_y, max_x, max_y)| {
                    let width = Fixed::from_raw(max_x).checked_sub(Fixed::from_raw(min_x))?;
                    let height = Fixed::from_raw(max_y).checked_sub(Fixed::from_raw(min_y))?;
                    (width > Fixed::ZERO && height > Fixed::ZERO).then_some(Rect {
                        x: Fixed::from_raw(min_x),
                        y: Fixed::from_raw(min_y),
                        width,
                        height,
                    })
                })
        })
        .flatten();
    let rect = ink_bounds.unwrap_or(nominal_bounds);
    let Some(rect_right) = rect.x.checked_add(rect.width) else {
        return false;
    };
    let Some(rect_bottom) = rect.y.checked_add(rect.height) else {
        return false;
    };
    let Some(clip_right) = clip.x.checked_add(clip.width) else {
        return false;
    };
    let clip_left = fixed_as_f64(clip.x);
    let clip_right = fixed_as_f64(clip_right);
    let (minimum_x, maximum_x) = [
        transform.point(rect.x, rect.y),
        transform.point(rect_right, rect.y),
        transform.point(rect_right, rect_bottom),
        transform.point(rect.x, rect_bottom),
    ]
    .into_iter()
    .map(|point| point[0])
    .fold(
        (f64::INFINITY, f64::NEG_INFINITY),
        |(minimum, maximum), x| (minimum.min(x), maximum.max(x)),
    );
    let clip_width = clip_right - clip_left;
    if clip_width <= 0.0 || !minimum_x.is_finite() || !maximum_x.is_finite() {
        return false;
    }
    let inset = fixed_as_f64(Fixed::from_raw(FIXED_UNITS_PER_PIXEL)).min(clip_width / 2.0);
    let center_x = (minimum_x + maximum_x) / 2.0;
    center_x >= clip_left + inset && center_x <= clip_right - inset
}

fn semantic_quadrilaterals_overlap(left: &[[f64; 2]; 4], right: &[[f64; 2]; 4]) -> bool {
    let normal = |start: [f64; 2], end: [f64; 2]| {
        let edge = [end[0] - start[0], end[1] - start[1]];
        [-edge[1], edge[0]]
    };
    [
        normal(left[0], left[1]),
        normal(left[0], left[3]),
        normal(right[0], right[1]),
        normal(right[0], right[3]),
    ]
    .into_iter()
    .filter(|axis| axis[0] != 0.0 || axis[1] != 0.0)
    .all(|axis| semantic_projected_intervals_overlap(left, right, axis))
}

fn semantic_projected_intervals_overlap(
    left: &[[f64; 2]; 4],
    right: &[[f64; 2]; 4],
    axis: [f64; 2],
) -> bool {
    let project = |point: [f64; 2]| point[0] * axis[0] + point[1] * axis[1];
    let interval = |points: &[[f64; 2]; 4]| {
        points.iter().copied().map(project).fold(
            (f64::INFINITY, f64::NEG_INFINITY),
            |(minimum, maximum), value| (minimum.min(value), maximum.max(value)),
        )
    };
    let (left_min, left_max) = interval(left);
    let (right_min, right_max) = interval(right);
    left_max > right_min && right_max > left_min
}

fn intersect_rects(left: Rect, right: Rect) -> Result<Option<Rect>, RenderError> {
    if left.width <= Fixed::ZERO
        || left.height <= Fixed::ZERO
        || right.width <= Fixed::ZERO
        || right.height <= Fixed::ZERO
    {
        return Ok(None);
    }
    let left_right = left
        .x
        .checked_add(left.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let left_bottom = left
        .y
        .checked_add(left.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let right_right = right
        .x
        .checked_add(right.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let right_bottom = right
        .y
        .checked_add(right.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let end_x = left_right.min(right_right);
    let end_y = left_bottom.min(right_bottom);
    if end_x <= x || end_y <= y {
        return Ok(None);
    }
    Ok(Some(Rect {
        x,
        y,
        width: end_x
            .checked_sub(x)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: end_y
            .checked_sub(y)
            .ok_or(RenderError::CoordinateOverflow)?,
    }))
}

fn glyph_command_bounds(commands: &[PathCommand]) -> Option<(i64, i64, i64, i64)> {
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
    bounds
}

fn glyph_commands_intersect_clip(commands: &[PathCommand], clip: (i64, i64, i64, i64)) -> bool {
    glyph_command_bounds(commands).is_some_and(|(min_x, min_y, max_x, max_y)| {
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
    use crate::scene::{GlyphCluster, GlyphClusterMetrics, GlyphPaint, GlyphSemanticGroup};

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
            glyphs: Vec::new(),
            font_faces: Vec::new(),
            text: text.to_string(),
            clip_bounds: Rect {
                x: Fixed::from_raw(clip_left),
                y: Fixed::from_raw(0),
                width: Fixed::from_raw(clip_right - clip_left),
                height: Fixed::from_raw(10),
            },
            commands,
            clusters,
            cluster_metrics: Vec::new(),
            semantic_groups: Vec::new(),
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

    fn authoritative_word_node(retain_complete_source: bool) -> GlyphRunNode {
        let pixel = FIXED_UNITS_PER_PIXEL;
        let mut commands = rectangle_commands(0, 4 * pixel);
        commands.extend(rectangle_commands(5 * pixel, 8 * pixel));
        commands.extend(rectangle_commands(8 * pixel, 9 * pixel));
        let mut node = glyph_node(
            "CELL 0003",
            0,
            8 * pixel,
            commands,
            vec![
                GlyphCluster {
                    source_start: 0,
                    source_end: 4,
                    command_start: 0,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 4,
                    source_end: 5,
                    command_start: 5,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 5,
                    source_end: 8,
                    command_start: 5,
                    command_end: 10,
                },
                GlyphCluster {
                    source_start: 8,
                    source_end: 9,
                    command_start: 10,
                    command_end: 15,
                },
            ],
        );
        node.clip_bounds.height = Fixed::from_pixels(10);
        let metrics = |origin_x, advance_x| GlyphClusterMetrics {
            origin_x: Fixed::from_raw(origin_x * pixel),
            advance_x: Fixed::from_raw(advance_x * pixel),
            baseline_y: Fixed::from_raw(8 * pixel),
            ascent: Fixed::from_raw(7 * pixel),
            descent: Fixed::from_raw(-2 * pixel),
        };
        node.cluster_metrics = vec![metrics(0, 4), metrics(4, 1), metrics(5, 3), metrics(8, 1)];
        node.semantic_groups = Vec::with_capacity(3);
        if retain_complete_source {
            node.semantic_groups.push(GlyphSemanticGroup {
                source_start: 0,
                source_end: 9,
            });
        }
        node.semantic_groups.extend([
            GlyphSemanticGroup {
                source_start: 9,
                source_end: 9,
            },
            GlyphSemanticGroup {
                source_start: 0,
                source_end: 9,
            },
        ]);
        assert!(node.metadata_is_valid());
        node
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
    fn glyph_visible_label_uses_authoritative_nominal_layout_not_outline_ink() {
        let mut commands = rectangle_commands(20, 25);
        commands.extend(rectangle_commands(0, 4));
        let mut node = glyph_node(
            "alpha beta",
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
            ],
        );
        let metrics = |origin_x, advance_x| GlyphClusterMetrics {
            origin_x: Fixed::from_raw(origin_x),
            advance_x: Fixed::from_raw(advance_x),
            baseline_y: Fixed::from_raw(8),
            ascent: Fixed::from_raw(7),
            descent: Fixed::from_raw(-2),
        };
        node.cluster_metrics = vec![metrics(0, 5), metrics(5, 1), metrics(20, 4)];
        node.semantic_groups = vec![
            GlyphSemanticGroup {
                source_start: 10,
                source_end: 10,
            },
            GlyphSemanticGroup {
                source_start: 0,
                source_end: 10,
            },
        ];

        assert!(node.metadata_is_valid());
        assert_eq!(
            visible_glyph_label(&node).unwrap(),
            "alpha ",
            "visible source follows prepared cursor geometry even when outline ink disagrees"
        );
    }

    #[test]
    fn glyph_visible_label_retains_a_visible_calc_edit_engine_script_group() {
        let mut commands = rectangle_commands(0, 5);
        commands.extend(rectangle_commands(20, 25));
        commands.extend(rectangle_commands(30, 35));
        let mut node = glyph_node(
            "中文 0006",
            0,
            10,
            commands,
            vec![
                GlyphCluster {
                    source_start: 0,
                    source_end: 3,
                    command_start: 0,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 3,
                    source_end: 6,
                    command_start: 5,
                    command_end: 10,
                },
                GlyphCluster {
                    source_start: 6,
                    source_end: 7,
                    command_start: 10,
                    command_end: 10,
                },
                GlyphCluster {
                    source_start: 7,
                    source_end: 11,
                    command_start: 10,
                    command_end: 15,
                },
            ],
        );
        node.semantic_groups = vec![
            GlyphSemanticGroup {
                source_start: 0,
                source_end: 7,
            },
            GlyphSemanticGroup {
                source_start: 7,
                source_end: 11,
            },
        ];

        assert!(node.metadata_is_valid());
        assert_eq!(visible_glyph_label(&node).unwrap(), "中文 ");
    }

    #[test]
    fn glyph_visible_label_retains_a_complete_clipped_ods_paragraph_group() {
        let mut commands = rectangle_commands(0, 3);
        commands.extend(rectangle_commands(4, 10));
        let mut node = glyph_node(
            "top bottom",
            0,
            10,
            commands,
            vec![
                GlyphCluster {
                    source_start: 0,
                    source_end: 3,
                    command_start: 0,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 3,
                    source_end: 4,
                    command_start: 5,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 4,
                    source_end: 10,
                    command_start: 5,
                    command_end: 10,
                },
            ],
        );
        node.cluster_metrics = vec![
            GlyphClusterMetrics {
                origin_x: Fixed::ZERO,
                advance_x: Fixed::from_raw(3),
                baseline_y: Fixed::from_raw(5),
                ascent: Fixed::from_raw(5),
                descent: Fixed::from_raw(-1),
            },
            GlyphClusterMetrics {
                origin_x: Fixed::from_raw(3),
                advance_x: Fixed::from_raw(1),
                baseline_y: Fixed::from_raw(5),
                ascent: Fixed::from_raw(5),
                descent: Fixed::from_raw(-1),
            },
            GlyphClusterMetrics {
                origin_x: Fixed::from_raw(4),
                advance_x: Fixed::from_raw(6),
                baseline_y: Fixed::from_raw(10),
                ascent: Fixed::from_raw(5),
                descent: Fixed::from_raw(-1),
            },
        ];
        node.semantic_groups = vec![GlyphSemanticGroup {
            source_start: 0,
            source_end: 10,
        }];

        assert!(node.metadata_is_valid());
        assert_eq!(visible_glyph_label(&node).unwrap(), "top bottom");
    }

    #[test]
    fn glyph_visible_label_omits_an_outline_sliver_below_the_semantic_baseline() {
        let mut node = glyph_node(
            "below",
            0,
            10,
            rectangle_commands(0, 5),
            vec![GlyphCluster {
                source_start: 0,
                source_end: 5,
                command_start: 0,
                command_end: 5,
            }],
        );
        node.cluster_metrics = vec![GlyphClusterMetrics {
            origin_x: Fixed::ZERO,
            advance_x: Fixed::from_raw(5),
            baseline_y: Fixed::from_raw(11),
            ascent: Fixed::from_raw(10),
            descent: Fixed::from_raw(-2),
        }];

        assert!(node.metadata_is_valid());
        assert_eq!(visible_glyph_label(&node).unwrap(), "");
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

    #[test]
    fn authoritative_word_ignores_owner_clip_but_respects_page_clip() {
        let node = authoritative_word_node(false);

        let scene = |width| Scene {
            title: "owner and page clips".to_string(),
            width: Fixed::from_pixels(width),
            height: Fixed::from_pixels(10),
            background: Rgb::WHITE,
            nodes: vec![SceneNode::GlyphRun(node.clone())],
        };
        let owner_clipped = render_scene_svg(&scene(10), 1 << 20).unwrap();
        assert!(owner_clipped.contains("data-rxls-visible-label=\"CELL 0003\""));
        assert!(owner_clipped.contains("aria-label=\"CELL 0003\""));

        let page_clipped = render_scene_svg(&scene(8), 1 << 20).unwrap();
        assert!(page_clipped.contains("data-rxls-visible-label=\"CELL 000\""));
        assert!(!page_clipped.contains("data-rxls-visible-label=\"CELL 0003\""));
        assert!(page_clipped.contains("aria-label=\"CELL 0003\""));
    }

    #[test]
    fn authored_group_expansion_stops_at_the_prepared_line_source() {
        let mut node = authoritative_word_node(true);
        node.semantic_groups
            .last_mut()
            .expect("prepared line")
            .source_end = 8;
        assert!(node.metadata_is_valid());
        let page = Rect {
            width: Fixed::from_pixels(10),
            ..node.clip_bounds
        };
        let ordinary = SvgGlyphSemanticClip {
            effective: Some(node.clip_bounds),
            outer: Some(page),
            retain_selected_groups_unbounded: false,
            retain_selected_line_unbounded: false,
            retain_outer_intersecting_words: false,
            require_interior_horizontal_center: false,
            use_nominal_horizontal_center: false,
        };

        assert_eq!(
            visible_glyph_label_in_clip(&node, ordinary).unwrap(),
            "CELL 000"
        );
        assert_eq!(
            visible_glyph_label_in_clip(
                &node,
                SvgGlyphSemanticClip {
                    retain_selected_groups_unbounded: true,
                    ..ordinary
                },
            )
            .unwrap(),
            "CELL 0003"
        );
    }

    #[test]
    fn authored_split_groups_expand_to_the_selected_prepared_line() {
        let mut node = authoritative_word_node(false);
        node.semantic_groups = vec![
            GlyphSemanticGroup {
                source_start: 0,
                source_end: 5,
            },
            GlyphSemanticGroup {
                source_start: 5,
                source_end: 9,
            },
            GlyphSemanticGroup {
                source_start: 9,
                source_end: 9,
            },
            GlyphSemanticGroup {
                source_start: 0,
                source_end: 9,
            },
        ];
        assert!(node.metadata_is_valid());
        let narrow = Rect {
            width: Fixed::from_pixels(4),
            ..node.clip_bounds
        };
        let clip = SvgGlyphSemanticClip {
            effective: Some(narrow),
            outer: Some(narrow),
            retain_selected_groups_unbounded: true,
            retain_selected_line_unbounded: true,
            retain_outer_intersecting_words: false,
            require_interior_horizontal_center: false,
            use_nominal_horizontal_center: false,
        };

        assert_eq!(
            visible_glyph_label_in_clip(&node, clip).unwrap(),
            "CELL 0003"
        );
    }

    #[test]
    fn authored_body_and_drawing_clips_follow_axis_specific_semantics() {
        fn rect(x: i64, y: i64, width: i64, height: i64) -> Rect {
            Rect {
                x: Fixed::from_pixels(x),
                y: Fixed::from_pixels(y),
                width: Fixed::from_pixels(width),
                height: Fixed::from_pixels(height),
            }
        }

        fn scene(
            content_clip: Rect,
            body_clip: Rect,
            drawing_clip: Option<Rect>,
            object_clip: Option<Rect>,
            authored: bool,
            multiple_bodies: bool,
        ) -> Scene {
            let page_bounds = Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(10),
                height: Fixed::from_pixels(10),
            };
            let mut run = authoritative_word_node(authored);
            if drawing_clip.is_some() {
                run.clip_bounds = page_bounds;
            }
            let mut body_nodes = vec![SceneNode::GlyphRun(run)];
            if let Some(clip) = object_clip {
                body_nodes = vec![SceneNode::ClipGroup(crate::scene::ClipGroupNode {
                    clip,
                    nodes: body_nodes,
                })];
            }
            if let Some(clip) = drawing_clip {
                body_nodes = vec![SceneNode::ClipGroup(crate::scene::ClipGroupNode {
                    clip,
                    nodes: body_nodes,
                })];
            }
            let mut bodies = vec![SceneNode::ClipGroup(crate::scene::ClipGroupNode {
                clip: body_clip,
                nodes: body_nodes,
            })];
            if multiple_bodies {
                bodies.push(SceneNode::ClipGroup(crate::scene::ClipGroupNode {
                    clip: rect(0, 8, 10, 2),
                    nodes: vec![SceneNode::Rect(RectNode {
                        rect: rect(0, 8, 1, 1),
                        fill: Some(Rgb::WHITE),
                        stroke: None,
                        stroke_width: Fixed::ZERO,
                    })],
                }));
            }
            Scene {
                title: "authored semantic clips".to_string(),
                width: page_bounds.width,
                height: page_bounds.height,
                background: Rgb::WHITE,
                nodes: vec![SceneNode::ClipGroup(crate::scene::ClipGroupNode {
                    clip: content_clip,
                    nodes: bodies,
                })],
            }
        }

        let page = rect(0, 0, 10, 10);
        let body_fragment = render_scene_svg(
            &scene(page, rect(0, 0, 8, 10), None, None, true, false),
            1 << 20,
        )
        .unwrap();
        assert!(body_fragment.contains("data-rxls-visible-label=\"CELL 0003\""));
        assert!(body_fragment.contains("aria-label=\"CELL 0003\""));

        let interior_owner_fragment =
            render_scene_svg(&scene(page, page, None, None, true, false), 1 << 20).unwrap();
        assert!(interior_owner_fragment.contains("data-rxls-visible-label=\"CELL 000\""));
        assert!(interior_owner_fragment.contains("aria-label=\"CELL 0003\""));

        let content_fragment = render_scene_svg(
            &scene(
                rect(0, 0, 8, 10),
                rect(0, 0, 8, 10),
                None,
                None,
                true,
                false,
            ),
            1 << 20,
        )
        .unwrap();
        assert!(content_fragment.contains("data-rxls-visible-label=\"CELL 000\""));
        assert!(content_fragment.contains("aria-label=\"CELL 0003\""));

        let vertical_drawing_fragment = render_scene_svg(
            &scene(
                page,
                rect(0, 0, 10, 4),
                Some(rect(0, 0, 10, 4)),
                None,
                true,
                true,
            ),
            1 << 20,
        )
        .unwrap();
        assert!(vertical_drawing_fragment.contains("data-rxls-visible-label=\"CELL 0003\""));
        assert!(vertical_drawing_fragment.contains("aria-label=\"CELL 0003\""));

        let horizontal_drawing_fragment = render_scene_svg(
            &scene(
                page,
                rect(0, 0, 8, 10),
                Some(rect(0, 0, 8, 10)),
                None,
                true,
                true,
            ),
            1 << 20,
        )
        .unwrap();
        assert!(horizontal_drawing_fragment.contains("data-rxls-visible-label=\"CELL 0003\""));
        assert!(horizontal_drawing_fragment.contains("aria-label=\"CELL 0003\""));

        let object_fragment = render_scene_svg(
            &scene(page, page, Some(page), Some(rect(0, 0, 8, 10)), true, true),
            1 << 20,
        )
        .unwrap();
        assert!(object_fragment.contains("data-rxls-visible-label=\"CELL 000\""));
        assert!(object_fragment.contains("aria-label=\"CELL 0003\""));

        let ordinary_nested_clip = render_scene_svg(
            &scene(page, rect(0, 0, 8, 10), None, None, false, false),
            1 << 20,
        )
        .unwrap();
        assert!(ordinary_nested_clip.contains("data-rxls-visible-label=\"CELL 000\""));
        assert!(ordinary_nested_clip.contains("aria-label=\"CELL 0003\""));
    }

    #[test]
    fn authored_rtl_body_bounds_latin_retention_at_the_page_edge() {
        let page = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(8),
            height: Fixed::from_pixels(10),
        };
        let context = SvgSemanticClipContext {
            outer_clip: Some(Rect {
                width: Fixed::from_pixels(4),
                ..page
            }),
            page_clip: page,
            authored_scope: SvgAuthoredClipScope::Body,
            body_clip: Some(Rect {
                width: Fixed::from_pixels(4),
                ..page
            }),
            body_right_to_left: true,
        };
        let latin = authoritative_word_node(true);
        let latin_clip = context.glyph_clips(&latin).unwrap();
        assert!(!latin_clip.retain_selected_groups_unbounded);
        assert!(!latin_clip.retain_selected_line_unbounded);
        assert!(latin_clip.retain_outer_intersecting_words);
        assert_eq!(
            visible_glyph_label_in_clip(&latin, latin_clip).unwrap(),
            "CELL 0003"
        );

        let narrow_page = Rect {
            width: Fixed::from_pixels(4),
            ..page
        };
        let narrow_context = SvgSemanticClipContext {
            outer_clip: Some(narrow_page),
            page_clip: narrow_page,
            body_clip: Some(narrow_page),
            ..context
        };
        assert_eq!(
            visible_glyph_label_in_clip(&latin, narrow_context.glyph_clips(&latin).unwrap())
                .unwrap(),
            "CELL"
        );

        let mut non_latin = latin;
        non_latin.text = "한글 0003".to_string();
        let non_latin_clip = context.glyph_clips(&non_latin).unwrap();
        assert!(non_latin_clip.retain_selected_groups_unbounded);
        assert!(
            !non_latin_clip.retain_selected_line_unbounded,
            "a single retained group does not need prepared-line expansion"
        );
        assert!(!non_latin_clip.retain_outer_intersecting_words);
    }

    #[test]
    fn glyph_semantic_boundary_uses_interior_ink_center() {
        let pixel = FIXED_UNITS_PER_PIXEL;
        let mut commands = rectangle_commands(9 * pixel + 2 * pixel / 5, 10 * pixel);
        commands.extend(rectangle_commands(8 * pixel, 9 * pixel));
        let node = glyph_node(
            "ab",
            0,
            10 * pixel,
            commands,
            vec![
                GlyphCluster {
                    source_start: 0,
                    source_end: 1,
                    command_start: 0,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 1,
                    source_end: 2,
                    command_start: 5,
                    command_end: 10,
                },
            ],
        );
        let clip = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(10),
            height: Fixed::from_pixels(10),
        };
        let transform = SvgSemanticTransform::rotation(0, Fixed::ZERO, Fixed::ZERO);
        let nominal_bounds = Rect {
            x: Fixed::from_pixels(8),
            y: Fixed::ZERO,
            width: Fixed::from_pixels(1),
            height: Fixed::from_pixels(1),
        };

        assert!(!glyph_cluster_has_interior_horizontal_center(
            &node,
            0,
            nominal_bounds,
            clip,
            transform,
            false,
        ));
        assert!(glyph_cluster_has_interior_horizontal_center(
            &node,
            1,
            nominal_bounds,
            clip,
            transform,
            false,
        ));
    }

    #[test]
    fn glyph_visible_label_respects_scene_and_nested_clips() {
        let mut root_commands = rectangle_commands(0, 4);
        root_commands.extend(rectangle_commands(12, 16));
        let mut nested_commands = rectangle_commands(0, 4);
        nested_commands.extend(rectangle_commands(6, 8));
        let clusters = vec![
            GlyphCluster {
                source_start: 0,
                source_end: 6,
                command_start: 0,
                command_end: 5,
            },
            GlyphCluster {
                source_start: 7,
                source_end: 14,
                command_start: 5,
                command_end: 10,
            },
        ];
        let root_clipped = glyph_node("inside outside", 0, 20, root_commands, clusters.clone());
        let nested_clipped = glyph_node("inside outside", 0, 20, nested_commands, clusters);
        let scene = Scene {
            title: "effective clips".to_string(),
            width: Fixed::from_raw(10),
            height: Fixed::from_raw(10),
            background: Rgb::WHITE,
            nodes: vec![
                SceneNode::GlyphRun(root_clipped),
                SceneNode::ClipGroup(crate::scene::ClipGroupNode {
                    clip: Rect {
                        x: Fixed::ZERO,
                        y: Fixed::ZERO,
                        width: Fixed::from_raw(5),
                        height: Fixed::from_raw(10),
                    },
                    nodes: vec![SceneNode::GlyphRun(nested_clipped)],
                }),
            ],
        };

        let svg = render_scene_svg(&scene, 1 << 20).unwrap();
        assert_eq!(svg.matches("data-rxls-visible-label=\"inside\"").count(), 2);
        assert!(!svg.contains("data-rxls-visible-label=\"inside outside\""));
    }
}
