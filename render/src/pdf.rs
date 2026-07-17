//! Deterministic PDF serialization from print-page scenes.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write as _;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

use crate::error::{LimitKind, RenderError};
use crate::print::PrintDocument;
use crate::scene::{
    backend_image_trace, backend_text_trace, format_fixed, BackendGeometryTrace,
    BackendGlyphTraceBuilder, BackendNodeTrace, BackendPathTraceBuilder, Fixed, GlyphRunNode,
    ImageNode, LineNode, PathCommand, PathNode, Rect, RectNode, Rgb, Scene, SceneNode, TextAnchor,
    TextBaseline, TextNode, FIXED_UNITS_PER_PIXEL,
};

const PDF_POINTS_PER_CSS_PIXEL_NUMERATOR: i64 = 3;
const PDF_POINTS_PER_CSS_PIXEL_DENOMINATOR: i64 = 4;
const TYPE3_GLYPHS_PER_SUBSET: usize = 255;
const MAX_TYPE3_GLYPH_PROGRAMS: u64 = 1_000_000;
const MAX_CLIP_GROUP_DEPTH: usize = 64;

#[derive(Debug)]
struct PdfPage {
    width_points: String,
    height_points: String,
    content: Vec<u8>,
    links: Vec<PdfLink>,
    images: Vec<PdfImage>,
    uses_standard_font: bool,
    subset_fonts: BTreeSet<usize>,
}

#[derive(Debug)]
struct PdfImage {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
    alpha: Option<Vec<u8>>,
}

#[derive(Debug)]
struct PdfLink {
    rect: [String; 4],
    target_hex: String,
}

#[derive(Debug, Clone, Copy)]
struct PdfGlyphReference {
    subset_index: usize,
    code: u8,
}

#[derive(Debug)]
struct PdfGlyphProgram {
    width: Fixed,
    unicode_hex: String,
    content: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct PdfGlyphBounds {
    min_x: Fixed,
    min_y: Fixed,
    max_x: Fixed,
    max_y: Fixed,
}

impl PdfGlyphBounds {
    fn include(&mut self, x: Fixed, y: Fixed) {
        self.min_x = Fixed::from_raw(self.min_x.raw().min(x.raw()));
        self.min_y = Fixed::from_raw(self.min_y.raw().min(y.raw()));
        self.max_x = Fixed::from_raw(self.max_x.raw().max(x.raw()));
        self.max_y = Fixed::from_raw(self.max_y.raw().max(y.raw()));
    }

    fn union(&mut self, other: Self) {
        self.include(other.min_x, other.min_y);
        self.include(other.max_x, other.max_y);
    }
}

#[derive(Debug, Default)]
struct PdfFontSubset {
    glyphs: Vec<PdfGlyphProgram>,
    bounds: Option<PdfGlyphBounds>,
}

#[derive(Debug)]
struct PdfFontRegistry {
    subsets: Vec<PdfFontSubset>,
    glyph_count: u64,
    retained_bytes: u64,
    glyph_limit: u64,
    byte_limit: u64,
}

impl PdfFontRegistry {
    fn new(backend_command_limit: u64, byte_limit: u64) -> Self {
        Self {
            subsets: Vec::new(),
            glyph_count: 0,
            retained_bytes: 0,
            glyph_limit: backend_command_limit.min(MAX_TYPE3_GLYPH_PROGRAMS),
            byte_limit,
        }
    }

    fn register_node(
        &mut self,
        node: &GlyphRunNode,
        mut trace: Option<&mut BackendGlyphTraceBuilder<'_>>,
    ) -> Result<Vec<PdfGlyphReference>, RenderError> {
        let mut references = Vec::with_capacity(node.clusters.len().max(1));
        if node.clusters.is_empty() {
            if !node.text.is_empty() {
                references.push(self.register_glyph(
                    node,
                    0,
                    0,
                    &node.text,
                    trace.as_deref_mut(),
                )?);
            }
            return Ok(references);
        }
        for cluster in &node.clusters {
            let source_start =
                usize::try_from(cluster.source_start).map_err(|_| RenderError::Backend {
                    reason: "invalid_glyph_metadata",
                })?;
            let source_end =
                usize::try_from(cluster.source_end).map_err(|_| RenderError::Backend {
                    reason: "invalid_glyph_metadata",
                })?;
            let source = node
                .text
                .get(source_start..source_end)
                .ok_or(RenderError::Backend {
                    reason: "invalid_glyph_metadata",
                })?;
            references.push(self.register_glyph(
                node,
                cluster.command_start,
                cluster.command_end,
                source,
                trace.as_deref_mut(),
            )?);
        }
        Ok(references)
    }

    fn register_glyph(
        &mut self,
        node: &GlyphRunNode,
        command_start: u64,
        command_end: u64,
        source: &str,
        trace: Option<&mut BackendGlyphTraceBuilder<'_>>,
    ) -> Result<PdfGlyphReference, RenderError> {
        let actual = self
            .glyph_count
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::BackendCommands, self.glyph_limit, actual)?;

        let start = usize::try_from(command_start).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let end = usize::try_from(command_end).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let commands = node.commands.get(start..end).ok_or(RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let bounds = glyph_bounds(commands);
        let width = bounds
            .and_then(|bounds| bounds.max_x.checked_sub(bounds.min_x))
            .filter(|width| width.raw() > 0)
            .unwrap_or_else(|| Fixed::from_pixels(1));
        let mut content = BoundedContent::new(self.byte_limit);
        content.push(&format!("{} 0 d0\n", format_fixed(width)))?;
        push_glyph_program_paints(&mut content, node, command_start, command_end, trace)?;
        let content = content.finish();
        let unicode_hex = utf16be_hex(source);
        let retained = (content.len() as u64)
            .checked_add(unicode_hex.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(retained)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::PdfBytes, self.byte_limit, self.retained_bytes)?;

        if self
            .subsets
            .last()
            .is_none_or(|subset| subset.glyphs.len() == TYPE3_GLYPHS_PER_SUBSET)
        {
            self.subsets.push(PdfFontSubset::default());
        }
        let subset_index = self.subsets.len() - 1;
        let subset = &mut self.subsets[subset_index];
        if let Some(bounds) = bounds {
            match subset.bounds.as_mut() {
                Some(combined) => combined.union(bounds),
                None => subset.bounds = Some(bounds),
            }
        }
        subset.glyphs.push(PdfGlyphProgram {
            width,
            unicode_hex,
            content,
        });
        self.glyph_count = actual;
        Ok(PdfGlyphReference {
            subset_index,
            code: u8::try_from(subset.glyphs.len()).map_err(|_| RenderError::Backend {
                reason: "pdf_type3_subset_overflow",
            })?,
        })
    }
}

#[derive(Debug)]
struct PdfFontObjectIds {
    font: u32,
    to_unicode: u32,
    glyphs: Vec<u32>,
}

/// Serialize all print pages into one byte-deterministic PDF 1.7 document.
///
/// Font-shaped cells remain the exact verified glyph outlines from the shared
/// scene. Deterministic Type 3 subsets retain each bounded source cluster as a
/// real glyph program, while `ActualText` and `ToUnicode` preserve logical text
/// and cluster mappings without consulting host fonts. Approximate `Text` nodes
/// retain the PDF standard Helvetica fallback only when the caller deliberately
/// renders without a verified font pack.
pub fn render_print_document_pdf(document: &PrintDocument) -> Result<Vec<u8>, RenderError> {
    render_print_document_pdf_impl(document, None)
}

#[cfg(test)]
pub(crate) fn render_print_document_pdf_with_trace(
    document: &PrintDocument,
) -> Result<(Vec<u8>, Vec<BackendGeometryTrace>), RenderError> {
    let mut traces = Vec::with_capacity(document.pages.len());
    let pdf = render_print_document_pdf_impl(document, Some(&mut traces))?;
    Ok((pdf, traces))
}

fn render_print_document_pdf_impl(
    document: &PrintDocument,
    mut traces: Option<&mut Vec<BackendGeometryTrace>>,
) -> Result<Vec<u8>, RenderError> {
    if document.pages.is_empty() {
        return Err(RenderError::Backend {
            reason: "pdf_requires_page",
        });
    }
    let mut pages = Vec::with_capacity(document.pages.len());
    let mut command_count = 0_u64;
    let mut content_bytes = 0_u64;
    let mut font_registry = PdfFontRegistry::new(
        document.limits.max_backend_commands,
        document.limits.max_pdf_bytes,
    );
    for page in &document.pages {
        let mut page_trace = traces
            .is_some()
            .then(|| BackendGeometryTrace::new(&page.scene));
        let page = build_pdf_page(
            &page.scene,
            document.limits.max_pdf_bytes,
            &mut command_count,
            &mut font_registry,
            page_trace.as_mut(),
        )?;
        if let (Some(traces), Some(page_trace)) = (traces.as_deref_mut(), page_trace) {
            traces.push(page_trace);
        }
        content_bytes = content_bytes
            .checked_add(page.content.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(
            LimitKind::PdfBytes,
            document.limits.max_pdf_bytes,
            content_bytes,
        )?;
        let retained_bytes = content_bytes
            .checked_add(font_registry.retained_bytes)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(
            LimitKind::PdfBytes,
            document.limits.max_pdf_bytes,
            retained_bytes,
        )?;
        pages.push(page);
        enforce(
            LimitKind::BackendCommands,
            document.limits.max_backend_commands,
            command_count,
        )?;
    }

    let annotation_count: usize = pages.iter().map(|page| page.links.len()).sum();
    let page_object_base = 3_u32;
    let annotation_object_base = page_object_base + (pages.len() as u32) * 2;
    let mut annotation_ids = Vec::with_capacity(pages.len());
    let mut next_annotation = annotation_object_base;
    for page in &pages {
        let mut ids = Vec::with_capacity(page.links.len());
        for _ in &page.links {
            ids.push(next_annotation);
            next_annotation += 1;
        }
        annotation_ids.push(ids);
    }
    let image_object_base = annotation_object_base + annotation_count as u32;
    let mut next_image = image_object_base;
    let mut image_ids = Vec::with_capacity(pages.len());
    for page in &pages {
        let mut ids = Vec::with_capacity(page.images.len());
        for image in &page.images {
            let main = next_image;
            next_image += 1;
            let alpha = image.alpha.as_ref().map(|_| {
                let id = next_image;
                next_image += 1;
                id
            });
            ids.push((main, alpha));
        }
        image_ids.push(ids);
    }
    let uses_standard_font = pages.iter().any(|page| page.uses_standard_font);
    let standard_font_object = uses_standard_font.then_some(next_image);
    if uses_standard_font {
        next_image += 1;
    }
    let notdef_object = if font_registry.subsets.is_empty() {
        None
    } else {
        let id = next_image;
        next_image += 1;
        Some(id)
    };
    let mut subset_object_ids = Vec::with_capacity(font_registry.subsets.len());
    for subset in &font_registry.subsets {
        let font = next_image;
        next_image += 1;
        let to_unicode = next_image;
        next_image += 1;
        let mut glyphs = Vec::with_capacity(subset.glyphs.len());
        for _ in &subset.glyphs {
            glyphs.push(next_image);
            next_image += 1;
        }
        subset_object_ids.push(PdfFontObjectIds {
            font,
            to_unicode,
            glyphs,
        });
    }
    let info_object = next_image;
    let object_count = info_object;

    let document_id = document_id(&pages, &font_registry.subsets);
    let mut output = BoundedPdf::new(document.limits.max_pdf_bytes);
    output.push(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n")?;
    let mut offsets = vec![0_u64; object_count as usize + 1];

    write_object(
        &mut output,
        &mut offsets,
        1,
        b"<< /Type /Catalog /Pages 2 0 R /Lang (en-US) /ViewerPreferences << /DisplayDocTitle true >> >>",
    )?;
    let mut kids = String::new();
    for index in 0..pages.len() {
        let _ = write!(&mut kids, "{} 0 R ", page_object_base + index as u32 * 2);
    }
    let pages_dictionary = format!(
        "<< /Type /Pages /Count {} /Kids [{}] >>",
        pages.len(),
        kids.trim_end()
    );
    write_object(&mut output, &mut offsets, 2, pages_dictionary.as_bytes())?;

    for (index, page) in pages.iter().enumerate() {
        let page_id = page_object_base + index as u32 * 2;
        let content_id = page_id + 1;
        let mut annotations = String::new();
        if !annotation_ids[index].is_empty() {
            annotations.push_str(" /Annots [");
            for id in &annotation_ids[index] {
                let _ = write!(&mut annotations, "{id} 0 R ");
            }
            annotations.push(']');
        }
        let mut xobjects = String::new();
        if !image_ids[index].is_empty() {
            xobjects.push_str(" /XObject <<");
            for (image_index, (id, _)) in image_ids[index].iter().enumerate() {
                let _ = write!(&mut xobjects, " /Im{image_index} {id} 0 R");
            }
            xobjects.push_str(" >>");
        }
        let mut fonts = String::new();
        if page.uses_standard_font {
            let id = standard_font_object.ok_or(RenderError::Backend {
                reason: "pdf_standard_font_identity",
            })?;
            let _ = write!(&mut fonts, "/F0 {id} 0 R");
        }
        for &subset_index in &page.subset_fonts {
            let ids = subset_object_ids
                .get(subset_index)
                .ok_or(RenderError::Backend {
                    reason: "pdf_type3_subset_identity",
                })?;
            let _ = write!(&mut fonts, " /RG{subset_index} {} 0 R", ids.font);
        }
        let dictionary = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /CropBox [0 0 {} {}] /Resources << /Font << {} >>{} >> /Contents {} 0 R{} >>",
            page.width_points,
            page.height_points,
            page.width_points,
            page.height_points,
            fonts,
            xobjects,
            content_id,
            annotations
        );
        write_object(&mut output, &mut offsets, page_id, dictionary.as_bytes())?;
        let header = format!("<< /Length {} >>\nstream\n", page.content.len());
        let mut stream = Vec::with_capacity(header.len() + page.content.len() + 11);
        stream.extend_from_slice(header.as_bytes());
        stream.extend_from_slice(&page.content);
        stream.extend_from_slice(b"endstream");
        write_object(&mut output, &mut offsets, content_id, &stream)?;
    }

    let mut annotation_index = 0_usize;
    for page in &pages {
        for link in &page.links {
            let id = annotation_object_base + annotation_index as u32;
            annotation_index += 1;
            let dictionary = format!(
                "<< /Type /Annot /Subtype /Link /Rect [{} {} {} {}] /Border [0 0 0] /A << /S /URI /URI <{}> >> >>",
                link.rect[0], link.rect[1], link.rect[2], link.rect[3], link.target_hex
            );
            write_object(&mut output, &mut offsets, id, dictionary.as_bytes())?;
        }
    }
    for (page_index, page) in pages.iter().enumerate() {
        for (image_index, image) in page.images.iter().enumerate() {
            let (image_id, alpha_id) = image_ids[page_index][image_index];
            write_pdf_image_object(&mut output, &mut offsets, image_id, image, alpha_id)?;
            if let (Some(alpha), Some(alpha_id)) = (image.alpha.as_deref(), alpha_id) {
                write_pdf_alpha_object(
                    &mut output,
                    &mut offsets,
                    alpha_id,
                    image.width,
                    image.height,
                    alpha,
                )?;
            }
        }
    }
    if let Some(id) = standard_font_object {
        write_object(
            &mut output,
            &mut offsets,
            id,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
        )?;
    }
    if let Some(id) = notdef_object {
        write_pdf_stream_object(&mut output, &mut offsets, id, b"1 0 d0\n")?;
    }
    for (subset_index, subset) in font_registry.subsets.iter().enumerate() {
        let ids = &subset_object_ids[subset_index];
        let notdef = notdef_object.ok_or(RenderError::Backend {
            reason: "pdf_type3_notdef_identity",
        })?;
        let dictionary = type3_font_dictionary(subset_index, subset, ids, notdef)?;
        write_object(&mut output, &mut offsets, ids.font, dictionary.as_bytes())?;
        let cmap = type3_to_unicode_cmap(subset_index, subset);
        write_pdf_stream_object(&mut output, &mut offsets, ids.to_unicode, cmap.as_bytes())?;
        for (glyph, &id) in subset.glyphs.iter().zip(&ids.glyphs) {
            write_pdf_stream_object(&mut output, &mut offsets, id, &glyph.content)?;
        }
    }
    write_object(
        &mut output,
        &mut offsets,
        info_object,
        b"<< /Title (rxls deterministic worksheet rendering) /Creator (rxls-render) /Producer (rxls-render) /CreationDate (D:19700101000000Z) /ModDate (D:19700101000000Z) >>",
    )?;

    let start_xref = output.len();
    output.push(format!("xref\n0 {}\n", object_count + 1).as_bytes())?;
    output.push(b"0000000000 65535 f \n")?;
    for offset in offsets.iter().skip(1) {
        output.push(format!("{offset:010} 00000 n \n").as_bytes())?;
    }
    let trailer = format!(
        "trailer\n<< /Size {} /Root 1 0 R /Info {} 0 R /ID [<{}><{}>] >>\nstartxref\n{}\n%%EOF\n",
        object_count + 1,
        info_object,
        document_id,
        document_id,
        start_xref
    );
    output.push(trailer.as_bytes())?;
    Ok(output.finish())
}

fn build_pdf_page(
    scene: &Scene,
    max_bytes: u64,
    command_count: &mut u64,
    font_registry: &mut PdfFontRegistry,
    trace: Option<&mut BackendGeometryTrace>,
) -> Result<PdfPage, RenderError> {
    let page_command_count = scene.nodes.iter().try_fold(0_u64, |sum, node| {
        sum.checked_add(node_command_count(node, 0)?)
            .ok_or(RenderError::CoordinateOverflow)
    })?;
    *command_count = (*command_count)
        .checked_add(page_command_count)
        .ok_or(RenderError::CoordinateOverflow)?;
    let width_points = fixed_to_pdf_points(scene.width)?;
    let height_points = fixed_to_pdf_points(scene.height)?;
    let mut content = BoundedContent::new(max_bytes);
    content.push("q\n")?;
    content.push(&format!("0.75 0 0 -0.75 0 {} cm\n", height_points))?;
    push_clip(
        &mut content,
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: scene.width,
            height: scene.height,
        },
    )?;
    push_rgb_fill(&mut content, scene.background)?;
    content.push(&format!(
        "0 0 {} {} re f\n",
        format_fixed(scene.width),
        format_fixed(scene.height)
    ))?;
    let mut links = Vec::new();
    let mut images = Vec::new();
    let mut uses_standard_font = false;
    let mut subset_fonts = BTreeSet::new();
    push_scene_nodes(
        &mut content,
        &scene.nodes,
        scene.height,
        font_registry,
        &mut subset_fonts,
        &mut links,
        &mut images,
        &mut uses_standard_font,
        Some(Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: scene.width,
            height: scene.height,
        }),
        trace,
        0,
    )?;
    content.push("Q\n")?;
    Ok(PdfPage {
        width_points,
        height_points,
        content: content.finish(),
        links,
        images,
        uses_standard_font,
        subset_fonts,
    })
}

#[allow(clippy::too_many_arguments)]
fn push_scene_nodes(
    content: &mut BoundedContent,
    nodes: &[SceneNode],
    scene_height: Fixed,
    font_registry: &mut PdfFontRegistry,
    subset_fonts: &mut BTreeSet<usize>,
    links: &mut Vec<PdfLink>,
    images: &mut Vec<PdfImage>,
    uses_standard_font: &mut bool,
    active_clip: Option<Rect>,
    mut trace: Option<&mut BackendGeometryTrace>,
    depth: usize,
) -> Result<(), RenderError> {
    for node in nodes {
        match node {
            SceneNode::ClipGroup(group) => {
                if depth >= MAX_CLIP_GROUP_DEPTH {
                    return Err(RenderError::Backend {
                        reason: "pdf_clip_group_depth",
                    });
                }
                content.push("q\n")?;
                push_clip(content, group.clip)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::ClipStart(group.clip));
                }
                let nested_clip = match active_clip {
                    Some(active_clip) => intersect_clip_rects(active_clip, group.clip)?,
                    None => None,
                };
                push_scene_nodes(
                    content,
                    &group.nodes,
                    scene_height,
                    font_registry,
                    subset_fonts,
                    links,
                    images,
                    uses_standard_font,
                    nested_clip,
                    trace.as_deref_mut(),
                    depth + 1,
                )?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::ClipEnd);
                }
                content.push("Q\n")?;
            }
            SceneNode::Rect(node) => {
                push_rect(content, node)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Rect(node.clone()));
                }
            }
            SceneNode::Line(node) => {
                push_line(content, node)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Line(node.clone()));
                }
            }
            SceneNode::Path(node) => {
                let mut path_trace = trace.is_some().then(|| BackendPathTraceBuilder::new(node));
                push_path_node(content, node, path_trace.as_mut())?;
                if let (Some(trace), Some(path_trace)) = (trace.as_deref_mut(), path_trace) {
                    trace.push(BackendNodeTrace::Path(
                        path_trace.finish().map_err(trace_error)?,
                    ));
                }
            }
            SceneNode::Image(node) => {
                let image_index = images.len();
                images.push(pdf_image(node)?);
                push_image(content, node, image_index)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Image(backend_image_trace(node)));
                }
            }
            SceneNode::Text(node) => {
                *uses_standard_font = true;
                push_text(content, node)?;
                let mut accepted_link = None;
                if let Some(target) = node.hyperlink.as_deref() {
                    if is_safe_hyperlink(target) {
                        accepted_link = Some(target);
                        let link_rect = match active_clip {
                            Some(active_clip) => {
                                intersect_clip_rects(active_clip, node.clip_bounds)?
                            }
                            None => None,
                        };
                        if let Some(link_rect) = link_rect {
                            links.push(pdf_link(link_rect, scene_height, target)?);
                        }
                    }
                }
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Text(backend_text_trace(
                        node,
                        accepted_link,
                    )));
                }
            }
            SceneNode::GlyphRun(node) => {
                let mut glyph_trace = trace.is_some().then(|| BackendGlyphTraceBuilder::new(node));
                push_glyph_run(
                    content,
                    node,
                    font_registry,
                    subset_fonts,
                    glyph_trace.as_mut(),
                )?;
                if let Some(target) = node.hyperlink.as_deref() {
                    if is_safe_hyperlink(target) {
                        let link_rect = match active_clip {
                            Some(active_clip) => {
                                intersect_clip_rects(active_clip, node.clip_bounds)?
                            }
                            None => None,
                        };
                        if let Some(link_rect) = link_rect {
                            links.push(pdf_link(link_rect, scene_height, target)?);
                        }
                        if let Some(trace) = glyph_trace.as_mut() {
                            trace
                                .record_link(node.clip_bounds, target)
                                .map_err(trace_error)?;
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
    Ok(())
}

fn push_rect(content: &mut BoundedContent, node: &RectNode) -> Result<(), RenderError> {
    content.push("q\n")?;
    if let Some(fill) = node.fill {
        push_rgb_fill(content, fill)?;
    }
    if let Some(stroke) = node.stroke {
        push_rgb_stroke(content, stroke)?;
        content.push(&format!("{} w\n", format_fixed(node.stroke_width)))?;
    }
    content.push(&format!(
        "{} {} {} {} re {}\nQ\n",
        format_fixed(node.rect.x),
        format_fixed(node.rect.y),
        format_fixed(node.rect.width),
        format_fixed(node.rect.height),
        match (node.fill.is_some(), node.stroke.is_some()) {
            (true, true) => "B",
            (true, false) => "f",
            (false, true) => "S",
            (false, false) => "n",
        }
    ))
}

fn push_line(content: &mut BoundedContent, node: &LineNode) -> Result<(), RenderError> {
    content.push("q\n")?;
    push_rgb_stroke(content, node.color)?;
    content.push(&format!(
        "{} w 0 J {} {} m {} {} l S\nQ\n",
        format_fixed(node.width),
        format_fixed(node.x1),
        format_fixed(node.y1),
        format_fixed(node.x2),
        format_fixed(node.y2)
    ))
}

fn push_path_node(
    content: &mut BoundedContent,
    node: &PathNode,
    mut trace: Option<&mut BackendPathTraceBuilder<'_>>,
) -> Result<(), RenderError> {
    content.push("q\n")?;
    if let Some(fill) = node.fill {
        push_rgb_fill(content, fill)?;
    }
    if let Some(stroke) = node.stroke {
        push_rgb_stroke(content, stroke)?;
        content.push(&format!("{} w\n", format_fixed(node.stroke_width)))?;
    }
    push_path_with_trace(content, &node.commands, |index, command| {
        if let Some(trace) = trace.as_deref_mut() {
            trace.record(index, command).map_err(trace_error)?;
        }
        Ok(())
    })?;
    content.push(match (node.fill.is_some(), node.stroke.is_some()) {
        (true, true) => "B\nQ\n",
        (true, false) => "f\nQ\n",
        (false, true) => "S\nQ\n",
        (false, false) => "n\nQ\n",
    })
}

fn push_image(
    content: &mut BoundedContent,
    node: &ImageNode,
    image_index: usize,
) -> Result<(), RenderError> {
    content.push("q\n")?;
    if node.rotation_mdeg != 0 {
        let pivot_x = Fixed::from_raw(
            node.rect
                .x
                .raw()
                .checked_add(node.rect.width.raw() / 2)
                .ok_or(RenderError::CoordinateOverflow)?,
        );
        let pivot_y = Fixed::from_raw(
            node.rect
                .y
                .raw()
                .checked_add(node.rect.height.raw() / 2)
                .ok_or(RenderError::CoordinateOverflow)?,
        );
        push_rotation_mdeg(content, node.rotation_mdeg, pivot_x, pivot_y)?;
    }
    content.push(&format!(
        "{} 0 0 {} {} {} cm\n/Im{} Do\nQ\n",
        format_fixed(node.rect.width),
        format_fixed(node.rect.height),
        format_fixed(node.rect.x),
        format_fixed(node.rect.y),
        image_index
    ))
}

fn pdf_image(node: &ImageNode) -> Result<PdfImage, RenderError> {
    let expected = u64::from(node.pixel_width)
        .checked_mul(u64::from(node.pixel_height))
        .and_then(|value| value.checked_mul(4))
        .ok_or(RenderError::CoordinateOverflow)?;
    if expected != node.rgba.len() as u64 {
        return Err(RenderError::Backend {
            reason: "invalid_image_rgba_length",
        });
    }
    let pixels = usize::try_from(expected / 4).map_err(|_| RenderError::CoordinateOverflow)?;
    let mut rgb = Vec::with_capacity(pixels.saturating_mul(3));
    let mut alpha = Vec::with_capacity(pixels);
    let mut opaque = true;
    for rgba in node.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&rgba[..3]);
        alpha.push(rgba[3]);
        opaque &= rgba[3] == 255;
    }
    Ok(PdfImage {
        width: node.pixel_width,
        height: node.pixel_height,
        rgb: zlib_compress(&rgb)?,
        alpha: if opaque {
            None
        } else {
            Some(zlib_compress(&alpha)?)
        },
    })
}

fn zlib_compress(bytes: &[u8]) -> Result<Vec<u8>, RenderError> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(bytes).map_err(|_| RenderError::Backend {
        reason: "pdf_image_compression",
    })?;
    encoder.finish().map_err(|_| RenderError::Backend {
        reason: "pdf_image_compression",
    })
}

fn push_glyph_run(
    content: &mut BoundedContent,
    node: &GlyphRunNode,
    font_registry: &mut PdfFontRegistry,
    subset_fonts: &mut BTreeSet<usize>,
    mut trace: Option<&mut BackendGlyphTraceBuilder<'_>>,
) -> Result<(), RenderError> {
    if !node.metadata_is_valid() {
        return Err(RenderError::Backend {
            reason: "invalid_glyph_metadata",
        });
    }
    let glyphs = font_registry.register_node(node, trace.as_deref_mut())?;
    content.push("q\n")?;
    push_clip(content, node.clip_bounds)?;
    if let Some(trace) = trace.as_deref_mut() {
        trace.record_clip(node.clip_bounds).map_err(trace_error)?;
    }
    if node.rotation_degrees != 0 {
        push_rotation(content, node.rotation_degrees, node.pivot_x, node.pivot_y)?;
    }
    if !glyphs.is_empty() {
        push_actual_text_begin(content, &node.text)?;
        for glyph in glyphs {
            subset_fonts.insert(glyph.subset_index);
            content.push(&format!(
                "BT /RG{} 1 Tf 1 0 0 1 0 0 Tm <{:02X}> Tj ET\n",
                glyph.subset_index, glyph.code
            ))?;
        }
        content.push("EMC\n")?;
    }
    for decoration in &node.decorations {
        push_rgb_stroke(content, decoration.color)?;
        content.push(&format!(
            "{} w {} {} m {} {} l S\n",
            format_fixed(decoration.width),
            format_fixed(decoration.x1),
            format_fixed(decoration.y1),
            format_fixed(decoration.x2),
            format_fixed(decoration.y2)
        ))?;
        if let Some(trace) = trace.as_deref_mut() {
            trace.record_decoration(decoration).map_err(trace_error)?;
        }
    }
    content.push("Q\n")
}

fn push_glyph_program_paints(
    content: &mut BoundedContent,
    node: &GlyphRunNode,
    command_start: u64,
    command_end: u64,
    mut trace: Option<&mut BackendGlyphTraceBuilder<'_>>,
) -> Result<(), RenderError> {
    let mut covered = command_start;
    for paint in &node.paints {
        let start = paint.command_start.max(command_start);
        let end = paint.command_end.min(command_end);
        if start >= end {
            continue;
        }
        if start != covered {
            return Err(RenderError::Backend {
                reason: "invalid_glyph_metadata",
            });
        }
        let start = usize::try_from(start).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let end_index = usize::try_from(end).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let commands = node
            .commands
            .get(start..end_index)
            .ok_or(RenderError::Backend {
                reason: "invalid_glyph_metadata",
            })?;
        push_rgb_fill(content, paint.color)?;
        push_path_with_trace(content, commands, |offset, command| {
            if let Some(trace) = trace.as_deref_mut() {
                trace
                    .record_command(start as u64 + offset as u64, command, paint.color)
                    .map_err(trace_error)?;
            }
            Ok(())
        })?;
        content.push("f\n")?;
        covered = end;
    }
    if covered != command_end {
        return Err(RenderError::Backend {
            reason: "invalid_glyph_metadata",
        });
    }
    Ok(())
}

fn glyph_bounds(commands: &[PathCommand]) -> Option<PdfGlyphBounds> {
    let mut bounds = None::<PdfGlyphBounds>;
    let mut include = |x: Fixed, y: Fixed| match bounds.as_mut() {
        Some(bounds) => bounds.include(x, y),
        None => {
            bounds = Some(PdfGlyphBounds {
                min_x: x,
                min_y: y,
                max_x: x,
                max_y: y,
            });
        }
    };
    for command in commands {
        match *command {
            PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => include(x, y),
            PathCommand::QuadraticTo {
                control_x,
                control_y,
                x,
                y,
            } => {
                include(control_x, control_y);
                include(x, y);
            }
            PathCommand::CubicTo {
                control1_x,
                control1_y,
                control2_x,
                control2_y,
                x,
                y,
            } => {
                include(control1_x, control1_y);
                include(control2_x, control2_y);
                include(x, y);
            }
            PathCommand::Close => {}
        }
    }
    bounds
}

fn push_text(content: &mut BoundedContent, node: &TextNode) -> Result<(), RenderError> {
    let (anchor_x, y) = text_anchor_point(node)?;
    let approximate = pdf_literal_escaped_ascii(&node.text);
    let approximate_width = Fixed::from_raw(
        node.style
            .size
            .raw()
            .checked_mul(node.text.chars().count() as i64)
            .and_then(|value| value.checked_div(2))
            .ok_or(RenderError::CoordinateOverflow)?,
    );
    let x = match node.style.anchor {
        TextAnchor::Start => anchor_x,
        TextAnchor::Middle => anchor_x
            .checked_sub(Fixed::from_raw(approximate_width.raw() / 2))
            .ok_or(RenderError::CoordinateOverflow)?,
        TextAnchor::End => anchor_x
            .checked_sub(approximate_width)
            .ok_or(RenderError::CoordinateOverflow)?,
    };
    content.push("q\n")?;
    push_clip(content, node.clip_bounds)?;
    if node.style.rotation_degrees != 0 {
        push_rotation(content, node.style.rotation_degrees, anchor_x, y)?;
    }
    push_actual_text_begin(content, &node.text)?;
    push_rgb_fill(content, node.style.color)?;
    content.push(&format!(
        "BT /F0 {} Tf 1 0 0 -1 {} {} Tm ({}) Tj ET\nEMC\nQ\n",
        format_fixed(node.style.size),
        format_fixed(x),
        format_fixed(y),
        approximate
    ))
}

fn push_path_with_trace<F>(
    content: &mut BoundedContent,
    commands: &[PathCommand],
    mut record: F,
) -> Result<(), RenderError>
where
    F: FnMut(usize, PathCommand) -> Result<(), RenderError>,
{
    let mut current = None::<(Fixed, Fixed)>;
    for (index, command) in commands.iter().enumerate() {
        match *command {
            PathCommand::MoveTo { x, y } => {
                content.push(&format!("{} {} m\n", format_fixed(x), format_fixed(y)))?;
                current = Some((x, y));
            }
            PathCommand::LineTo { x, y } => {
                content.push(&format!("{} {} l\n", format_fixed(x), format_fixed(y)))?;
                current = Some((x, y));
            }
            PathCommand::QuadraticTo {
                control_x,
                control_y,
                x,
                y,
            } => {
                let (start_x, start_y) = current.ok_or(RenderError::Backend {
                    reason: "quadratic_without_current_point",
                })?;
                let control1_x = quadratic_cubic_control(start_x, control_x)?;
                let control1_y = quadratic_cubic_control(start_y, control_y)?;
                let control2_x = quadratic_cubic_control(x, control_x)?;
                let control2_y = quadratic_cubic_control(y, control_y)?;
                content.push(&format!(
                    "{} {} {} {} {} {} c\n",
                    format_fixed(control1_x),
                    format_fixed(control1_y),
                    format_fixed(control2_x),
                    format_fixed(control2_y),
                    format_fixed(x),
                    format_fixed(y)
                ))?;
                current = Some((x, y));
            }
            PathCommand::CubicTo {
                control1_x,
                control1_y,
                control2_x,
                control2_y,
                x,
                y,
            } => {
                content.push(&format!(
                    "{} {} {} {} {} {} c\n",
                    format_fixed(control1_x),
                    format_fixed(control1_y),
                    format_fixed(control2_x),
                    format_fixed(control2_y),
                    format_fixed(x),
                    format_fixed(y)
                ))?;
                current = Some((x, y));
            }
            PathCommand::Close => content.push("h\n")?,
        }
        record(index, *command)?;
    }
    Ok(())
}

fn trace_error(reason: &'static str) -> RenderError {
    RenderError::Backend { reason }
}

fn quadratic_cubic_control(endpoint: Fixed, quadratic: Fixed) -> Result<Fixed, RenderError> {
    let delta = i128::from(quadratic.raw()) - i128::from(endpoint.raw());
    let raw = i128::from(endpoint.raw()) + (delta * 2 + delta.signum()) / 3;
    Ok(Fixed::from_raw(
        i64::try_from(raw).map_err(|_| RenderError::CoordinateOverflow)?,
    ))
}

fn push_clip(content: &mut BoundedContent, rect: Rect) -> Result<(), RenderError> {
    content.push(&format!(
        "{} {} {} {} re W n\n",
        format_fixed(rect.x),
        format_fixed(rect.y),
        format_fixed(rect.width),
        format_fixed(rect.height)
    ))
}

fn push_rotation(
    content: &mut BoundedContent,
    degrees: i16,
    pivot_x: Fixed,
    pivot_y: Fixed,
) -> Result<(), RenderError> {
    let radians = f64::from(degrees).to_radians();
    let cosine = radians.cos();
    let sine = radians.sin();
    let x = fixed_as_f64(pivot_x);
    let y = fixed_as_f64(pivot_y);
    let tx = x - cosine * x + sine * y;
    let ty = y - sine * x - cosine * y;
    content.push(&format!(
        "{} {} {} {} {} {} cm\n",
        format_decimal(cosine),
        format_decimal(sine),
        format_decimal(-sine),
        format_decimal(cosine),
        format_decimal(tx),
        format_decimal(ty)
    ))
}

fn push_rotation_mdeg(
    content: &mut BoundedContent,
    millidegrees: i32,
    pivot_x: Fixed,
    pivot_y: Fixed,
) -> Result<(), RenderError> {
    let radians = f64::from(millidegrees) * std::f64::consts::PI / 180_000.0;
    let cosine = radians.cos();
    let sine = radians.sin();
    let x = fixed_as_f64(pivot_x);
    let y = fixed_as_f64(pivot_y);
    let tx = x - cosine * x + sine * y;
    let ty = y - sine * x - cosine * y;
    content.push(&format!(
        "{} {} {} {} {} {} cm\n",
        format_decimal(cosine),
        format_decimal(sine),
        format_decimal(-sine),
        format_decimal(cosine),
        format_decimal(tx),
        format_decimal(ty)
    ))
}

fn push_actual_text_begin(content: &mut BoundedContent, text: &str) -> Result<(), RenderError> {
    content.push("/Span << /ActualText <FEFF")?;
    for unit in text.encode_utf16() {
        content.push(&format!("{unit:04X}"))?;
    }
    content.push("> >> BDC\n")
}

fn text_anchor_point(node: &TextNode) -> Result<(Fixed, Fixed), RenderError> {
    let right = node
        .bounds
        .x
        .checked_add(node.bounds.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = node
        .bounds
        .y
        .checked_add(node.bounds.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let x = match node.style.anchor {
        TextAnchor::Start => node
            .bounds
            .x
            .checked_add(node.horizontal_padding)
            .ok_or(RenderError::CoordinateOverflow)?,
        TextAnchor::Middle => Fixed::from_raw(
            node.bounds
                .x
                .raw()
                .checked_add(node.bounds.width.raw() / 2)
                .ok_or(RenderError::CoordinateOverflow)?,
        ),
        TextAnchor::End => right
            .checked_sub(node.horizontal_padding)
            .ok_or(RenderError::CoordinateOverflow)?,
    };
    // Helvetica fallback uses an alphabetic baseline. The fixed ratios keep the
    // placement deterministic and close to SVG's dominant-baseline behavior.
    let y = match node.style.baseline {
        TextBaseline::Top => node
            .bounds
            .y
            .checked_add(Fixed::from_raw(node.style.size.raw() * 4 / 5))
            .ok_or(RenderError::CoordinateOverflow)?,
        TextBaseline::Middle => Fixed::from_raw(
            node.bounds
                .y
                .raw()
                .checked_add(node.bounds.height.raw() / 2)
                .and_then(|value| value.checked_add(node.style.size.raw() * 3 / 10))
                .ok_or(RenderError::CoordinateOverflow)?,
        ),
        TextBaseline::Bottom => bottom,
    };
    Ok((x, y))
}

fn push_rgb_fill(content: &mut BoundedContent, color: Rgb) -> Result<(), RenderError> {
    content.push(&format!(
        "{} {} {} rg\n",
        channel(color.red),
        channel(color.green),
        channel(color.blue)
    ))
}

fn push_rgb_stroke(content: &mut BoundedContent, color: Rgb) -> Result<(), RenderError> {
    content.push(&format!(
        "{} {} {} RG\n",
        channel(color.red),
        channel(color.green),
        channel(color.blue)
    ))
}

fn channel(value: u8) -> String {
    if value == 0 {
        "0".to_string()
    } else if value == 255 {
        "1".to_string()
    } else {
        format_decimal(f64::from(value) / 255.0)
    }
}

fn is_safe_hyperlink(target: &str) -> bool {
    if target.is_empty() || target.trim() != target || target.chars().any(char::is_control) {
        return false;
    }
    let Some((scheme, remainder)) = target.split_once(':') else {
        return false;
    };
    !remainder.is_empty()
        && ["http", "https", "mailto"]
            .iter()
            .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
}

fn intersect_clip_rects(left: Rect, right: Rect) -> Result<Option<Rect>, RenderError> {
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

fn pdf_link(rect: Rect, page_height: Fixed, target: &str) -> Result<PdfLink, RenderError> {
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let pdf_bottom = page_height
        .checked_sub(bottom)
        .ok_or(RenderError::CoordinateOverflow)?;
    let pdf_top = page_height
        .checked_sub(rect.y)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(PdfLink {
        rect: [
            fixed_to_pdf_points(rect.x)?,
            fixed_to_pdf_points(pdf_bottom)?,
            fixed_to_pdf_points(right)?,
            fixed_to_pdf_points(pdf_top)?,
        ],
        target_hex: hex_bytes(target.as_bytes()),
    })
}

fn fixed_to_pdf_points(value: Fixed) -> Result<String, RenderError> {
    let raw = i128::from(value.raw())
        .checked_mul(i128::from(PDF_POINTS_PER_CSS_PIXEL_NUMERATOR))
        .ok_or(RenderError::CoordinateOverflow)?;
    let denominator = i128::from(FIXED_UNITS_PER_PIXEL * PDF_POINTS_PER_CSS_PIXEL_DENOMINATOR);
    Ok(format_rational(raw, denominator))
}

fn format_rational(numerator: i128, denominator: i128) -> String {
    let negative = numerator < 0;
    let magnitude = numerator.unsigned_abs();
    let denominator = denominator as u128;
    let whole = magnitude / denominator;
    let mut remainder = magnitude % denominator;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&whole.to_string());
    if remainder != 0 {
        out.push('.');
        for _ in 0..8 {
            remainder *= 10;
            out.push(char::from(b'0' + (remainder / denominator) as u8));
            remainder %= denominator;
            if remainder == 0 {
                break;
            }
        }
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.pop();
        }
    }
    out
}

fn format_decimal(value: f64) -> String {
    if value.abs() < 0.000_000_5 {
        return "0".to_string();
    }
    let mut out = format!("{value:.6}");
    while out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.pop();
    }
    out
}

fn fixed_as_f64(value: Fixed) -> f64 {
    value.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64
}

fn pdf_literal_escaped_ascii(text: &str) -> String {
    let mut output = String::new();
    for ch in text.chars() {
        match ch {
            '(' => output.push_str("\\("),
            ')' => output.push_str("\\)"),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_ascii() && !ch.is_ascii_control() => output.push(ch),
            _ => output.push('?'),
        }
    }
    output
}

fn node_command_count(node: &SceneNode, depth: usize) -> Result<u64, RenderError> {
    match node {
        SceneNode::ClipGroup(group) => {
            if depth >= MAX_CLIP_GROUP_DEPTH {
                return Err(RenderError::Backend {
                    reason: "pdf_clip_group_depth",
                });
            }
            group.nodes.iter().try_fold(2_u64, |sum, node| {
                sum.checked_add(node_command_count(node, depth + 1)?)
                    .ok_or(RenderError::CoordinateOverflow)
            })
        }
        SceneNode::Rect(_) | SceneNode::Text(_) => Ok(1),
        SceneNode::Line(_) => Ok(2),
        SceneNode::Path(node) => Ok(node.commands.len() as u64),
        SceneNode::Image(_) => Ok(1),
        SceneNode::GlyphRun(node) => (node.commands.len() as u64)
            .checked_add(node.clusters.len().max(1) as u64)
            .and_then(|count| count.checked_add(node.decorations.len() as u64 * 2))
            .ok_or(RenderError::CoordinateOverflow),
    }
}

fn utf16be_hex(text: &str) -> String {
    let mut output = String::with_capacity(text.len().saturating_mul(4));
    for unit in text.encode_utf16() {
        let _ = write!(&mut output, "{unit:04X}");
    }
    output
}

fn type3_font_dictionary(
    subset_index: usize,
    subset: &PdfFontSubset,
    ids: &PdfFontObjectIds,
    notdef_object: u32,
) -> Result<String, RenderError> {
    if subset.glyphs.is_empty() || subset.glyphs.len() != ids.glyphs.len() {
        return Err(RenderError::Backend {
            reason: "pdf_type3_subset_identity",
        });
    }
    let bounds = subset.bounds.unwrap_or(PdfGlyphBounds {
        min_x: Fixed::ZERO,
        min_y: Fixed::ZERO,
        max_x: Fixed::from_pixels(1),
        max_y: Fixed::from_pixels(1),
    });
    let mut char_procs = format!("/.notdef {notdef_object} 0 R");
    let mut differences = String::from("1");
    let mut widths = String::new();
    for (index, (glyph, id)) in subset.glyphs.iter().zip(&ids.glyphs).enumerate() {
        let code = index + 1;
        let _ = write!(&mut char_procs, " /g{code:03} {id} 0 R");
        let _ = write!(&mut differences, " /g{code:03}");
        if !widths.is_empty() {
            widths.push(' ');
        }
        widths.push_str(&format_fixed(glyph.width));
    }
    Ok(format!(
        "<< /Type /Font /Subtype /Type3 /Name /RXLSRF+OutlinedSubset{subset_index:04} /FontBBox [{} {} {} {}] /FontMatrix [1 0 0 1 0 0] /CharProcs << {} >> /Encoding << /Type /Encoding /Differences [{}] >> /FirstChar 1 /LastChar {} /Widths [{}] /Resources << >> /ToUnicode {} 0 R >>",
        format_fixed(bounds.min_x),
        format_fixed(bounds.min_y),
        format_fixed(bounds.max_x),
        format_fixed(bounds.max_y),
        char_procs,
        differences,
        subset.glyphs.len(),
        widths,
        ids.to_unicode
    ))
}

fn type3_to_unicode_cmap(subset_index: usize, subset: &PdfFontSubset) -> String {
    let mut output = format!(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /RXLSRF-OutlinedSubset{subset_index:04} def\n/CMapType 2 def\n1 begincodespacerange\n<01> <FF>\nendcodespacerange\n"
    );
    for (chunk_index, chunk) in subset.glyphs.chunks(100).enumerate() {
        let _ = writeln!(&mut output, "{} beginbfchar", chunk.len());
        for (offset, glyph) in chunk.iter().enumerate() {
            let code = chunk_index * 100 + offset + 1;
            let _ = writeln!(&mut output, "<{code:02X}> <{}>", glyph.unicode_hex);
        }
        output.push_str("endbfchar\n");
    }
    output.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    output
}

fn document_id(pages: &[PdfPage], subsets: &[PdfFontSubset]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"rxls-render-pdf-v3\0");
    for page in pages {
        digest.update((page.width_points.len() as u64).to_le_bytes());
        digest.update(page.width_points.as_bytes());
        digest.update((page.height_points.len() as u64).to_le_bytes());
        digest.update(page.height_points.as_bytes());
        digest.update((page.content.len() as u64).to_le_bytes());
        digest.update(&page.content);
        for link in &page.links {
            for value in &link.rect {
                digest.update((value.len() as u64).to_le_bytes());
                digest.update(value.as_bytes());
            }
            digest.update(link.target_hex.as_bytes());
        }
        for image in &page.images {
            digest.update(image.width.to_le_bytes());
            digest.update(image.height.to_le_bytes());
            digest.update((image.rgb.len() as u64).to_le_bytes());
            digest.update(&image.rgb);
            match &image.alpha {
                Some(alpha) => {
                    digest.update([1]);
                    digest.update((alpha.len() as u64).to_le_bytes());
                    digest.update(alpha);
                }
                None => digest.update([0]),
            }
        }
    }
    for subset in subsets {
        digest.update((subset.glyphs.len() as u64).to_le_bytes());
        for glyph in &subset.glyphs {
            digest.update(glyph.width.raw().to_le_bytes());
            digest.update((glyph.unicode_hex.len() as u64).to_le_bytes());
            digest.update(glyph.unicode_hex.as_bytes());
            digest.update((glyph.content.len() as u64).to_le_bytes());
            digest.update(&glyph.content);
        }
    }
    hex_bytes(&digest.finalize()[..16])
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn write_object(
    output: &mut BoundedPdf,
    offsets: &mut [u64],
    id: u32,
    body: &[u8],
) -> Result<(), RenderError> {
    offsets[id as usize] = output.len();
    output.push(format!("{id} 0 obj\n").as_bytes())?;
    output.push(body)?;
    output.push(b"\nendobj\n")
}

fn write_pdf_stream_object(
    output: &mut BoundedPdf,
    offsets: &mut [u64],
    id: u32,
    content: &[u8],
) -> Result<(), RenderError> {
    let header = format!("<< /Length {} >>\nstream\n", content.len());
    let mut body = Vec::with_capacity(header.len() + content.len() + 11);
    body.extend_from_slice(header.as_bytes());
    body.extend_from_slice(content);
    if !content.ends_with(b"\n") {
        body.push(b'\n');
    }
    body.extend_from_slice(b"endstream");
    write_object(output, offsets, id, &body)
}

fn write_pdf_image_object(
    output: &mut BoundedPdf,
    offsets: &mut [u64],
    id: u32,
    image: &PdfImage,
    alpha_id: Option<u32>,
) -> Result<(), RenderError> {
    let soft_mask = alpha_id.map_or_else(String::new, |id| format!(" /SMask {id} 0 R"));
    let header = format!(
        "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode /Length {}{} >>\nstream\n",
        image.width,
        image.height,
        image.rgb.len(),
        soft_mask
    );
    let mut body = Vec::with_capacity(header.len() + image.rgb.len() + 10);
    body.extend_from_slice(header.as_bytes());
    body.extend_from_slice(&image.rgb);
    body.extend_from_slice(b"\nendstream");
    write_object(output, offsets, id, &body)
}

fn write_pdf_alpha_object(
    output: &mut BoundedPdf,
    offsets: &mut [u64],
    id: u32,
    width: u32,
    height: u32,
    alpha: &[u8],
) -> Result<(), RenderError> {
    let header = format!(
        "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode /Length {} >>\nstream\n",
        alpha.len()
    );
    let mut body = Vec::with_capacity(header.len() + alpha.len() + 10);
    body.extend_from_slice(header.as_bytes());
    body.extend_from_slice(alpha);
    body.extend_from_slice(b"\nendstream");
    write_object(output, offsets, id, &body)
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

struct BoundedContent {
    bytes: Vec<u8>,
    limit: u64,
}

impl BoundedContent {
    fn new(limit: u64) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn push(&mut self, value: &str) -> Result<(), RenderError> {
        let actual = (self.bytes.len() as u64)
            .checked_add(value.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::PdfBytes, self.limit, actual)?;
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct BoundedPdf {
    bytes: Vec<u8>,
    limit: u64,
}

impl BoundedPdf {
    fn new(limit: u64) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn push(&mut self, value: &[u8]) -> Result<(), RenderError> {
        let actual = (self.bytes.len() as u64)
            .checked_add(value.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::PdfBytes, self.limit, actual)?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::png::render_print_page_png_with_trace;
    use crate::print::{build_print_document, PrintOptions};
    use crate::scene::{
        BackendCommandRangeTrace, BackendNodeTrace, ClipGroupNode, GlyphCluster, GlyphPaint,
        ImageNode, LineNode, PathCommand, PathNode, RectNode,
    };
    use crate::svg::render_scene_svg_with_trace;

    fn rectangle_commands(left: i64, top: i64) -> Vec<PathCommand> {
        vec![
            PathCommand::MoveTo {
                x: Fixed::from_pixels(left),
                y: Fixed::from_pixels(top),
            },
            PathCommand::LineTo {
                x: Fixed::from_pixels(left + 4),
                y: Fixed::from_pixels(top),
            },
            PathCommand::LineTo {
                x: Fixed::from_pixels(left + 4),
                y: Fixed::from_pixels(top + 6),
            },
            PathCommand::LineTo {
                x: Fixed::from_pixels(left),
                y: Fixed::from_pixels(top + 6),
            },
            PathCommand::Close,
        ]
    }

    fn backend_equivalence_scene() -> Scene {
        let path_commands = vec![
            PathCommand::MoveTo {
                x: Fixed::from_pixels(8),
                y: Fixed::from_pixels(34),
            },
            PathCommand::LineTo {
                x: Fixed::from_pixels(24),
                y: Fixed::from_pixels(34),
            },
            PathCommand::QuadraticTo {
                control_x: Fixed::from_pixels(28),
                control_y: Fixed::from_pixels(42),
                x: Fixed::from_pixels(36),
                y: Fixed::from_pixels(34),
            },
            PathCommand::CubicTo {
                control1_x: Fixed::from_pixels(40),
                control1_y: Fixed::from_pixels(26),
                control2_x: Fixed::from_pixels(48),
                control2_y: Fixed::from_pixels(42),
                x: Fixed::from_pixels(56),
                y: Fixed::from_pixels(34),
            },
            PathCommand::Close,
        ];
        let mut glyph_commands = Vec::new();
        for (left, top) in [(70, 20), (76, 20), (82, 20), (88, 20)] {
            glyph_commands.extend(rectangle_commands(left, top));
        }
        Scene {
            title: "backend-equivalence".to_string(),
            width: Fixed::from_pixels(160),
            height: Fixed::from_pixels(120),
            background: Rgb::new(248, 249, 250),
            nodes: vec![
                SceneNode::Rect(RectNode {
                    rect: Rect {
                        x: Fixed::from_pixels(4),
                        y: Fixed::from_pixels(5),
                        width: Fixed::from_pixels(32),
                        height: Fixed::from_pixels(18),
                    },
                    fill: Some(Rgb::new(12, 34, 56)),
                    stroke: Some(Rgb::new(90, 80, 70)),
                    stroke_width: Fixed::from_raw(1_536),
                }),
                SceneNode::Line(LineNode {
                    x1: Fixed::from_pixels(40),
                    y1: Fixed::from_pixels(8),
                    x2: Fixed::from_pixels(62),
                    y2: Fixed::from_pixels(22),
                    color: Rgb::new(3, 120, 210),
                    width: Fixed::from_raw(768),
                }),
                SceneNode::Path(PathNode {
                    commands: path_commands,
                    fill: Some(Rgb::new(200, 210, 220)),
                    stroke: Some(Rgb::new(20, 30, 40)),
                    stroke_width: Fixed::from_pixels(1),
                }),
                SceneNode::Image(ImageNode {
                    rect: Rect {
                        x: Fixed::from_pixels(10),
                        y: Fixed::from_pixels(52),
                        width: Fixed::from_pixels(36),
                        height: Fixed::from_pixels(24),
                    },
                    pixel_width: 2,
                    pixel_height: 2,
                    rgba: std::sync::Arc::from([
                        255, 0, 0, 255, 0, 255, 0, 192, 0, 0, 255, 128, 255, 255, 255, 64,
                    ]),
                    rotation_mdeg: 12_500,
                    alt_text: Some("four pixels".to_string()),
                }),
                SceneNode::GlyphRun(GlyphRunNode {
                    text: "AB".to_string(),
                    clip_bounds: Rect {
                        x: Fixed::from_pixels(64),
                        y: Fixed::from_pixels(12),
                        width: Fixed::from_pixels(48),
                        height: Fixed::from_pixels(24),
                    },
                    commands: glyph_commands,
                    clusters: vec![
                        GlyphCluster {
                            source_start: 0,
                            source_end: 1,
                            command_start: 0,
                            command_end: 10,
                        },
                        GlyphCluster {
                            source_start: 1,
                            source_end: 2,
                            command_start: 10,
                            command_end: 20,
                        },
                    ],
                    paints: vec![
                        GlyphPaint {
                            command_start: 0,
                            command_end: 5,
                            color: Rgb::new(220, 20, 60),
                        },
                        GlyphPaint {
                            command_start: 5,
                            command_end: 15,
                            color: Rgb::new(30, 110, 210),
                        },
                        GlyphPaint {
                            command_start: 15,
                            command_end: 20,
                            color: Rgb::new(20, 150, 80),
                        },
                    ],
                    decorations: vec![LineNode {
                        x1: Fixed::from_pixels(69),
                        y1: Fixed::from_pixels(29),
                        x2: Fixed::from_pixels(94),
                        y2: Fixed::from_pixels(29),
                        color: Rgb::new(30, 110, 210),
                        width: Fixed::from_raw(512),
                    }],
                    color: Rgb::BLACK,
                    rotation_degrees: 15,
                    pivot_x: Fixed::from_pixels(88),
                    pivot_y: Fixed::from_pixels(24),
                    hyperlink: Some("https://example.com/render".to_string()),
                }),
            ],
        }
    }

    #[test]
    fn svg_pdf_and_png_replay_identical_operation_geometry() {
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("trace").write(0, 0, "seed");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        document.pages.truncate(1);
        let scene = backend_equivalence_scene();
        assert!(matches!(
            scene.nodes.last(),
            Some(SceneNode::GlyphRun(node)) if node.metadata_is_valid()
        ));
        document.pages[0].scene = scene.clone();

        let (svg, svg_trace) = render_scene_svg_with_trace(&scene, 2 << 20).unwrap();
        let (pdf, pdf_traces) = render_print_document_pdf_with_trace(&document).unwrap();
        let (png, png_trace) =
            render_print_page_png_with_trace(&document.pages[0], 96, &document).unwrap();
        assert!(svg.starts_with("<?xml"));
        assert!(pdf.starts_with(b"%PDF-1.7"));
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(pdf_traces, vec![svg_trace.clone()]);
        assert_eq!(png_trace, svg_trace);

        assert_eq!(png_trace.nodes.len(), scene.nodes.len());
        let BackendNodeTrace::Path(path) = &png_trace.nodes[2] else {
            panic!("third operation must be the traced vector path")
        };
        assert_eq!(
            path.command_range,
            BackendCommandRangeTrace { start: 0, end: 5 }
        );
        assert_eq!(path.commands.len(), 5);
        let BackendNodeTrace::Image(image) = png_trace.nodes[3] else {
            panic!("fourth operation must be the traced image")
        };
        assert_eq!(image.rotation_mdeg, 12_500);
        assert_eq!((image.pixel_width, image.pixel_height), (2, 2));
        let BackendNodeTrace::Glyph(glyph) = &png_trace.nodes[4] else {
            panic!("fifth operation must be the traced glyph run")
        };
        assert_eq!(glyph.commands.len(), 20);
        assert_eq!(glyph.commands[0].index, 0);
        assert_eq!(glyph.commands[5].color, Rgb::new(30, 110, 210));
        assert_eq!(glyph.commands[14].color, Rgb::new(30, 110, 210));
        assert_eq!(glyph.commands[15].color, Rgb::new(20, 150, 80));
        assert_eq!(glyph.decorations.len(), 1);
        assert_eq!(glyph.rotation_degrees, 15);
        assert_eq!(
            glyph
                .link
                .as_ref()
                .map(|link| (link.rect, link.target.as_str())),
            Some((glyph.clip_bounds, "https://example.com/render"))
        );
    }

    #[test]
    fn clip_groups_are_replayed_identically_and_bound_paint() {
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("clip").write(0, 0, "seed");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        document.pages.truncate(1);
        let clip = Rect {
            x: Fixed::from_pixels(1),
            y: Fixed::ZERO,
            width: Fixed::from_pixels(2),
            height: Fixed::from_pixels(4),
        };
        let painted = RectNode {
            rect: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(4),
                height: Fixed::from_pixels(4),
            },
            fill: Some(Rgb::new(220, 20, 60)),
            stroke: None,
            stroke_width: Fixed::ZERO,
        };
        let scene = Scene {
            title: "clip-equivalence".to_string(),
            width: Fixed::from_pixels(4),
            height: Fixed::from_pixels(4),
            background: Rgb::WHITE,
            nodes: vec![SceneNode::ClipGroup(ClipGroupNode {
                clip,
                nodes: vec![SceneNode::Rect(painted.clone())],
            })],
        };
        document.pages[0].scene = scene.clone();

        let (svg, svg_trace) = render_scene_svg_with_trace(&scene, 1 << 20).unwrap();
        let (pdf, pdf_traces) = render_print_document_pdf_with_trace(&document).unwrap();
        let (png, png_trace) =
            render_print_page_png_with_trace(&document.pages[0], 96, &document).unwrap();
        assert_eq!(pdf_traces, vec![svg_trace.clone()]);
        assert_eq!(png_trace, svg_trace);
        assert_eq!(
            png_trace.nodes,
            vec![
                BackendNodeTrace::ClipStart(clip),
                BackendNodeTrace::Rect(painted),
                BackendNodeTrace::ClipEnd,
            ]
        );

        assert!(svg.contains("overflow=\"hidden\""));
        assert!(svg
            .contains("<clipPath id=\"clip-0\"><rect x=\"1\" y=\"0\" width=\"2\" height=\"4\"/>"));
        assert!(svg.contains("<g clip-path=\"url(#clip-0)\">"));
        let pdf_text = String::from_utf8_lossy(&pdf);
        assert!(pdf_text.contains("0 0 4 4 re W n"));
        assert!(pdf_text.contains("1 0 2 4 re W n"));

        let raster = tiny_skia::Pixmap::decode_png(&png).unwrap();
        assert_eq!(raster.pixel(0, 1).unwrap().red(), 255);
        assert_eq!(raster.pixel(1, 1).unwrap().red(), 220);
        assert_eq!(raster.pixel(2, 1).unwrap().red(), 220);
        assert_eq!(raster.pixel(3, 1).unwrap().red(), 255);
    }

    #[test]
    fn clip_groups_bound_pdf_link_annotations() {
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("clip-link").write(0, 0, "seed");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        document.pages.truncate(1);
        let full_page = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(20),
            height: Fixed::from_pixels(20),
        };
        let clip = Rect {
            x: Fixed::from_pixels(4),
            y: Fixed::from_pixels(5),
            width: Fixed::from_pixels(6),
            height: Fixed::from_pixels(7),
        };
        document.pages[0].scene = Scene {
            title: "clip-link".to_string(),
            width: full_page.width,
            height: full_page.height,
            background: Rgb::WHITE,
            nodes: vec![SceneNode::ClipGroup(ClipGroupNode {
                clip,
                nodes: vec![SceneNode::Text(TextNode {
                    text: "link".to_string(),
                    bounds: full_page,
                    clip_bounds: full_page,
                    horizontal_padding: Fixed::ZERO,
                    style: crate::scene::TextStyle {
                        family: "sans-serif".to_string(),
                        size: Fixed::from_pixels(10),
                        color: Rgb::BLACK,
                        bold: false,
                        italic: false,
                        underline: false,
                        strikethrough: false,
                        anchor: TextAnchor::Start,
                        baseline: TextBaseline::Top,
                        rotation_degrees: 0,
                    },
                    hyperlink: Some("https://example.com/clipped".to_string()),
                })],
            })],
        };

        let pdf = render_print_document_pdf(&document).unwrap();
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/Rect [3 6 7.5 11.25]"));
        assert!(!source.contains("/Rect [0 0 15 15]"));
    }

    #[test]
    fn rational_points_are_exact_and_path_free() {
        assert_eq!(fixed_to_pdf_points(Fixed::from_pixels(96)).unwrap(), "72");
        assert_eq!(fixed_to_pdf_points(Fixed::from_pixels(1)).unwrap(), "0.75");
    }

    #[test]
    fn unicode_actual_text_is_utf16be() {
        let mut content = BoundedContent::new(1024);
        push_actual_text_begin(&mut content, "한A").unwrap();
        let text = String::from_utf8(content.finish()).unwrap();
        assert_eq!(text, "/Span << /ActualText <FEFFD55C0041> >> BDC\n");
    }

    #[test]
    fn outlined_text_uses_real_bounded_type3_programs_and_cluster_maps() {
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("Subset").write(0, 0, "한A");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        for node in &mut document.pages[0].scene.nodes {
            let SceneNode::Text(text) = node else {
                continue;
            };
            *node = SceneNode::GlyphRun(GlyphRunNode {
                text: text.text.clone(),
                clip_bounds: text.clip_bounds,
                commands: vec![
                    PathCommand::MoveTo {
                        x: Fixed::from_pixels(10),
                        y: Fixed::from_pixels(10),
                    },
                    PathCommand::LineTo {
                        x: Fixed::from_pixels(15),
                        y: Fixed::from_pixels(10),
                    },
                    PathCommand::LineTo {
                        x: Fixed::from_pixels(15),
                        y: Fixed::from_pixels(20),
                    },
                    PathCommand::LineTo {
                        x: Fixed::from_pixels(10),
                        y: Fixed::from_pixels(20),
                    },
                    PathCommand::Close,
                    PathCommand::MoveTo {
                        x: Fixed::from_pixels(18),
                        y: Fixed::from_pixels(10),
                    },
                    PathCommand::LineTo {
                        x: Fixed::from_pixels(23),
                        y: Fixed::from_pixels(10),
                    },
                    PathCommand::LineTo {
                        x: Fixed::from_pixels(23),
                        y: Fixed::from_pixels(20),
                    },
                    PathCommand::LineTo {
                        x: Fixed::from_pixels(18),
                        y: Fixed::from_pixels(20),
                    },
                    PathCommand::Close,
                ],
                clusters: vec![
                    crate::scene::GlyphCluster {
                        source_start: 0,
                        source_end: 3,
                        command_start: 0,
                        command_end: 5,
                    },
                    crate::scene::GlyphCluster {
                        source_start: 3,
                        source_end: 4,
                        command_start: 5,
                        command_end: 10,
                    },
                ],
                paints: vec![crate::scene::GlyphPaint {
                    command_start: 0,
                    command_end: 10,
                    color: Rgb::new(12, 34, 56),
                }],
                decorations: Vec::new(),
                color: text.style.color,
                rotation_degrees: text.style.rotation_degrees,
                pivot_x: text.bounds.x,
                pivot_y: text.bounds.y,
                hyperlink: Some("javascript:alert(1)".to_string()),
            });
        }
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/Subtype /Type3"));
        assert!(source.contains("/Name /RXLSRF+OutlinedSubset0000"));
        assert!(source.contains("/CharProcs"));
        assert!(source.contains("/Widths [5 5]"));
        assert!(source.matches("5 0 d0").count() >= 2);
        assert!(source.contains("10 10 m"));
        assert!(source.contains("0.047059 0.133333 0.219608 rg"));
        assert!(source.contains("/ToUnicode"));
        assert!(source.contains("<01> <D55C>"));
        assert!(source.contains("<02> <0041>"));
        assert!(source.contains("<01> Tj"));
        assert!(source.contains("<02> Tj"));
        assert!(!source.contains("3 Tr"));
        assert!(!source.contains("/Helvetica"));
        assert!(!source.contains("/Subtype /Link"));

        let glyph_node = document.pages[0]
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::GlyphRun(node) => Some(node.clone()),
                _ => None,
            })
            .unwrap();
        let mut registry = PdfFontRegistry::new(u64::MAX, u64::MAX);
        registry.glyph_count = MAX_TYPE3_GLYPH_PROGRAMS;
        assert!(matches!(
            registry.register_node(&glyph_node, None),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::BackendCommands,
                limit: MAX_TYPE3_GLYPH_PROGRAMS,
                actual,
            }) if actual == MAX_TYPE3_GLYPH_PROGRAMS + 1
        ));

        document.limits.max_backend_commands = 1;
        assert!(matches!(
            render_print_document_pdf(&document),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::BackendCommands,
                ..
            })
        ));
    }

    #[test]
    fn type3_character_codes_split_at_the_single_byte_boundary() {
        let text = "A".repeat(TYPE3_GLYPHS_PER_SUBSET + 1);
        let mut commands = Vec::new();
        let mut clusters = Vec::new();
        for index in 0..text.len() {
            let command_start = commands.len() as u64;
            commands.extend([
                PathCommand::MoveTo {
                    x: Fixed::from_pixels(1),
                    y: Fixed::from_pixels(1),
                },
                PathCommand::LineTo {
                    x: Fixed::from_pixels(2),
                    y: Fixed::from_pixels(1),
                },
                PathCommand::LineTo {
                    x: Fixed::from_pixels(2),
                    y: Fixed::from_pixels(2),
                },
                PathCommand::Close,
            ]);
            clusters.push(crate::scene::GlyphCluster {
                source_start: index as u64,
                source_end: index as u64 + 1,
                command_start,
                command_end: commands.len() as u64,
            });
        }
        let node = GlyphRunNode {
            text,
            clip_bounds: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(10),
                height: Fixed::from_pixels(10),
            },
            paints: vec![crate::scene::GlyphPaint {
                command_start: 0,
                command_end: commands.len() as u64,
                color: Rgb::BLACK,
            }],
            commands,
            clusters,
            decorations: Vec::new(),
            color: Rgb::BLACK,
            rotation_degrees: 0,
            pivot_x: Fixed::ZERO,
            pivot_y: Fixed::ZERO,
            hyperlink: None,
        };
        assert!(node.metadata_is_valid());
        let mut registry = PdfFontRegistry::new(10_000, 10 << 20);
        let references = registry.register_node(&node, None).unwrap();
        assert_eq!(registry.subsets.len(), 2);
        assert_eq!(registry.subsets[0].glyphs.len(), 255);
        assert_eq!(registry.subsets[1].glyphs.len(), 1);
        assert_eq!(references[254].subset_index, 0);
        assert_eq!(references[254].code, 255);
        assert_eq!(references[255].subset_index, 1);
        assert_eq!(references[255].code, 1);
    }
}
