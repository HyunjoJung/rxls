//! Font-program subsetting for PDF embedding.
//!
//! Outlined Type 3 glyphs are always correct but make every consumer recompute
//! text geometry from an em that is not the font's, and they store one outline
//! copy per subset. When a face permits embedding, a subset of the real font
//! program is both smaller and semantically truthful, so the PDF backend
//! prefers it and keeps the Type 3 path as the fallback.
//!
//! The PDF consumer of this module is not wired yet, so every item here is
//! currently reachable only from tests. The allow below is scoped to this
//! module and must come off with the consumer.
#![allow(dead_code)]

use ttf_parser::{Face, GlyphId, Permissions};

/// Which font-program flavour a subset carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FontProgramKind {
    /// `glyf` outlines, embedded as `/FontFile2`.
    TrueType,
    /// Compact Font Format outlines, embedded as `/FontFile3`.
    Cff,
}

/// A subsetted font program plus the descriptor metrics PDF requires.
#[derive(Debug, Clone)]
pub(crate) struct EmbeddedFace {
    /// Subsetted font program bytes.
    pub(crate) program: Vec<u8>,
    /// Program flavour.
    pub(crate) kind: FontProgramKind,
    /// Original glyph ids in ascending order; the subset gid is the index.
    pub(crate) gids: Vec<u16>,
    /// Horizontal advance per subset gid, in design units.
    pub(crate) advances: Vec<u16>,
    /// Design units per em.
    pub(crate) units_per_em: u16,
    /// Typographic ascender in design units.
    pub(crate) ascender: i16,
    /// Typographic descender in design units, non-positive.
    pub(crate) descender: i16,
    /// Global bounding box in design units as `(x_min, y_min, x_max, y_max)`.
    pub(crate) bbox: (i16, i16, i16, i16),
    /// Capital height in design units.
    pub(crate) cap_height: i16,
    /// Italic angle in degrees, negative for a forward slant.
    pub(crate) italic_angle: f32,
    /// Whether the face reports fixed-pitch metrics.
    pub(crate) monospaced: bool,
}

/// Reasons a face cannot be embedded.
///
/// Every variant is non-fatal: the caller falls back to outlined Type 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmbedRejection {
    /// The face is unparseable.
    InvalidFace,
    /// The face's OS/2 `fsType` withholds installable embedding.
    EmbeddingRestricted,
    /// The face's OS/2 `fsType` withholds subsetting.
    SubsettingRestricted,
    /// The face uses an outline format the subsetter does not support.
    UnsupportedOutlines,
    /// Subsetting failed.
    SubsetFailed,
    /// No glyphs were requested.
    NoGlyphs,
}

/// Reduce requested glyph ids to the exact subset contents.
///
/// The result is sorted, deduplicated, and always contains glyph 0 so the
/// subset keeps a valid `.notdef`. Determinism of the emitted program rests on
/// this being a pure function of the requested *set*, independent of the order
/// or multiplicity the caller happened to collect them in.
fn normalize_gids(requested: &[u16]) -> Vec<u16> {
    let mut gids = Vec::with_capacity(requested.len() + 1);
    gids.push(0);
    gids.extend_from_slice(requested);
    gids.sort_unstable();
    gids.dedup();
    gids
}

/// Subset `face_bytes` down to `requested` glyph ids.
///
/// `requested` need not be sorted or deduplicated. Glyph 0 is always included
/// so the subset keeps a valid `.notdef`.
pub(crate) fn subset_face(
    face_bytes: &[u8],
    requested: &[u16],
) -> Result<EmbeddedFace, EmbedRejection> {
    if requested.is_empty() {
        return Err(EmbedRejection::NoGlyphs);
    }
    let face = Face::parse(face_bytes, 0).map_err(|_| EmbedRejection::InvalidFace)?;

    // `fsType` is a licensing statement by the font vendor. Anything short of
    // installable embedding is a refusal, and so is the absence of a statement:
    // a face with no OS/2 table has granted nothing, and guessing in the
    // vendor's place is not ours to do. Refusing only costs a fallback to the
    // Type 3 path, which always renders correctly.
    if face.permissions() != Some(Permissions::Installable) {
        return Err(EmbedRejection::EmbeddingRestricted);
    }
    if !face.is_subsetting_allowed() {
        return Err(EmbedRejection::SubsettingRestricted);
    }

    let kind = if face.tables().cff.is_some() {
        FontProgramKind::Cff
    } else if face.tables().glyf.is_some() {
        FontProgramKind::TrueType
    } else {
        return Err(EmbedRejection::UnsupportedOutlines);
    };

    let gids = normalize_gids(requested);

    // Sorted construction keeps the subset gid order a pure function of the
    // requested set, which is what makes the output byte-stable.
    let mapper = subsetter::GlyphRemapper::new_from_glyphs_sorted(&gids);
    let program = subsetter::subset(face_bytes, 0, &mapper).map_err(|_| {
        // A malformed or unsupported face must never fail a render.
        EmbedRejection::SubsetFailed
    })?;

    let advances = gids
        .iter()
        .map(|gid| face.glyph_hor_advance(GlyphId(*gid)).unwrap_or(0))
        .collect();
    let bounds = face.global_bounding_box();

    Ok(EmbeddedFace {
        program,
        kind,
        gids,
        advances,
        units_per_em: face.units_per_em(),
        ascender: face.ascender(),
        descender: face.descender(),
        bbox: (bounds.x_min, bounds.y_min, bounds.x_max, bounds.y_max),
        cap_height: face.capital_height().unwrap_or_else(|| face.ascender()),
        italic_angle: face.italic_angle(),
        monospaced: face.is_monospaced(),
    })
}

impl EmbeddedFace {
    /// Return the subset gid for an original glyph id.
    pub(crate) fn subset_gid(&self, original: u16) -> Option<u16> {
        self.gids
            .binary_search(&original)
            .ok()
            .and_then(|index| u16::try_from(index).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but structurally valid TrueType face is more trouble to build
    /// by hand than it is worth here; these cover the pure decision logic that
    /// guards the subsetter, which is where a wrong answer would ship a font
    /// the vendor withheld.
    #[test]
    fn empty_glyph_requests_are_rejected_before_parsing() {
        assert_eq!(subset_face(&[], &[]).unwrap_err(), EmbedRejection::NoGlyphs);
    }

    #[test]
    fn unparseable_faces_are_rejected_rather_than_panicking() {
        assert_eq!(
            subset_face(b"not a font at all", &[3]).unwrap_err(),
            EmbedRejection::InvalidFace
        );
    }

    #[test]
    fn normalized_glyph_ids_do_not_depend_on_caller_order_or_repetition() {
        // Determinism of the emitted program reduces to this being a pure
        // function of the requested set.
        let expected = vec![0, 2, 5, 9];
        assert_eq!(normalize_gids(&[5, 2, 9, 2]), expected);
        assert_eq!(normalize_gids(&[9, 5, 2]), expected);
        assert_eq!(normalize_gids(&[2, 5, 9, 9, 5, 2]), expected);
        assert_eq!(normalize_gids(&[0, 9, 2, 5]), expected);
        assert_eq!(normalize_gids(&[7]), vec![0, 7], "notdef is always kept");
    }

    #[test]
    fn a_face_without_a_licensing_statement_is_refused() {
        // The synthetic pack's faces carry no OS/2 table, so they grant no
        // embedding permission. Refusing costs only the Type 3 fallback.
        let pack = crate::font::synthetic_test_pack();
        let id = pack
            .resolve(crate::font::FontRequest {
                family: "Wide Sans",
                weight: 400,
                italic: false,
            })
            .id;
        let digest = pack.selected_face_identity(id).unwrap().face_sha256;
        let bytes = pack
            .face_program(digest)
            .expect("pack exposes its own face");
        assert!(
            ttf_parser::Face::parse(bytes, 0).is_ok(),
            "the face itself is valid; only its permissions are missing"
        );
        assert_eq!(
            subset_face(bytes, &[2, 5]).unwrap_err(),
            EmbedRejection::EmbeddingRestricted
        );
    }

    #[test]
    fn truncated_font_headers_are_rejected_rather_than_panicking() {
        // A plausible sfnt magic followed by nothing: the shape of input that
        // would tempt a parser into reading past the end.
        assert_eq!(
            subset_face(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x01], &[1]).unwrap_err(),
            EmbedRejection::InvalidFace
        );
    }
}
