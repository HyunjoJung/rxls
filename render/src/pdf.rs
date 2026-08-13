//! Deterministic PDF serialization from print-page scenes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

use crate::embed::{glyph_id_for_char, subset_face, EmbeddedFace, FontProgramKind};
use crate::error::{LimitKind, RenderError};
use crate::font::{
    helvetica_advance_units, helvetica_text_advance_units, standard_fallback_byte, FontPack,
};
use crate::print::PrintDocument;
use crate::scene::{
    backend_image_trace, backend_text_trace, format_fixed, BackendGeometryTrace,
    BackendGlyphTraceBuilder, BackendNodeTrace, BackendPathTraceBuilder, Fixed,
    GlyphClusterMetrics, GlyphRunNode, ImageNode, LineNode, PathCommand, PathNode, Rect, RectNode,
    Rgb, Scene, SceneNode, TextAnchor, TextBaseline, TextNode, FIXED_UNITS_PER_PIXEL,
};

const PDF_POINTS_PER_CSS_PIXEL_NUMERATOR: i64 = 3;
const PDF_POINTS_PER_CSS_PIXEL_DENOMINATOR: i64 = 4;
const TYPE3_TEXT_SCALE: u16 = 1_000;
const TYPE3_GLYPHS_PER_SUBSET: usize = 255;
const CMAP_ENTRIES_PER_BLOCK: usize = 100;

/// Subset index carried by references whose run paints from an embedded font.
const EMBEDDED_SUBSET_SENTINEL: usize = usize::MAX;
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
    embedded_fonts: BTreeSet<usize>,
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
    origin_x: Fixed,
    origin_y: Fixed,
    height: Fixed,
    reverse_y: bool,
}

#[derive(Debug, Clone, Copy)]
struct PdfSemanticBoundaryAnchor {
    glyph: PdfGlyphReference,
    embedded_separator: Option<PdfEmbeddedGlyph>,
    clip_bounds: Rect,
    rotation_degrees: i16,
}

impl PdfSemanticBoundaryAnchor {
    fn new(
        node: &GlyphRunNode,
        glyph: PdfGlyphReference,
        embedded_separator: Option<PdfEmbeddedGlyph>,
    ) -> Self {
        Self {
            glyph,
            embedded_separator,
            clip_bounds: node.clip_bounds,
            rotation_degrees: node.rotation_degrees.rem_euclid(360),
        }
    }

    fn shares_layout_line_with(self, other: Self) -> bool {
        // Cell clips retain the layout row origin even when scripts, font
        // sizes, descenders, bottom clipping, or a vertical merge change their
        // semantic glyph boxes and clip heights. Exact row-origin and
        // orientation identity therefore avoids both ink-overlap guesses
        // across adjacent rows and baseline guesses within one row. Equal
        // complete clips describe one semantic owner rather than two adjacent
        // cells, so they must not manufacture a boundary.
        self.rotation_degrees == other.rotation_degrees
            && self.clip_bounds.y == other.clip_bounds.y
            && self.clip_bounds != other.clip_bounds
    }
}

#[derive(Debug, Clone, Copy)]
struct PdfGlyphPlacement {
    origin_x: Fixed,
    origin_y: Fixed,
    width: Fixed,
    height: Fixed,
    reverse_y: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PdfGlyphSemanticSpan {
    source: std::ops::Range<usize>,
    glyphs: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PdfGlyphSourceOrder {
    Forward,
    Reverse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PdfGlyphSemanticSegment {
    source: std::ops::Range<usize>,
    glyphs: Vec<usize>,
    source_order: PdfGlyphSourceOrder,
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

#[derive(Debug, Clone, Copy)]
struct PdfTransform {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
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

impl PdfTransform {
    fn rotation(degrees: i16, pivot_x: Fixed, pivot_y: Fixed) -> Self {
        if degrees.rem_euclid(360) == 0 {
            return Self {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: 1.0,
                e: 0.0,
                f: 0.0,
            };
        }
        let radians = f64::from(degrees).to_radians();
        let cosine = radians.cos();
        let sine = radians.sin();
        let x = fixed_as_f64(pivot_x);
        let y = fixed_as_f64(pivot_y);
        Self {
            a: pdf_decimal_value(cosine),
            b: pdf_decimal_value(sine),
            c: pdf_decimal_value(-sine),
            d: pdf_decimal_value(cosine),
            e: pdf_decimal_value(x - cosine * x + sine * y),
            f: pdf_decimal_value(y - sine * x - cosine * y),
        }
    }

    fn point(self, x: Fixed, y: Fixed) -> [f64; 2] {
        self.point_f64(fixed_as_f64(x), fixed_as_f64(y))
    }

    fn point_f64(self, x: f64, y: f64) -> [f64; 2] {
        [
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        ]
    }

    fn inverse_point(self, point: [f64; 2]) -> Option<[f64; 2]> {
        let determinant = self.a * self.d - self.b * self.c;
        if !determinant.is_finite() || determinant.abs() < f64::EPSILON {
            return None;
        }
        let x = point[0] - self.e;
        let y = point[1] - self.f;
        Some([
            (self.d * x - self.c * y) / determinant,
            (-self.b * x + self.a * y) / determinant,
        ])
    }
}

#[derive(Debug, Default)]
struct PdfFontSubset {
    glyphs: Vec<PdfGlyphProgram>,
    bounds: Option<PdfGlyphBounds>,
}

/// One glyph shown from an embedded font program.
#[derive(Debug, Clone, Copy)]
struct PdfEmbeddedGlyph {
    font_index: usize,
    cid: u16,
    origin_x: Fixed,
    origin_y: Fixed,
    size: Fixed,
}

#[derive(Debug, Clone, Copy)]
enum PdfSemanticSeparator {
    Embedded(PdfEmbeddedGlyph),
    Outlined(PdfGlyphReference),
}

/// One face embedded as a Type0 composite font.
#[derive(Debug)]
struct PdfEmbeddedFace {
    digest: String,
    face: EmbeddedFace,
    /// CID of an unambiguous U+0020 glyph available for semantic boundaries.
    semantic_space_cid: Option<u16>,
    /// Subset gid to the text it stands for, for `/ToUnicode`.
    to_unicode: BTreeMap<u16, String>,
}

#[derive(Debug)]
struct PdfFontRegistry<'a> {
    subsets: Vec<PdfFontSubset>,
    glyph_count: u64,
    retained_bytes: u64,
    glyph_limit: u64,
    byte_limit: u64,
    /// Pack backing embedded font programs; `None` keeps every run on the
    /// outlined Type 3 path.
    fonts: Option<&'a FontPack>,
    embedded: Vec<PdfEmbeddedFace>,
}

impl<'a> PdfFontRegistry<'a> {
    fn new(backend_command_limit: u64, byte_limit: u64, fonts: Option<&'a FontPack>) -> Self {
        Self {
            subsets: Vec::new(),
            glyph_count: 0,
            retained_bytes: 0,
            glyph_limit: backend_command_limit.min(MAX_TYPE3_GLYPH_PROGRAMS),
            byte_limit,
            fonts,
            embedded: Vec::new(),
        }
    }

    /// Subset every embeddable face used anywhere in the document.
    ///
    /// This must happen before any page is built. Subsetting lazily per run
    /// would fix each face's glyph set to whatever the first run happened to
    /// use, so every later run needing another glyph would fall back and the
    /// document would end up mostly outlined anyway.
    fn prepare_embedded_faces(&mut self, document: &PrintDocument) {
        let Some(fonts) = self.fonts else {
            return;
        };
        let mut wanted: BTreeMap<String, BTreeSet<u16>> = BTreeMap::new();
        for page in &document.pages {
            collect_embeddable_glyphs(&page.scene.nodes, &mut wanted);
        }
        let semantic_space_gids = wanted
            .keys()
            .filter_map(|digest| {
                fonts
                    .face_program(digest)
                    .and_then(|bytes| glyph_id_for_char(bytes, ' '))
                    .map(|gid| (digest.clone(), gid))
            })
            .collect::<BTreeMap<_, _>>();
        let ambiguous_semantic_spaces = ambiguous_semantic_spaces(document, &semantic_space_gids);
        for (digest, mut gids) in wanted {
            let Some(bytes) = fonts.face_program(&digest) else {
                continue;
            };
            let semantic_space_gid = semantic_space_gids
                .get(&digest)
                .copied()
                .filter(|_| !ambiguous_semantic_spaces.contains(&digest));
            if let Some(gid) = semantic_space_gid {
                gids.insert(gid);
            }
            let requested = gids.into_iter().collect::<Vec<_>>();
            let Ok(face) = subset_face(bytes, &requested) else {
                continue;
            };
            let semantic_space_cid = semantic_space_gid.and_then(|gid| face.subset_gid(gid));
            let retained = (digest.len() as u64)
                .checked_add(face.program.len() as u64)
                .and_then(|bytes| bytes.checked_add(face.gids.len() as u64 * 2))
                .and_then(|bytes| bytes.checked_add(face.advances.len() as u64 * 2));
            let Some(next_retained) = retained
                .and_then(|retained| self.retained_bytes.checked_add(retained))
                .filter(|retained| *retained <= self.byte_limit)
            else {
                // The outlined path remains available and may be materially
                // smaller than retaining this whole subset program.
                continue;
            };
            self.retained_bytes = next_retained;
            self.embedded.push(PdfEmbeddedFace {
                digest,
                face,
                semantic_space_cid,
                to_unicode: BTreeMap::new(),
            });
        }
    }

    /// Plan an embedded-font emission for one run, or decline it.
    ///
    /// Declining is always safe: the caller falls back to the outlined Type 3
    /// path, which renders identically. The result is indexed by source cluster
    /// so the semantic span machinery keeps working unchanged.
    fn register_embedded_node(
        &mut self,
        node: &GlyphRunNode,
    ) -> Option<Vec<Vec<PdfEmbeddedGlyph>>> {
        if self.fonts.is_none() || !node_is_embeddable(node) {
            return None;
        }

        // A run is embedded whole or not at all, so one unavailable face keeps
        // the whole run outlined rather than splitting it across two strategies.
        let mut font_indices = Vec::with_capacity(node.font_faces.len());
        for scene_face in &node.font_faces {
            font_indices.push(
                self.embedded
                    .iter()
                    .position(|candidate| candidate.digest == scene_face.face_sha256)?,
            );
        }

        let mut per_cluster: Vec<Vec<PdfEmbeddedGlyph>> = vec![Vec::new(); node.clusters.len()];
        for glyph in &node.glyphs {
            let face_index = usize::try_from(glyph.face).ok()?;
            let font_index = *font_indices.get(face_index)?;
            let cid = self.embedded[font_index].face.subset_gid(glyph.glyph_id)?;
            let cluster = usize::try_from(glyph.cluster).ok()?;
            per_cluster.get_mut(cluster)?.push(PdfEmbeddedGlyph {
                font_index,
                cid,
                origin_x: glyph.origin_x,
                origin_y: glyph.origin_y,
                size: glyph.size,
            });
        }

        // Record what each code stands for while cluster text is still in hand.
        // Stage the additions first so a run can fall back to Type 3 without
        // leaving partial maps behind when the retained-byte budget is full.
        let mut pending_mappings = BTreeMap::<(usize, u16), String>::new();
        for (cluster_index, cluster) in node.clusters.iter().enumerate() {
            let start = usize::try_from(cluster.source_start).ok()?;
            let end = usize::try_from(cluster.source_end).ok()?;
            let text = node.text.get(start..end)?;
            if let Some(first) = per_cluster.get(cluster_index).and_then(|list| list.first()) {
                pending_mappings
                    .entry((first.font_index, first.cid))
                    .or_insert_with(|| text.to_string());
            }
        }
        let added_mapping_bytes =
            pending_mappings
                .iter()
                .try_fold(0_u64, |total, ((font_index, cid), text)| {
                    if self.embedded[*font_index].to_unicode.contains_key(cid) {
                        Some(total)
                    } else {
                        total.checked_add(2)?.checked_add(text.len() as u64)
                    }
                })?;
        let next_retained = self.retained_bytes.checked_add(added_mapping_bytes)?;
        if next_retained > self.byte_limit {
            return None;
        }
        for ((font_index, cid), text) in pending_mappings {
            self.embedded[font_index]
                .to_unicode
                .entry(cid)
                .or_insert(text);
        }
        self.retained_bytes = next_retained;

        Some(per_cluster)
    }

    /// Reserve this face's real U+0020 CID for an invisible semantic boundary.
    ///
    /// A shared CID can have only one `/ToUnicode` meaning. A missing space,
    /// an ambiguous cmap, or an existing non-space mapping therefore declines
    /// the embedded path without mutation so the caller can use Type 3.
    fn register_embedded_semantic_separator(
        &mut self,
        anchor: PdfEmbeddedGlyph,
    ) -> Result<Option<PdfEmbeddedGlyph>, RenderError> {
        let Some(entry) = self.embedded.get(anchor.font_index) else {
            return Ok(None);
        };
        let Some(cid) = entry.semantic_space_cid else {
            return Ok(None);
        };
        match entry.to_unicode.get(&cid) {
            Some(text) if text == " " => {}
            Some(_) => return Ok(None),
            None => {
                let retained = 2_u64 + 1;
                let next_retained = self
                    .retained_bytes
                    .checked_add(retained)
                    .ok_or(RenderError::CoordinateOverflow)?;
                if next_retained > self.byte_limit {
                    return Ok(None);
                }
                self.embedded[anchor.font_index]
                    .to_unicode
                    .insert(cid, " ".to_string());
                self.retained_bytes = next_retained;
            }
        }
        Ok(Some(PdfEmbeddedGlyph { cid, ..anchor }))
    }

    fn register_semantic_separator(
        &mut self,
        anchor: PdfSemanticBoundaryAnchor,
    ) -> Result<PdfSemanticSeparator, RenderError> {
        if let Some(embedded) = anchor.embedded_separator {
            if let Some(separator) = self.register_embedded_semantic_separator(embedded)? {
                return Ok(PdfSemanticSeparator::Embedded(separator));
            }
        }
        self.register_type3_semantic_separator(anchor.glyph)
            .map(PdfSemanticSeparator::Outlined)
    }

    fn register_node(
        &mut self,
        node: &GlyphRunNode,
        semantic_clip: Option<Rect>,
        mut trace: Option<&mut BackendGlyphTraceBuilder<'_>>,
        retain_program: bool,
    ) -> Result<Vec<PdfGlyphReference>, RenderError> {
        let placement_clip = if node.rotation_degrees.rem_euclid(360) == 0 {
            semantic_clip
        } else {
            None
        };
        let mut references = Vec::with_capacity(node.clusters.len().max(1));
        if node.clusters.is_empty() {
            if !node.text.is_empty() {
                references.push(self.register_glyph(
                    node,
                    0..0,
                    &node.text,
                    0..0,
                    Some(default_glyph_placement(node)?),
                    trace.as_deref_mut(),
                    retain_program,
                )?);
            }
            return Ok(references);
        }
        let cluster_bounds = node
            .clusters
            .iter()
            .map(|cluster| {
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
                Ok(glyph_bounds(commands))
            })
            .collect::<Result<Vec<_>, RenderError>>()?;
        let placements = if node.cluster_metrics.is_empty() {
            fallback_glyph_placements(node, &cluster_bounds)?
        } else {
            node.cluster_metrics
                .iter()
                .copied()
                .map(nominal_glyph_placement)
                .map(|placement| placement.map(Some))
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut paint_cursor = 0_usize;
        for (index, cluster) in node.clusters.iter().enumerate() {
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
            while node
                .paints
                .get(paint_cursor)
                .is_some_and(|paint| paint.command_end <= cluster.command_start)
            {
                paint_cursor += 1;
            }
            let mut paint_end = paint_cursor;
            while node
                .paints
                .get(paint_end)
                .is_some_and(|paint| paint.command_start < cluster.command_end)
            {
                paint_end += 1;
            }
            let placement = placements[index]
                .map(|placement| placement_including_ink(placement, cluster_bounds[index]))
                .transpose()?
                .map(|placement| {
                    placement_within_unrotated_clip_bottom(
                        placement,
                        cluster_bounds[index],
                        placement_clip,
                    )
                })
                .transpose()?;
            references.push(self.register_glyph(
                node,
                cluster.command_start..cluster.command_end,
                source,
                paint_cursor..paint_end,
                placement,
                trace.as_deref_mut(),
                retain_program,
            )?);
        }
        Ok(references)
    }

    #[allow(clippy::too_many_arguments)]
    fn register_glyph(
        &mut self,
        node: &GlyphRunNode,
        command_range: std::ops::Range<u64>,
        source: &str,
        paint_range: std::ops::Range<usize>,
        placement: Option<PdfGlyphPlacement>,
        trace: Option<&mut BackendGlyphTraceBuilder<'_>>,
        retain_program: bool,
    ) -> Result<PdfGlyphReference, RenderError> {
        let actual = self
            .glyph_count
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::BackendCommands, self.glyph_limit, actual)?;

        let start = usize::try_from(command_range.start).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let end = usize::try_from(command_range.end).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let commands = node.commands.get(start..end).ok_or(RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let bounds = glyph_bounds(commands);
        let (origin_x, origin_y, width, height, reverse_y, local_bounds) = match (placement, bounds)
        {
            (Some(placement), _) => (
                placement.origin_x,
                placement.origin_y,
                placement.width,
                placement.height,
                placement.reverse_y,
                Some(PdfGlyphBounds {
                    min_x: Fixed::ZERO,
                    min_y: if placement.reverse_y {
                        Fixed::ZERO
                    } else {
                        Fixed::from_pixels(-i64::from(TYPE3_TEXT_SCALE))
                    },
                    max_x: placement.width,
                    max_y: if placement.reverse_y {
                        Fixed::from_pixels(i64::from(TYPE3_TEXT_SCALE))
                    } else {
                        Fixed::ZERO
                    },
                }),
            ),
            (None, Some(bounds)) => {
                let width = bounds
                    .max_x
                    .checked_sub(bounds.min_x)
                    .ok_or(RenderError::CoordinateOverflow)?;
                let height = bounds
                    .max_y
                    .checked_sub(bounds.min_y)
                    .ok_or(RenderError::CoordinateOverflow)?;
                (
                    bounds.min_x,
                    bounds.max_y,
                    width,
                    height,
                    false,
                    Some(PdfGlyphBounds {
                        min_x: Fixed::ZERO,
                        min_y: Fixed::from_pixels(-i64::from(TYPE3_TEXT_SCALE)),
                        max_x: width,
                        max_y: Fixed::ZERO,
                    }),
                )
            }
            (None, None) => {
                let placement = default_glyph_placement(node)?;
                (
                    placement.origin_x,
                    placement.origin_y,
                    placement.width,
                    placement.height,
                    placement.reverse_y,
                    None,
                )
            }
        };
        let width = if width.raw() > 0 {
            width
        } else {
            Fixed::from_pixels(1)
        };
        let height = if height.raw() > 0 {
            height
        } else {
            Fixed::from_pixels(1)
        };
        if !retain_program {
            // The run paints from an embedded font program, so retaining an
            // outlined copy would emit a font object nothing draws and give
            // back the size this path exists to save. Only the placement is
            // needed, to keep semantic spans and boundary anchors identical.
            return Ok(PdfGlyphReference {
                subset_index: EMBEDDED_SUBSET_SENTINEL,
                code: 0,
                origin_x,
                origin_y,
                height,
                reverse_y,
            });
        }
        let mut content = BoundedContent::new(self.byte_limit);
        content.push(&format!("{} 0 d0\n", format_fixed(width)))?;
        push_glyph_program_paints(
            &mut content,
            node,
            command_range,
            paint_range,
            PdfGlyphPlacement {
                origin_x,
                origin_y,
                width,
                height,
                reverse_y,
            },
            trace,
        )?;
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
        if let Some(bounds) = local_bounds {
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
            origin_x,
            origin_y,
            height,
            reverse_y,
        })
    }

    /// Register the paint-free Type 3 fallback for a Unicode space boundary.
    ///
    /// Poppler can merge touching text from adjacent cells even when each cell
    /// has its own ActualText span. If a real embedded U+0020 is unavailable or
    /// ambiguous, this mapped metrics-only glyph preserves the boundary without
    /// changing raster output.
    fn register_type3_semantic_separator(
        &mut self,
        anchor: PdfGlyphReference,
    ) -> Result<PdfGlyphReference, RenderError> {
        let actual = self
            .glyph_count
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::BackendCommands, self.glyph_limit, actual)?;

        let width = Fixed::from_pixels(1);
        let content = format!("{} 0 d0\n", format_fixed(width)).into_bytes();
        let unicode_hex = utf16be_hex(" ");
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
            origin_x: anchor.origin_x,
            origin_y: anchor.origin_y,
            height: anchor.height,
            reverse_y: anchor.reverse_y,
        })
    }
}

#[derive(Debug)]
struct PdfFontObjectIds {
    font: u32,
    to_unicode: u32,
    glyphs: Vec<u32>,
}

/// Object ids backing one embedded Type0 composite font.
#[derive(Debug)]
struct PdfEmbeddedObjectIds {
    font: u32,
    descendant: u32,
    descriptor: u32,
    program: u32,
    to_unicode: u32,
}

/// Resolve the fill colour a cluster's glyphs are painted with.
///
/// Paint spans cover outline command ranges, so a cluster takes the colour of
/// the span containing the first command it paints. Clusters that paint nothing
/// fall back to the run's cell-level colour.
fn cluster_paint_color(node: &GlyphRunNode, cluster_index: usize) -> Rgb {
    let Some(cluster) = node.clusters.get(cluster_index) else {
        return node.color;
    };
    node.paints
        .iter()
        .find(|paint| {
            paint.command_start <= cluster.command_start
                && cluster.command_start < paint.command_end
        })
        .map_or(node.color, |paint| paint.color)
}

/// Whether a run can paint from an embedded font program.
///
/// Synthetic bold and italic are painted by transforming outlines, which
/// embedding the face alone cannot reproduce.
fn node_is_embeddable(node: &GlyphRunNode) -> bool {
    node.metadata_is_valid()
        && !node.glyphs.is_empty()
        && !node.font_faces.is_empty()
        && !node.glyphs.iter().any(|glyph| glyph.synthetic)
}

/// Gather every glyph id each face must retain, across a whole scene tree.
fn collect_embeddable_glyphs(nodes: &[SceneNode], wanted: &mut BTreeMap<String, BTreeSet<u16>>) {
    for node in nodes {
        match node {
            SceneNode::ClipGroup(group) => collect_embeddable_glyphs(&group.nodes, wanted),
            SceneNode::GlyphRun(run) => {
                if !node_is_embeddable(run) {
                    continue;
                }
                for glyph in &run.glyphs {
                    let Some(face) = usize::try_from(glyph.face)
                        .ok()
                        .and_then(|index| run.font_faces.get(index))
                    else {
                        continue;
                    };
                    wanted
                        .entry(face.face_sha256.clone())
                        .or_default()
                        .insert(glyph.glyph_id);
                }
            }
            _ => {}
        }
    }
}

/// Find faces whose U+0020 glyph id is also used for non-space source text.
///
/// Scan the bounded scene tree once for every candidate face together. Doing a
/// whole-document pass per face would multiply the glyph ceiling by the face
/// ceiling for an adversarial document.
fn ambiguous_semantic_spaces(
    document: &PrintDocument,
    candidates: &BTreeMap<String, u16>,
) -> BTreeSet<String> {
    fn visit(
        nodes: &[SceneNode],
        candidates: &BTreeMap<String, u16>,
        ambiguous: &mut BTreeSet<String>,
    ) {
        for node in nodes {
            match node {
                SceneNode::ClipGroup(group) => {
                    visit(&group.nodes, candidates, ambiguous);
                }
                SceneNode::GlyphRun(run) if node_is_embeddable(run) => {
                    for glyph in &run.glyphs {
                        let Some(face) = usize::try_from(glyph.face)
                            .ok()
                            .and_then(|index| run.font_faces.get(index))
                        else {
                            continue;
                        };
                        let Some(&space_gid) = candidates.get(&face.face_sha256) else {
                            continue;
                        };
                        if glyph.glyph_id != space_gid {
                            continue;
                        }
                        let is_space = usize::try_from(glyph.cluster)
                            .ok()
                            .and_then(|index| run.clusters.get(index))
                            .and_then(|cluster| {
                                let start = usize::try_from(cluster.source_start).ok()?;
                                let end = usize::try_from(cluster.source_end).ok()?;
                                run.text.get(start..end)
                            })
                            == Some(" ");
                        if !is_space {
                            ambiguous.insert(face.face_sha256.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let mut ambiguous = BTreeSet::new();
    for page in &document.pages {
        visit(&page.scene.nodes, candidates, &mut ambiguous);
    }
    ambiguous
}

/// Build the `/ToUnicode` CMap for an embedded subset.
fn embedded_to_unicode_cmap(entry: &PdfEmbeddedFace) -> String {
    let mut output = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let mappings = entry.to_unicode.iter().collect::<Vec<_>>();
    for chunk in mappings.chunks(CMAP_ENTRIES_PER_BLOCK) {
        let _ = writeln!(&mut output, "{} beginbfchar", chunk.len());
        for (cid, text) in chunk {
            let _ = writeln!(&mut output, "<{cid:04X}> <{}>", utf16be_hex(text));
        }
        output.push_str("endbfchar\n");
    }
    output.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend");
    output
}

/// Build the `/W` array mapping subset CIDs to advance widths.
///
/// Widths are expressed in the PDF glyph space of 1/1000 em, so design units
/// are rescaled by the face's own units per em rather than assumed to be 1000.
fn embedded_widths_array(face: &EmbeddedFace) -> String {
    let upem = i64::from(face.units_per_em.max(1));
    let mut widths = String::from("[0 [");
    for (index, advance) in face.advances.iter().enumerate() {
        if index > 0 {
            widths.push(' ');
        }
        let scaled = (i64::from(*advance) * 1000 + upem / 2) / upem;
        let _ = write!(&mut widths, "{scaled}");
    }
    widths.push_str("]]");
    widths
}

/// Build the Type0 font, descendant CIDFont, and descriptor dictionaries.
fn embedded_font_dictionaries(
    index: usize,
    entry: &PdfEmbeddedFace,
    ids: &PdfEmbeddedObjectIds,
) -> (String, String, String) {
    let face = &entry.face;
    let upem = i64::from(face.units_per_em.max(1));
    let scale = |value: i64| value * 1000 / upem;
    let name = format!("RXLSEM+EmbeddedSubset{index:04}");
    let (subtype, program_key) = match face.kind {
        FontProgramKind::Cff => ("CIDFontType0", "FontFile3"),
        FontProgramKind::TrueType => ("CIDFontType2", "FontFile2"),
    };

    let font = format!(
        "<< /Type /Font /Subtype /Type0 /BaseFont /{name} /Encoding /Identity-H /DescendantFonts [{} 0 R] /ToUnicode {} 0 R >>",
        ids.descendant, ids.to_unicode
    );
    // Symbolic: the subset's built-in encoding is authoritative, and a
    // nonsymbolic claim would invite a reader to substitute its own.
    let mut flags = 4_u32;
    if face.monospaced {
        flags |= 1;
    }
    if face.italic_angle != 0.0 {
        flags |= 64;
    }
    let descendant = format!(
        "<< /Type /Font /Subtype /{subtype} /BaseFont /{name} /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor {} 0 R /DW 0 /W {}{} >>",
        ids.descriptor,
        embedded_widths_array(face),
        match face.kind {
            FontProgramKind::TrueType => " /CIDToGIDMap /Identity",
            FontProgramKind::Cff => "",
        }
    );
    let descriptor = format!(
        "<< /Type /FontDescriptor /FontName /{name} /Flags {flags} /FontBBox [{} {} {} {}] /ItalicAngle {} /Ascent {} /Descent {} /CapHeight {} /StemV 80 /{program_key} {} 0 R >>",
        scale(i64::from(face.bbox.0)),
        scale(i64::from(face.bbox.1)),
        scale(i64::from(face.bbox.2)),
        scale(i64::from(face.bbox.3)),
        pdf_decimal_value(f64::from(face.italic_angle)),
        scale(i64::from(face.pdf_ascent)),
        scale(i64::from(face.pdf_descent)),
        scale(i64::from(face.cap_height)),
        ids.program,
    );
    (font, descendant, descriptor)
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
    render_print_document_pdf_impl(document, None, None)
}

/// Render a print document to PDF, embedding font programs where the pack
/// permits it.
///
/// Outlined Type 3 glyphs are always correct to look at, but they make every
/// consumer recompute text geometry against an em that is not the font's, so
/// extracted word and line boxes are wrong. Given the pack that produced the
/// scene, runs whose faces allow installable embedding are emitted as Type0
/// composite fonts instead, which both shrinks the file and makes the reported
/// geometry true.
///
/// Any face that withholds embedding, any run carrying synthetic bold or
/// italic, and any subsetting failure fall back to the outlined path, so this
/// never renders worse than [`render_print_document_pdf`].
pub fn render_print_document_pdf_with_fonts(
    document: &PrintDocument,
    fonts: &FontPack,
) -> Result<Vec<u8>, RenderError> {
    render_print_document_pdf_impl(document, None, Some(fonts))
}

#[cfg(test)]
pub(crate) fn render_print_document_pdf_with_trace(
    document: &PrintDocument,
) -> Result<(Vec<u8>, Vec<BackendGeometryTrace>), RenderError> {
    let mut traces = Vec::with_capacity(document.pages.len());
    let pdf = render_print_document_pdf_impl(document, Some(&mut traces), None)?;
    Ok((pdf, traces))
}

fn render_print_document_pdf_impl(
    document: &PrintDocument,
    mut traces: Option<&mut Vec<BackendGeometryTrace>>,
    fonts: Option<&FontPack>,
) -> Result<Vec<u8>, RenderError> {
    if document.pages.is_empty() {
        return Err(RenderError::Backend {
            reason: "pdf_requires_page",
        });
    }
    let mut pages = Vec::with_capacity(document.pages.len());
    let mut command_count = 0_u64;
    let mut content_bytes = 0_u64;
    let preflight_command_count = document.pages.iter().try_fold(0_u64, |total, page| {
        page.scene.nodes.iter().try_fold(total, |sum, node| {
            sum.checked_add(node_command_count(node, 0)?)
                .ok_or(RenderError::CoordinateOverflow)
        })
    })?;
    enforce(
        LimitKind::BackendCommands,
        document.limits.max_backend_commands,
        preflight_command_count,
    )?;
    let mut font_registry = PdfFontRegistry::new(
        document.limits.max_backend_commands,
        document.limits.max_pdf_bytes,
        fonts,
    );
    font_registry.prepare_embedded_faces(document);
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
    let mut embedded_object_ids = Vec::with_capacity(font_registry.embedded.len());
    for _ in &font_registry.embedded {
        let font = next_image;
        let descendant = next_image + 1;
        let descriptor = next_image + 2;
        let program = next_image + 3;
        let to_unicode = next_image + 4;
        next_image += 5;
        embedded_object_ids.push(PdfEmbeddedObjectIds {
            font,
            descendant,
            descriptor,
            program,
            to_unicode,
        });
    }
    let info_object = next_image;
    let object_count = info_object;

    let document_id = document_id(&pages, &font_registry.subsets, &font_registry.embedded);
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
        for &embedded_index in &page.embedded_fonts {
            let ids = embedded_object_ids
                .get(embedded_index)
                .ok_or(RenderError::Backend {
                    reason: "pdf_embedded_font_identity",
                })?;
            let _ = write!(&mut fonts, " /RE{embedded_index} {} 0 R", ids.font);
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
        write_pdf_stream_object(&mut output, &mut offsets, id, b"500 0 d0\n")?;
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
    for (index, entry) in font_registry.embedded.iter().enumerate() {
        let ids = &embedded_object_ids[index];
        let (font, descendant, descriptor) = embedded_font_dictionaries(index, entry, ids);
        write_object(&mut output, &mut offsets, ids.font, font.as_bytes())?;
        write_object(
            &mut output,
            &mut offsets,
            ids.descendant,
            descendant.as_bytes(),
        )?;
        write_object(
            &mut output,
            &mut offsets,
            ids.descriptor,
            descriptor.as_bytes(),
        )?;
        write_pdf_font_program_object(
            &mut output,
            &mut offsets,
            ids.program,
            &entry.face.program,
            entry.face.kind,
        )?;
        let cmap = embedded_to_unicode_cmap(entry);
        write_pdf_stream_object(&mut output, &mut offsets, ids.to_unicode, cmap.as_bytes())?;
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
    font_registry: &mut PdfFontRegistry<'_>,
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
    let page_bounds = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: scene.width,
        height: scene.height,
    };
    push_clip(&mut content, page_bounds)?;
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
    let mut embedded_fonts = BTreeSet::new();
    let mut semantic_boundary_anchor = None;
    push_scene_nodes(
        &mut content,
        &scene.nodes,
        scene.height,
        font_registry,
        &mut subset_fonts,
        &mut embedded_fonts,
        &mut links,
        &mut images,
        &mut uses_standard_font,
        Some(page_bounds),
        &mut semantic_boundary_anchor,
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
        embedded_fonts,
    })
}

#[allow(clippy::too_many_arguments)]
fn push_scene_nodes(
    content: &mut BoundedContent,
    nodes: &[SceneNode],
    scene_height: Fixed,
    font_registry: &mut PdfFontRegistry<'_>,
    subset_fonts: &mut BTreeSet<usize>,
    embedded_fonts: &mut BTreeSet<usize>,
    links: &mut Vec<PdfLink>,
    images: &mut Vec<PdfImage>,
    uses_standard_font: &mut bool,
    active_clip: Option<Rect>,
    semantic_boundary_anchor: &mut Option<PdfSemanticBoundaryAnchor>,
    mut trace: Option<&mut BackendGeometryTrace>,
    depth: usize,
) -> Result<(), RenderError> {
    for node in nodes {
        match node {
            SceneNode::ClipGroup(group) => {
                *semantic_boundary_anchor = None;
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
                    embedded_fonts,
                    links,
                    images,
                    uses_standard_font,
                    nested_clip,
                    semantic_boundary_anchor,
                    trace.as_deref_mut(),
                    depth + 1,
                )?;
                *semantic_boundary_anchor = None;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::ClipEnd);
                }
                content.push("Q\n")?;
            }
            SceneNode::Rect(node) => {
                *semantic_boundary_anchor = None;
                push_rect(content, node)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Rect(node.clone()));
                }
            }
            SceneNode::Line(node) => {
                *semantic_boundary_anchor = None;
                push_line(content, node)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Line(node.clone()));
                }
            }
            SceneNode::Path(node) => {
                *semantic_boundary_anchor = None;
                let mut path_trace = trace.is_some().then(|| BackendPathTraceBuilder::new(node));
                push_path_node(content, node, path_trace.as_mut())?;
                if let (Some(trace), Some(path_trace)) = (trace.as_deref_mut(), path_trace) {
                    trace.push(BackendNodeTrace::Path(
                        path_trace.finish().map_err(trace_error)?,
                    ));
                }
            }
            SceneNode::Image(node) => {
                *semantic_boundary_anchor = None;
                let image_index = images.len();
                images.push(pdf_image(node)?);
                push_image(content, node, image_index)?;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.push(BackendNodeTrace::Image(backend_image_trace(node)));
                }
            }
            SceneNode::Text(node) => {
                *semantic_boundary_anchor = None;
                let effective_clip = match active_clip {
                    Some(active_clip) => intersect_clip_rects(active_clip, node.clip_bounds)?,
                    None => None,
                };
                let visible_text = push_text(content, node, effective_clip)?;
                *uses_standard_font |= visible_text;
                let mut accepted_link = None;
                if let Some(target) = node.hyperlink.as_deref().filter(|_| visible_text) {
                    if is_safe_hyperlink(target) {
                        accepted_link = Some(target);
                        if let Some(link_rect) = effective_clip {
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
                let effective_clip = match active_clip {
                    Some(active_clip) => intersect_clip_rects(active_clip, node.clip_bounds)?,
                    None => None,
                };
                let mut glyph_trace = trace.is_some().then(|| BackendGlyphTraceBuilder::new(node));
                let visible_glyph = push_glyph_run(
                    content,
                    node,
                    font_registry,
                    subset_fonts,
                    embedded_fonts,
                    effective_clip,
                    semantic_boundary_anchor,
                    glyph_trace.as_mut(),
                )?;
                if let Some(target) = node.hyperlink.as_deref().filter(|_| visible_glyph) {
                    if is_safe_hyperlink(target) {
                        if let Some(link_rect) = effective_clip {
                            links.push(pdf_link(link_rect, scene_height, target)?);
                            if let Some(trace) = glyph_trace.as_mut() {
                                trace.record_link(link_rect, target).map_err(trace_error)?;
                            }
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
    let bottom = node
        .rect
        .y
        .checked_add(node.rect.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let flipped_height = Fixed::ZERO
        .checked_sub(node.rect.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    content.push(&format!(
        "{} 0 0 {} {} {} cm\n/Im{} Do\nQ\n",
        format_fixed(node.rect.width),
        format_fixed(flipped_height),
        format_fixed(node.rect.x),
        format_fixed(bottom),
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

fn push_semantic_separator(
    content: &mut BoundedContent,
    font_registry: &mut PdfFontRegistry<'_>,
    subset_fonts: &mut BTreeSet<usize>,
    embedded_fonts: &mut BTreeSet<usize>,
    anchor: PdfSemanticBoundaryAnchor,
    text: &str,
) -> Result<(), RenderError> {
    let separator = font_registry.register_semantic_separator(anchor)?;
    push_actual_text_begin(content, text)?;
    match separator {
        PdfSemanticSeparator::Embedded(separator) => {
            embedded_fonts.insert(separator.font_index);
            // Rendering mode 3 makes the boundary unpainted even for a
            // malformed space outline. Isolate the text state so the
            // following visible run returns to normal rendering.
            content.push(&format!(
                "q\nBT /RE{} {} Tf 3 Tr 1 0 0 -1 {} {} Tm <{:04X}> Tj ET\nQ\n",
                separator.font_index,
                format_fixed(separator.size),
                format_fixed(separator.origin_x),
                format_fixed(separator.origin_y),
                separator.cid
            ))?;
        }
        PdfSemanticSeparator::Outlined(separator) => {
            subset_fonts.insert(separator.subset_index);
            content.push(&format!(
                "BT /RG{} {} Tf 1 0 0 {} {} {} Tm <{:02X}> Tj ET\n",
                separator.subset_index,
                TYPE3_TEXT_SCALE,
                type3_height_scale(separator.height, separator.reverse_y),
                format_fixed(separator.origin_x),
                format_fixed(separator.origin_y),
                separator.code
            ))?;
        }
    }
    content.push("EMC\n")
}

fn push_pdf_glyph_cluster(
    content: &mut BoundedContent,
    node: &GlyphRunNode,
    glyph_index: usize,
    glyph: PdfGlyphReference,
    embedded: Option<&[Vec<PdfEmbeddedGlyph>]>,
    subset_fonts: &mut BTreeSet<usize>,
    embedded_fonts: &mut BTreeSet<usize>,
) -> Result<(), RenderError> {
    match embedded.and_then(|plan| plan.get(glyph_index)) {
        Some(placed_glyphs) => {
            // Type 3 CharProcs carry their own colour, so the outlined path
            // never sets one here. Text shown from an embedded font instead
            // inherits the current fill, which at this point is the page
            // background.
            push_rgb_fill(content, cluster_paint_color(node, glyph_index))?;
            for placed in placed_glyphs {
                embedded_fonts.insert(placed.font_index);
                // The page matrix flips y, so the text matrix flips back; the
                // font size then lands in `Tf` where a reader expects it,
                // rather than being folded into the matrix as the Type 3 path
                // must do.
                content.push(&format!(
                    "BT /RE{} {} Tf 1 0 0 -1 {} {} Tm <{:04X}> Tj ET\n",
                    placed.font_index,
                    format_fixed(placed.size),
                    format_fixed(placed.origin_x),
                    format_fixed(placed.origin_y),
                    placed.cid
                ))?;
            }
        }
        None => {
            subset_fonts.insert(glyph.subset_index);
            content.push(&format!(
                "BT /RG{} {} Tf 1 0 0 {} {} {} Tm <{:02X}> Tj ET\n",
                glyph.subset_index,
                TYPE3_TEXT_SCALE,
                type3_height_scale(glyph.height, glyph.reverse_y),
                format_fixed(glyph.origin_x),
                format_fixed(glyph.origin_y),
                glyph.code
            ))?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_glyph_run(
    content: &mut BoundedContent,
    node: &GlyphRunNode,
    font_registry: &mut PdfFontRegistry<'_>,
    subset_fonts: &mut BTreeSet<usize>,
    embedded_fonts: &mut BTreeSet<usize>,
    effective_clip: Option<Rect>,
    semantic_boundary_anchor: &mut Option<PdfSemanticBoundaryAnchor>,
    mut trace: Option<&mut BackendGlyphTraceBuilder<'_>>,
) -> Result<bool, RenderError> {
    if !node.metadata_is_valid() {
        return Err(RenderError::Backend {
            reason: "invalid_glyph_metadata",
        });
    }
    // Decide the emission strategy before registering anything, so an embedded
    // run never also retains an outlined copy of the same glyphs.
    let embedded = font_registry.register_embedded_node(node);
    let glyphs = font_registry.register_node(
        node,
        effective_clip,
        trace.as_deref_mut(),
        embedded.is_none(),
    )?;
    content.push("q\n")?;
    push_clip(content, node.clip_bounds)?;
    if let Some(trace) = trace.as_deref_mut() {
        trace.record_clip(node.clip_bounds).map_err(trace_error)?;
    }
    if node.rotation_degrees != 0 {
        push_rotation(content, node.rotation_degrees, node.pivot_x, node.pivot_y)?;
    }
    let spans = if glyphs.is_empty() {
        Vec::new()
    } else {
        match effective_clip {
            Some(effective_clip) => glyph_semantic_spans(node, glyphs.len(), effective_clip)?,
            None => Vec::new(),
        }
    };
    let visible_glyph = spans.iter().any(|span| !span.glyphs.is_empty());
    let mut emitted_glyphs = vec![false; glyphs.len()];
    let first_glyph = spans
        .iter()
        .flat_map(|span| span.glyphs.iter())
        .next()
        .map(|index| {
            let embedded_separator = embedded
                .as_ref()
                .and_then(|plan| plan.get(*index))
                .and_then(|placed| placed.first())
                .copied();
            PdfSemanticBoundaryAnchor::new(node, glyphs[*index], embedded_separator)
        });
    if let (Some(previous), Some(current)) = (*semantic_boundary_anchor, first_glyph) {
        if previous.shares_layout_line_with(current) {
            push_semantic_separator(
                content,
                font_registry,
                subset_fonts,
                embedded_fonts,
                current,
                " ",
            )?;
        }
    }
    let mut current_boundary_anchor = None;
    if !spans.is_empty() {
        for (span_index, span) in spans.iter().enumerate() {
            let word_start = span.source.start;
            let span_end = span.source.end;
            let word = &node.text[word_start..span_end];
            let semantic_end = if let Some(next_span) = spans.get(span_index + 1).filter(|_| {
                word.chars().any(|character| {
                    matches!(
                        unicode_bidi::bidi_class(character),
                        unicode_bidi::BidiClass::R | unicode_bidi::BidiClass::AL
                    )
                })
            }) {
                let next_start = next_span.source.start;
                let gap = node.text.get(span_end..next_start).unwrap_or_default();
                if !gap.is_empty()
                    && gap.chars().all(char::is_whitespace)
                    && glyph_spans_are_on_distinct_lines(span, next_span, &glyphs)
                {
                    next_start
                } else {
                    span_end
                }
            } else {
                span_end
            };
            let semantic_text =
                node.text
                    .get(word_start..semantic_end)
                    .ok_or(RenderError::Backend {
                        reason: "invalid_glyph_metadata",
                    })?;
            let marked = !semantic_text.chars().all(char::is_whitespace);
            if !marked {
                content.push("/Artifact BMC\n")?;
                for &glyph_index in &span.glyphs {
                    emitted_glyphs[glyph_index] = true;
                    push_pdf_glyph_cluster(
                        content,
                        node,
                        glyph_index,
                        glyphs[glyph_index],
                        embedded.as_deref(),
                        subset_fonts,
                        embedded_fonts,
                    )?;
                    let embedded_separator = embedded
                        .as_ref()
                        .and_then(|plan| plan.get(glyph_index))
                        .and_then(|placed| placed.first())
                        .copied();
                    current_boundary_anchor = Some(PdfSemanticBoundaryAnchor::new(
                        node,
                        glyphs[glyph_index],
                        embedded_separator,
                    ));
                }
                content.push("EMC\n")?;
                continue;
            }

            let mut segments = glyph_semantic_segments(node, span)?;
            if semantic_end > span_end {
                if let Some(last) = segments.last_mut() {
                    last.source.end = semantic_end;
                }
            }
            for segment in segments {
                let reversed = segment.source_order == PdfGlyphSourceOrder::Reverse;
                let text = node
                    .text
                    .get(segment.source.clone())
                    .ok_or(RenderError::Backend {
                        reason: "invalid_glyph_metadata",
                    })?;
                if reversed {
                    content.push("/ReversedChars BMC\n")?;
                    let reverse = reversed_unicode_scalars(text);
                    push_actual_text_begin(content, &reverse)?;
                } else {
                    push_actual_text_begin(content, text)?;
                }
                for &glyph_index in &segment.glyphs {
                    emitted_glyphs[glyph_index] = true;
                    push_pdf_glyph_cluster(
                        content,
                        node,
                        glyph_index,
                        glyphs[glyph_index],
                        embedded.as_deref(),
                        subset_fonts,
                        embedded_fonts,
                    )?;
                    let embedded_separator = embedded
                        .as_ref()
                        .and_then(|plan| plan.get(glyph_index))
                        .and_then(|placed| placed.first())
                        .copied();
                    current_boundary_anchor = Some(PdfSemanticBoundaryAnchor::new(
                        node,
                        glyphs[glyph_index],
                        embedded_separator,
                    ));
                }
                content.push("EMC\n")?;
                if reversed {
                    content.push("EMC\n")?;
                }
            }
        }
    }
    let semantic_cell_clip = effective_clip.is_some_and(|clip| clip == node.clip_bounds);
    if (visible_glyph || semantic_cell_clip) && emitted_glyphs.iter().any(|emitted| !emitted) {
        // Semantic clipping must never become paint clipping. Emit clusters
        // excluded from extracted text under empty ActualText; the already-
        // active scene, group, and cell clips remain authoritative for ink.
        push_actual_text_begin(content, "")?;
        for (glyph_index, emitted) in emitted_glyphs.into_iter().enumerate() {
            if !emitted {
                push_pdf_glyph_cluster(
                    content,
                    node,
                    glyph_index,
                    glyphs[glyph_index],
                    embedded.as_deref(),
                    subset_fonts,
                    embedded_fonts,
                )?;
            }
        }
        content.push("EMC\n")?;
    }
    *semantic_boundary_anchor = current_boundary_anchor;
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
    content.push("Q\n")?;
    Ok(visible_glyph)
}

fn glyph_cluster_source_range(
    node: &GlyphRunNode,
    index: usize,
) -> Result<std::ops::Range<usize>, RenderError> {
    let cluster = node.clusters.get(index).ok_or(RenderError::Backend {
        reason: "invalid_glyph_metadata",
    })?;
    let start = usize::try_from(cluster.source_start).map_err(|_| RenderError::Backend {
        reason: "invalid_glyph_metadata",
    })?;
    let end = usize::try_from(cluster.source_end).map_err(|_| RenderError::Backend {
        reason: "invalid_glyph_metadata",
    })?;
    node.text.get(start..end).ok_or(RenderError::Backend {
        reason: "invalid_glyph_metadata",
    })?;
    Ok(start..end)
}

fn adjacent_glyph_source_order(
    left: &std::ops::Range<usize>,
    right: &std::ops::Range<usize>,
) -> Option<PdfGlyphSourceOrder> {
    if left.end == right.start {
        Some(PdfGlyphSourceOrder::Forward)
    } else if right.end == left.start {
        Some(PdfGlyphSourceOrder::Reverse)
    } else {
        None
    }
}

fn source_range_uses_strong_rtl(
    node: &GlyphRunNode,
    source: std::ops::Range<usize>,
) -> Result<bool, RenderError> {
    let text = node.text.get(source).ok_or(RenderError::Backend {
        reason: "invalid_glyph_metadata",
    })?;
    Ok(text.chars().any(|character| {
        matches!(
            unicode_bidi::bidi_class(character),
            unicode_bidi::BidiClass::R | unicode_bidi::BidiClass::AL
        )
    }))
}

/// Split one whitespace-free semantic span into monotonic bidi segments.
///
/// A mixed-direction span can contain several monotonic visual runs, so
/// treating the whole span as either forward or reverse is not sufficient.
fn glyph_semantic_segments(
    node: &GlyphRunNode,
    span: &PdfGlyphSemanticSpan,
) -> Result<Vec<PdfGlyphSemanticSegment>, RenderError> {
    let Some(&first_index) = span.glyphs.first() else {
        return Ok(Vec::new());
    };
    let first_source = glyph_cluster_source_range(node, first_index)?;
    let mut segments = Vec::<PdfGlyphSemanticSegment>::new();
    let mut glyphs = vec![first_index];
    let mut source = first_source.clone();
    let mut order = None::<PdfGlyphSourceOrder>;
    let mut previous_source = first_source;

    for &index in span.glyphs.iter().skip(1) {
        let next_source = glyph_cluster_source_range(node, index)?;
        let next_order = adjacent_glyph_source_order(&previous_source, &next_source);
        let continues = match (order, next_order) {
            (_, None) => false,
            (None, Some(next_order)) => {
                order = Some(next_order);
                true
            }
            (Some(order), Some(next_order)) => order == next_order,
        };
        if continues {
            source.start = source.start.min(next_source.start);
            source.end = source.end.max(next_source.end);
            glyphs.push(index);
        } else {
            let source_order = match order {
                Some(order) => order,
                None if source_range_uses_strong_rtl(node, source.clone())? => {
                    PdfGlyphSourceOrder::Reverse
                }
                None => PdfGlyphSourceOrder::Forward,
            };
            segments.push(PdfGlyphSemanticSegment {
                source,
                glyphs,
                source_order,
            });
            source = next_source.clone();
            glyphs = vec![index];
            order = None;
        }
        previous_source = next_source;
    }
    let source_order = match order {
        Some(order) => order,
        None if source_range_uses_strong_rtl(node, source.clone())? => PdfGlyphSourceOrder::Reverse,
        None => PdfGlyphSourceOrder::Forward,
    };
    segments.push(PdfGlyphSemanticSegment {
        source,
        glyphs,
        source_order,
    });

    // Emit directional segments in source order. Retain the shaped visual
    // order within each segment so text geometry and Poppler bbox inference
    // remain identical to the visible glyph sequence.
    segments.sort_unstable_by_key(|segment| (segment.source.start, segment.source.end));
    Ok(segments)
}

fn reversed_unicode_scalars(text: &str) -> String {
    text.chars().rev().collect()
}

fn glyph_spans_are_on_distinct_lines(
    left: &PdfGlyphSemanticSpan,
    right: &PdfGlyphSemanticSpan,
    glyphs: &[PdfGlyphReference],
) -> bool {
    // Poppler moves neutral trailing whitespace outside an RTL embedding.
    // Retain it in ActualText only across materially different baselines
    // (wrapped/styled runs); same-line words rely on geometry so bbox tokens
    // remain free of leading or trailing spaces. Ink centers are not a valid
    // baseline proxy because glyphs from one shaped line can have very
    // different ascender/descender heights. Treat spans as different lines
    // only when their complete vertical ink intervals are disjoint.
    let vertical_bounds = |span: &PdfGlyphSemanticSpan| {
        span.glyphs
            .iter()
            .map(|index| {
                let glyph = glyphs[*index];
                (
                    i128::from(glyph.origin_y.raw()) - i128::from(glyph.height.raw()),
                    i128::from(glyph.origin_y.raw()),
                )
            })
            .reduce(|combined, bounds| (combined.0.min(bounds.0), combined.1.max(bounds.1)))
    };
    match (vertical_bounds(left), vertical_bounds(right)) {
        (Some(left), Some(right)) => {
            let tolerance = i128::from(FIXED_UNITS_PER_PIXEL);
            left.1 + tolerance < right.0 || right.1 + tolerance < left.0
        }
        _ => false,
    }
}

fn cluster_source_text(node: &GlyphRunNode, cluster_index: usize) -> &str {
    node.clusters
        .get(cluster_index)
        .and_then(|cluster| {
            let start = usize::try_from(cluster.source_start).ok()?;
            let end = usize::try_from(cluster.source_end).ok()?;
            node.text.get(start..end)
        })
        .unwrap_or_default()
}

fn nominal_clusters_start_new_visual_line(
    node: &GlyphRunNode,
    left_index: usize,
    right_index: usize,
) -> bool {
    // Layout-produced clusters on one visual line retain exact horizontal
    // cursor continuity even when rich-text scripts displace their baselines.
    // A wrapped line both advances by approximately one nominal cluster
    // height and resets that cursor. Caller-authored nodes without metrics
    // retain the legacy grouping.
    let Some((left, right)) = node
        .cluster_metrics
        .get(left_index)
        .zip(node.cluster_metrics.get(right_index))
    else {
        return false;
    };
    let baseline_delta = i128::from(right.baseline_y.raw()) - i128::from(left.baseline_y.raw());
    if baseline_delta <= 0 {
        return false;
    }
    let nominal_height = |metrics: &GlyphClusterMetrics| {
        i128::from(metrics.ascent.raw()) - i128::from(metrics.descent.raw())
    };
    let baseline_tolerance = i128::from(FIXED_UNITS_PER_PIXEL);
    // `build_glyph_run` (layout.rs) positions each line's baseline at
    // `line_top + ascent` and steps `line_top` forward by that line's own
    // reserved height, which is always at least its own nominal
    // `ascent - descent`. So any genuine line break must satisfy
    // `baseline_delta >= right.ascent - left.descent`, regardless of how the
    // two lines' font sizes compare. Requiring the *larger* of the two
    // nominal heights instead (a symmetric guess) under-splits whenever a
    // wrapped line's style changes size across the break - e.g. a small run
    // wrapping onto a line that opens with a much larger run - because the
    // smaller ending line never reserved enough height on its own to clear
    // the larger start line's ascent. Same-line script displacement
    // (superscript/subscript, see the same-line control fixtures below) can
    // also clear this looser asymmetric bound, though, so it alone is not
    // sufficient: pair it with the symmetric bound whenever either cluster's
    // own text is strongly right-to-left. Bidi-reordered runs are laid out
    // with a visual gap at the embedding boundary that the cursor-continuity
    // check below cannot distinguish from a real wrap reset (the gap is not
    // reliably signed the way a pure LTR/RTL run's advance is), so for those
    // pairs we keep the original conservative requirement and only rely on
    // the tighter bound for the non-bidi case the symmetric guess handles
    // correctly already.
    let straddles_strong_rtl = cluster_source_text(node, left_index)
        .chars()
        .chain(cluster_source_text(node, right_index).chars())
        .any(|character| {
            matches!(
                unicode_bidi::bidi_class(character),
                unicode_bidi::BidiClass::R | unicode_bidi::BidiClass::AL
            )
        });
    let required_bound = if straddles_strong_rtl {
        nominal_height(left).max(nominal_height(right))
    } else {
        i128::from(right.ascent.raw()) - i128::from(left.descent.raw())
    };
    if baseline_delta + baseline_tolerance < required_bound {
        return false;
    }

    let left_end = i128::from(left.origin_x.raw()) + i128::from(left.advance_x.raw());
    let right_end = i128::from(right.origin_x.raw()) + i128::from(right.advance_x.raw());
    let cursor_tolerance = 1_i128;
    let logical_cursors_are_contiguous = match (
        left.advance_x.raw().signum(),
        right.advance_x.raw().signum(),
    ) {
        (1, 1) => (left_end - i128::from(right.origin_x.raw())).abs() <= cursor_tolerance,
        (-1, -1) => (i128::from(left.origin_x.raw()) - right_end).abs() <= cursor_tolerance,
        // Zero and mixed-direction advances do not provide an unambiguous
        // source-order cursor. Preserve one span rather than risk splitting a
        // same-line bidi or suppressed-whitespace transition.
        _ => true,
    };
    !logical_cursors_are_contiguous
}

fn glyph_semantic_spans(
    node: &GlyphRunNode,
    glyph_count: usize,
    effective_clip: Rect,
) -> Result<Vec<PdfGlyphSemanticSpan>, RenderError> {
    if glyph_count == 0 {
        return Ok(Vec::new());
    }
    let transform = PdfTransform::rotation(node.rotation_degrees, node.pivot_x, node.pivot_y);
    let clip_bottom = effective_clip
        .y
        .checked_add(effective_clip.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let mut visible = vec![false; glyph_count];
    if node.clusters.is_empty() {
        visible[0] = glyph_bounds(&node.commands).is_some_and(|bounds| {
            transformed_bounds_intersect_clip(bounds, effective_clip, transform)
        });
    } else {
        if node.clusters.len() != glyph_count {
            return Err(RenderError::Backend {
                reason: "invalid_glyph_metadata",
            });
        }
        for (index, cluster) in node.clusters.iter().enumerate() {
            let start =
                usize::try_from(cluster.command_start).map_err(|_| RenderError::Backend {
                    reason: "invalid_glyph_metadata",
                })?;
            let end = usize::try_from(cluster.command_end).map_err(|_| RenderError::Backend {
                reason: "invalid_glyph_metadata",
            })?;
            let commands = node.commands.get(start..end).ok_or(RenderError::Backend {
                reason: "invalid_glyph_metadata",
            })?;
            let semantic_clip_is_cell_clip = effective_clip == node.clip_bounds;
            let nominal_baseline_visible = node.rotation_degrees != 0
                || !semantic_clip_is_cell_clip
                || node.cluster_metrics.is_empty()
                || node.cluster_metrics.get(index).is_some_and(|metrics| {
                    metrics.baseline_y >= effective_clip.y && metrics.baseline_y < clip_bottom
                });
            visible[index] = nominal_baseline_visible
                && glyph_bounds(commands).is_some_and(|bounds| {
                    transformed_bounds_intersect_clip(bounds, effective_clip, transform)
                });
        }
    }
    node.expand_semantic_visibility(&mut visible);
    let visible_glyphs = || -> Vec<usize> {
        visible
            .iter()
            .enumerate()
            .filter_map(|(index, visible)| visible.then_some(index))
            .collect::<Vec<_>>()
    };
    let whole_node = || {
        let glyphs = visible_glyphs();
        if glyphs.is_empty() {
            Vec::new()
        } else {
            vec![PdfGlyphSemanticSpan {
                source: 0..node.text.len(),
                glyphs,
            }]
        }
    };
    if node.clusters.is_empty() {
        return Ok(if visible[0] { whole_node() } else { Vec::new() });
    }

    let semantic_ranges = semantic_text_ranges(&node.text);
    let mut source_order = Vec::with_capacity(node.clusters.len());
    for (index, cluster) in node.clusters.iter().enumerate() {
        let start = usize::try_from(cluster.source_start).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        let end = usize::try_from(cluster.source_end).map_err(|_| RenderError::Backend {
            reason: "invalid_glyph_metadata",
        })?;
        source_order.push((start, end, index));
    }
    source_order.sort_unstable_by_key(|&(start, end, index)| (start, end, index));

    // Shaped clusters normally form a complete source partition. Mandatory
    // line terminators are the one intentional exception: shapers do not emit
    // outlines for them, but their gaps must not collapse a multiline run into
    // one page-sized ActualText span.
    let mut covered = 0_usize;
    for &(start, end, _) in &source_order {
        if start < covered
            || end <= start
            || (start != covered
                && !is_mandatory_line_terminator_gap(
                    node.text.get(covered..start).unwrap_or_default(),
                ))
        {
            return Ok(whole_node());
        }
        covered = end;
    }
    if covered != node.text.len()
        && !is_mandatory_line_terminator_gap(
            node.text.get(covered..node.text.len()).unwrap_or_default(),
        )
    {
        return Ok(whole_node());
    }

    // Map source-sorted clusters to source-sorted semantic ranges with one
    // forward cursor, then populate each span in visual glyph order. This is
    // linear after the unavoidable source-order sort for bidirectional runs.
    let mut spans = Vec::<PdfGlyphSemanticSpan>::with_capacity(semantic_ranges.len());
    let mut cluster_span = vec![None; node.clusters.len()];
    let mut semantic_index = 0_usize;
    let mut previous_visible = None::<(usize, usize, usize)>;
    for &(start, end, cluster_index) in &source_order {
        while semantic_ranges
            .get(semantic_index)
            .is_some_and(|range| range.end <= start)
        {
            semantic_index += 1;
        }
        let Some(semantic_range) = semantic_ranges.get(semantic_index) else {
            return Ok(whole_node());
        };
        if start < semantic_range.start || semantic_range.end < end {
            return Ok(whole_node());
        }
        if !visible[cluster_index] {
            continue;
        }
        let continue_span = previous_visible.is_some_and(
            |(previous_semantic, previous_end, previous_cluster_index)| {
                previous_semantic == semantic_index
                    && previous_end == start
                    && !nominal_clusters_start_new_visual_line(
                        node,
                        previous_cluster_index,
                        cluster_index,
                    )
            },
        );
        let span_index = if continue_span {
            let span_index = spans.len() - 1;
            spans[span_index].source.end = end;
            span_index
        } else {
            let span_index = spans.len();
            spans.push(PdfGlyphSemanticSpan {
                source: start..end,
                glyphs: Vec::new(),
            });
            span_index
        };
        cluster_span[cluster_index] = Some(span_index);
        previous_visible = Some((semantic_index, end, cluster_index));
    }
    for (cluster_index, span_index) in cluster_span.into_iter().enumerate() {
        if let Some(span_index) = span_index {
            spans[span_index].glyphs.push(cluster_index);
        }
    }
    Ok(spans)
}

fn is_mandatory_line_terminator_gap(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
            }
            '\n' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}' => {}
            _ => return false,
        }
    }
    true
}

fn semantic_text_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut current = None::<(usize, bool)>;
    for (start, character) in text.char_indices() {
        let whitespace = character.is_whitespace();
        if let Some((range_start, range_whitespace)) = current {
            if whitespace != range_whitespace {
                ranges.push(range_start..start);
                current = Some((start, whitespace));
            }
        } else {
            current = Some((start, whitespace));
        }
    }
    if let Some((start, _)) = current {
        ranges.push(start..text.len());
    }
    ranges
}

fn push_glyph_program_paints(
    content: &mut BoundedContent,
    node: &GlyphRunNode,
    command_range: std::ops::Range<u64>,
    paint_range: std::ops::Range<usize>,
    placement: PdfGlyphPlacement,
    mut trace: Option<&mut BackendGlyphTraceBuilder<'_>>,
) -> Result<(), RenderError> {
    let command_start = command_range.start;
    let command_end = command_range.end;
    let origin_x = placement.origin_x;
    let origin_y = placement.origin_y;
    if command_start != command_end {
        content.push(&format!(
            "q\n1 0 0 {} 0 0 cm\n",
            type3_height_normalization(placement.height, placement.reverse_y)
        ))?;
    }
    let mut covered = command_start;
    let paints = node.paints.get(paint_range).ok_or(RenderError::Backend {
        reason: "invalid_glyph_metadata",
    })?;
    for paint in paints {
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
        push_path_with_trace_at(content, commands, origin_x, origin_y, |offset, command| {
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
    if command_start != command_end {
        content.push("Q\n")?;
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

fn transformed_bounds_intersect_clip(
    bounds: PdfGlyphBounds,
    clip: Rect,
    transform: PdfTransform,
) -> bool {
    if clip.width <= Fixed::ZERO || clip.height <= Fixed::ZERO {
        return false;
    }
    let Some(right) = clip.x.checked_add(clip.width) else {
        return false;
    };
    let Some(bottom) = clip.y.checked_add(clip.height) else {
        return false;
    };
    let transformed = [
        transform.point(bounds.min_x, bounds.min_y),
        transform.point(bounds.max_x, bounds.min_y),
        transform.point(bounds.max_x, bounds.max_y),
        transform.point(bounds.min_x, bounds.max_y),
    ];
    let clip = [
        [fixed_as_f64(clip.x), fixed_as_f64(clip.y)],
        [fixed_as_f64(right), fixed_as_f64(clip.y)],
        [fixed_as_f64(right), fixed_as_f64(bottom)],
        [fixed_as_f64(clip.x), fixed_as_f64(bottom)],
    ];
    quadrilaterals_overlap(&transformed, &clip)
}

fn quadrilaterals_overlap(left: &[[f64; 2]; 4], right: &[[f64; 2]; 4]) -> bool {
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
    .all(|axis| projected_intervals_overlap(left, right, axis))
}

fn projected_intervals_overlap(
    left: &[[f64; 2]; 4],
    right: &[[f64; 2]; 4],
    axis: [f64; 2],
) -> bool {
    let project = |point: [f64; 2]| point[0] * axis[0] + point[1] * axis[1];
    let (left_min, left_max) = left
        .iter()
        .copied()
        .map(project)
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), value| {
            (min.min(value), max.max(value))
        });
    let (right_min, right_max) = right
        .iter()
        .copied()
        .map(project)
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), value| {
            (min.min(value), max.max(value))
        });
    left_max > right_min && right_max > left_min
}

fn fallback_glyph_placements(
    node: &GlyphRunNode,
    bounds: &[Option<PdfGlyphBounds>],
) -> Result<Vec<Option<PdfGlyphPlacement>>, RenderError> {
    let mut next_bounds = vec![None; bounds.len()];
    let mut next = None;
    for index in (0..bounds.len()).rev() {
        next_bounds[index] = next;
        if let Some(bounds) = bounds[index] {
            next = Some(bounds);
        }
    }
    let mut placements = Vec::with_capacity(bounds.len());
    let mut previous = None;
    for (index, bounds) in bounds.iter().copied().enumerate() {
        let placement = if bounds.is_none() {
            Some(fallback_glyph_placement(
                node,
                previous,
                next_bounds[index],
            )?)
        } else {
            None
        };
        placements.push(placement);
        if bounds.is_some() {
            previous = bounds;
        }
    }
    Ok(placements)
}

fn nominal_glyph_placement(metrics: GlyphClusterMetrics) -> Result<PdfGlyphPlacement, RenderError> {
    let advance_end = metrics
        .origin_x
        .checked_add(metrics.advance_x)
        .ok_or(RenderError::CoordinateOverflow)?;
    let origin_x = Fixed::from_raw(metrics.origin_x.raw().min(advance_end.raw()));
    let right = Fixed::from_raw(metrics.origin_x.raw().max(advance_end.raw()));
    let top = metrics
        .baseline_y
        .checked_sub(metrics.ascent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let origin_y = metrics
        .baseline_y
        .checked_sub(metrics.descent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let width = right
        .checked_sub(origin_x)
        .ok_or(RenderError::CoordinateOverflow)?
        .max(Fixed::from_raw(1));
    let height = origin_y
        .checked_sub(top)
        .ok_or(RenderError::CoordinateOverflow)?
        .max(Fixed::from_raw(1));
    Ok(PdfGlyphPlacement {
        origin_x,
        origin_y,
        width,
        height,
        reverse_y: true,
    })
}

fn placement_including_ink(
    placement: PdfGlyphPlacement,
    ink: Option<PdfGlyphBounds>,
) -> Result<PdfGlyphPlacement, RenderError> {
    let Some(ink) = ink else {
        return Ok(placement);
    };
    let nominal_right = placement
        .origin_x
        .checked_add(placement.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let nominal_top = placement
        .origin_y
        .checked_sub(placement.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let origin_x = Fixed::from_raw(placement.origin_x.raw().min(ink.min_x.raw()));
    let right = Fixed::from_raw(nominal_right.raw().max(ink.max_x.raw()));
    let top = Fixed::from_raw(nominal_top.raw().min(ink.min_y.raw()));
    let origin_y = Fixed::from_raw(placement.origin_y.raw().max(ink.max_y.raw()));
    Ok(PdfGlyphPlacement {
        origin_x,
        origin_y,
        width: right
            .checked_sub(origin_x)
            .ok_or(RenderError::CoordinateOverflow)?
            .max(Fixed::from_raw(1)),
        height: origin_y
            .checked_sub(top)
            .ok_or(RenderError::CoordinateOverflow)?
            .max(Fixed::from_raw(1)),
        reverse_y: placement.reverse_y,
    })
}

fn placement_within_unrotated_clip_bottom(
    placement: PdfGlyphPlacement,
    ink: Option<PdfGlyphBounds>,
    clip_bounds: Option<Rect>,
) -> Result<PdfGlyphPlacement, RenderError> {
    let (Some(ink), Some(clip_bounds)) = (ink, clip_bounds) else {
        return Ok(placement);
    };
    if clip_bounds.height <= Fixed::ZERO {
        return Ok(placement);
    }
    let clip_bottom = clip_bounds
        .y
        .checked_add(clip_bounds.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let clip_inner_bottom = clip_bottom
        .checked_sub(Fixed::from_raw(1))
        .ok_or(RenderError::CoordinateOverflow)?;
    let placement_top = placement
        .origin_y
        .checked_sub(placement.height)
        .ok_or(RenderError::CoordinateOverflow)?;

    // Poppler discards marked Type3 text whose text origin lies just beyond an
    // effective page/cell clip, even when visible outline ink intersects it.
    // Clamp semantic metrics to the visible ink intersection. Rebuilding the
    // CharProc around the new origin preserves the exact scene-space path, and
    // the existing PDF clip still removes the non-visible ink.
    let ink_intersects_clip = ink.max_y > clip_bounds.y && ink.min_y < clip_bottom;
    let bottom = if placement.origin_y >= clip_bottom && ink_intersects_clip {
        clip_inner_bottom
    } else {
        placement.origin_y
    };
    if bottom <= placement_top {
        return Ok(placement);
    }
    Ok(PdfGlyphPlacement {
        origin_x: placement.origin_x,
        origin_y: bottom,
        width: placement.width,
        height: bottom
            .checked_sub(placement_top)
            .ok_or(RenderError::CoordinateOverflow)?,
        reverse_y: placement.reverse_y,
    })
}

fn fallback_glyph_placement(
    node: &GlyphRunNode,
    previous: Option<PdfGlyphBounds>,
    next: Option<PdfGlyphBounds>,
) -> Result<PdfGlyphPlacement, RenderError> {
    if let Some(reference) = previous.or(next) {
        let origin_x = match (previous, next) {
            (Some(previous), Some(next)) if previous.max_x <= next.min_x => previous.max_x,
            (Some(previous), Some(next)) if next.max_x <= previous.min_x => next.max_x,
            (Some(previous), _) => previous.max_x,
            (None, Some(next)) => next.min_x,
            (None, None) => unreachable!(),
        };
        let height = reference
            .max_y
            .checked_sub(reference.min_y)
            .filter(|height| height.raw() > 0)
            .unwrap_or_else(|| Fixed::from_pixels(1));
        return Ok(PdfGlyphPlacement {
            origin_x,
            origin_y: reference.max_y,
            width: Fixed::from_pixels(1),
            height,
            reverse_y: false,
        });
    }

    default_glyph_placement(node)
}

fn default_glyph_placement(node: &GlyphRunNode) -> Result<PdfGlyphPlacement, RenderError> {
    let height = Fixed::from_raw(
        node.clip_bounds
            .height
            .raw()
            .clamp(1, FIXED_UNITS_PER_PIXEL),
    );
    Ok(PdfGlyphPlacement {
        origin_x: node.clip_bounds.x,
        origin_y: node
            .clip_bounds
            .y
            .checked_add(height)
            .ok_or(RenderError::CoordinateOverflow)?,
        width: Fixed::from_pixels(1),
        height,
        reverse_y: false,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PdfTextFragmentKind {
    AdvanceOnly,
    Artifact,
    PaintOnly,
    Semantic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PdfTextFragment {
    source: std::ops::Range<usize>,
    kind: PdfTextFragmentKind,
    whitespace: bool,
    advance_units: i64,
}

#[derive(Debug)]
struct PdfTextFragmentPlan {
    fragments: Vec<PdfTextFragment>,
    visible_paint: bool,
}

fn pdf_text_fragment_plan(
    node: &TextNode,
    x: Fixed,
    baseline_y: Fixed,
    effective_clip: Rect,
    transform: PdfTransform,
) -> Result<PdfTextFragmentPlan, RenderError> {
    let Some(local_clip) = inverse_transformed_clip(effective_clip, transform)? else {
        return Ok(PdfTextFragmentPlan {
            fragments: Vec::new(),
            visible_paint: false,
        });
    };
    let font_size = fixed_as_f64(node.style.size);
    if font_size == 0.0 {
        return Ok(PdfTextFragmentPlan {
            fragments: Vec::new(),
            visible_paint: false,
        });
    }
    let baseline_y = fixed_as_f64(baseline_y);
    // Standard Helvetica's fixed FontBBox is [-166 -225 1000 931].
    // Include its complete vertical ink extent so a descender-only clip still
    // counts as visible paint and can retain an interactive annotation.
    let text_top = baseline_y - font_size * 931.0 / 1_000.0;
    let text_bottom = baseline_y + font_size * 225.0 / 1_000.0;
    let text_midpoint_y = (text_top + text_bottom) / 2.0;
    let mut cursor_x = fixed_as_f64(x);
    let mut fragments = Vec::<PdfTextFragment>::new();
    let mut visible_paint = false;
    let mut characters = node.text.char_indices().peekable();
    while let Some((source_start, character)) = characters.next() {
        let source_end = characters
            .peek()
            .map_or(node.text.len(), |(start, _)| *start);
        let encoded = standard_fallback_byte(character);
        let advance_units = i64::from(helvetica_advance_units(encoded));
        let next_x = cursor_x + font_size * advance_units as f64 / 1_000.0;
        let min_x = cursor_x.min(next_x);
        let max_x = cursor_x.max(next_x);
        let min_y = text_top.min(text_bottom);
        let max_y = text_top.max(text_bottom);
        let advance_box = [
            [min_x, min_y],
            [max_x, min_y],
            [max_x, max_y],
            [min_x, max_y],
        ];
        let has_ink = pdf_fallback_has_ink(encoded);
        let paint_visible = has_ink && quadrilaterals_overlap(&advance_box, &local_clip);
        visible_paint |= paint_visible;
        let center = transform.point_f64((cursor_x + next_x) / 2.0, text_midpoint_y);
        let semantic_owner = paint_visible && point_in_half_open_clip(center, effective_clip)?;
        let whitespace = character.is_whitespace();
        let kind = if !paint_visible {
            PdfTextFragmentKind::AdvanceOnly
        } else if !semantic_owner {
            PdfTextFragmentKind::PaintOnly
        } else if !whitespace || !character.is_ascii() {
            PdfTextFragmentKind::Semantic
        } else {
            PdfTextFragmentKind::Artifact
        };
        append_pdf_text_fragment(
            &mut fragments,
            source_start..source_end,
            kind,
            whitespace,
            advance_units,
        )?;
        cursor_x = next_x;
    }
    while fragments
        .last()
        .is_some_and(|fragment| fragment.kind == PdfTextFragmentKind::AdvanceOnly)
    {
        fragments.pop();
    }
    Ok(PdfTextFragmentPlan {
        fragments,
        visible_paint,
    })
}

fn append_pdf_text_fragment(
    fragments: &mut Vec<PdfTextFragment>,
    source: std::ops::Range<usize>,
    kind: PdfTextFragmentKind,
    whitespace: bool,
    advance_units: i64,
) -> Result<(), RenderError> {
    if let Some(previous) = fragments.last_mut().filter(|previous| {
        previous.kind == kind
            && previous.source.end == source.start
            && (kind == PdfTextFragmentKind::AdvanceOnly || previous.whitespace == whitespace)
    }) {
        previous.source.end = source.end;
        previous.advance_units = previous
            .advance_units
            .checked_add(advance_units)
            .ok_or(RenderError::CoordinateOverflow)?;
        return Ok(());
    }
    fragments.push(PdfTextFragment {
        source,
        kind,
        whitespace,
        advance_units,
    });
    Ok(())
}

fn inverse_transformed_clip(
    clip: Rect,
    transform: PdfTransform,
) -> Result<Option<[[f64; 2]; 4]>, RenderError> {
    if clip.width <= Fixed::ZERO || clip.height <= Fixed::ZERO {
        return Ok(None);
    }
    let right = clip
        .x
        .checked_add(clip.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = clip
        .y
        .checked_add(clip.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let corners = [
        [fixed_as_f64(clip.x), fixed_as_f64(clip.y)],
        [fixed_as_f64(right), fixed_as_f64(clip.y)],
        [fixed_as_f64(right), fixed_as_f64(bottom)],
        [fixed_as_f64(clip.x), fixed_as_f64(bottom)],
    ];
    let Some(first) = transform.inverse_point(corners[0]) else {
        return Ok(None);
    };
    let Some(second) = transform.inverse_point(corners[1]) else {
        return Ok(None);
    };
    let Some(third) = transform.inverse_point(corners[2]) else {
        return Ok(None);
    };
    let Some(fourth) = transform.inverse_point(corners[3]) else {
        return Ok(None);
    };
    Ok(Some([first, second, third, fourth]))
}

fn point_in_half_open_clip(point: [f64; 2], clip: Rect) -> Result<bool, RenderError> {
    if clip.width <= Fixed::ZERO || clip.height <= Fixed::ZERO {
        return Ok(false);
    }
    let right = clip
        .x
        .checked_add(clip.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = clip
        .y
        .checked_add(clip.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(point[0] >= fixed_as_f64(clip.x)
        && point[0] < fixed_as_f64(right)
        && point[1] >= fixed_as_f64(clip.y)
        && point[1] < fixed_as_f64(bottom))
}

fn pdf_fallback_has_ink(encoded: u8) -> bool {
    encoded.is_ascii_graphic()
}

/// Measure standard-font fallback text in the same Helvetica glyph space used
/// by [`pdf_text_fragment_plan`]. The scene already bounds source text; checked
/// accumulation and scaling keep this backend calculation fail-closed too.
fn pdf_fallback_text_width(text: &str, size: Fixed) -> Result<Fixed, RenderError> {
    let advance_units =
        helvetica_text_advance_units(text).ok_or(RenderError::CoordinateOverflow)?;
    let raw = size
        .raw()
        .checked_mul(advance_units)
        .and_then(|value| value.checked_div(1_000))
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(Fixed::from_raw(raw))
}

fn push_text(
    content: &mut BoundedContent,
    node: &TextNode,
    effective_clip: Option<Rect>,
) -> Result<bool, RenderError> {
    let (anchor_x, y) = text_anchor_point(node)?;
    let width = pdf_fallback_text_width(&node.text, node.style.size)?;
    let x = match node.style.anchor {
        TextAnchor::Start => anchor_x,
        TextAnchor::Middle => anchor_x
            .checked_sub(Fixed::from_raw(width.raw() / 2))
            .ok_or(RenderError::CoordinateOverflow)?,
        TextAnchor::End => anchor_x
            .checked_sub(width)
            .ok_or(RenderError::CoordinateOverflow)?,
    };
    let transform = PdfTransform::rotation(node.style.rotation_degrees, anchor_x, y);
    let Some(effective_clip) = effective_clip else {
        return Ok(false);
    };
    let plan = pdf_text_fragment_plan(node, x, y, effective_clip, transform)?;
    if !plan.visible_paint {
        return Ok(false);
    }
    content.push("q\n")?;
    push_clip(content, node.clip_bounds)?;
    if node.style.rotation_degrees != 0 {
        push_rotation(content, node.style.rotation_degrees, anchor_x, y)?;
    }
    push_rgb_fill(content, node.style.color)?;
    content.push(&format!(
        "BT /F0 {} Tf 1 0 0 -1 {} {} Tm\n",
        format_fixed(node.style.size),
        format_fixed(x),
        format_fixed(y),
    ))?;
    for fragment in plan.fragments {
        let text = &node.text[fragment.source];
        match fragment.kind {
            PdfTextFragmentKind::AdvanceOnly => {
                if fragment.advance_units != 0 {
                    content.push(&format!("[-{}] TJ\n", fragment.advance_units))?;
                }
            }
            PdfTextFragmentKind::Artifact => {
                content.push("/Artifact BMC\n")?;
                content.push(&format!("({}) Tj\n", pdf_literal_escaped_ascii(text)))?;
                content.push("EMC\n")?;
            }
            PdfTextFragmentKind::PaintOnly => {
                push_actual_text_begin(content, "")?;
                content.push(&format!("({}) Tj\n", pdf_literal_escaped_ascii(text)))?;
                content.push("EMC\n")?;
            }
            PdfTextFragmentKind::Semantic => {
                push_actual_text_begin(content, text)?;
                content.push(&format!("({}) Tj\n", pdf_literal_escaped_ascii(text)))?;
                content.push("EMC\n")?;
            }
        }
    }
    content.push("ET\nQ\n")?;
    Ok(true)
}

fn push_path_with_trace<F>(
    content: &mut BoundedContent,
    commands: &[PathCommand],
    record: F,
) -> Result<(), RenderError>
where
    F: FnMut(usize, PathCommand) -> Result<(), RenderError>,
{
    push_path_with_trace_at(content, commands, Fixed::ZERO, Fixed::ZERO, record)
}

fn push_path_with_trace_at<F>(
    content: &mut BoundedContent,
    commands: &[PathCommand],
    origin_x: Fixed,
    origin_y: Fixed,
    mut record: F,
) -> Result<(), RenderError>
where
    F: FnMut(usize, PathCommand) -> Result<(), RenderError>,
{
    let mut current = None::<(Fixed, Fixed)>;
    for (index, command) in commands.iter().enumerate() {
        let local = translate_path_command(*command, origin_x, origin_y)?;
        match local {
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

fn translate_path_command(
    command: PathCommand,
    origin_x: Fixed,
    origin_y: Fixed,
) -> Result<PathCommand, RenderError> {
    let x = |value: Fixed| {
        value
            .checked_sub(origin_x)
            .ok_or(RenderError::CoordinateOverflow)
    };
    let y = |value: Fixed| {
        value
            .checked_sub(origin_y)
            .ok_or(RenderError::CoordinateOverflow)
    };
    Ok(match command {
        PathCommand::MoveTo {
            x: command_x,
            y: command_y,
        } => PathCommand::MoveTo {
            x: x(command_x)?,
            y: y(command_y)?,
        },
        PathCommand::LineTo {
            x: command_x,
            y: command_y,
        } => PathCommand::LineTo {
            x: x(command_x)?,
            y: y(command_y)?,
        },
        PathCommand::QuadraticTo {
            control_x,
            control_y,
            x: command_x,
            y: command_y,
        } => PathCommand::QuadraticTo {
            control_x: x(control_x)?,
            control_y: y(control_y)?,
            x: x(command_x)?,
            y: y(command_y)?,
        },
        PathCommand::CubicTo {
            control1_x,
            control1_y,
            control2_x,
            control2_y,
            x: command_x,
            y: command_y,
        } => PathCommand::CubicTo {
            control1_x: x(control1_x)?,
            control1_y: y(control1_y)?,
            control2_x: x(control2_x)?,
            control2_y: y(control2_y)?,
            x: x(command_x)?,
            y: y(command_y)?,
        },
        PathCommand::Close => PathCommand::Close,
    })
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

fn type3_height_scale(height: Fixed, reverse_y: bool) -> String {
    let numerator = i128::from(height.raw());
    format_rational_with_precision(
        if reverse_y { -numerator } else { numerator },
        i128::from(FIXED_UNITS_PER_PIXEL) * i128::from(TYPE3_TEXT_SCALE),
        16,
    )
}

fn type3_height_normalization(height: Fixed, reverse_y: bool) -> String {
    let numerator = i128::from(FIXED_UNITS_PER_PIXEL) * i128::from(TYPE3_TEXT_SCALE);
    format_rational_with_precision(
        if reverse_y { -numerator } else { numerator },
        i128::from(height.raw()),
        16,
    )
}

fn format_rational(numerator: i128, denominator: i128) -> String {
    format_rational_with_precision(numerator, denominator, 8)
}

fn format_rational_with_precision(numerator: i128, denominator: i128, precision: usize) -> String {
    debug_assert!(denominator > 0);
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
        for _ in 0..precision {
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

fn pdf_decimal_value(value: f64) -> f64 {
    if value.abs() < 0.000_000_5 {
        0.0
    } else {
        (value * 1_000_000.0).round() / 1_000_000.0
    }
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
        SceneNode::GlyphRun(node) => {
            let text_emissions = node.clusters.len().max(node.glyphs.len()).max(1) as u64;
            let decoration_commands = (node.decorations.len() as u64)
                .checked_mul(2)
                .ok_or(RenderError::CoordinateOverflow)?;
            (node.commands.len() as u64)
                .checked_add(text_emissions)
                .and_then(|count| count.checked_add(decoration_commands))
                .ok_or(RenderError::CoordinateOverflow)
        }
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
        min_y: Fixed::from_pixels(-i64::from(TYPE3_TEXT_SCALE)),
        max_x: Fixed::from_pixels(1),
        max_y: Fixed::ZERO,
    });
    let mut char_procs = format!("/.notdef {notdef_object} 0 R /m {notdef_object} 0 R");
    let mut differences = String::from("0 /m 1");
    let mut widths = String::from("500");
    for (index, (glyph, id)) in subset.glyphs.iter().zip(&ids.glyphs).enumerate() {
        let code = index + 1;
        let _ = write!(&mut char_procs, " /g{code:03} {id} 0 R");
        let _ = write!(&mut differences, " /g{code:03}");
        widths.push(' ');
        widths.push_str(&format_fixed(glyph.width));
    }
    Ok(format!(
        "<< /Type /Font /Subtype /Type3 /Name /RXLSRF+OutlinedSubset{subset_index:04} /FontBBox [{} {} {} {}] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << {} >> /Encoding << /Type /Encoding /Differences [{}] >> /FirstChar 0 /LastChar {} /Widths [{}] /FontDescriptor << /Type /FontDescriptor /FontName /RXLSRF+OutlinedSubset{subset_index:04} /Flags 4 /FontBBox [{} {} {} {}] /ItalicAngle 0 /Ascent 1000 /Descent -0.000001 /CapHeight 1000 /StemV 80 >> /Resources << >> /ToUnicode {} 0 R >>",
        format_fixed(bounds.min_x),
        format_fixed(bounds.min_y),
        format_fixed(bounds.max_x),
        format_fixed(bounds.max_y),
        char_procs,
        differences,
        subset.glyphs.len(),
        widths,
        format_fixed(bounds.min_x),
        format_fixed(bounds.min_y),
        format_fixed(bounds.max_x),
        format_fixed(bounds.max_y),
        ids.to_unicode
    ))
}

fn type3_to_unicode_cmap(subset_index: usize, subset: &PdfFontSubset) -> String {
    let mut output = format!(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /RXLSRF-OutlinedSubset{subset_index:04} def\n/CMapType 2 def\n1 begincodespacerange\n<01> <FF>\nendcodespacerange\n"
    );
    for (chunk_index, chunk) in subset.glyphs.chunks(CMAP_ENTRIES_PER_BLOCK).enumerate() {
        let _ = writeln!(&mut output, "{} beginbfchar", chunk.len());
        for (offset, glyph) in chunk.iter().enumerate() {
            let code = chunk_index * CMAP_ENTRIES_PER_BLOCK + offset + 1;
            let _ = writeln!(&mut output, "<{code:02X}> <{}>", glyph.unicode_hex);
        }
        output.push_str("endbfchar\n");
    }
    output.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    output
}

fn document_id(
    pages: &[PdfPage],
    subsets: &[PdfFontSubset],
    embedded: &[PdfEmbeddedFace],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"rxls-render-pdf-v4\0");
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
    digest.update((embedded.len() as u64).to_le_bytes());
    for entry in embedded {
        digest.update((entry.digest.len() as u64).to_le_bytes());
        digest.update(entry.digest.as_bytes());
        digest.update([match entry.face.kind {
            FontProgramKind::TrueType => 0,
            FontProgramKind::Cff => 1,
        }]);
        digest.update((entry.face.program.len() as u64).to_le_bytes());
        digest.update(&entry.face.program);
        digest.update((entry.face.gids.len() as u64).to_le_bytes());
        for gid in &entry.face.gids {
            digest.update(gid.to_le_bytes());
        }
        digest.update((entry.face.advances.len() as u64).to_le_bytes());
        for advance in &entry.face.advances {
            digest.update(advance.to_le_bytes());
        }
        digest.update(entry.face.units_per_em.to_le_bytes());
        digest.update(entry.face.pdf_ascent.to_le_bytes());
        digest.update(entry.face.pdf_descent.to_le_bytes());
        for value in [
            entry.face.bbox.0,
            entry.face.bbox.1,
            entry.face.bbox.2,
            entry.face.bbox.3,
        ] {
            digest.update(value.to_le_bytes());
        }
        digest.update(entry.face.cap_height.to_le_bytes());
        digest.update(entry.face.italic_angle.to_bits().to_le_bytes());
        digest.update([u8::from(entry.face.monospaced)]);
        digest.update((entry.to_unicode.len() as u64).to_le_bytes());
        for (cid, text) in &entry.to_unicode {
            digest.update(cid.to_le_bytes());
            digest.update((text.len() as u64).to_le_bytes());
            digest.update(text.as_bytes());
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

/// Write an embedded font program stream.
///
/// The stream dictionary, not the descriptor, is where a font file declares
/// what it actually is: `/Subtype` for `/FontFile3`, and `/Length1` for the
/// uncompressed TrueType file behind `/FontFile2`.
fn write_pdf_font_program_object(
    output: &mut BoundedPdf,
    offsets: &mut [u64],
    id: u32,
    program: &[u8],
    kind: FontProgramKind,
) -> Result<(), RenderError> {
    let extra = match kind {
        // The subsetter returns a complete sfnt, so a CFF-flavoured face
        // arrives wrapped as OpenType rather than as a bare CFF table.
        FontProgramKind::Cff => " /Subtype /OpenType".to_string(),
        FontProgramKind::TrueType => format!(" /Length1 {}", program.len()),
    };
    let header = format!("<< /Length {}{extra} >>\nstream\n", program.len());
    let mut body = Vec::with_capacity(header.len() + program.len() + 11);
    body.extend_from_slice(header.as_bytes());
    body.extend_from_slice(program);
    body.push(b'\n');
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
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::font::{synthetic_cff_test_pack, synthetic_test_pack, FontPack, FontRequest};
    use crate::layout::RenderOptions;
    use crate::png::render_print_page_png_with_trace;
    use crate::print::{build_print_document, PrintOptions};
    use crate::scene::{
        BackendCommandRangeTrace, BackendNodeTrace, ClipGroupNode, GlyphCluster, GlyphPaint,
        ImageNode, LineNode, PathCommand, PathNode, RectNode, TextStyle,
    };
    use crate::svg::render_scene_svg_with_trace;
    use zip::write::SimpleFileOptions;

    const TEST_FACE_SHA256: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

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

    fn sized_rectangle_commands(left: i64, top: i64, width: i64, height: i64) -> Vec<PathCommand> {
        vec![
            PathCommand::MoveTo {
                x: Fixed::from_pixels(left),
                y: Fixed::from_pixels(top),
            },
            PathCommand::LineTo {
                x: Fixed::from_pixels(left + width),
                y: Fixed::from_pixels(top),
            },
            PathCommand::LineTo {
                x: Fixed::from_pixels(left + width),
                y: Fixed::from_pixels(top + height),
            },
            PathCommand::LineTo {
                x: Fixed::from_pixels(left),
                y: Fixed::from_pixels(top + height),
            },
            PathCommand::Close,
        ]
    }

    /// Build a one-cluster run carrying shaped-glyph identity.
    fn embeddable_run(synthetic: bool) -> GlyphRunNode {
        let mut node = positioned_outline("ab", 10, 10, 20, 12);
        node.font_faces = vec![crate::scene::SceneFontFace {
            family: "Wide Sans".to_string(),
            weight: 400,
            italic: false,
            units_per_em: 1_000,
            face_sha256: TEST_FACE_SHA256.to_string(),
        }];
        node.glyphs = vec![crate::scene::ShapedGlyph {
            face: 0,
            cluster: 0,
            glyph_id: 7,
            origin_x: Fixed::from_pixels(10),
            origin_y: Fixed::from_pixels(20),
            size: Fixed::from_pixels(11),
            synthetic,
        }];
        node
    }

    #[test]
    fn runs_without_shaped_identity_cannot_be_embedded() {
        // Caller-authored scenes carry outlines only, so there is no face to
        // embed and they must stay on the Type 3 path.
        assert!(!node_is_embeddable(&positioned_outline(
            "ab", 10, 10, 20, 12
        )));
    }

    #[test]
    fn synthetic_styling_blocks_embedding() {
        assert!(node_is_embeddable(&embeddable_run(false)));
        // Synthetic bold and italic are painted by transforming outlines, which
        // the face alone cannot reproduce.
        assert!(!node_is_embeddable(&embeddable_run(true)));
    }

    #[test]
    fn collected_glyphs_are_grouped_by_face_digest() {
        let mut wanted = BTreeMap::new();
        collect_embeddable_glyphs(
            &[
                SceneNode::GlyphRun(embeddable_run(false)),
                SceneNode::GlyphRun(embeddable_run(true)),
            ],
            &mut wanted,
        );
        // The synthetic run contributes nothing, and the plain one contributes
        // exactly its glyph.
        assert_eq!(wanted.len(), 1);
        assert_eq!(
            wanted
                .get(TEST_FACE_SHA256)
                .map(|gids| gids.iter().copied().collect::<Vec<_>>()),
            Some(vec![7])
        );
    }

    #[test]
    fn a_non_space_source_makes_the_face_space_gid_ambiguous() {
        let node = embeddable_run(false);
        let document =
            document_with_nodes("ambiguous-semantic-space", vec![SceneNode::GlyphRun(node)]);
        let safe = BTreeMap::from([(TEST_FACE_SHA256.to_string(), 2)]);
        assert!(ambiguous_semantic_spaces(&document, &safe).is_empty());

        let conflicting = BTreeMap::from([(TEST_FACE_SHA256.to_string(), 7)]);
        assert_eq!(
            ambiguous_semantic_spaces(&document, &conflicting),
            BTreeSet::from([TEST_FACE_SHA256.to_string()]),
            "the glyph used for the non-space source must not be reused as U+0020"
        );
    }

    fn semantic_separator_registry(
        semantic_space_cid: Option<u16>,
        mapping: Option<&str>,
    ) -> PdfFontRegistry<'static> {
        let mut to_unicode = BTreeMap::new();
        if let (Some(cid), Some(mapping)) = (semantic_space_cid, mapping) {
            to_unicode.insert(cid, mapping.to_string());
        }
        let mut registry = PdfFontRegistry::new(16, 4096, None);
        registry.embedded.push(PdfEmbeddedFace {
            digest: TEST_FACE_SHA256.to_string(),
            face: EmbeddedFace {
                program: Vec::new(),
                kind: FontProgramKind::TrueType,
                gids: vec![0, 1, 2],
                advances: vec![500, 600, 250],
                units_per_em: 1_000,
                pdf_ascent: 800,
                pdf_descent: -200,
                bbox: (0, -200, 1_000, 800),
                cap_height: 700,
                italic_angle: 0.0,
                monospaced: false,
            },
            semantic_space_cid,
            to_unicode,
        });
        registry
    }

    fn semantic_boundary_anchor() -> PdfSemanticBoundaryAnchor {
        PdfSemanticBoundaryAnchor {
            glyph: PdfGlyphReference {
                subset_index: EMBEDDED_SUBSET_SENTINEL,
                code: 0,
                origin_x: Fixed::from_pixels(12),
                origin_y: Fixed::from_pixels(24),
                height: Fixed::from_pixels(10),
                reverse_y: true,
            },
            embedded_separator: Some(PdfEmbeddedGlyph {
                font_index: 0,
                cid: 1,
                origin_x: Fixed::from_pixels(12),
                origin_y: Fixed::from_pixels(24),
                size: Fixed::from_pixels(11),
            }),
            clip_bounds: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(20),
                height: Fixed::from_pixels(20),
            },
            rotation_degrees: 0,
        }
    }

    #[test]
    fn missing_or_conflicting_type0_space_uses_type3_without_mutating_the_map() {
        for (semantic_space_cid, mapping) in [(None, None), (Some(2), Some("X"))] {
            let mut registry = semantic_separator_registry(semantic_space_cid, mapping);
            let before_map = registry.embedded[0].to_unicode.clone();
            let before_bytes = registry.retained_bytes;
            let separator = registry
                .register_semantic_separator(semantic_boundary_anchor())
                .unwrap();
            let PdfSemanticSeparator::Outlined(separator) = separator else {
                panic!("missing or conflicting U+0020 must retain the Type3 fallback");
            };
            assert_ne!(separator.subset_index, EMBEDDED_SUBSET_SENTINEL);
            assert_eq!(registry.embedded[0].to_unicode, before_map);
            assert!(registry.retained_bytes > before_bytes);
            assert_eq!(registry.subsets[0].glyphs[0].unicode_hex, "0020");
            assert_eq!(registry.subsets[0].glyphs[0].content, b"1 0 d0\n");
        }
    }

    #[test]
    fn embedded_to_unicode_mappings_are_split_at_the_cmap_block_limit() {
        let mut to_unicode = BTreeMap::new();
        for cid in 0..101_u16 {
            to_unicode.insert(cid, "A".to_string());
        }
        let entry = PdfEmbeddedFace {
            digest: "digest".to_string(),
            face: EmbeddedFace {
                program: Vec::new(),
                kind: FontProgramKind::TrueType,
                gids: Vec::new(),
                advances: Vec::new(),
                units_per_em: 1_000,
                pdf_ascent: 800,
                pdf_descent: -200,
                bbox: (0, -200, 1_000, 800),
                cap_height: 700,
                italic_angle: 0.0,
                monospaced: false,
            },
            semantic_space_cid: None,
            to_unicode,
        };

        let cmap = embedded_to_unicode_cmap(&entry);
        assert!(cmap.contains("100 beginbfchar"));
        assert!(cmap.contains("1 beginbfchar"));
        assert!(!cmap.contains("101 beginbfchar"));
        assert_eq!(cmap.matches("endbfchar").count(), 2);
    }

    #[test]
    fn embedded_widths_use_nearest_design_unit_in_pdf_glyph_space() {
        let face = EmbeddedFace {
            program: Vec::new(),
            kind: FontProgramKind::TrueType,
            gids: vec![0, 1],
            advances: vec![0, 684],
            units_per_em: 2_048,
            pdf_ascent: 1_600,
            pdf_descent: -400,
            bbox: (0, -400, 2_048, 1_600),
            cap_height: 1_400,
            italic_angle: 0.0,
            monospaced: false,
        };

        assert_eq!(embedded_widths_array(&face), "[0 [0 334]]");
    }

    #[test]
    fn fallback_text_width_uses_exact_bounded_helvetica_advances() {
        let size = Fixed::from_pixels(18);
        assert_eq!(
            pdf_fallback_text_width("MWij?", size).unwrap(),
            Fixed::from_raw(51_185),
            "833+944+222+222+556 Helvetica units at 18px"
        );
        assert_eq!(pdf_fallback_text_width("\n\t", size).unwrap(), Fixed::ZERO);
        assert_eq!(
            pdf_fallback_text_width("한", size).unwrap(),
            pdf_fallback_text_width("?", size).unwrap(),
            "measurement and emission must use the same fallback byte"
        );
        assert_eq!(
            pdf_fallback_text_width("WW", Fixed::from_raw(i64::MAX)),
            Err(RenderError::CoordinateOverflow)
        );
    }

    #[test]
    fn embedded_descriptor_uses_windows_metrics_in_pdf_glyph_space() {
        let pack = synthetic_test_pack();
        let id = pack
            .resolve(FontRequest {
                family: "Wide Sans",
                weight: 400,
                italic: false,
            })
            .id;
        let identity = pack.selected_face_identity(id).unwrap();
        let bytes = pack.face_program(identity.face_sha256).unwrap();
        let glyph = glyph_id_for_char(bytes, 'Q').unwrap();
        let entry = PdfEmbeddedFace {
            digest: identity.face_sha256.to_string(),
            face: subset_face(bytes, &[glyph]).unwrap(),
            semantic_space_cid: None,
            to_unicode: BTreeMap::new(),
        };
        let ids = PdfEmbeddedObjectIds {
            font: 1,
            descendant: 2,
            descriptor: 3,
            program: 4,
            to_unicode: 5,
        };

        let (_, _, descriptor) = embedded_font_dictionaries(0, &entry, &ids);
        assert!(
            descriptor.contains("/Ascent 900 /Descent -300"),
            "{descriptor}"
        );
    }

    #[test]
    fn pinned_arimo_and_noto_descriptors_match_libreoffice_pdf_metrics() {
        let Some(manifest) = std::env::var_os("RXLS_TEST_FONT_PACK_MANIFEST") else {
            return;
        };
        let pack = FontPack::load_manifest(manifest).unwrap();
        let ids = PdfEmbeddedObjectIds {
            font: 1,
            descendant: 2,
            descriptor: 3,
            program: 4,
            to_unicode: 5,
        };

        for (family, expected) in [
            ("Arimo", "/Ascent 1042 /Descent -389"),
            ("Noto Sans CJK KR", "/Ascent 1160 /Descent -288"),
        ] {
            let id = pack
                .resolve(FontRequest {
                    family,
                    weight: 400,
                    italic: false,
                })
                .id;
            let identity = pack.selected_face_identity(id).unwrap();
            assert_eq!(identity.family, family);
            let bytes = pack.face_program(identity.face_sha256).unwrap();
            let glyph = glyph_id_for_char(bytes, 'Q').unwrap();
            let entry = PdfEmbeddedFace {
                digest: identity.face_sha256.to_string(),
                face: subset_face(bytes, &[glyph]).unwrap(),
                semantic_space_cid: None,
                to_unicode: BTreeMap::new(),
            };
            let (_, _, descriptor) = embedded_font_dictionaries(0, &entry, &ids);
            assert!(descriptor.contains(expected), "{family}: {descriptor}");
        }
    }

    #[test]
    fn pdf_document_identity_binds_embedded_programs_and_unicode_maps() {
        let entry = |program: Vec<u8>, text: &str| PdfEmbeddedFace {
            digest: TEST_FACE_SHA256.to_string(),
            face: EmbeddedFace {
                program,
                kind: FontProgramKind::TrueType,
                gids: vec![0, 7],
                advances: vec![500, 600],
                units_per_em: 1_000,
                pdf_ascent: 800,
                pdf_descent: -200,
                bbox: (0, -200, 1_000, 800),
                cap_height: 700,
                italic_angle: 0.0,
                monospaced: false,
            },
            semantic_space_cid: None,
            to_unicode: BTreeMap::from([(1, text.to_string())]),
        };
        let base = entry(vec![1, 2, 3], "A");
        let base_id = document_id(&[], &[], std::slice::from_ref(&base));
        let changed_program = entry(vec![1, 2, 4], "A");
        let changed_mapping = entry(vec![1, 2, 3], "B");

        assert_ne!(
            document_id(&[], &[], std::slice::from_ref(&changed_program)),
            base_id
        );
        assert_ne!(
            document_id(&[], &[], std::slice::from_ref(&changed_mapping)),
            base_id
        );
    }

    #[test]
    fn embedded_glyph_emissions_are_counted_individually() {
        let mut node = embeddable_run(false);
        let one_glyph = node_command_count(&SceneNode::GlyphRun(node.clone()), 0).unwrap();
        let mut second = node.glyphs[0];
        second.origin_x = Fixed::from_pixels(16);
        node.glyphs.push(second);
        assert!(node.metadata_is_valid());

        assert_eq!(
            node_command_count(&SceneNode::GlyphRun(node), 0).unwrap(),
            one_glyph + 1
        );
    }

    #[test]
    fn embedded_faces_and_unicode_maps_respect_the_retained_byte_budget() {
        let pack = synthetic_test_pack();
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("embedding-budget").write(0, 0, "A");
        let document = build_print_document(
            &workbook,
            0,
            &PrintOptions {
                single_page_sheets: true,
                render: RenderOptions {
                    gridlines: false,
                    default_font_family: pack.default_family().to_string(),
                    font_pack: Some(pack.clone()),
                    ..RenderOptions::default()
                },
                ..PrintOptions::default()
            },
        )
        .unwrap();
        let node = first_glyph_run(&document.pages[0].scene.nodes).unwrap();

        let mut measured = PdfFontRegistry::new(u64::MAX, u64::MAX, Some(&pack));
        measured.prepare_embedded_faces(&document);
        assert_eq!(measured.embedded.len(), 1);
        let program_budget = measured.retained_bytes;
        assert!(program_budget > 0);

        let mut too_small = PdfFontRegistry::new(u64::MAX, program_budget - 1, Some(&pack));
        too_small.prepare_embedded_faces(&document);
        assert!(too_small.embedded.is_empty());
        assert_eq!(too_small.retained_bytes, 0);

        measured.byte_limit = program_budget;
        assert!(measured.register_embedded_node(node).is_none());
        assert!(measured.embedded[0].to_unicode.is_empty());
        assert_eq!(measured.retained_bytes, program_budget);

        let mapping_budget = program_budget + 2 + node.text.len() as u64;
        let mut exact = PdfFontRegistry::new(u64::MAX, mapping_budget, Some(&pack));
        exact.prepare_embedded_faces(&document);
        assert!(exact.register_embedded_node(node).is_some());
        assert_eq!(exact.retained_bytes, mapping_budget);
    }

    #[test]
    fn generated_cff_face_uses_the_complete_type0_embedding_path() {
        let pack = synthetic_cff_test_pack();
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("cff-embedding");
        sheet.set_col_width(0, 20.0);
        sheet.write(0, 0, "A");
        let document = build_print_document(
            &workbook,
            0,
            &PrintOptions {
                single_page_sheets: true,
                render: RenderOptions {
                    gridlines: false,
                    default_font_family: pack.default_family().to_string(),
                    font_pack: Some(pack.clone()),
                    ..RenderOptions::default()
                },
                ..PrintOptions::default()
            },
        )
        .expect("layout generated CFF fixture");

        let pdf = render_print_document_pdf_with_fonts(&document, &pack)
            .expect("embed generated CFF fixture");
        assert_eq!(
            pdf,
            render_print_document_pdf_with_fonts(&document, &pack).unwrap(),
            "CFF PDF output must be byte deterministic"
        );
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/Subtype /Type0"));
        assert!(source.contains("/Subtype /CIDFontType0"));
        assert!(source.contains("/FontFile3"));
        assert!(source.contains("/Subtype /OpenType"));
        assert!(pdf.windows(4).any(|window| window == b"OTTO"));
        assert!(!source.contains("/Subtype /CIDFontType2"));
        assert!(!source.contains("/FontFile2"));
        assert!(!source.contains("/CIDToGIDMap /Identity"));
        assert!(!source.contains("/Subtype /Type3"));

        // When Poppler is available, make it reopen the serialized file and
        // consume the embedded CFF program instead of only inspecting bytes.
        if let Some(text) = poppler_text(&pdf) {
            assert_eq!(text.trim(), "A");
        }
    }

    #[test]
    fn clusters_take_the_paint_span_colour_they_fall_in() {
        // Embedded text inherits the current fill, so the wrong colour here
        // paints glyphs invisibly against the page.
        let mut node = embeddable_run(false);
        node.color = Rgb::new(1, 2, 3);
        let painted = node.paints[0].color;
        assert_eq!(cluster_paint_color(&node, 0), painted);
        // A cluster index past the end falls back to the run colour rather
        // than panicking.
        assert_eq!(cluster_paint_color(&node, 99), Rgb::new(1, 2, 3));
    }

    fn positioned_outline(
        text: &str,
        left: i64,
        top: i64,
        width: i64,
        height: i64,
    ) -> GlyphRunNode {
        let commands = sized_rectangle_commands(left, top, width, height);
        GlyphRunNode {
            glyphs: Vec::new(),
            font_faces: Vec::new(),
            text: text.to_string(),
            clip_bounds: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(200),
                height: Fixed::from_pixels(120),
            },
            clusters: vec![GlyphCluster {
                source_start: 0,
                source_end: text.len() as u64,
                command_start: 0,
                command_end: commands.len() as u64,
            }],
            cluster_metrics: Vec::new(),
            semantic_groups: Vec::new(),
            paints: vec![GlyphPaint {
                command_start: 0,
                command_end: commands.len() as u64,
                color: Rgb::BLACK,
            }],
            commands,
            decorations: Vec::new(),
            color: Rgb::BLACK,
            rotation_degrees: 0,
            pivot_x: Fixed::ZERO,
            pivot_y: Fixed::ZERO,
            hyperlink: None,
        }
    }

    fn positioned_outline_in_clip(
        text: &str,
        left: i64,
        top: i64,
        width: i64,
        height: i64,
        clip_bounds: Rect,
    ) -> GlyphRunNode {
        let mut node = positioned_outline(text, left, top, width, height);
        node.clip_bounds = clip_bounds;
        node
    }

    fn positioned_outline_document() -> PrintDocument {
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("bbox").write(0, 0, "seed");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        document.pages.truncate(1);
        document.pages[0].scene = Scene {
            title: "type3-bbox".to_string(),
            width: Fixed::from_pixels(200),
            height: Fixed::from_pixels(120),
            background: Rgb::WHITE,
            nodes: vec![
                SceneNode::GlyphRun(positioned_outline("LEFT", 24, 18, 32, 10)),
                SceneNode::GlyphRun(positioned_outline("RIGHT", 136, 18, 40, 10)),
                SceneNode::GlyphRun(positioned_outline("LOWER", 84, 82, 44, 12)),
            ],
        };
        document
    }

    fn touching_adjacent_outline_document() -> PrintDocument {
        document_with_nodes(
            "type3-touching-adjacent-cells",
            vec![
                SceneNode::GlyphRun(positioned_outline_in_clip(
                    "日本語",
                    24,
                    16,
                    32,
                    12,
                    Rect {
                        x: Fixed::from_pixels(24),
                        y: Fixed::from_pixels(12),
                        width: Fixed::from_pixels(32),
                        height: Fixed::from_pixels(20),
                    },
                )),
                SceneNode::GlyphRun(positioned_outline_in_clip(
                    "中文",
                    56,
                    20,
                    24,
                    8,
                    Rect {
                        x: Fixed::from_pixels(56),
                        y: Fixed::from_pixels(12),
                        width: Fixed::from_pixels(24),
                        height: Fixed::from_pixels(20),
                    },
                )),
                SceneNode::GlyphRun(positioned_outline_in_clip(
                    "00",
                    24,
                    52,
                    16,
                    10,
                    Rect {
                        x: Fixed::from_pixels(24),
                        y: Fixed::from_pixels(48),
                        width: Fixed::from_pixels(16),
                        height: Fixed::from_pixels(20),
                    },
                )),
                SceneNode::GlyphRun(positioned_outline_in_clip(
                    "Cell",
                    40,
                    52,
                    32,
                    10,
                    Rect {
                        x: Fixed::from_pixels(40),
                        y: Fixed::from_pixels(48),
                        width: Fixed::from_pixels(32),
                        height: Fixed::from_pixels(20),
                    },
                )),
            ],
        )
    }

    fn overlapping_adjacent_row_outline_document() -> PrintDocument {
        document_with_nodes(
            "type3-overlapping-adjacent-rows",
            vec![
                SceneNode::GlyphRun(positioned_outline_in_clip(
                    "UPPER",
                    24,
                    18,
                    40,
                    14,
                    Rect {
                        x: Fixed::from_pixels(20),
                        y: Fixed::from_pixels(10),
                        width: Fixed::from_pixels(80),
                        height: Fixed::from_pixels(20),
                    },
                )),
                SceneNode::GlyphRun(positioned_outline_in_clip(
                    "LOWER",
                    24,
                    30,
                    40,
                    14,
                    Rect {
                        x: Fixed::from_pixels(20),
                        y: Fixed::from_pixels(30),
                        width: Fixed::from_pixels(80),
                        height: Fixed::from_pixels(20),
                    },
                )),
            ],
        )
    }

    fn touching_unequal_row_span_outline_document() -> PrintDocument {
        document_with_nodes(
            "type3-touching-unequal-row-span-cells",
            vec![
                SceneNode::GlyphRun(positioned_outline_in_clip(
                    "MERGED",
                    24,
                    16,
                    32,
                    12,
                    Rect {
                        x: Fixed::from_pixels(24),
                        y: Fixed::from_pixels(12),
                        width: Fixed::from_pixels(32),
                        height: Fixed::from_pixels(40),
                    },
                )),
                SceneNode::GlyphRun(positioned_outline_in_clip(
                    "ROW",
                    56,
                    16,
                    24,
                    12,
                    Rect {
                        x: Fixed::from_pixels(56),
                        y: Fixed::from_pixels(12),
                        width: Fixed::from_pixels(24),
                        height: Fixed::from_pixels(20),
                    },
                )),
            ],
        )
    }

    fn touching_rtl_outline_document() -> PrintDocument {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let style = rxls::CellStyle::new().font_name(&family).size(11);
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("type3-touching-rtl-cells");
        sheet.set_right_to_left(true);
        sheet.set_col_width(0, 3.0);
        sheet.set_col_width(1, 3.0);
        sheet.write_styled(0, 0, "אב", &style);
        sheet.write_styled(0, 1, "גד", &style);
        outlined_test_document(&workbook, pack)
    }

    fn embedded_touching_cell_document(
        left: &str,
        right: &str,
        family: &str,
        right_to_left: bool,
        synthetic_bold: bool,
    ) -> (PrintDocument, crate::font::FontPack) {
        let pack = synthetic_test_pack();
        let mut style = rxls::CellStyle::new().font_name(family).size(11);
        if synthetic_bold {
            style = style.bold();
        }
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("embedded-touching-cells");
        sheet.set_right_to_left(right_to_left);
        sheet.set_col_width(0, 8.0);
        sheet.set_col_width(1, 8.0);
        sheet.write_styled(0, 0, left, &style);
        sheet.write_styled(0, 1, right, &style);
        let document = outlined_test_document(&workbook, pack.clone());
        (document, pack)
    }

    fn mixed_font_script_adjacent_outline_document() -> PrintDocument {
        let pack = synthetic_test_pack();
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("type3-mixed-font-script-cells");
        sheet.set_col_width(0, 4.0);
        sheet.set_col_width(1, 4.0);
        sheet.write_styled(
            0,
            0,
            "漢字",
            &rxls::CellStyle::new().font_name("Wide Sans").size(18),
        );
        sheet.write_styled(
            0,
            1,
            "42",
            &rxls::CellStyle::new().font_name("RTL Sans").size(9),
        );
        outlined_test_document(&workbook, pack)
    }

    fn rotated_boundary_outline_document() -> PrintDocument {
        let row = |x| Rect {
            x: Fixed::from_pixels(x),
            y: Fixed::from_pixels(36),
            width: Fixed::from_pixels(40),
            height: Fixed::from_pixels(48),
        };
        let mut first = positioned_outline_in_clip("ROT-A", 28, 50, 12, 20, row(20));
        first.rotation_degrees = 90;
        first.pivot_x = Fixed::from_pixels(40);
        first.pivot_y = Fixed::from_pixels(60);
        let mut second = positioned_outline_in_clip("ROT-B", 68, 50, 12, 20, row(60));
        second.rotation_degrees = 90;
        second.pivot_x = Fixed::from_pixels(80);
        second.pivot_y = Fixed::from_pixels(60);
        let plain = positioned_outline_in_clip("PLAIN", 104, 54, 28, 12, row(100));
        document_with_nodes(
            "type3-rotated-cell-boundaries",
            vec![
                SceneNode::GlyphRun(first),
                SceneNode::GlyphRun(second),
                SceneNode::GlyphRun(plain),
            ],
        )
    }

    fn reset_boundary_outline_document() -> PrintDocument {
        let cell = |text, left| {
            positioned_outline_in_clip(
                text,
                left + 4,
                18,
                20,
                10,
                Rect {
                    x: Fixed::from_pixels(left),
                    y: Fixed::from_pixels(12),
                    width: Fixed::from_pixels(28),
                    height: Fixed::from_pixels(20),
                },
            )
        };
        document_with_nodes(
            "type3-boundary-resets",
            vec![
                SceneNode::GlyphRun(cell("BEFORE", 4)),
                SceneNode::Rect(RectNode {
                    rect: Rect {
                        x: Fixed::from_pixels(34),
                        y: Fixed::from_pixels(12),
                        width: Fixed::from_pixels(1),
                        height: Fixed::from_pixels(20),
                    },
                    fill: None,
                    stroke: None,
                    stroke_width: Fixed::ZERO,
                }),
                SceneNode::GlyphRun(cell("AFTER", 40)),
                SceneNode::ClipGroup(crate::scene::ClipGroupNode {
                    clip: Rect {
                        x: Fixed::ZERO,
                        y: Fixed::ZERO,
                        width: Fixed::from_pixels(200),
                        height: Fixed::from_pixels(120),
                    },
                    nodes: vec![
                        SceneNode::GlyphRun(cell("INNER-A", 76)),
                        SceneNode::GlyphRun(cell("INNER-B", 104)),
                    ],
                }),
                SceneNode::GlyphRun(cell("OUTSIDE", 140)),
            ],
        )
    }

    fn whitespace_outline_document() -> PrintDocument {
        let text = "ALPHA  BETA";
        let mut commands = sized_rectangle_commands(24, 18, 32, 10);
        let second_command_start = commands.len() as u64;
        commands.extend(sized_rectangle_commands(80, 18, 40, 10));
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("bbox").write(0, 0, "seed");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        document.pages.truncate(1);
        document.pages[0].scene = Scene {
            title: "type3-word-bbox".to_string(),
            width: Fixed::from_pixels(200),
            height: Fixed::from_pixels(120),
            background: Rgb::WHITE,
            nodes: vec![SceneNode::GlyphRun(GlyphRunNode {
                glyphs: Vec::new(),
                font_faces: Vec::new(),
                text: text.to_string(),
                clip_bounds: Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(200),
                    height: Fixed::from_pixels(120),
                },
                clusters: vec![
                    GlyphCluster {
                        source_start: 0,
                        source_end: 5,
                        command_start: 0,
                        command_end: second_command_start,
                    },
                    GlyphCluster {
                        source_start: 5,
                        source_end: 7,
                        command_start: second_command_start,
                        command_end: second_command_start,
                    },
                    GlyphCluster {
                        source_start: 7,
                        source_end: text.len() as u64,
                        command_start: second_command_start,
                        command_end: commands.len() as u64,
                    },
                ],
                cluster_metrics: Vec::new(),
                semantic_groups: Vec::new(),
                paints: vec![GlyphPaint {
                    command_start: 0,
                    command_end: commands.len() as u64,
                    color: Rgb::BLACK,
                }],
                commands,
                decorations: Vec::new(),
                color: Rgb::BLACK,
                rotation_degrees: 0,
                pivot_x: Fixed::ZERO,
                pivot_y: Fixed::ZERO,
                hyperlink: None,
            })],
        };
        document
    }

    fn multiline_outline_document() -> PrintDocument {
        let text = "TOP\nBOTTOM";
        let mut commands = sized_rectangle_commands(24, 18, 32, 10);
        let second_command_start = commands.len() as u64;
        commands.extend(sized_rectangle_commands(24, 58, 48, 10));
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("bbox").write(0, 0, "seed");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        document.pages.truncate(1);
        document.pages[0].scene = Scene {
            title: "type3-multiline-bbox".to_string(),
            width: Fixed::from_pixels(200),
            height: Fixed::from_pixels(120),
            background: Rgb::WHITE,
            nodes: vec![SceneNode::GlyphRun(GlyphRunNode {
                glyphs: Vec::new(),
                font_faces: Vec::new(),
                text: text.to_string(),
                clip_bounds: Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(200),
                    height: Fixed::from_pixels(120),
                },
                clusters: vec![
                    GlyphCluster {
                        source_start: 0,
                        source_end: 3,
                        command_start: 0,
                        command_end: second_command_start,
                    },
                    GlyphCluster {
                        source_start: 4,
                        source_end: text.len() as u64,
                        command_start: second_command_start,
                        command_end: commands.len() as u64,
                    },
                ],
                cluster_metrics: Vec::new(),
                semantic_groups: Vec::new(),
                paints: vec![GlyphPaint {
                    command_start: 0,
                    command_end: commands.len() as u64,
                    color: Rgb::BLACK,
                }],
                commands,
                decorations: Vec::new(),
                color: Rgb::BLACK,
                rotation_degrees: 0,
                pivot_x: Fixed::ZERO,
                pivot_y: Fixed::ZERO,
                hyperlink: None,
            })],
        };
        document
    }

    fn imported_xlsx_workbook(styles: &str, worksheet: &str) -> rxls::Workbook {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (name, body) in [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
            ),
            ("xl/styles.xml", styles),
            ("xl/worksheets/sheet1.xml", worksheet),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        rxls::Workbook::open(&zip.finish().unwrap().into_inner()).unwrap()
    }

    fn outlined_test_document(
        workbook: &rxls::Workbook,
        pack: crate::font::FontPack,
    ) -> PrintDocument {
        let family = pack.default_family().to_string();
        build_print_document(
            workbook,
            0,
            &PrintOptions {
                single_page_sheets: true,
                render: RenderOptions {
                    gridlines: false,
                    default_font_family: family,
                    font_pack: Some(pack),
                    ..RenderOptions::default()
                },
                ..PrintOptions::default()
            },
        )
        .unwrap()
    }

    const SOFT_WRAPPED_CJK: &str = "天地玄黄宇宙洪荒日月盈昃辰宿列張";
    const RIGHT_ALIGNED_CJK: &str = "天地玄黄宇宙洪荒日月盈昃辰宿列張寒";

    fn wrapped_cjk_outline_document(
        text: &str,
        column_width: f32,
        alignment: Option<rxls::HAlign>,
        reopen_xlsx: bool,
    ) -> PrintDocument {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let workbook = if reopen_xlsx {
            let horizontal = match alignment {
                Some(rxls::HAlign::Left) => r#" horizontal="left""#,
                Some(rxls::HAlign::Center) => r#" horizontal="center""#,
                Some(rxls::HAlign::Right) => r#" horizontal="right""#,
                None => "",
            };
            let styles = format!(
                r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"{horizontal}/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
            );
            let worksheet = format!(
                r#"<worksheet><cols><col min="1" max="1" width="{column_width}" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{text}</t></is></c></row></sheetData></worksheet>"#
            );
            imported_xlsx_workbook(&styles, &worksheet)
        } else {
            let mut workbook = rxls::Workbook::new();
            let sheet = workbook.add_sheet("wrapped-cjk");
            sheet.set_col_width(0, column_width);
            let mut style = rxls::CellStyle::new().font_name(family).size(11).wrap();
            if let Some(alignment) = alignment {
                style = style.align(alignment);
            }
            sheet.write_styled(0, 0, text, &style);
            workbook
        };
        outlined_test_document(&workbook, pack)
    }

    fn soft_wrapped_cjk_outline_document() -> PrintDocument {
        wrapped_cjk_outline_document(SOFT_WRAPPED_CJK, 4.0, None, false)
    }

    fn same_line_metric_control_document() -> PrintDocument {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let base = rxls::CellStyle::new().font_name(&family).size(11);
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("same-line-controls");
        sheet.set_col_width(0, 40.0);
        sheet.write_styled(0, 0, "漢字仮名", &base);
        sheet.write_rich_styled(
            1,
            0,
            [
                rxls::TextRun::new(
                    "AB",
                    rxls::Font::new()
                        .with_name("Wide Sans")
                        .with_size(24)
                        .with_script(rxls::FormatScript::Superscript),
                ),
                rxls::TextRun::new(
                    "אב",
                    rxls::Font::new()
                        .with_name("RTL Sans")
                        .with_size(11)
                        .with_script(rxls::FormatScript::Subscript),
                ),
            ],
            &base,
        );
        sheet.write_rich_styled(
            2,
            0,
            [
                rxls::TextRun::new(
                    "A",
                    rxls::Font::new()
                        .with_name("Wide Sans")
                        .with_size(24)
                        .with_script(rxls::FormatScript::Superscript),
                ),
                rxls::TextRun::new(
                    "B",
                    rxls::Font::new()
                        .with_name("Wide Sans")
                        .with_size(11)
                        .with_script(rxls::FormatScript::Subscript),
                ),
                rxls::TextRun::new("C", rxls::Font::new().with_name("Wide Sans").with_size(11)),
            ],
            &base,
        );
        sheet.write_styled(3, 0, "回転文字", &base.clone().text_rotation(30));
        outlined_test_document(&workbook, pack)
    }

    fn final_line_nominal_descent_document() -> PrintDocument {
        let text = "FINAL-LINE";
        let commands = sized_rectangle_commands(12, 68, 64, 10);
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("final-line").write(0, 0, "seed");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        document.pages.truncate(1);
        document.pages[0].scene = Scene {
            title: "type3-final-line".to_string(),
            width: Fixed::from_pixels(100),
            height: Fixed::from_pixels(80),
            background: Rgb::WHITE,
            nodes: vec![SceneNode::GlyphRun(GlyphRunNode {
                glyphs: Vec::new(),
                font_faces: Vec::new(),
                text: text.to_string(),
                clip_bounds: Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(100),
                    height: Fixed::from_pixels(80),
                },
                clusters: vec![GlyphCluster {
                    source_start: 0,
                    source_end: text.len() as u64,
                    command_start: 0,
                    command_end: commands.len() as u64,
                }],
                cluster_metrics: vec![GlyphClusterMetrics {
                    origin_x: Fixed::from_pixels(12),
                    advance_x: Fixed::from_pixels(64),
                    baseline_y: Fixed::from_pixels(78),
                    ascent: Fixed::from_pixels(12),
                    descent: Fixed::from_raw(-2 * FIXED_UNITS_PER_PIXEL - 640),
                }],
                semantic_groups: Vec::new(),
                paints: vec![GlyphPaint {
                    command_start: 0,
                    command_end: commands.len() as u64,
                    color: Rgb::BLACK,
                }],
                commands,
                decorations: Vec::new(),
                color: Rgb::BLACK,
                rotation_degrees: 0,
                pivot_x: Fixed::ZERO,
                pivot_y: Fixed::ZERO,
                hyperlink: None,
            })],
        };
        document
    }

    fn bottom_straddling_outline_document() -> PrintDocument {
        let text = "CLIPPED-BOTTOM";
        let commands = sized_rectangle_commands(12, 45, 72, 11);
        let mut document = document_with_nodes(
            "type3-bottom-straddling",
            vec![SceneNode::GlyphRun(GlyphRunNode {
                glyphs: Vec::new(),
                font_faces: Vec::new(),
                text: text.to_string(),
                clip_bounds: Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(100),
                    height: Fixed::from_pixels(51),
                },
                clusters: vec![GlyphCluster {
                    source_start: 0,
                    source_end: text.len() as u64,
                    command_start: 0,
                    command_end: commands.len() as u64,
                }],
                cluster_metrics: vec![GlyphClusterMetrics {
                    origin_x: Fixed::from_pixels(12),
                    advance_x: Fixed::from_pixels(72),
                    baseline_y: Fixed::from_pixels(50),
                    ascent: Fixed::from_pixels(8),
                    descent: Fixed::from_pixels(-6),
                }],
                semantic_groups: Vec::new(),
                paints: vec![GlyphPaint {
                    command_start: 0,
                    command_end: commands.len() as u64,
                    color: Rgb::BLACK,
                }],
                commands,
                decorations: Vec::new(),
                color: Rgb::BLACK,
                rotation_degrees: 0,
                pivot_x: Fixed::ZERO,
                pivot_y: Fixed::ZERO,
                hyperlink: None,
            })],
        );
        document.pages[0].scene.width = Fixed::from_pixels(100);
        document.pages[0].scene.height = Fixed::from_pixels(51);
        document
    }

    fn touching_bottom_straddling_outline_document() -> PrintDocument {
        let row = |x, width| Rect {
            x: Fixed::from_pixels(x),
            y: Fixed::from_pixels(40),
            width: Fixed::from_pixels(width),
            height: Fixed::from_pixels(11),
        };
        let mut first = positioned_outline_in_clip("BOTTOM-A", 4, 45, 44, 11, row(0, 48));
        first.cluster_metrics = vec![GlyphClusterMetrics {
            origin_x: Fixed::from_pixels(4),
            advance_x: Fixed::from_pixels(44),
            baseline_y: Fixed::from_pixels(50),
            ascent: Fixed::from_pixels(8),
            descent: Fixed::from_pixels(-6),
        }];
        let mut second = positioned_outline_in_clip("BOTTOM-B", 48, 45, 48, 11, row(48, 52));
        second.cluster_metrics = vec![GlyphClusterMetrics {
            origin_x: Fixed::from_pixels(48),
            advance_x: Fixed::from_pixels(48),
            baseline_y: Fixed::from_pixels(50),
            ascent: Fixed::from_pixels(8),
            descent: Fixed::from_pixels(-6),
        }];
        let mut document = document_with_nodes(
            "type3-touching-bottom-straddling",
            vec![SceneNode::GlyphRun(first), SceneNode::GlyphRun(second)],
        );
        document.pages[0].scene.width = Fixed::from_pixels(100);
        document.pages[0].scene.height = Fixed::from_pixels(51);
        document
    }

    fn unequal_height_rtl_outline_document() -> PrintDocument {
        let text = "אב גד";
        // Both words share the same visual baseline/bottom edge, but their ink
        // boxes have different heights. Center-based line inference would
        // incorrectly attach the neutral gap to the first RTL ActualText span.
        let mut commands = sized_rectangle_commands(80, 18, 24, 8);
        let second_command_start = commands.len() as u64;
        commands.extend(sized_rectangle_commands(40, 14, 24, 12));
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("rtl-bbox").write(0, 0, "seed");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        document.pages.truncate(1);
        document.pages[0].scene = Scene {
            title: "type3-rtl-unequal-height".to_string(),
            width: Fixed::from_pixels(160),
            height: Fixed::from_pixels(80),
            background: Rgb::WHITE,
            nodes: vec![SceneNode::GlyphRun(GlyphRunNode {
                glyphs: Vec::new(),
                font_faces: Vec::new(),
                text: text.to_string(),
                clip_bounds: Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(160),
                    height: Fixed::from_pixels(80),
                },
                clusters: vec![
                    GlyphCluster {
                        source_start: 0,
                        source_end: 4,
                        command_start: 0,
                        command_end: second_command_start,
                    },
                    GlyphCluster {
                        source_start: 4,
                        source_end: 5,
                        command_start: second_command_start,
                        command_end: second_command_start,
                    },
                    GlyphCluster {
                        source_start: 5,
                        source_end: text.len() as u64,
                        command_start: second_command_start,
                        command_end: commands.len() as u64,
                    },
                ],
                cluster_metrics: Vec::new(),
                semantic_groups: Vec::new(),
                paints: vec![GlyphPaint {
                    command_start: 0,
                    command_end: commands.len() as u64,
                    color: Rgb::BLACK,
                }],
                commands,
                decorations: Vec::new(),
                color: Rgb::BLACK,
                rotation_degrees: 0,
                pivot_x: Fixed::ZERO,
                pivot_y: Fixed::ZERO,
                hyperlink: None,
            })],
        };
        document
    }

    fn document_with_nodes(title: &str, nodes: Vec<SceneNode>) -> PrintDocument {
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet(title).write(0, 0, "seed");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        document.pages.truncate(1);
        document.pages[0].scene = Scene {
            title: title.to_string(),
            width: Fixed::from_pixels(200),
            height: Fixed::from_pixels(120),
            background: Rgb::WHITE,
            nodes,
        };
        document
    }

    fn document_with_clipped_text_pages(
        title: &str,
        text: TextNode,
        width: i64,
        height: i64,
        clips: &[Rect],
    ) -> PrintDocument {
        let mut document = document_with_nodes(title, Vec::new());
        let template = document.pages[0].clone();
        document.pages = clips
            .iter()
            .enumerate()
            .map(|(index, clip)| {
                let mut page = template.clone();
                page.scene = Scene {
                    title: format!("{title}-{index}"),
                    width: Fixed::from_pixels(width),
                    height: Fixed::from_pixels(height),
                    background: Rgb::WHITE,
                    nodes: vec![SceneNode::ClipGroup(ClipGroupNode {
                        clip: *clip,
                        nodes: vec![SceneNode::Text(text.clone())],
                    })],
                };
                page
            })
            .collect();
        document
    }

    fn font_pack_glyph_run(text: &str) -> GlyphRunNode {
        let pack = synthetic_test_pack();
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("font-pack-clip");
        sheet.set_col_width(0, 40.0);
        sheet.write(0, 0, text);
        let options = PrintOptions {
            single_page_sheets: true,
            render: RenderOptions {
                gridlines: false,
                default_font_family: pack.default_family().to_string(),
                font_pack: Some(pack),
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        };
        let document = build_print_document(&workbook, 0, &options).unwrap();
        first_glyph_run(&document.pages[0].scene.nodes)
            .cloned()
            .expect("font-pack layout must emit a GlyphRun")
    }

    fn font_pack_pdf_variants(text: &str) -> [Vec<u8>; 2] {
        let pack = synthetic_test_pack();
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("pdf-text-semantics");
        sheet.set_col_width(0, 60.0);
        sheet.write(0, 0, text);
        let options = PrintOptions {
            single_page_sheets: true,
            render: RenderOptions {
                gridlines: false,
                default_font_family: pack.default_family().to_string(),
                font_pack: Some(pack.clone()),
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        };
        let document = build_print_document(&workbook, 0, &options).unwrap();
        [
            render_print_document_pdf(&document).unwrap(),
            render_print_document_pdf_with_fonts(&document, &pack).unwrap(),
        ]
    }

    fn fallback_text_node(text: &str) -> TextNode {
        let bounds = Rect {
            x: Fixed::from_pixels(20),
            y: Fixed::from_pixels(20),
            width: Fixed::from_pixels(120),
            height: Fixed::from_pixels(24),
        };
        TextNode {
            text: text.to_string(),
            bounds,
            clip_bounds: bounds,
            horizontal_padding: Fixed::from_pixels(2),
            style: crate::scene::TextStyle {
                family: "sans-serif".to_string(),
                size: Fixed::from_pixels(14),
                color: Rgb::BLACK,
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
                anchor: TextAnchor::Start,
                baseline: TextBaseline::Top,
                rotation_degrees: 0,
            },
            hyperlink: None,
        }
    }

    fn poppler_bbox_layout(pdf: &[u8]) -> Option<String> {
        let available = std::process::Command::new("pdftotext")
            .arg("-v")
            .output()
            .is_ok_and(|output| output.status.success());
        if std::env::var_os("RXLS_REQUIRE_POPPLER").is_some() {
            assert!(available, "RXLS_REQUIRE_POPPLER requires pdftotext");
        }
        if !available {
            return None;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = poppler_temp_directory("bbox", nonce);
        std::fs::create_dir(&directory).unwrap();
        let pdf_path = directory.join("fixture.pdf");
        std::fs::write(&pdf_path, pdf).unwrap();
        let output = std::process::Command::new("pdftotext")
            .args(["-bbox-layout", "-enc", "UTF-8"])
            .arg(&pdf_path)
            .arg("-")
            .output()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(String::from_utf8(output.stdout).unwrap())
    }

    fn poppler_text(pdf: &[u8]) -> Option<String> {
        let available = std::process::Command::new("pdftotext")
            .arg("-v")
            .output()
            .is_ok_and(|output| output.status.success());
        if std::env::var_os("RXLS_REQUIRE_POPPLER").is_some() {
            assert!(available, "RXLS_REQUIRE_POPPLER requires pdftotext");
        }
        if !available {
            return None;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = poppler_temp_directory("text", nonce);
        std::fs::create_dir(&directory).unwrap();
        let pdf_path = directory.join("fixture.pdf");
        std::fs::write(&pdf_path, pdf).unwrap();
        let output = std::process::Command::new("pdftotext")
            .args(["-layout", "-enc", "UTF-8"])
            .arg(&pdf_path)
            .arg("-")
            .output()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(String::from_utf8(output.stdout).unwrap())
    }

    fn normalized_poppler_logical_text(text: &str) -> String {
        let without_controls = text
            .chars()
            .filter(|character| {
                !matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'
                        | '\u{202b}'
                        | '\u{202c}'
                        | '\u{202d}'
                        | '\u{202e}'
                        | '\u{2066}'
                        | '\u{2067}'
                        | '\u{2068}'
                        | '\u{2069}'
                )
            })
            .collect::<String>();
        without_controls
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn poppler_text_pages(pdf: &[u8]) -> Option<Vec<String>> {
        let text = poppler_text(pdf)?;
        let mut pages = text
            .split('\u{000C}')
            .map(str::to_string)
            .collect::<Vec<_>>();
        if pages.last().is_some_and(String::is_empty) {
            pages.pop();
        }
        Some(pages)
    }

    fn poppler_fonts(pdf: &[u8]) -> Option<String> {
        let available = std::process::Command::new("pdffonts")
            .arg("-v")
            .output()
            .is_ok_and(|output| output.status.success());
        if std::env::var_os("RXLS_REQUIRE_POPPLER").is_some() {
            assert!(available, "RXLS_REQUIRE_POPPLER requires pdffonts");
        }
        if !available {
            return None;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = poppler_temp_directory("fonts", nonce);
        std::fs::create_dir(&directory).unwrap();
        let pdf_path = directory.join("fixture.pdf");
        std::fs::write(&pdf_path, pdf).unwrap();
        let output = std::process::Command::new("pdffonts")
            .arg(&pdf_path)
            .output()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(String::from_utf8(output.stdout).unwrap())
    }

    fn poppler_raster(pdf: &[u8], kind: &str) -> Option<tiny_skia::Pixmap> {
        let available = std::process::Command::new("pdftoppm")
            .arg("-v")
            .output()
            .is_ok_and(|output| output.status.success());
        if std::env::var_os("RXLS_REQUIRE_POPPLER").is_some() {
            assert!(available, "RXLS_REQUIRE_POPPLER requires pdftoppm");
        }
        if !available {
            return None;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = poppler_temp_directory(kind, nonce);
        std::fs::create_dir(&directory).unwrap();
        let pdf_path = directory.join("fixture.pdf");
        let raster_prefix = directory.join("fixture-raster");
        std::fs::write(&pdf_path, pdf).unwrap();
        let output = std::process::Command::new("pdftoppm")
            .args(["-png", "-singlefile", "-r", "96"])
            .arg(&pdf_path)
            .arg(&raster_prefix)
            .output()
            .unwrap();
        let raster_path = raster_prefix.with_extension("png");
        let raster = output
            .status
            .success()
            .then(|| std::fs::read(&raster_path).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(tiny_skia::Pixmap::decode_png(&raster.unwrap()).unwrap())
    }

    fn poppler_temp_directory(kind: &str, nonce: u128) -> std::path::PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "rxls-type3-{kind}-{}-{nonce}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn poppler_word_bbox(xml: &str, word: &str) -> [f64; 4] {
        let end_marker = format!(">{word}</word>");
        let word_end = xml
            .find(&end_marker)
            .unwrap_or_else(|| panic!("word {word:?} absent from Poppler bbox XML"));
        let tag_start = xml[..word_end]
            .rfind("<word ")
            .expect("word start tag absent");
        let tag = &xml[tag_start..word_end];
        let attribute = |name: &str| {
            let marker = format!("{name}=\"");
            let start = tag
                .find(&marker)
                .unwrap_or_else(|| panic!("{name} absent from {tag:?}"))
                + marker.len();
            let end = tag[start..]
                .find('"')
                .map(|offset| start + offset)
                .expect("bbox attribute is unterminated");
            tag[start..end].parse::<f64>().unwrap()
        };
        [
            attribute("xMin"),
            attribute("yMin"),
            attribute("xMax"),
            attribute("yMax"),
        ]
    }

    fn nominal_source_bbox_points(node: &GlyphRunNode, source: &str) -> [f64; 4] {
        let source_start = node
            .text
            .find(source)
            .unwrap_or_else(|| panic!("{source:?} is absent from {:?}", node.text));
        let source_end = source_start + source.len();
        assert_eq!(node.cluster_metrics.len(), node.clusters.len());
        let transform = PdfTransform::rotation(node.rotation_degrees, node.pivot_x, node.pivot_y);
        let mut bounds = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        let mut included = 0_usize;
        for (cluster, metrics) in node.clusters.iter().zip(&node.cluster_metrics) {
            let start = usize::try_from(cluster.source_start).unwrap();
            let end = usize::try_from(cluster.source_end).unwrap();
            if start < source_start || end > source_end {
                continue;
            }
            let advance_end = metrics.origin_x.checked_add(metrics.advance_x).unwrap();
            let left = Fixed::from_raw(metrics.origin_x.raw().min(advance_end.raw()));
            let right = Fixed::from_raw(metrics.origin_x.raw().max(advance_end.raw()));
            let top = metrics.baseline_y.checked_sub(metrics.ascent).unwrap();
            let bottom = metrics.baseline_y.checked_sub(metrics.descent).unwrap();
            for point in [
                transform.point(left, top),
                transform.point(right, top),
                transform.point(right, bottom),
                transform.point(left, bottom),
            ] {
                bounds[0] = bounds[0].min(point[0]);
                bounds[1] = bounds[1].min(point[1]);
                bounds[2] = bounds[2].max(point[0]);
                bounds[3] = bounds[3].max(point[1]);
            }
            included += 1;
        }
        assert!(included > 0, "no cluster metrics cover {source:?}");
        bounds.map(|value| {
            value * PDF_POINTS_PER_CSS_PIXEL_NUMERATOR as f64
                / PDF_POINTS_PER_CSS_PIXEL_DENOMINATOR as f64
        })
    }

    fn assert_poppler_bbox_close(actual: [f64; 4], expected: [f64; 4]) {
        let actual_bbox = actual;
        let expected_bbox = expected;
        for (actual, expected) in actual.into_iter().zip(expected) {
            assert!(
                (actual - expected).abs() <= 0.002,
                "bbox coordinate {actual} differs from nominal {expected}: \
                 actual={actual_bbox:?}, expected={expected_bbox:?}"
            );
        }
    }

    fn poppler_words(xml: &str) -> Vec<&str> {
        let mut words = Vec::new();
        let mut remainder = xml;
        while let Some(tag_offset) = remainder.find("<word ") {
            remainder = &remainder[tag_offset..];
            let Some(text_offset) = remainder.find('>') else {
                break;
            };
            remainder = &remainder[text_offset + 1..];
            let Some(end_offset) = remainder.find("</word>") else {
                break;
            };
            words.push(&remainder[..end_offset]);
            remainder = &remainder[end_offset + "</word>".len()..];
        }
        words
    }

    #[allow(clippy::collapsible_match)]
    fn replace_first_text_with_test_outline(nodes: &mut [SceneNode]) -> bool {
        for node in nodes {
            match node {
                SceneNode::ClipGroup(group) => {
                    if replace_first_text_with_test_outline(&mut group.nodes) {
                        return true;
                    }
                }
                SceneNode::Text(text) => {
                    *node = SceneNode::GlyphRun(GlyphRunNode {
                        glyphs: Vec::new(),
                        font_faces: Vec::new(),
                        text: text.text.clone(),
                        clip_bounds: Rect {
                            x: Fixed::ZERO,
                            y: Fixed::ZERO,
                            width: Fixed::from_pixels(200),
                            height: Fixed::from_pixels(120),
                        },
                        commands: vec![
                            PathCommand::MoveTo {
                                x: Fixed::from_pixels(70),
                                y: Fixed::from_pixels(75),
                            },
                            PathCommand::LineTo {
                                x: Fixed::from_pixels(75),
                                y: Fixed::from_pixels(75),
                            },
                            PathCommand::LineTo {
                                x: Fixed::from_pixels(75),
                                y: Fixed::from_pixels(85),
                            },
                            PathCommand::LineTo {
                                x: Fixed::from_pixels(70),
                                y: Fixed::from_pixels(85),
                            },
                            PathCommand::Close,
                            PathCommand::MoveTo {
                                x: Fixed::from_pixels(78),
                                y: Fixed::from_pixels(75),
                            },
                            PathCommand::LineTo {
                                x: Fixed::from_pixels(83),
                                y: Fixed::from_pixels(75),
                            },
                            PathCommand::LineTo {
                                x: Fixed::from_pixels(83),
                                y: Fixed::from_pixels(85),
                            },
                            PathCommand::LineTo {
                                x: Fixed::from_pixels(78),
                                y: Fixed::from_pixels(85),
                            },
                            PathCommand::Close,
                        ],
                        clusters: vec![
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
                                command_end: 10,
                            },
                        ],
                        cluster_metrics: Vec::new(),
                        semantic_groups: Vec::new(),
                        paints: vec![GlyphPaint {
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
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    fn first_glyph_run(nodes: &[SceneNode]) -> Option<&GlyphRunNode> {
        nodes.iter().find_map(|node| match node {
            SceneNode::ClipGroup(group) => first_glyph_run(&group.nodes),
            SceneNode::GlyphRun(node) => Some(node),
            _ => None,
        })
    }

    fn glyph_run_with_text<'a>(nodes: &'a [SceneNode], expected: &str) -> Option<&'a GlyphRunNode> {
        nodes.iter().find_map(|node| match node {
            SceneNode::ClipGroup(group) => glyph_run_with_text(&group.nodes, expected),
            SceneNode::GlyphRun(node) if node.text == expected => Some(node),
            _ => None,
        })
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
                    glyphs: Vec::new(),
                    font_faces: Vec::new(),
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
                    cluster_metrics: Vec::new(),
                    semantic_groups: Vec::new(),
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

    fn assert_raster_rgb(raster: &tiny_skia::Pixmap, x: u32, y: u32, expected: [u8; 3]) {
        let pixel = raster
            .pixel(x, y)
            .unwrap_or_else(|| panic!("missing raster pixel ({x}, {y})"));
        let actual = [pixel.red(), pixel.green(), pixel.blue()];
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 2),
            "raster pixel ({x}, {y}) was {actual:?}, expected {expected:?}"
        );
    }

    #[test]
    fn native_pdf_image_rows_preserve_asymmetric_corners_with_rotation() {
        const RED: [u8; 3] = [255, 0, 0];
        const GREEN: [u8; 3] = [0, 255, 0];
        const BLUE: [u8; 3] = [0, 0, 255];
        const YELLOW: [u8; 3] = [255, 255, 0];
        let samples = [(68, 40), (108, 40), (68, 80), (108, 80)];
        for (rotation_mdeg, expected) in [
            (0, [RED, GREEN, BLUE, YELLOW]),
            (90_000, [BLUE, RED, YELLOW, GREEN]),
        ] {
            let document = document_with_nodes(
                "asymmetric-image-corners",
                vec![SceneNode::Image(ImageNode {
                    rect: Rect {
                        x: Fixed::from_pixels(48),
                        y: Fixed::from_pixels(20),
                        width: Fixed::from_pixels(80),
                        height: Fixed::from_pixels(80),
                    },
                    pixel_width: 2,
                    pixel_height: 2,
                    rgba: std::sync::Arc::from([
                        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
                    ]),
                    rotation_mdeg,
                    alt_text: Some("four asymmetric colored corners".to_string()),
                })],
            );
            let pdf = render_print_document_pdf(&document).unwrap();
            let Some(raster) = poppler_raster(&pdf, &format!("image-orientation-{rotation_mdeg}"))
            else {
                continue;
            };
            assert_eq!((raster.width(), raster.height()), (200, 120));
            for ((x, y), expected) in samples.into_iter().zip(expected) {
                assert_raster_rgb(&raster, x, y, expected);
            }
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
    fn fully_clipped_text_and_glyph_runs_do_not_emit_link_annotations() {
        let outer_clip = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(20),
            height: Fixed::from_pixels(20),
        };
        let full_page = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(200),
            height: Fixed::from_pixels(120),
        };
        let mut glyph = positioned_outline("HIDDEN", 100, 90, 30, 12);
        glyph.hyperlink = Some("https://example.com/hidden-glyph".to_string());
        let hidden_text = TextNode {
            text: "HIDDEN".to_string(),
            bounds: Rect {
                x: Fixed::from_pixels(100),
                y: Fixed::from_pixels(90),
                width: Fixed::from_pixels(40),
                height: Fixed::from_pixels(20),
            },
            clip_bounds: full_page,
            horizontal_padding: Fixed::ZERO,
            style: TextStyle {
                family: "sans-serif".to_string(),
                size: Fixed::from_pixels(12),
                color: Rgb::BLACK,
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
                anchor: TextAnchor::Start,
                baseline: TextBaseline::Top,
                rotation_degrees: 0,
            },
            hyperlink: Some("https://example.com/hidden-text".to_string()),
        };
        let nested_disjoint = ClipGroupNode {
            clip: Rect {
                x: Fixed::from_pixels(80),
                y: Fixed::from_pixels(80),
                width: Fixed::from_pixels(20),
                height: Fixed::from_pixels(20),
            },
            nodes: vec![SceneNode::Text(hidden_text.clone())],
        };
        let mut document = document_with_nodes(
            "clipped-links",
            vec![SceneNode::ClipGroup(ClipGroupNode {
                clip: outer_clip,
                nodes: vec![
                    SceneNode::Text(hidden_text),
                    SceneNode::GlyphRun(glyph),
                    SceneNode::ClipGroup(nested_disjoint),
                ],
            })],
        );
        document.pages[0].scene.width = full_page.width;
        document.pages[0].scene.height = full_page.height;

        let pdf = render_print_document_pdf(&document).unwrap();
        let source = String::from_utf8_lossy(&pdf);
        assert!(!source.contains("/Subtype /Link"), "{source}");
        assert!(!source.contains(&hex_bytes(b"https://example.com/hidden-text")));
        assert!(!source.contains(&hex_bytes(b"https://example.com/hidden-glyph")));
    }

    #[test]
    fn partially_clipped_rotated_text_and_glyph_links_remain_visible() {
        let clip = Rect {
            x: Fixed::from_pixels(20),
            y: Fixed::from_pixels(20),
            width: Fixed::from_pixels(40),
            height: Fixed::from_pixels(40),
        };
        let mut glyph = positioned_outline("GLYPH", 25, 30, 24, 12);
        glyph.rotation_degrees = 35;
        glyph.pivot_x = Fixed::from_pixels(37);
        glyph.pivot_y = Fixed::from_pixels(36);
        glyph.hyperlink = Some("https://example.com/visible-glyph".to_string());
        let text = TextNode {
            text: "TEXT".to_string(),
            bounds: Rect {
                x: Fixed::from_pixels(16),
                y: Fixed::from_pixels(16),
                width: Fixed::from_pixels(48),
                height: Fixed::from_pixels(24),
            },
            clip_bounds: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(200),
                height: Fixed::from_pixels(120),
            },
            horizontal_padding: Fixed::ZERO,
            style: TextStyle {
                family: "sans-serif".to_string(),
                size: Fixed::from_pixels(14),
                color: Rgb::BLACK,
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
                anchor: TextAnchor::Start,
                baseline: TextBaseline::Top,
                rotation_degrees: 25,
            },
            hyperlink: Some("https://example.com/visible-text".to_string()),
        };
        let document = document_with_nodes(
            "visible-clipped-links",
            vec![SceneNode::ClipGroup(ClipGroupNode {
                clip,
                nodes: vec![SceneNode::Text(text), SceneNode::GlyphRun(glyph)],
            })],
        );

        let pdf = render_print_document_pdf(&document).unwrap();
        let source = String::from_utf8_lossy(&pdf);
        assert_eq!(source.matches("/Subtype /Link").count(), 2, "{source}");
        assert!(source.contains(&hex_bytes(b"https://example.com/visible-text")));
        assert!(source.contains(&hex_bytes(b"https://example.com/visible-glyph")));
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
    fn semantic_text_ranges_preserve_unicode_words_and_whitespace() {
        let text = "alpha \t 한글  אב";
        let ranges = semantic_text_ranges(text);
        assert_eq!(
            ranges
                .iter()
                .map(|range| &text[range.clone()])
                .collect::<Vec<_>>(),
            ["alpha", " \t ", "한글", "  ", "אב"]
        );
    }

    #[test]
    fn mandatory_line_terminator_gaps_are_narrowly_allowlisted() {
        for terminator in [
            "\n", "\r", "\r\n", "\u{000B}", "\u{000C}", "\u{0085}", "\u{2028}", "\u{2029}",
        ] {
            assert!(is_mandatory_line_terminator_gap(terminator));
        }
        for unsupported in ["", " ", "\t", "\n ", "x\n"] {
            assert!(!is_mandatory_line_terminator_gap(unsupported));
        }
    }

    #[test]
    fn rotated_glyph_visibility_uses_the_serialized_pdf_transform() {
        let bounds = PdfGlyphBounds {
            min_x: Fixed::from_pixels(10),
            min_y: Fixed::from_pixels(10),
            max_x: Fixed::from_pixels(30),
            max_y: Fixed::from_pixels(20),
        };
        let transform = PdfTransform::rotation(90, Fixed::from_pixels(50), Fixed::from_pixels(50));
        assert!(transformed_bounds_intersect_clip(
            bounds,
            Rect {
                x: Fixed::from_pixels(82),
                y: Fixed::from_pixels(12),
                width: Fixed::from_pixels(6),
                height: Fixed::from_pixels(16),
            },
            transform,
        ));
        assert!(!transformed_bounds_intersect_clip(
            bounds,
            Rect {
                x: Fixed::from_pixels(12),
                y: Fixed::from_pixels(12),
                width: Fixed::from_pixels(6),
                height: Fixed::from_pixels(6),
            },
            transform,
        ));
    }

    #[test]
    fn glyph_semantic_spans_restore_logical_bidi_order_without_splitting_cjk() {
        let mut commands = sized_rectangle_commands(100, 10, 12, 10);
        commands.extend(sized_rectangle_commands(70, 10, 12, 10));
        commands.extend(sized_rectangle_commands(10, 10, 24, 10));
        let node = GlyphRunNode {
            glyphs: Vec::new(),
            font_faces: Vec::new(),
            text: "漢字 אב גד".to_string(),
            clip_bounds: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(200),
                height: Fixed::from_pixels(120),
            },
            clusters: vec![
                GlyphCluster {
                    source_start: 12,
                    source_end: 16,
                    command_start: 0,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 11,
                    source_end: 12,
                    command_start: 5,
                    command_end: 5,
                },
                GlyphCluster {
                    source_start: 7,
                    source_end: 11,
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
                    source_start: 0,
                    source_end: 6,
                    command_start: 10,
                    command_end: 15,
                },
            ],
            cluster_metrics: Vec::new(),
            semantic_groups: Vec::new(),
            paints: vec![GlyphPaint {
                command_start: 0,
                command_end: 15,
                color: Rgb::BLACK,
            }],
            commands,
            decorations: Vec::new(),
            color: Rgb::BLACK,
            rotation_degrees: 0,
            pivot_x: Fixed::ZERO,
            pivot_y: Fixed::ZERO,
            hyperlink: None,
        };
        assert!(node.metadata_is_valid());
        let spans = glyph_semantic_spans(&node, node.clusters.len(), node.clip_bounds).unwrap();
        assert_eq!(
            spans
                .iter()
                .map(|span| &node.text[span.source.clone()])
                .collect::<Vec<_>>(),
            ["漢字", "אב", "גד"]
        );
    }

    #[test]
    fn layout_same_line_metrics_preserve_cjk_rich_scripts_bidi_and_rotation() {
        let document = same_line_metric_control_document();
        let nodes = &document.pages[0].scene.nodes;
        for expected in ["漢字仮名", "ABאב", "ABC", "回転文字"] {
            let node = glyph_run_with_text(nodes, expected)
                .unwrap_or_else(|| panic!("missing layout GlyphRun for {expected:?}"));
            assert!(node.metadata_is_valid());
            assert_eq!(node.cluster_metrics.len(), node.clusters.len());
            let spans = glyph_semantic_spans(node, node.clusters.len(), node.clip_bounds).unwrap();
            assert_eq!(
                spans
                    .iter()
                    .map(|span| &node.text[span.source.clone()])
                    .collect::<Vec<_>>(),
                [expected],
                "{expected:?}"
            );
        }

        let scripts = glyph_run_with_text(nodes, "ABC").unwrap();
        assert!(
            scripts
                .cluster_metrics
                .windows(2)
                .any(|pair| pair[0].baseline_y != pair[1].baseline_y),
            "the control must exercise script-displaced baselines"
        );
        let superscript = scripts
            .clusters
            .iter()
            .position(|cluster| cluster.source_start == 0)
            .unwrap();
        let subscript = scripts
            .clusters
            .iter()
            .position(|cluster| cluster.source_start == 1)
            .unwrap();
        let superscript_metrics = scripts.cluster_metrics[superscript];
        let subscript_metrics = scripts.cluster_metrics[subscript];
        let baseline_delta =
            subscript_metrics.baseline_y.raw() - superscript_metrics.baseline_y.raw();
        let smaller_height = (superscript_metrics.ascent.raw() - superscript_metrics.descent.raw())
            .min(subscript_metrics.ascent.raw() - subscript_metrics.descent.raw());
        let larger_height = (superscript_metrics.ascent.raw() - superscript_metrics.descent.raw())
            .max(subscript_metrics.ascent.raw() - subscript_metrics.descent.raw());
        assert!(
            baseline_delta + FIXED_UNITS_PER_PIXEL >= smaller_height,
            "the control must reject the smaller-height threshold"
        );
        assert!(
            baseline_delta + FIXED_UNITS_PER_PIXEL < larger_height,
            "the conservative larger-height threshold must retain the script run"
        );
        assert!(
            !nominal_clusters_start_new_visual_line(scripts, superscript, subscript),
            "same-line script displacement must not create a visual line"
        );

        let mixed_bidi_scripts = glyph_run_with_text(nodes, "ABאב").unwrap();
        let latin = mixed_bidi_scripts
            .clusters
            .iter()
            .position(|cluster| cluster.source_start == 1)
            .unwrap();
        let rtl = mixed_bidi_scripts
            .clusters
            .iter()
            .position(|cluster| cluster.source_start == 2)
            .unwrap();
        let latin_metrics = mixed_bidi_scripts.cluster_metrics[latin];
        let rtl_metrics = mixed_bidi_scripts.cluster_metrics[rtl];
        let mixed_delta = rtl_metrics.baseline_y.raw() - latin_metrics.baseline_y.raw();
        let mixed_smaller = (latin_metrics.ascent.raw() - latin_metrics.descent.raw())
            .min(rtl_metrics.ascent.raw() - rtl_metrics.descent.raw());
        let mixed_larger = (latin_metrics.ascent.raw() - latin_metrics.descent.raw())
            .max(rtl_metrics.ascent.raw() - rtl_metrics.descent.raw());
        assert!(mixed_delta + FIXED_UNITS_PER_PIXEL >= mixed_smaller);
        assert!(mixed_delta + FIXED_UNITS_PER_PIXEL < mixed_larger);
        assert!(!nominal_clusters_start_new_visual_line(
            mixed_bidi_scripts,
            latin,
            rtl
        ));
        let rotated = glyph_run_with_text(nodes, "回転文字").unwrap();
        assert_eq!(rotated.rotation_degrees, 30);
    }

    #[test]
    fn aligned_wraps_use_directional_source_cursor_continuity() {
        for (text, alignment, expected_lengths) in [
            (RIGHT_ALIGNED_CJK, rxls::HAlign::Right, (3, 2)),
            (SOFT_WRAPPED_CJK, rxls::HAlign::Center, (3, 1)),
        ] {
            let document = wrapped_cjk_outline_document(text, 4.0, Some(alignment), false);
            let node = first_glyph_run(&document.pages[0].scene.nodes).unwrap();
            let spans = glyph_semantic_spans(node, node.clusters.len(), node.clip_bounds).unwrap();
            let [penultimate, last] = &spans[spans.len() - 2..] else {
                panic!("expected at least two wrapped lines");
            };
            assert_eq!(
                node.text[penultimate.source.clone()].chars().count(),
                expected_lengths.0
            );
            assert_eq!(
                node.text[last.source.clone()].chars().count(),
                expected_lengths.1
            );
            let left_index = *penultimate
                .glyphs
                .iter()
                .max_by_key(|index| node.clusters[**index].source_end)
                .unwrap();
            let right_index = *last
                .glyphs
                .iter()
                .min_by_key(|index| node.clusters[**index].source_start)
                .unwrap();
            let left = node.cluster_metrics[left_index];
            let right = node.cluster_metrics[right_index];
            assert!(left.advance_x > Fixed::ZERO);
            assert!(right.advance_x > Fixed::ZERO);
            let left_end = left.origin_x.raw() + left.advance_x.raw();
            let right_end = right.origin_x.raw() + right.advance_x.raw();
            assert!(
                (left.origin_x.raw() - right_end).abs() <= 1,
                "the old symmetric check must be ambiguous for {alignment:?}"
            );
            assert!(
                (left_end - right.origin_x.raw()).abs() > 1,
                "positive source cursors must reset for {alignment:?}"
            );
            assert!(nominal_clusters_start_new_visual_line(
                node,
                left_index,
                right_index
            ));
        }
    }

    #[test]
    fn outlined_text_uses_real_bounded_type3_programs_and_cluster_maps() {
        let mut workbook = rxls::Workbook::new();
        workbook.add_sheet("Subset").write(0, 0, "한A");
        let mut document = build_print_document(&workbook, 0, &PrintOptions::default()).unwrap();
        assert!(replace_first_text_with_test_outline(
            &mut document.pages[0].scene.nodes
        ));
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/Subtype /Type3"));
        assert!(source.contains("/Name /RXLSRF+OutlinedSubset0000"));
        assert!(source.contains("/CharProcs"));
        assert!(source.contains("/Widths [500 5 5]"));
        assert!(source.matches("5 0 d0").count() >= 2);
        assert!(source.contains("/FontDescriptor"));
        assert!(source.contains("/Ascent 1000 /Descent -0.000001"));
        assert!(source.contains("1 0 0 100 0 0 cm"));
        assert!(source.contains("0 -10 m"));
        assert!(source.contains("1 0 0 0.01 70 85 Tm <01> Tj"));
        assert!(source.contains("1 0 0 0.01 78 85 Tm <02> Tj"));
        assert!(source.contains("0.047059 0.133333 0.219608 rg"));
        assert!(source.contains("/ToUnicode"));
        assert!(source.contains("<01> <D55C>"));
        assert!(source.contains("<02> <0041>"));
        assert!(source.contains("<01> Tj"));
        assert!(source.contains("<02> Tj"));
        assert!(!source.contains("3 Tr"));
        assert!(!source.contains("/Helvetica"));
        assert!(!source.contains("/Subtype /Link"));

        let glyph_node = first_glyph_run(&document.pages[0].scene.nodes)
            .cloned()
            .unwrap();
        let mut registry = PdfFontRegistry::new(u64::MAX, u64::MAX, None);
        registry.glyph_count = MAX_TYPE3_GLYPH_PROGRAMS;
        assert!(matches!(
            registry.register_node(&glyph_node, None, None, true),
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
            glyphs: Vec::new(),
            font_faces: Vec::new(),
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
            cluster_metrics: Vec::new(),
            semantic_groups: Vec::new(),
            decorations: Vec::new(),
            color: Rgb::BLACK,
            rotation_degrees: 0,
            pivot_x: Fixed::ZERO,
            pivot_y: Fixed::ZERO,
            hyperlink: None,
        };
        assert!(node.metadata_is_valid());
        let mut registry = PdfFontRegistry::new(10_000, 10 << 20, None);
        let references = registry.register_node(&node, None, None, true).unwrap();
        assert_eq!(registry.subsets.len(), 2);
        assert_eq!(registry.subsets[0].glyphs.len(), 255);
        assert_eq!(registry.subsets[1].glyphs.len(), 1);
        assert_eq!(references[254].subset_index, 0);
        assert_eq!(references[254].code, 255);
        assert_eq!(references[255].subset_index, 1);
        assert_eq!(references[255].code, 1);
    }

    #[test]
    fn type3_poppler_bbox_layout_is_page_relative() {
        let document = positioned_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("1 0 0 0.01 24 28 Tm <01> Tj"));
        assert!(source.contains("1 0 0 0.01 136 28 Tm <02> Tj"));
        assert!(source.contains("1 0 0 0.012 84 94 Tm <03> Tj"));
        assert!(!source.contains("<0020>"));
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_word_bbox(&xml, "LEFT"), [18.0, 13.5, 42.0, 21.0]);
            assert_eq!(poppler_word_bbox(&xml, "RIGHT"), [102.0, 13.5, 132.0, 21.0]);
            assert_eq!(poppler_word_bbox(&xml, "LOWER"), [63.0, 61.5, 96.0, 70.5]);
        }
    }

    #[test]
    fn type3_poppler_bbox_layout_splits_whitespace_delimited_actual_text() {
        let document = whitespace_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/ActualText <FEFF0041004C005000480041>"));
        assert!(!source.contains("/ActualText <FEFF00200020>"));
        assert!(source.contains("/ActualText <FEFF0042004500540041>"));
        assert!(!source.contains("/ActualText <FEFF0041004C005000480041002000200042004500540041>"));
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_word_bbox(&xml, "ALPHA"), [18.0, 13.5, 42.0, 21.0]);
            assert_eq!(poppler_word_bbox(&xml, "BETA"), [60.0, 13.5, 90.0, 21.0]);
            assert!(!xml.contains(">ALPHA  BETA</word>"));
        }
    }

    #[test]
    fn type3_semantic_separator_splits_touching_adjacent_cell_words_without_paint() {
        let document = touching_adjacent_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(
            source.contains("<03> <0020>"),
            "the first cell boundary must map a real Type3 glyph to U+0020"
        );
        assert_eq!(
            source.matches("<0020>").count(),
            2,
            "only distinct horizontal clips in the same row may add boundaries"
        );
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["日本語", "中文", "00", "Cell"]);
        }

        let mut registry = PdfFontRegistry::new(16, 4096, None);
        let separator = registry
            .register_type3_semantic_separator(PdfGlyphReference {
                subset_index: 0,
                code: 1,
                origin_x: Fixed::from_pixels(12),
                origin_y: Fixed::from_pixels(24),
                height: Fixed::from_pixels(10),
                reverse_y: true,
            })
            .unwrap();
        assert_eq!(separator.origin_x, Fixed::from_pixels(12));
        assert_eq!(registry.subsets[0].glyphs[0].unicode_hex, "0020");
        assert_eq!(
            registry.subsets[0].glyphs[0].content, b"1 0 d0\n",
            "the semantic separator CharProc must contain metrics only"
        );
    }

    fn assert_type0_semantic_separator(left: &str, right: &str, family: &str, right_to_left: bool) {
        let (document, pack) =
            embedded_touching_cell_document(left, right, family, right_to_left, false);
        let pdf = render_print_document_pdf_with_fonts(&document, &pack).unwrap();
        assert_eq!(
            pdf,
            render_print_document_pdf_with_fonts(&document, &pack).unwrap(),
            "embedded semantic boundaries must be byte deterministic"
        );
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/Subtype /Type0"));
        assert!(
            !source.contains("/Subtype /Type3"),
            "a fully embeddable boundary must not retain a Type3 font"
        );
        assert_eq!(
            source.matches("/ActualText <FEFF0020>").count(),
            1,
            "the adjacent cells need one explicit U+0020 boundary"
        );
        assert_eq!(
            source.matches(" 3 Tr ").count(),
            1,
            "the Type0 space must use invisible text rendering mode"
        );
        assert!(
            source.contains("<0002> <0020>"),
            "the embedded space CID must map to U+0020"
        );
        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains(left), "{text:?}");
            assert!(text.contains(right), "{text:?}");
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            let words = poppler_words(&xml);
            assert_eq!(words.len(), 2, "{words:?}");
            let visual_left = left.chars().rev().collect::<String>();
            let visual_right = right.chars().rev().collect::<String>();
            let expected_left = if right_to_left { &visual_left } else { left };
            let expected_right = if right_to_left { &visual_right } else { right };
            assert!(words.contains(&expected_left), "{words:?}");
            assert!(words.contains(&expected_right), "{words:?}");
        }
    }

    #[test]
    fn type0_semantic_separator_is_unpainted_for_touching_latin_cells() {
        assert_type0_semantic_separator("LEFT", "RIGHT", "Wide Sans", false);
    }

    #[test]
    fn type0_semantic_separator_is_unpainted_for_touching_cjk_cells() {
        assert_type0_semantic_separator("日本語", "中文", "Wide Sans", false);
    }

    #[test]
    fn type0_semantic_separator_is_unpainted_for_touching_rtl_cells() {
        assert_type0_semantic_separator("אב", "גד", "RTL Sans", true);
    }

    #[test]
    fn synthetic_runs_keep_the_type3_semantic_separator_fallback() {
        let (document, pack) =
            embedded_touching_cell_document("BOLD-A", "BOLD-B", "Wide Sans", false, true);
        let pdf = render_print_document_pdf_with_fonts(&document, &pack).unwrap();
        assert_eq!(
            pdf,
            render_print_document_pdf_with_fonts(&document, &pack).unwrap()
        );
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/Subtype /Type3"));
        assert!(!source.contains("/Subtype /Type0"));
        assert_eq!(source.matches("/ActualText <FEFF0020>").count(), 1);
        assert_eq!(source.matches(" 3 Tr ").count(), 0);
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["BOLD-A", "BOLD-B"]);
        }
    }

    #[test]
    fn type3_semantic_separator_rejects_overlapping_ink_from_adjacent_rows() {
        let document = overlapping_adjacent_row_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(!source.contains("<0020>"));
        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains("UPPER"), "{text:?}");
            assert!(text.contains("LOWER"), "{text:?}");
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["UPPER", "LOWER"]);
            let upper = poppler_word_bbox(&xml, "UPPER");
            let lower = poppler_word_bbox(&xml, "LOWER");
            assert!(
                upper[3] > lower[1],
                "the control must retain vertically overlapping semantic boxes: {upper:?} {lower:?}"
            );
        }
    }

    #[test]
    fn type3_semantic_separator_handles_adjacent_cells_with_unequal_row_spans() {
        let document = touching_unequal_row_span_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert_eq!(
            source.matches("<0020>").count(),
            1,
            "a vertically merged cell and its same-row neighbor remain distinct owners"
        );
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["MERGED", "ROW"]);
        }
    }

    #[test]
    fn type3_semantic_separator_preserves_touching_rtl_cell_boundaries() {
        let document = touching_rtl_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert_eq!(source.matches("<0020>").count(), 1);
        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains(' '), "{text:?}");
            for character in ['א', 'ב', 'ג', 'ד'] {
                assert!(text.contains(character), "{text:?}");
            }
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            let words = poppler_words(&xml);
            assert_eq!(words.len(), 2, "{words:?}");
            assert!(words.contains(&"בא"), "{words:?}");
            assert!(words.contains(&"דג"), "{words:?}");
        }
    }

    #[test]
    fn type3_semantic_separator_uses_row_geometry_across_mixed_fonts_and_scripts() {
        let document = mixed_font_script_adjacent_outline_document();
        let runs = document.pages[0]
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::GlyphRun(run) => Some(run),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].clip_bounds.y, runs[1].clip_bounds.y);
        assert_eq!(runs[0].clip_bounds.height, runs[1].clip_bounds.height);
        assert_ne!(
            runs[0].cluster_metrics[0].ascent, runs[1].cluster_metrics[0].ascent,
            "the control must use materially different font metrics"
        );

        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert_eq!(source.matches("<0020>").count(), 1);
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["漢字", "42"]);
        }
    }

    #[test]
    fn type3_semantic_separator_uses_rotation_as_part_of_the_layout_line() {
        let document = rotated_boundary_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert_eq!(
            source.matches("<0020>").count(),
            1,
            "parallel rotated cells share a boundary, but a differently oriented run does not"
        );
        if let Some(text) = poppler_text(&pdf) {
            for expected in ["ROT-A", "ROT-B", "PLAIN"] {
                assert!(text.contains(expected), "{text:?}");
            }
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            let words = poppler_words(&xml);
            for expected in ["ROT-A", "ROT-B", "PLAIN"] {
                assert!(words.contains(&expected), "{words:?}");
            }
        }
    }

    #[test]
    fn type3_semantic_separator_resets_at_group_and_non_text_boundaries() {
        let document = reset_boundary_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert_eq!(
            source.matches("<0020>").count(),
            1,
            "only the consecutive GlyphRuns inside one clip group share state"
        );
        if let Some(text) = poppler_text(&pdf) {
            for expected in ["BEFORE", "AFTER", "INNER-A", "INNER-B", "OUTSIDE"] {
                assert!(text.contains(expected), "{text:?}");
            }
        }
    }

    #[test]
    fn type3_poppler_multiline_actual_text_tolerates_unshaped_line_breaks() {
        let document = multiline_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/ActualText <FEFF0054004F0050>"));
        assert!(source.contains("/ActualText <FEFF0042004F00540054004F004D>"));
        assert!(!source.contains("/ActualText <FEFF0054004F0050000A0042004F00540054004F004D>"));
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["TOP", "BOTTOM"]);
            let top = poppler_word_bbox(&xml, "TOP");
            let bottom = poppler_word_bbox(&xml, "BOTTOM");
            assert!(top[1] < bottom[1], "{top:?} {bottom:?}");
            assert!(top[3] < bottom[3], "{top:?} {bottom:?}");
        }
        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains("TOP"), "{text:?}");
            assert!(text.contains("BOTTOM"), "{text:?}");
        }
    }

    #[test]
    fn type3_poppler_soft_wrapped_cjk_preserves_order_and_line_boxes() {
        let document = soft_wrapped_cjk_outline_document();
        let node = first_glyph_run(&document.pages[0].scene.nodes).unwrap();
        assert_eq!(node.text, SOFT_WRAPPED_CJK);
        assert_eq!(node.cluster_metrics.len(), node.clusters.len());
        let spans = glyph_semantic_spans(node, node.clusters.len(), node.clip_bounds).unwrap();
        let span_text = spans
            .iter()
            .map(|span| &node.text[span.source.clone()])
            .collect::<Vec<_>>();
        let layout_baselines = node
            .cluster_metrics
            .iter()
            .map(|metrics| metrics.baseline_y.raw())
            .collect::<BTreeSet<_>>();
        assert!(layout_baselines.len() >= 2, "{layout_baselines:?}");
        assert_eq!(span_text.len(), layout_baselines.len(), "{span_text:?}");
        assert_eq!(spans.first().unwrap().source.start, 0);
        assert_eq!(spans.last().unwrap().source.end, node.text.len());
        assert!(spans
            .windows(2)
            .all(|pair| pair[0].source.end == pair[1].source.start));
        for span in &spans {
            let baselines = span
                .glyphs
                .iter()
                .map(|index| node.cluster_metrics[*index].baseline_y.raw())
                .collect::<BTreeSet<_>>();
            assert_eq!(baselines.len(), 1, "{span:?}");
        }

        let mut no_metrics = node.clone();
        no_metrics.cluster_metrics.clear();
        let legacy = glyph_semantic_spans(
            &no_metrics,
            no_metrics.clusters.len(),
            no_metrics.clip_bounds,
        )
        .unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].source, 0..no_metrics.text.len());

        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        for line in &span_text {
            assert!(
                source.contains(&format!("/ActualText <FEFF{}>", utf16be_hex(line))),
                "{line:?}"
            );
        }
        assert!(!source.contains(&format!(
            "/ActualText <FEFF{}>",
            utf16be_hex(SOFT_WRAPPED_CJK)
        )));

        if let Some(text) = poppler_text(&pdf) {
            assert_eq!(
                text.chars()
                    .filter(|character| !character.is_whitespace())
                    .collect::<String>(),
                SOFT_WRAPPED_CJK
            );
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), span_text);
            let boxes = span_text
                .iter()
                .map(|line| {
                    let actual = poppler_word_bbox(&xml, line);
                    assert_poppler_bbox_close(actual, nominal_source_bbox_points(node, line));
                    actual
                })
                .collect::<Vec<_>>();
            assert!(
                boxes
                    .windows(2)
                    .all(|pair| pair[0][3] <= pair[1][1] + 0.002),
                "{boxes:?}"
            );
        }
    }

    fn mixed_size_soft_wrapped_no_whitespace_document() -> PrintDocument {
        // Two whitespace-free CJK runs, glued together with no space between
        // them, wrapped onto a narrow column so the automatic break falls
        // exactly at the run boundary: "天地玄" (small, fills line 1) then
        // "宇宙洪" (much larger, each character alone fills a line). This is
        // the "wrap can occur mid-run with no space cluster" shape from a
        // script other than uniformly-sized CJK: the visual line boundary
        // also coincides with a rich-text size change, which is exactly what
        // defeated the old symmetric "larger of the two nominal heights"
        // requirement in `nominal_clusters_start_new_visual_line`.
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("mixed-size-wrap");
        sheet.set_col_width(0, 4.0);
        let base = rxls::CellStyle::new().font_name(&family).size(11).wrap();
        sheet.write_rich_styled(
            0,
            0,
            [
                rxls::TextRun::new("天地玄", rxls::Font::new().with_name(&family).with_size(11)),
                rxls::TextRun::new("宇宙洪", rxls::Font::new().with_name(&family).with_size(72)),
            ],
            &base,
        );
        outlined_test_document(&workbook, pack)
    }

    #[test]
    fn soft_wrapped_mixed_size_run_splits_at_every_visual_line_without_whitespace() {
        const TEXT: &str = "天地玄宇宙洪";
        let document = mixed_size_soft_wrapped_no_whitespace_document();
        let node = first_glyph_run(&document.pages[0].scene.nodes).unwrap();
        assert_eq!(node.text, TEXT);
        assert_eq!(node.cluster_metrics.len(), node.clusters.len());

        // The layout produces four distinct visual lines ("天地玄", "宇",
        // "宙", "洪"): the size change from 11pt to 72pt forces each large
        // character onto its own line in this narrow column.
        let layout_baselines = node
            .cluster_metrics
            .iter()
            .map(|metrics| metrics.baseline_y.raw())
            .collect::<BTreeSet<_>>();
        assert_eq!(layout_baselines.len(), 4, "{layout_baselines:?}");

        let spans = glyph_semantic_spans(node, node.clusters.len(), node.clip_bounds).unwrap();
        let span_text = spans
            .iter()
            .map(|span| &node.text[span.source.clone()])
            .collect::<Vec<_>>();
        // Before the fix, the small-to-large transition at the first wrap
        // boundary was missed (the ending line's own reserved height never
        // reached the much taller next line's nominal height), collapsing
        // "天地玄" and "宇" into one span: ["天地玄宇", "宙", "洪"]. Each
        // visual line must instead surface as its own span.
        assert_eq!(span_text, ["天地玄", "宇", "宙", "洪"], "{span_text:?}");

        // Spans must partition the source text exactly (logical reading
        // order preserved, no characters dropped or duplicated) and every
        // span's glyphs share one baseline (a genuine visual line).
        assert_eq!(spans.first().unwrap().source.start, 0);
        assert_eq!(spans.last().unwrap().source.end, node.text.len());
        assert!(spans
            .windows(2)
            .all(|pair| pair[0].source.end == pair[1].source.start));
        for span in &spans {
            let baselines = span
                .glyphs
                .iter()
                .map(|index| node.cluster_metrics[*index].baseline_y.raw())
                .collect::<BTreeSet<_>>();
            assert_eq!(baselines.len(), 1, "{span:?}");
        }

        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        for line in &span_text {
            assert!(
                source.contains(&format!("/ActualText <FEFF{}>", utf16be_hex(line))),
                "{line:?}"
            );
        }
        // The whole run must never be emitted as one page-sized ActualText
        // span; ToUnicode/ActualText round-trips the same logical text
        // regardless, split only along visual line boundaries.
        assert!(!source.contains(&format!("/ActualText <FEFF{}>", utf16be_hex(TEXT))));

        if let Some(text) = poppler_text(&pdf) {
            assert_eq!(
                text.chars()
                    .filter(|character| !character.is_whitespace())
                    .collect::<String>(),
                TEXT
            );
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), span_text);
        }
    }

    #[test]
    fn same_line_script_displaced_bidi_run_is_not_split_by_the_looser_wrap_bound() {
        // Guards the fix above: a same-line superscript/subscript baseline
        // shift across a Latin/RTL boundary must still be retained as one
        // ActualText span. Bidi-reordered runs are laid out with a visual
        // gap at the embedding boundary that is not a reliable line-wrap
        // signal, so `nominal_clusters_start_new_visual_line` must keep
        // requiring the stricter symmetric height bound whenever either
        // cluster's own text is strongly right-to-left.
        let document = same_line_metric_control_document();
        let nodes = &document.pages[0].scene.nodes;
        let mixed_bidi_scripts = glyph_run_with_text(nodes, "ABאב").unwrap();
        let spans = glyph_semantic_spans(
            mixed_bidi_scripts,
            mixed_bidi_scripts.clusters.len(),
            mixed_bidi_scripts.clip_bounds,
        )
        .unwrap();
        let span_text = spans
            .iter()
            .map(|span| &mixed_bidi_scripts.text[span.source.clone()])
            .collect::<Vec<_>>();
        assert_eq!(span_text, ["ABאב"], "{span_text:?}");
    }

    #[test]
    fn imported_xlsx_calc_wraps_have_exact_touch_spans_and_poppler_boxes() {
        for (text, alignment) in [
            (SOFT_WRAPPED_CJK, None),
            (RIGHT_ALIGNED_CJK, Some(rxls::HAlign::Right)),
            (SOFT_WRAPPED_CJK, Some(rxls::HAlign::Center)),
        ] {
            let document = wrapped_cjk_outline_document(text, 4.0, alignment, true);
            let node = first_glyph_run(&document.pages[0].scene.nodes).unwrap();
            assert_eq!(node.text, text);
            let spans = glyph_semantic_spans(node, node.clusters.len(), node.clip_bounds).unwrap();
            assert!(spans.len() >= 2, "{alignment:?}");
            let span_text = spans
                .iter()
                .map(|span| &node.text[span.source.clone()])
                .collect::<Vec<_>>();
            assert!(spans
                .windows(2)
                .all(|pair| pair[0].source.end == pair[1].source.start));
            for pair in spans.windows(2) {
                let left_index = *pair[0]
                    .glyphs
                    .iter()
                    .max_by_key(|index| node.clusters[**index].source_end)
                    .unwrap();
                let right_index = *pair[1]
                    .glyphs
                    .iter()
                    .min_by_key(|index| node.clusters[**index].source_start)
                    .unwrap();
                let left = node.cluster_metrics[left_index];
                let right = node.cluster_metrics[right_index];
                let left_height = left.ascent.raw() - left.descent.raw();
                let right_height = right.ascent.raw() - right.descent.raw();
                assert_eq!(left_height, right_height, "{alignment:?}");
                assert_eq!(
                    right.baseline_y.raw() - left.baseline_y.raw(),
                    left_height,
                    "{alignment:?}"
                );
                assert!(nominal_clusters_start_new_visual_line(
                    node,
                    left_index,
                    right_index
                ));
            }

            let pdf = render_print_document_pdf(&document).unwrap();
            if let Some(extracted) = poppler_text(&pdf) {
                assert_eq!(
                    extracted
                        .chars()
                        .filter(|character| !character.is_whitespace())
                        .collect::<String>(),
                    text
                );
            }
            if let Some(xml) = poppler_bbox_layout(&pdf) {
                assert_eq!(poppler_words(&xml), span_text);
                let boxes = span_text
                    .iter()
                    .map(|line| {
                        let actual = poppler_word_bbox(&xml, line);
                        assert_poppler_bbox_close(actual, nominal_source_bbox_points(node, line));
                        actual
                    })
                    .collect::<Vec<_>>();
                assert!(
                    boxes
                        .windows(2)
                        .all(|pair| pair[0][3] <= pair[1][1] + 0.002),
                    "{alignment:?} {boxes:?}"
                );
            }
        }
    }

    #[test]
    fn type3_poppler_retains_visible_final_line_when_nominal_descent_crosses_page_clip() {
        let document = final_line_nominal_descent_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/ActualText <FEFF00460049004E0041004C002D004C0049004E0045>"));
        assert!(source.contains("79.9990234375 Tm <01> Tj"));

        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains("FINAL-LINE"), "{text:?}");
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["FINAL-LINE"]);
            assert_poppler_bbox_close(
                poppler_word_bbox(&xml, "FINAL-LINE"),
                [
                    9.0,
                    49.5,
                    57.0,
                    fixed_as_f64(Fixed::from_raw(80 * FIXED_UNITS_PER_PIXEL - 1))
                        * PDF_POINTS_PER_CSS_PIXEL_NUMERATOR as f64
                        / PDF_POINTS_PER_CSS_PIXEL_DENOMINATOR as f64,
                ],
            );
        }
        if let Some(raster) = poppler_raster(&pdf, "final-line") {
            assert_raster_rgb(&raster, 20, 67, [255, 255, 255]);
            assert_raster_rgb(&raster, 20, 68, [0, 0, 0]);
            assert_raster_rgb(&raster, 20, 77, [0, 0, 0]);
            assert_raster_rgb(&raster, 20, 78, [255, 255, 255]);
        }
    }

    #[test]
    fn type3_poppler_retains_bottom_straddling_visible_ink_on_a_51px_page() {
        let document = bottom_straddling_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains(
            "/ActualText <FEFF0043004C00490050005000450044002D0042004F00540054004F004D>"
        ));
        assert!(source.contains("50.9990234375 Tm <01> Tj"));

        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains("CLIPPED-BOTTOM"), "{text:?}");
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["CLIPPED-BOTTOM"]);
            let bbox = poppler_word_bbox(&xml, "CLIPPED-BOTTOM");
            assert!((bbox[3] - 38.249_267_578_125).abs() <= 0.002, "{bbox:?}");
        }
        if let Some(raster) = poppler_raster(&pdf, "bottom-straddling") {
            assert_eq!((raster.width(), raster.height()), (100, 51));
            assert_raster_rgb(&raster, 20, 44, [255, 255, 255]);
            assert_raster_rgb(&raster, 20, 45, [0, 0, 0]);
            assert_raster_rgb(&raster, 20, 50, [0, 0, 0]);
        }
    }

    #[test]
    fn type3_semantic_separator_retains_touching_cells_at_the_bottom_clip() {
        let document = touching_bottom_straddling_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert_eq!(source.matches("<0020>").count(), 1);
        assert_eq!(source.matches("50.9990234375 Tm").count(), 3);
        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains("BOTTOM-A"), "{text:?}");
            assert!(text.contains("BOTTOM-B"), "{text:?}");
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["BOTTOM-A", "BOTTOM-B"]);
            for word in ["BOTTOM-A", "BOTTOM-B"] {
                let bbox = poppler_word_bbox(&xml, word);
                assert!(bbox[3] <= 38.25, "{word}: {bbox:?}");
            }
        }
    }

    #[test]
    fn type3_rtl_actual_text_does_not_infer_lines_from_unequal_ink_heights() {
        let document = unequal_height_rtl_outline_document();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/ActualText <FEFF05D105D0>"));
        assert!(!source.contains("/ActualText <FEFF002005D105D0>"));
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["בא", "דג"]);
            assert!(!xml.contains(">בא </word>"), "{xml}");
        }
    }

    #[test]
    fn nested_partial_clip_retains_font_pack_glyph_semantics() {
        let glyph = font_pack_glyph_run("FONTCLIP");
        let bounds = glyph_bounds(&glyph.commands).expect("font-pack glyph bounds");
        let middle_y =
            Fixed::from_raw(bounds.min_y.raw() + (bounds.max_y.raw() - bounds.min_y.raw()) / 2);
        let partial_clip = Rect {
            x: bounds.min_x,
            y: middle_y,
            width: bounds.max_x.checked_sub(bounds.min_x).unwrap(),
            height: bounds.max_y.checked_sub(middle_y).unwrap(),
        };
        let document = document_with_nodes(
            "font-pack-partial-clip",
            vec![SceneNode::ClipGroup(ClipGroupNode {
                clip: Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(200),
                    height: Fixed::from_pixels(120),
                },
                nodes: vec![SceneNode::ClipGroup(ClipGroupNode {
                    clip: partial_clip,
                    nodes: vec![SceneNode::GlyphRun(glyph)],
                })],
            })],
        );
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains("FONTCLIP"), "{text:?}");
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["FONTCLIP"]);
        }
    }

    #[test]
    fn semantic_baseline_filter_preserves_glyph_paint_as_an_artifact() {
        let mut artifact = font_pack_glyph_run("PAINTONLY");
        artifact.clip_bounds.width = Fixed::from_pixels(180);
        let clip_bottom = artifact
            .clip_bounds
            .y
            .checked_add(artifact.clip_bounds.height)
            .unwrap();
        assert!(!artifact.cluster_metrics.is_empty());
        for metrics in &mut artifact.cluster_metrics {
            metrics.baseline_y = clip_bottom;
        }
        let reference = artifact.clone();
        let reference_clip = Rect {
            width: artifact
                .clip_bounds
                .width
                .checked_sub(Fixed::from_pixels(1))
                .unwrap(),
            ..artifact.clip_bounds
        };
        let bounds = glyph_bounds(&reference.commands).expect("font-pack glyph bounds");
        let reference_right = reference_clip
            .x
            .checked_add(reference_clip.width)
            .unwrap();
        assert!(bounds.max_x < reference_right);
        let reference_pdf = render_print_document_pdf(&document_with_nodes(
            "semantic-paint-reference",
            vec![SceneNode::ClipGroup(ClipGroupNode {
                clip: reference_clip,
                nodes: vec![SceneNode::GlyphRun(reference)],
            })],
        ))
        .unwrap();
        let artifact_pdf = render_print_document_pdf(&document_with_nodes(
            "semantic-paint-artifact",
            vec![SceneNode::GlyphRun(artifact)],
        ))
        .unwrap();
        let artifact_source = String::from_utf8_lossy(&artifact_pdf);
        assert!(artifact_source.contains("/ActualText <FEFF>"));

        if let (Some(reference_text), Some(artifact_text)) =
            (poppler_text(&reference_pdf), poppler_text(&artifact_pdf))
        {
            assert!(reference_text.contains("PAINTONLY"), "{reference_text:?}");
            assert!(!artifact_text.contains("PAINTONLY"), "{artifact_text:?}");
        }
        if let (Some(reference_raster), Some(artifact_raster)) = (
            poppler_raster(&reference_pdf, "semantic-paint-reference"),
            poppler_raster(&artifact_pdf, "semantic-paint-artifact"),
        ) {
            assert_eq!(reference_raster.data(), artifact_raster.data());
        }
    }

    #[test]
    fn nested_disjoint_clip_suppresses_font_pack_glyph_semantics() {
        let glyph = font_pack_glyph_run("HIDDENFONT");
        let bounds = glyph_bounds(&glyph.commands).expect("font-pack glyph bounds");
        let hidden_x = bounds
            .max_x
            .checked_add(Fixed::from_pixels(20))
            .expect("hidden clip x");
        let hidden_right = hidden_x
            .checked_add(Fixed::from_pixels(4))
            .expect("hidden clip right");
        let node_right = glyph
            .clip_bounds
            .x
            .checked_add(glyph.clip_bounds.width)
            .expect("glyph clip right");
        assert!(hidden_right < node_right);
        assert!(hidden_right < Fixed::from_pixels(200));
        let document = document_with_nodes(
            "font-pack-disjoint-clip",
            vec![SceneNode::ClipGroup(ClipGroupNode {
                clip: Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(200),
                    height: Fixed::from_pixels(120),
                },
                nodes: vec![SceneNode::ClipGroup(ClipGroupNode {
                    clip: Rect {
                        x: hidden_x,
                        y: bounds.min_y,
                        width: Fixed::from_pixels(4),
                        height: bounds.max_y.checked_sub(bounds.min_y).unwrap(),
                    },
                    nodes: vec![SceneNode::GlyphRun(glyph)],
                })],
            })],
        );
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(!source.contains("/ActualText"));
        if let Some(text) = poppler_text(&pdf) {
            assert!(!text.contains("HIDDENFONT"), "{text:?}");
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert!(poppler_words(&xml).is_empty(), "{xml}");
        }
    }

    #[test]
    fn nested_partial_and_disjoint_clips_bound_fallback_text_semantics() {
        let visible = fallback_text_node("FALLBACK");
        let hidden = fallback_text_node("HIDDENTEXT");
        let document = document_with_nodes(
            "fallback-nested-clips",
            vec![
                SceneNode::ClipGroup(ClipGroupNode {
                    clip: Rect {
                        x: Fixed::ZERO,
                        y: Fixed::ZERO,
                        width: Fixed::from_pixels(200),
                        height: Fixed::from_pixels(120),
                    },
                    nodes: vec![SceneNode::ClipGroup(ClipGroupNode {
                        clip: Rect {
                            x: Fixed::from_pixels(24),
                            y: Fixed::from_pixels(20),
                            width: Fixed::from_pixels(24),
                            height: Fixed::from_pixels(12),
                        },
                        nodes: vec![SceneNode::Text(visible)],
                    })],
                }),
                SceneNode::ClipGroup(ClipGroupNode {
                    clip: Rect {
                        x: Fixed::from_pixels(130),
                        y: Fixed::from_pixels(20),
                        width: Fixed::from_pixels(8),
                        height: Fixed::from_pixels(12),
                    },
                    nodes: vec![SceneNode::Text(hidden)],
                }),
            ],
        );
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert_eq!(
            source
                .matches(&format!("/ActualText <FEFF{}>", utf16be_hex("FAL")))
                .count(),
            1
        );
        assert!(!source.contains(&format!("/ActualText <FEFF{}>", utf16be_hex("FALLBACK"))));
        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains("FAL"), "{text:?}");
            assert!(!text.contains("FALLBACK"), "{text:?}");
            assert!(!text.contains("HIDDENTEXT"), "{text:?}");
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["FAL"]);
        }
    }

    #[test]
    fn rotated_glyph_actual_text_follows_transformed_clip_visibility() {
        let mut visible = positioned_outline("ROTATED", 10, 10, 20, 10);
        visible.rotation_degrees = 90;
        visible.pivot_x = Fixed::from_pixels(50);
        visible.pivot_y = Fixed::from_pixels(50);
        let mut hidden = positioned_outline("HIDDEN", 10, 10, 20, 10);
        hidden.rotation_degrees = 90;
        hidden.pivot_x = Fixed::from_pixels(50);
        hidden.pivot_y = Fixed::from_pixels(50);
        let document = document_with_nodes(
            "rotated-glyph-clips",
            vec![
                SceneNode::ClipGroup(ClipGroupNode {
                    clip: Rect {
                        x: Fixed::from_pixels(82),
                        y: Fixed::from_pixels(12),
                        width: Fixed::from_pixels(6),
                        height: Fixed::from_pixels(16),
                    },
                    nodes: vec![SceneNode::GlyphRun(visible)],
                }),
                SceneNode::ClipGroup(ClipGroupNode {
                    clip: Rect {
                        x: Fixed::from_pixels(12),
                        y: Fixed::from_pixels(12),
                        width: Fixed::from_pixels(6),
                        height: Fixed::from_pixels(6),
                    },
                    nodes: vec![SceneNode::GlyphRun(hidden)],
                }),
            ],
        );
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains("ROTATED"), "{text:?}");
            assert!(!text.contains("HIDDEN"), "{text:?}");
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["ROTATED"]);
        }
    }

    #[test]
    fn fontless_ltr_page_seams_emit_ordered_fragments_once_and_gate_links() {
        let text = "ALPHA BETA GAMMA";
        let full_page = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(240),
            height: Fixed::from_pixels(100),
        };
        let node = TextNode {
            text: text.to_string(),
            bounds: Rect {
                x: Fixed::from_pixels(10),
                y: Fixed::from_pixels(20),
                width: Fixed::from_pixels(220),
                height: Fixed::from_pixels(30),
            },
            clip_bounds: full_page,
            horizontal_padding: Fixed::ZERO,
            style: TextStyle {
                family: "sans-serif".to_string(),
                size: Fixed::from_pixels(20),
                color: Rgb::BLACK,
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
                anchor: TextAnchor::Start,
                baseline: TextBaseline::Top,
                rotation_degrees: 0,
            },
            hyperlink: Some("https://example.com/page-seam".to_string()),
        };
        let document = document_with_clipped_text_pages(
            "fontless-ltr-seams",
            node,
            240,
            100,
            &[
                Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(80),
                    height: full_page.height,
                },
                Rect {
                    x: Fixed::from_pixels(80),
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(65),
                    height: full_page.height,
                },
                Rect {
                    x: Fixed::from_pixels(145),
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(95),
                    height: full_page.height,
                },
                Rect {
                    x: Fixed::from_pixels(260),
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(10),
                    height: full_page.height,
                },
            ],
        );
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(!source.contains(&format!("/ActualText <FEFF{}>", utf16be_hex(text))));
        assert_eq!(source.matches("/Subtype /Link").count(), 3, "{source}");
        assert_eq!(
            source
                .matches(&hex_bytes(b"https://example.com/page-seam"))
                .count(),
            3
        );
        if let Some(pages) = poppler_text_pages(&pdf) {
            assert_eq!(pages.len(), 4, "{pages:?}");
            assert_eq!(pages[0].trim(), "ALPHA", "{pages:?}");
            assert_eq!(pages[1].trim(), "BETA", "{pages:?}");
            assert_eq!(pages[2].trim(), "GAMMA", "{pages:?}");
            assert!(pages[3].trim().is_empty(), "{pages:?}");
            assert_eq!(
                pages
                    .iter()
                    .flat_map(|page| page.split_whitespace())
                    .collect::<Vec<_>>(),
                ["ALPHA", "BETA", "GAMMA"]
            );
        }
    }

    #[test]
    fn fontless_cjk_page_seams_preserve_original_source_fragments() {
        let text = "漢字한글";
        let full_page = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(120),
            height: Fixed::from_pixels(80),
        };
        let node = TextNode {
            text: text.to_string(),
            bounds: Rect {
                x: Fixed::from_pixels(10),
                y: Fixed::from_pixels(20),
                width: Fixed::from_pixels(100),
                height: Fixed::from_pixels(30),
            },
            clip_bounds: full_page,
            horizontal_padding: Fixed::ZERO,
            style: TextStyle {
                family: "sans-serif".to_string(),
                size: Fixed::from_pixels(20),
                color: Rgb::BLACK,
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
                anchor: TextAnchor::Start,
                baseline: TextBaseline::Top,
                rotation_degrees: 0,
            },
            hyperlink: None,
        };
        let document = document_with_clipped_text_pages(
            "fontless-cjk-seams",
            node,
            120,
            80,
            &[
                Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(32),
                    height: full_page.height,
                },
                Rect {
                    x: Fixed::from_pixels(32),
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(88),
                    height: full_page.height,
                },
            ],
        );
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(!source.contains(&format!("/ActualText <FEFF{}>", utf16be_hex(text))));
        if let Some(pages) = poppler_text_pages(&pdf) {
            assert_eq!(pages.len(), 2, "{pages:?}");
            assert_eq!(pages[0].trim(), "漢字", "{pages:?}");
            assert_eq!(pages[1].trim(), "한글", "{pages:?}");
            assert_eq!(
                pages.iter().map(|page| page.trim()).collect::<String>(),
                text
            );
            assert!(!pages.iter().any(|page| page.contains('?')), "{pages:?}");
        }
    }

    #[test]
    fn rotated_fontless_page_seams_use_inverse_clip_geometry() {
        let text = "ROTATEDSEAM";
        let full_page = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(200),
            height: Fixed::from_pixels(140),
        };
        let node = TextNode {
            text: text.to_string(),
            bounds: Rect {
                x: Fixed::from_pixels(20),
                y: Fixed::from_pixels(50),
                width: Fixed::from_pixels(160),
                height: Fixed::from_pixels(30),
            },
            clip_bounds: full_page,
            horizontal_padding: Fixed::ZERO,
            style: TextStyle {
                family: "sans-serif".to_string(),
                size: Fixed::from_pixels(20),
                color: Rgb::BLACK,
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
                anchor: TextAnchor::Start,
                baseline: TextBaseline::Top,
                rotation_degrees: 30,
            },
            hyperlink: None,
        };
        let document = document_with_clipped_text_pages(
            "fontless-rotated-seams",
            node,
            200,
            140,
            &[
                Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(80),
                    height: full_page.height,
                },
                Rect {
                    x: Fixed::from_pixels(80),
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(120),
                    height: full_page.height,
                },
            ],
        );
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        if let Some(pages) = poppler_text_pages(&pdf) {
            assert_eq!(pages.len(), 2, "{pages:?}");
            let fragments = pages
                .iter()
                .map(|page| page.split_whitespace().collect::<String>())
                .collect::<Vec<_>>();
            assert_eq!(fragments, ["ROTAT", "EDSEAM"], "{pages:?}");
            assert_eq!(fragments.concat(), text);
        }
    }

    #[test]
    fn horizontal_chart_title_repro_emits_exact_page_local_poppler_fragments() {
        const TITLE: &str =
            "HORIZONTAL-SEMANTIC-SEAM-ABCDEFGHIJKLMNOPQRSTUVWXYZ-0123456789-abcdefghijk";
        fn title_count(nodes: &[SceneNode]) -> usize {
            nodes
                .iter()
                .map(|node| match node {
                    SceneNode::ClipGroup(group) => title_count(&group.nodes),
                    SceneNode::Text(text) if text.text == TITLE => 1,
                    SceneNode::GlyphRun(run) if run.text == TITLE => 1,
                    _ => 0,
                })
                .sum()
        }

        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("Horizontal");
        for row in 0..4 {
            sheet.write_number(row, 2, f64::from(row + 1));
            sheet.set_row_height(row, 120.0);
        }
        for column in 0..3 {
            sheet.set_col_width(column, 85.0);
        }
        sheet.add_chart(
            rxls::Chart::new(rxls::ChartKind::Line, (0, 0), (3, 2))
                .with_title(TITLE)
                .add_series(rxls::Series::new("Horizontal!$C$1:$C$4")),
        );
        sheet.set_page_setup(
            rxls::PageSetup::new()
                .with_print_area((0, 0, 3, 2))
                .with_paper_size(1)
                .with_scale(100),
        );
        let document = build_print_document(
            &workbook,
            0,
            &PrintOptions {
                omit_sparse_pages: false,
                ..PrintOptions::default()
            },
        )
        .unwrap();
        assert_eq!(TITLE.chars().count(), 74);
        assert_eq!(document.pages.len(), 3);
        assert_eq!(
            document
                .pages
                .iter()
                .map(|page| title_count(&page.scene.nodes))
                .collect::<Vec<_>>(),
            [1, 1, 0]
        );
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let source = String::from_utf8_lossy(&pdf);
        assert!(!source.contains(&format!("/ActualText <FEFF{}>", utf16be_hex(TITLE))));
        let first = "HORIZONTAL-SEMANTIC-SEAM-ABCDEFGHIJK";
        let second = "LMNOPQRSTUVWXYZ-0123456789-abcdefghijk";
        assert_eq!(
            source
                .matches(&format!("/ActualText <FEFF{}>", utf16be_hex(first)))
                .count(),
            1
        );
        assert_eq!(
            source
                .matches(&format!("/ActualText <FEFF{}>", utf16be_hex(second)))
                .count(),
            1
        );
        if let Some(pages) = poppler_text_pages(&pdf) {
            assert_eq!(pages.len(), 3, "{pages:?}");
            assert!(pages[0].contains(first), "{pages:?}");
            assert!(!pages[0].contains(second), "{pages:?}");
            assert!(pages[1].contains(second), "{pages:?}");
            assert!(!pages[1].contains(first), "{pages:?}");
            assert!(!pages[2].contains(first), "{pages:?}");
            assert!(!pages[2].contains(second), "{pages:?}");
            assert_eq!(format!("{first}{second}"), TITLE);
            assert_eq!(pages.iter().filter(|page| page.contains(TITLE)).count(), 0);
        }
    }

    #[test]
    fn project_font_pack_type0_pdf_exposes_exact_poppler_word_tokens() {
        let pack = synthetic_test_pack();
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("words");
        sheet.set_col_width(0, 40.0);
        sheet.write(0, 0, "alpha beta gamma");
        let options = PrintOptions {
            single_page_sheets: true,
            render: RenderOptions {
                gridlines: false,
                default_font_family: pack.default_family().to_string(),
                font_pack: Some(pack.clone()),
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        };
        let document = build_print_document(&workbook, 0, &options).unwrap();
        let pdf = render_print_document_pdf_with_fonts(&document, &pack).unwrap();
        if let Some(fonts) = poppler_fonts(&pdf) {
            let rows = fonts.lines().skip(2).collect::<Vec<_>>();
            assert_eq!(rows.len(), 1, "{fonts}");
            let fields = rows[0].split_ascii_whitespace().collect::<Vec<_>>();
            assert!(fields.len() >= 7, "{fonts}");
            assert_eq!(
                &fields[..7],
                [
                    "RXLSEM+EmbeddedSubset0000",
                    "CID",
                    "TrueType",
                    "Identity-H",
                    "yes",
                    "yes",
                    "yes",
                ],
                "{fonts}"
            );
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["alpha", "beta", "gamma"]);
            let alpha = poppler_word_bbox(&xml, "alpha");
            let beta = poppler_word_bbox(&xml, "beta");
            let gamma = poppler_word_bbox(&xml, "gamma");
            for bounds in [alpha, beta, gamma] {
                assert!(bounds[0] < bounds[2], "{bounds:?}");
                assert!(bounds[1] < bounds[3], "{bounds:?}");
                assert!(
                    (bounds[3] - bounds[1] - 13.2).abs() <= 0.002,
                    "the synthetic 900/-300 descriptor at 11pt must yield a 13.2pt box: {bounds:?}"
                );
            }
            assert!(alpha[2] < beta[0], "{alpha:?} {beta:?}");
            assert!(beta[2] < gamma[0], "{beta:?} {gamma:?}");
            assert!((alpha[1] - beta[1]).abs() < 0.01);
            assert!((beta[1] - gamma[1]).abs() < 0.01);
        }
    }

    #[test]
    fn pinned_type0_poppler_boxes_follow_libreoffice_descriptor_metrics() {
        let Some(manifest) = std::env::var_os("RXLS_TEST_FONT_PACK_MANIFEST") else {
            return;
        };
        let pack = FontPack::load_manifest(manifest).unwrap();

        for (family, descriptor, expected_height) in [
            ("Arimo", "/Ascent 1042 /Descent -389", 15.741),
            ("Noto Sans CJK KR", "/Ascent 1160 /Descent -288", 15.928),
        ] {
            let mut workbook = rxls::Workbook::new();
            let sheet = workbook.add_sheet("descriptor");
            sheet.set_col_width(0, 40.0);
            sheet.write(0, 0, "Q1");
            let options = PrintOptions {
                single_page_sheets: true,
                render: RenderOptions {
                    gridlines: false,
                    default_font_family: family.to_string(),
                    font_pack: Some(pack.clone()),
                    ..RenderOptions::default()
                },
                ..PrintOptions::default()
            };
            let document = build_print_document(&workbook, 0, &options).unwrap();
            let pdf = render_print_document_pdf_with_fonts(&document, &pack).unwrap();
            let source = String::from_utf8_lossy(&pdf);
            assert!(source.contains(descriptor), "{family}: {source}");
            if let Some(xml) = poppler_bbox_layout(&pdf) {
                assert_eq!(poppler_words(&xml), ["Q1"], "{family}: {xml}");
                let bounds = poppler_word_bbox(&xml, "Q1");
                let height = bounds[3] - bounds[1];
                assert!(
                    (height - expected_height).abs() <= 0.002,
                    "{family} Poppler height {height} does not match the descriptor: {bounds:?}"
                );
            }
        }
    }

    #[test]
    fn type3_poppler_boxes_use_nominal_metrics_for_latin_and_cjk() {
        let node = font_pack_glyph_run("ACE 漢字");
        assert_eq!(node.cluster_metrics.len(), node.clusters.len());
        let latin_expected = nominal_source_bbox_points(&node, "ACE");
        let cjk_expected = nominal_source_bbox_points(&node, "漢字");
        assert!((latin_expected[3] - latin_expected[1] - 11.0).abs() <= 0.002);
        assert!((cjk_expected[3] - cjk_expected[1] - 11.0).abs() <= 0.002);

        let document =
            document_with_nodes("type3-nominal-latin-cjk", vec![SceneNode::GlyphRun(node)]);
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["ACE", "漢字"]);
            assert_poppler_bbox_close(poppler_word_bbox(&xml, "ACE"), latin_expected);
            assert_poppler_bbox_close(poppler_word_bbox(&xml, "漢字"), cjk_expected);
        }
    }

    #[test]
    fn type3_poppler_box_uses_nominal_metrics_for_rotated_rtl() {
        let mut node = font_pack_glyph_run("אב");
        node.rotation_degrees = 90;
        node.pivot_x = Fixed::from_pixels(50);
        node.pivot_y = Fixed::from_pixels(50);
        node.clip_bounds = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(200),
            height: Fixed::from_pixels(120),
        };
        let expected = nominal_source_bbox_points(&node, "אב");
        let unrotated_height = {
            let mut unrotated = node.clone();
            unrotated.rotation_degrees = 0;
            let bounds = nominal_source_bbox_points(&unrotated, "אב");
            bounds[3] - bounds[1]
        };
        assert!((expected[2] - expected[0] - unrotated_height).abs() <= 0.002);

        let document =
            document_with_nodes("type3-nominal-rotated-rtl", vec![SceneNode::GlyphRun(node)]);
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["בא"]);
            assert_poppler_bbox_close(poppler_word_bbox(&xml, "בא"), expected);
        }
    }

    #[test]
    fn nominal_type3_placement_expands_to_retain_outline_overhangs() {
        let nominal = nominal_glyph_placement(GlyphClusterMetrics {
            origin_x: Fixed::from_pixels(10),
            advance_x: Fixed::from_pixels(4),
            baseline_y: Fixed::from_pixels(20),
            ascent: Fixed::from_pixels(8),
            descent: Fixed::from_pixels(-2),
        })
        .unwrap();
        assert_eq!(nominal.origin_x, Fixed::from_pixels(10));
        assert_eq!(nominal.origin_y, Fixed::from_pixels(22));
        assert_eq!(nominal.width, Fixed::from_pixels(4));
        assert_eq!(nominal.height, Fixed::from_pixels(10));

        let expanded = placement_including_ink(
            nominal,
            Some(PdfGlyphBounds {
                min_x: Fixed::from_pixels(8),
                min_y: Fixed::from_pixels(10),
                max_x: Fixed::from_pixels(16),
                max_y: Fixed::from_pixels(25),
            }),
        )
        .unwrap();
        assert_eq!(expanded.origin_x, Fixed::from_pixels(8));
        assert_eq!(expanded.origin_y, Fixed::from_pixels(25));
        assert_eq!(expanded.width, Fixed::from_pixels(8));
        assert_eq!(expanded.height, Fixed::from_pixels(15));
    }

    #[test]
    fn native_fallback_pdf_exposes_exact_poppler_word_tokens() {
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("words");
        sheet.set_col_width(0, 40.0);
        sheet.write(0, 0, "alpha beta gamma");
        let options = PrintOptions {
            single_page_sheets: true,
            render: RenderOptions {
                gridlines: false,
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        };
        let document = build_print_document(&workbook, 0, &options).unwrap();
        let pdf = render_print_document_pdf(&document).unwrap();
        let source = String::from_utf8_lossy(&pdf);
        assert!(source.contains("/BaseFont /Helvetica"));
        assert!(source.matches("[-278] TJ").count() >= 2);
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            assert_eq!(poppler_words(&xml), ["alpha", "beta", "gamma"]);
            let alpha = poppler_word_bbox(&xml, "alpha");
            let beta = poppler_word_bbox(&xml, "beta");
            let gamma = poppler_word_bbox(&xml, "gamma");
            assert!(alpha[2] < beta[0], "{alpha:?} {beta:?}");
            assert!(beta[2] < gamma[0], "{beta:?} {gamma:?}");
        }
    }

    #[test]
    fn project_font_pack_pdf_preserves_mixed_direction_semantics_and_geometry() {
        let pack = synthetic_test_pack();
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("bidi");
        sheet.set_col_width(0, 40.0);
        sheet.write(0, 0, "left אב גד 漢字");
        let options = PrintOptions {
            single_page_sheets: true,
            render: RenderOptions {
                gridlines: false,
                default_font_family: pack.default_family().to_string(),
                font_pack: Some(pack),
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        };
        let document = build_print_document(&workbook, 0, &options).unwrap();
        let pdf = render_print_document_pdf(&document).unwrap();
        let source = String::from_utf8_lossy(&pdf);
        for word in ["בא", "דג"] {
            assert!(
                source.contains(&format!("/ActualText <FEFF{}>", utf16be_hex(word))),
                "{word:?}"
            );
        }
        assert_eq!(source.matches("/ReversedChars BMC").count(), 2);
        assert!(source.contains("/ActualText <FEFF6F225B57>"));
        if let Some(text) = poppler_text(&pdf) {
            assert!(text.contains('\u{202b}'), "{text:?}");
            assert!(text.contains('\u{202c}'), "{text:?}");
            assert!(text.contains("אב גד"), "{text:?}");
            assert!(text.contains("漢字"), "{text:?}");
        }
        if let Some(xml) = poppler_bbox_layout(&pdf) {
            // Poppler's bbox mode exposes the visual spelling and ignores the
            // standard ReversedChars marker; plain pdftotext above consumes it
            // and is the semantic extraction contract.
            assert_eq!(poppler_words(&xml), ["left", "דג", "בא", "漢字"]);
            let first_logical_rtl_word = poppler_word_bbox(&xml, "בא");
            let second_logical_rtl_word = poppler_word_bbox(&xml, "דג");
            assert!(
                second_logical_rtl_word[0] < first_logical_rtl_word[0],
                "{first_logical_rtl_word:?} {second_logical_rtl_word:?}"
            );
        }
    }

    #[test]
    fn poppler_extracts_pure_arabic_in_logical_order() {
        const LOGICAL: &str = "مرحبا بالعالم";
        for (kind, pdf) in ["outlined", "embedded"]
            .into_iter()
            .zip(font_pack_pdf_variants(LOGICAL))
        {
            let source = String::from_utf8_lossy(&pdf);
            assert_eq!(source.matches("/ReversedChars BMC").count(), 2, "{kind}");
            if let Some(text) = poppler_text(&pdf) {
                assert!(text.contains('\u{202b}'), "{kind}: {text:?}");
                assert!(text.contains('\u{202c}'), "{kind}: {text:?}");
                assert_eq!(normalized_poppler_logical_text(&text), LOGICAL, "{kind}");
            }
        }
    }

    #[test]
    fn poppler_extracts_arabic_base_mixed_text_in_logical_order() {
        const LOGICAL: &str = "مرحبا بالعالم rxls";
        for (kind, pdf) in ["outlined", "embedded"]
            .into_iter()
            .zip(font_pack_pdf_variants(LOGICAL))
        {
            if let Some(text) = poppler_text(&pdf) {
                assert!(text.contains('\u{202b}'), "{kind}: {text:?}");
                assert!(text.contains('\u{202a}'), "{kind}: {text:?}");
                assert!(text.contains('\u{202c}'), "{kind}: {text:?}");
                assert_eq!(normalized_poppler_logical_text(&text), LOGICAL, "{kind}");
            }
        }
    }

    #[test]
    fn poppler_ltr_base_mixed_text_keeps_logical_rtl_words_and_controls() {
        const SOURCE: &str = "left مرحبا بالعالم right";
        for (kind, pdf) in ["outlined", "embedded"]
            .into_iter()
            .zip(font_pack_pdf_variants(SOURCE))
        {
            if let Some(text) = poppler_text(&pdf) {
                assert!(text.contains('\u{202b}'), "{kind}: {text:?}");
                assert!(text.contains('\u{202a}'), "{kind}: {text:?}");
                assert!(text.contains('\u{202c}'), "{kind}: {text:?}");
                assert!(text.contains("مرحبا بالعالم"), "{kind}: {text:?}");
                assert!(!text.contains("ابحرم"), "{kind}: {text:?}");
                assert!(!text.contains("ملاعلاب"), "{kind}: {text:?}");
                assert!(text.contains("left"), "{kind}: {text:?}");
                assert!(text.contains("right"), "{kind}: {text:?}");
            }
        }
    }

    #[test]
    fn poppler_exactly_extracts_rtl_marks_ligatures_and_mixed_runs() {
        let cases = [
            ("arabic-ligature-marks", "لَا", "\u{202b}لَا\u{202c}"),
            ("arabic-combining", "اللّٰه", "\u{202b}اللّٰه\u{202c}"),
            ("arabic-many-marks", "مَرْحَبًا", "\u{202b}مَرْحَبًا\u{202c}"),
            ("hebrew-niqqud", "שָׁלוֹם", "\u{202b}שָׁלוֹם\u{202c}"),
            (
                "mixed-no-whitespace",
                "rxlsمرحبا42",
                "\u{202b}مرحبا\u{202a}rxls42\u{202c}\u{202c}",
            ),
            (
                "mixed-digits",
                "مرحبا 123 بالعالم",
                "\u{202b}مرحبا \u{202a} 123\u{202c}بالعالم\u{202c}",
            ),
        ];
        for (case, source_text, expected) in cases {
            let variants = font_pack_pdf_variants(source_text);
            assert_eq!(variants, font_pack_pdf_variants(source_text), "{case}");
            for (kind, pdf) in ["outlined", "embedded"].into_iter().zip(variants) {
                if let Some(text) = poppler_text(&pdf) {
                    let actual = text.trim_end_matches(['\r', '\n', '\u{000c}']);
                    assert_eq!(actual, expected, "{case}/{kind}");
                }
            }
        }
    }
}
