//! Verified, host-independent font packs and deterministic text shaping.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustybuzz::{Direction, Face as BuzzFace, UnicodeBuffer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use ttf_parser::{name_id, Face, GlyphId, OutlineBuilder};
use unicode_bidi::{BidiInfo, Level};
use unicode_segmentation::UnicodeSegmentation;

const FONT_PACK_SCHEMA: &str = "rxls.render-font-pack.v1";
const SHA256_HEX_LEN: usize = 64;
const MAX_FONT_ALIASES: u64 = 128;
const OUTLINE_UNITS: f32 = 64.0;

/// Resource ceilings enforced while loading and using a verified font pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FontPackLimits {
    /// Maximum manifest bytes.
    pub max_manifest_bytes: u64,
    /// Maximum declared font faces.
    pub max_fonts: u64,
    /// Maximum bytes in one font file.
    pub max_font_bytes: u64,
    /// Maximum bytes retained across font, license, and configuration files.
    pub max_total_bytes: u64,
    /// Maximum bytes in one license or configuration file.
    pub max_auxiliary_bytes: u64,
    /// Maximum regular files in the pack tree.
    pub max_files: u64,
    /// Maximum directory depth below the manifest.
    pub max_directory_depth: u64,
    /// Maximum vector commands accepted from one glyph outline.
    pub max_outline_commands_per_glyph: u64,
}

impl Default for FontPackLimits {
    fn default() -> Self {
        Self {
            max_manifest_bytes: 4 << 20,
            max_fonts: 128,
            max_font_bytes: 32 << 20,
            max_total_bytes: 128 << 20,
            max_auxiliary_bytes: 1 << 20,
            max_files: 512,
            max_directory_depth: 16,
            max_outline_commands_per_glyph: 16_384,
        }
    }
}

/// A path-free font-pack validation or shaping failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontPackError {
    /// A bounded filesystem operation failed.
    Io {
        /// Stable operation name; host paths and OS messages are omitted.
        operation: &'static str,
    },
    /// The JSON manifest or one of its rows violated the pack contract.
    InvalidManifest {
        /// Stable validation reason.
        reason: &'static str,
    },
    /// A pack member was absolute, escaping, non-canonical, or a symlink.
    UnsafePath,
    /// The pack tree contained a regular file not declared by the manifest.
    UnexpectedFile,
    /// A declared in-memory member was absent.
    MissingMember,
    /// A declared file length did not match its manifest row.
    SizeMismatch,
    /// A declared SHA-256 identity did not match its bytes.
    DigestMismatch,
    /// A declared font could not be parsed as an OpenType face.
    InvalidFont,
    /// A configured resource ceiling was exceeded.
    LimitExceeded {
        /// Stable resource name.
        resource: &'static str,
        /// Configured inclusive limit.
        limit: u64,
        /// Required amount, when exactly known.
        actual: u64,
    },
    /// A text range did not end on UTF-8 boundaries.
    InvalidTextRange,
}

impl fmt::Display for FontPackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation } => write!(f, "font-pack I/O failed during {operation}"),
            Self::InvalidManifest { reason } => write!(f, "invalid font-pack manifest: {reason}"),
            Self::UnsafePath => f.write_str("font-pack member path is unsafe"),
            Self::UnexpectedFile => f.write_str("font pack contains an undeclared file"),
            Self::MissingMember => f.write_str("font pack is missing a declared member"),
            Self::SizeMismatch => f.write_str("font-pack file size does not match its manifest"),
            Self::DigestMismatch => f.write_str("font-pack digest does not match its manifest"),
            Self::InvalidFont => f.write_str("font-pack face is not a valid OpenType font"),
            Self::LimitExceeded {
                resource,
                limit,
                actual,
            } => write!(
                f,
                "font-pack {resource} limit exceeded: limit {limit}, required {actual}"
            ),
            Self::InvalidTextRange => f.write_str("font shaping received an invalid UTF-8 range"),
        }
    }
}

impl std::error::Error for FontPackError {}

/// An owned, verified font collection that never consults host font state.
#[derive(Clone)]
pub struct FontPack {
    inner: Arc<FontPackInner>,
}

/// One owned virtual file supplied to [`FontPack::load_memory`].
///
/// Paths use canonical forward-slash relative names such as
/// `fonts/NotoSans.ttf`; the manifest itself is passed separately and must not
/// appear in this list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontPackMember {
    /// Canonical pack-relative member name.
    pub name: String,
    /// Complete member bytes.
    pub bytes: Vec<u8>,
}

impl FontPackMember {
    /// Construct one owned in-memory pack member.
    pub fn new(name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            bytes: bytes.into(),
        }
    }
}

/// Path-independent identity of one verified font face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FontFaceIdentity<'a> {
    /// Declared family name.
    pub family: &'a str,
    /// CSS-style numeric weight.
    pub weight: u16,
    /// Whether the face is italic.
    pub italic: bool,
    /// Verified lowercase SHA-256 of the complete face bytes.
    pub sha256: &'a str,
}

impl FontPack {
    /// Load an `rxls.render-font-pack.v1` manifest with default safety limits.
    pub fn load_manifest(path: impl AsRef<Path>) -> Result<Self, FontPackError> {
        Self::load_manifest_with_limits(path, FontPackLimits::default())
    }

    /// Load and verify a font-pack manifest with explicit safety limits.
    pub fn load_manifest_with_limits(
        path: impl AsRef<Path>,
        limits: FontPackLimits,
    ) -> Result<Self, FontPackError> {
        load_pack(path.as_ref(), limits, PackLicensePolicy::OflOnly)
    }

    /// Load a caller-supplied verified pack with a bounded declared license.
    ///
    /// This is intended for fonts the caller is independently authorized to
    /// use. It applies the same byte, path, hash, OpenType identity, alias, and
    /// host-isolation checks as the OFL-only loader, but accepts a printable
    /// manifest license identifier other than `SIL-OFL-1.1`.
    pub fn load_caller_manifest(path: impl AsRef<Path>) -> Result<Self, FontPackError> {
        load_pack(
            path.as_ref(),
            FontPackLimits::default(),
            PackLicensePolicy::CallerDeclared,
        )
    }

    /// Load a verified font pack entirely from owned virtual members.
    ///
    /// This is the filesystem-free entry point for WASM and sandboxed workers.
    /// It applies the same manifest, size, SHA-256, license, OpenType parsing,
    /// fallback ordering, and shaping limits as [`Self::load_manifest`], rejects
    /// undeclared or duplicate members, and never performs host font discovery.
    pub fn load_memory(
        manifest: &[u8],
        members: impl IntoIterator<Item = FontPackMember>,
    ) -> Result<Self, FontPackError> {
        Self::load_memory_with_limits(manifest, members, FontPackLimits::default())
    }

    /// Filesystem-free loader with explicit safety ceilings.
    pub fn load_memory_with_limits(
        manifest: &[u8],
        members: impl IntoIterator<Item = FontPackMember>,
        limits: FontPackLimits,
    ) -> Result<Self, FontPackError> {
        load_memory_pack(manifest, members, limits, PackLicensePolicy::OflOnly)
    }

    /// Filesystem-free caller pack loader for independently authorized fonts.
    pub fn load_caller_memory(
        manifest: &[u8],
        members: impl IntoIterator<Item = FontPackMember>,
    ) -> Result<Self, FontPackError> {
        load_memory_pack(
            manifest,
            members,
            FontPackLimits::default(),
            PackLicensePolicy::CallerDeclared,
        )
    }

    /// Return a deterministic caller-first stack with a verified fallback pack.
    ///
    /// Exact family matches are searched in caller order before aliases are
    /// considered. If no layer contains the requested family or alias, the
    /// final fallback pack supplies the default face. All bytes remain owned by
    /// the verified input packs and no host font discovery is performed.
    pub fn with_fallback(&self, fallback: &Self) -> Result<Self, FontPackError> {
        let face_count = self
            .inner
            .faces
            .len()
            .checked_add(fallback.inner.faces.len())
            .ok_or(FontPackError::LimitExceeded {
                resource: "font_count",
                limit: u16::MAX as u64 + 1,
                actual: u64::MAX,
            })?;
        if face_count > u16::MAX as usize + 1 {
            return Err(FontPackError::LimitExceeded {
                resource: "font_count",
                limit: u16::MAX as u64 + 1,
                actual: face_count as u64,
            });
        }
        let offset = self.inner.faces.len();
        let mut faces = self.inner.faces.clone();
        faces.extend(fallback.inner.faces.iter().cloned());
        let mut layers = self.inner.layers.clone();
        for layer in &fallback.inner.layers {
            let default = usize::from(layer.default_face.0)
                .checked_add(offset)
                .and_then(|index| u16::try_from(index).ok())
                .map(FontId)
                .ok_or(FontPackError::LimitExceeded {
                    resource: "font_count",
                    limit: u16::MAX as u64 + 1,
                    actual: face_count as u64,
                })?;
            layers.push(FontLayer {
                face_start: layer.face_start + offset,
                face_end: layer.face_end + offset,
                aliases: layer.aliases.clone(),
                default_face: default,
            });
        }
        let default_face = layers.last().map(|layer| layer.default_face).ok_or(
            FontPackError::InvalidManifest {
                reason: "empty_layers",
            },
        )?;
        let identity = serde_json::json!({
            "packs": [self.pack_sha256(), fallback.pack_sha256()],
            "schema": "rxls.render-font-stack.v1"
        });
        let mut canonical = serde_json::to_string_pretty(&identity).map_err(|_| {
            FontPackError::InvalidManifest {
                reason: "identity_json",
            }
        })?;
        canonical.push('\n');
        let mut limits = self.inner.limits;
        limits.max_fonts = self
            .inner
            .limits
            .max_fonts
            .saturating_add(fallback.inner.limits.max_fonts)
            .min(u16::MAX as u64 + 1);
        limits.max_outline_commands_per_glyph = self
            .inner
            .limits
            .max_outline_commands_per_glyph
            .min(fallback.inner.limits.max_outline_commands_per_glyph);
        Ok(Self {
            inner: Arc::new(FontPackInner {
                pack_sha256: sha256_hex(canonical.as_bytes()),
                faces,
                layers,
                default_face,
                limits,
            }),
        })
    }

    /// Verified path-independent pack SHA-256 as lowercase hexadecimal.
    pub fn pack_sha256(&self) -> &str {
        &self.inner.pack_sha256
    }

    /// Number of verified font faces.
    pub fn font_count(&self) -> usize {
        self.inner.faces.len()
    }

    /// Deterministic fallback family selected from the widest regular face.
    pub fn default_family(&self) -> &str {
        &self.inner.faces[usize::from(self.inner.default_face.0)].family
    }

    /// Iterate verified per-face identities in deterministic manifest order.
    pub fn face_identities(&self) -> impl ExactSizeIterator<Item = FontFaceIdentity<'_>> {
        self.inner.faces.iter().map(|face| FontFaceIdentity {
            family: &face.family,
            weight: face.weight,
            italic: face.italic,
            sha256: &face.sha256,
        })
    }

    pub(crate) fn selected_face_identity(
        &self,
        id: FontId,
    ) -> Result<SelectedFaceIdentity<'_>, FontPackError> {
        let face = self.entry(id)?;
        Ok(SelectedFaceIdentity {
            family: &face.family,
            weight: face.weight,
            italic: face.italic,
            face_sha256: &face.sha256,
            source_pack_sha256: &face.source_pack_sha256,
        })
    }
}

impl fmt::Debug for FontPack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FontPack")
            .field("pack_sha256", &self.inner.pack_sha256)
            .field("font_count", &self.inner.faces.len())
            .field("layer_count", &self.inner.layers.len())
            .field("default_family", &self.default_family())
            .finish()
    }
}

impl PartialEq for FontPack {
    fn eq(&self, other: &Self) -> bool {
        self.inner.pack_sha256 == other.inner.pack_sha256
    }
}

impl Eq for FontPack {}

struct FontPackInner {
    pack_sha256: String,
    faces: Vec<FontEntry>,
    layers: Vec<FontLayer>,
    default_face: FontId,
    limits: FontPackLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackLicensePolicy {
    OflOnly,
    CallerDeclared,
}

#[derive(Clone)]
struct FontLayer {
    face_start: usize,
    face_end: usize,
    aliases: BTreeMap<String, String>,
    default_face: FontId,
}

#[derive(Clone)]
struct FontEntry {
    family: String,
    normalized_family: String,
    weight: u16,
    italic: bool,
    glyph_count: u16,
    bytes: Arc<[u8]>,
    sha256: String,
    source_pack_sha256: String,
}

/// Stable index into a verified pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct FontId(pub(crate) u16);

/// Requested workbook font properties.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FontRequest<'a> {
    pub(crate) family: &'a str,
    pub(crate) weight: u16,
    pub(crate) italic: bool,
}

/// One logical UTF-8 range with an independently requested workbook style.
#[derive(Debug, Clone)]
pub(crate) struct StyledFontRequest<'a> {
    pub(crate) source: Range<usize>,
    pub(crate) request: FontRequest<'a>,
    pub(crate) style_index: usize,
}

/// Direction supplied to the Unicode bidirectional algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaseDirection {
    Auto,
    LeftToRight,
    RightToLeft,
}

/// Per-call shaping ceilings and direction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShapeOptions {
    pub(crate) direction: BaseDirection,
    pub(crate) max_glyphs: usize,
    pub(crate) max_runs: usize,
}

/// Selected face metrics in font units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FontMetrics {
    pub(crate) units_per_em: u16,
    pub(crate) ascent: i16,
    pub(crate) descent: i16,
    pub(crate) line_gap: i16,
    pub(crate) underline_position: i16,
    pub(crate) underline_thickness: i16,
    pub(crate) strikeout_position: i16,
    pub(crate) strikeout_thickness: i16,
}

/// One shaped glyph and its font-unit positioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShapedGlyph {
    pub(crate) glyph_id: u16,
    pub(crate) cluster: u32,
    pub(crate) x_advance: i32,
    pub(crate) y_advance: i32,
    pub(crate) x_offset: i32,
    pub(crate) y_offset: i32,
}

/// One visually ordered, single-face shaped run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShapedRun {
    pub(crate) font_id: FontId,
    pub(crate) direction: BaseDirection,
    pub(crate) source: Range<usize>,
    pub(crate) style_index: usize,
    pub(crate) glyphs: Vec<ShapedGlyph>,
}

/// Fully shaped single-line text in left-to-right paint order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShapedText {
    pub(crate) runs: Vec<ShapedRun>,
    pub(crate) glyph_count: usize,
    pub(crate) missing_glyphs: usize,
    pub(crate) requested_family_matched: bool,
    pub(crate) selected_faces: Vec<ShapedFaceUse>,
    pub(crate) base_direction: BaseDirection,
}

/// One verified face selected by a shaping pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ShapedFaceUse {
    pub(crate) font_id: FontId,
    pub(crate) substituted: bool,
}

/// Path-free identity used by render reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectedFaceIdentity<'a> {
    pub(crate) family: &'a str,
    pub(crate) weight: u16,
    pub(crate) italic: bool,
    pub(crate) face_sha256: &'a str,
    pub(crate) source_pack_sha256: &'a str,
}

/// Quantized glyph outline coordinate scale (1/64 font unit).
pub(crate) const FONT_OUTLINE_UNITS: i64 = 64;

/// One glyph-outline command quantized to 1/64 font unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FontOutlineCommand {
    MoveTo(i32, i32),
    LineTo(i32, i32),
    QuadraticTo(i32, i32, i32, i32),
    CubicTo(i32, i32, i32, i32, i32, i32),
    Close,
}

fn validate_styled_requests(
    text: &str,
    styles: &[StyledFontRequest<'_>],
) -> Result<(), FontPackError> {
    if styles.is_empty() {
        return if text.is_empty() {
            Ok(())
        } else {
            Err(FontPackError::InvalidTextRange)
        };
    }
    let mut expected_start = 0_usize;
    for style in styles {
        if style.source.start != expected_start
            || style.source.start > style.source.end
            || style.source.end > text.len()
            || !text.is_char_boundary(style.source.start)
            || !text.is_char_boundary(style.source.end)
        {
            return Err(FontPackError::InvalidTextRange);
        }
        expected_start = style.source.end;
    }
    if expected_start != text.len() {
        return Err(FontPackError::InvalidTextRange);
    }
    Ok(())
}

fn load_pack(
    path: &Path,
    limits: FontPackLimits,
    license_policy: PackLicensePolicy,
) -> Result<FontPack, FontPackError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| FontPackError::Io {
        operation: "manifest_metadata",
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(FontPackError::InvalidManifest {
            reason: "manifest_file",
        });
    }
    enforce_limit("manifest_bytes", limits.max_manifest_bytes, metadata.len())?;
    let manifest_bytes = read_bounded(path, limits.max_manifest_bytes, "manifest_read")?;
    let manifest: Value =
        serde_json::from_slice(&manifest_bytes).map_err(|_| FontPackError::InvalidManifest {
            reason: "manifest_json",
        })?;
    let object = manifest.as_object().ok_or(FontPackError::InvalidManifest {
        reason: "manifest_object",
    })?;
    if string_field(object, "schema")? != FONT_PACK_SCHEMA {
        return Err(FontPackError::InvalidManifest {
            reason: "manifest_schema",
        });
    }
    validate_pack_license(object, license_policy)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let root = parent.canonicalize().map_err(|_| FontPackError::Io {
        operation: "pack_root",
    })?;
    let canonical_manifest = path.canonicalize().map_err(|_| FontPackError::Io {
        operation: "manifest_canonicalize",
    })?;
    if canonical_manifest != root.join("manifest.json") {
        return Err(FontPackError::InvalidManifest {
            reason: "manifest_name",
        });
    }

    let font_rows = array_field(object, "fonts")?;
    let license_rows = array_field(object, "licenses")?;
    if font_rows.is_empty() || license_rows.is_empty() {
        return Err(FontPackError::InvalidManifest {
            reason: "empty_rows",
        });
    }
    enforce_limit("font_count", limits.max_fonts, font_rows.len() as u64)?;
    enforce_limit("license_count", limits.max_fonts, license_rows.len() as u64)?;

    let mut expected =
        BTreeSet::<String>::from(["manifest.json".to_string(), "fonts.conf".to_string()]);
    let mut total_bytes = 0_u64;
    let mut faces = Vec::with_capacity(font_rows.len());
    for row in font_rows {
        let row = row
            .as_object()
            .ok_or(FontPackError::InvalidManifest { reason: "font_row" })?;
        let output = string_field(row, "output")?;
        if !expected.insert(output.to_string()) {
            return Err(FontPackError::InvalidManifest {
                reason: "duplicate_output",
            });
        }
        let declared = positive_u64_field(row, "bytes")?;
        enforce_limit("font_bytes", limits.max_font_bytes, declared)?;
        let digest = digest_field(row, "sha256")?;
        let family = string_field(row, "family")?;
        if family.is_empty() || family.len() > 512 || family.chars().any(char::is_control) {
            return Err(FontPackError::InvalidManifest {
                reason: "font_family",
            });
        }
        let style = string_field(row, "style")?;
        let italic = match style {
            "normal" => false,
            "italic" => true,
            _ => {
                return Err(FontPackError::InvalidManifest {
                    reason: "font_style",
                });
            }
        };
        let weight = positive_u64_field(row, "weight")?;
        if weight > 1_000 {
            return Err(FontPackError::InvalidManifest {
                reason: "font_weight",
            });
        }
        let member = safe_member(&root, output)?;
        let bytes = verify_member(
            &member,
            declared,
            digest,
            limits.max_font_bytes,
            "font_read",
        )?;
        let face = Face::parse(&bytes, 0).map_err(|_| FontPackError::InvalidFont)?;
        validate_face_declaration(&face, family, weight as u16, italic)?;
        let glyph_count = face.number_of_glyphs();
        if BuzzFace::from_slice(&bytes, 0).is_none() {
            return Err(FontPackError::InvalidFont);
        }
        total_bytes = total_bytes
            .checked_add(declared)
            .ok_or(FontPackError::LimitExceeded {
                resource: "total_bytes",
                limit: limits.max_total_bytes,
                actual: u64::MAX,
            })?;
        enforce_limit("total_bytes", limits.max_total_bytes, total_bytes)?;
        faces.push(FontEntry {
            family: family.to_string(),
            normalized_family: normalize_family(family),
            weight: weight as u16,
            italic,
            glyph_count,
            bytes: Arc::from(bytes),
            sha256: digest.to_string(),
            source_pack_sha256: String::new(),
        });
    }

    let alias_rows = optional_array_field(object, "aliases")?;
    let aliases = parse_aliases(alias_rows, &faces)?;

    for row in license_rows {
        let row = row.as_object().ok_or(FontPackError::InvalidManifest {
            reason: "license_row",
        })?;
        let output = string_field(row, "output")?;
        if !expected.insert(output.to_string()) {
            return Err(FontPackError::InvalidManifest {
                reason: "duplicate_output",
            });
        }
        let declared = positive_u64_field(row, "bytes")?;
        enforce_limit("auxiliary_bytes", limits.max_auxiliary_bytes, declared)?;
        let digest = digest_field(row, "sha256")?;
        let member = safe_member(&root, output)?;
        let _ = verify_member(
            &member,
            declared,
            digest,
            limits.max_auxiliary_bytes,
            "license_read",
        )?;
        total_bytes = total_bytes
            .checked_add(declared)
            .ok_or(FontPackError::LimitExceeded {
                resource: "total_bytes",
                limit: limits.max_total_bytes,
                actual: u64::MAX,
            })?;
        enforce_limit("total_bytes", limits.max_total_bytes, total_bytes)?;
    }

    let config_path = safe_member(&root, "fonts.conf")?;
    let config_metadata = fs::metadata(&config_path).map_err(|_| FontPackError::Io {
        operation: "configuration_metadata",
    })?;
    enforce_limit(
        "auxiliary_bytes",
        limits.max_auxiliary_bytes,
        config_metadata.len(),
    )?;
    let config_digest = digest_field(object, "fonts_conf_sha256")?;
    let _ = verify_member(
        &config_path,
        config_metadata.len(),
        config_digest,
        limits.max_auxiliary_bytes,
        "configuration_read",
    )?;
    total_bytes =
        total_bytes
            .checked_add(config_metadata.len())
            .ok_or(FontPackError::LimitExceeded {
                resource: "total_bytes",
                limit: limits.max_total_bytes,
                actual: u64::MAX,
            })?;
    enforce_limit("total_bytes", limits.max_total_bytes, total_bytes)?;
    if positive_u64_field(object, "total_bytes")? != total_bytes {
        return Err(FontPackError::SizeMismatch);
    }

    let actual = collect_pack_files(&root, limits)?;
    if actual != expected {
        return Err(FontPackError::UnexpectedFile);
    }

    let declared_pack_sha = digest_field(object, "pack_sha256")?;
    let mut identity = Map::new();
    if let Some(rows) = alias_rows {
        identity.insert("aliases".to_string(), Value::Array(rows.clone()));
    }
    identity.insert("fonts".to_string(), Value::Array(font_rows.clone()));
    identity.insert(
        "fonts_conf_sha256".to_string(),
        Value::String(config_digest.to_string()),
    );
    identity.insert("licenses".to_string(), Value::Array(license_rows.clone()));
    let mut canonical = serde_json::to_string_pretty(&Value::Object(identity)).map_err(|_| {
        FontPackError::InvalidManifest {
            reason: "identity_json",
        }
    })?;
    canonical.push('\n');
    if sha256_hex(canonical.as_bytes()) != declared_pack_sha {
        return Err(FontPackError::DigestMismatch);
    }

    let default_face = select_default_face(&faces)?;
    for face in &mut faces {
        face.source_pack_sha256 = declared_pack_sha.to_string();
    }
    let face_count = faces.len();
    Ok(FontPack {
        inner: Arc::new(FontPackInner {
            pack_sha256: declared_pack_sha.to_string(),
            faces,
            layers: vec![FontLayer {
                face_start: 0,
                face_end: face_count,
                aliases,
                default_face,
            }],
            default_face,
            limits,
        }),
    })
}

fn load_memory_pack(
    manifest_bytes: &[u8],
    members: impl IntoIterator<Item = FontPackMember>,
    limits: FontPackLimits,
    license_policy: PackLicensePolicy,
) -> Result<FontPack, FontPackError> {
    enforce_limit(
        "manifest_bytes",
        limits.max_manifest_bytes,
        manifest_bytes.len() as u64,
    )?;
    let manifest: Value =
        serde_json::from_slice(manifest_bytes).map_err(|_| FontPackError::InvalidManifest {
            reason: "manifest_json",
        })?;
    let object = manifest.as_object().ok_or(FontPackError::InvalidManifest {
        reason: "manifest_object",
    })?;
    if string_field(object, "schema")? != FONT_PACK_SCHEMA {
        return Err(FontPackError::InvalidManifest {
            reason: "manifest_schema",
        });
    }
    validate_pack_license(object, license_policy)?;

    let mut files = BTreeMap::<String, Vec<u8>>::new();
    let mut actual = BTreeSet::from(["manifest.json".to_string()]);
    let mut supplied_bytes = 0_u64;
    for member in members {
        validate_virtual_member_name(&member.name, limits)?;
        if member.name == "manifest.json"
            || !actual.insert(member.name.clone())
            || files.insert(member.name, member.bytes).is_some()
        {
            return Err(FontPackError::InvalidManifest {
                reason: "duplicate_member",
            });
        }
        enforce_limit("file_count", limits.max_files, actual.len() as u64)?;
    }
    for bytes in files.values() {
        supplied_bytes =
            supplied_bytes
                .checked_add(bytes.len() as u64)
                .ok_or(FontPackError::LimitExceeded {
                    resource: "total_bytes",
                    limit: limits.max_total_bytes,
                    actual: u64::MAX,
                })?;
        enforce_limit("total_bytes", limits.max_total_bytes, supplied_bytes)?;
    }

    let font_rows = array_field(object, "fonts")?;
    let license_rows = array_field(object, "licenses")?;
    if font_rows.is_empty() || license_rows.is_empty() {
        return Err(FontPackError::InvalidManifest {
            reason: "empty_rows",
        });
    }
    enforce_limit("font_count", limits.max_fonts, font_rows.len() as u64)?;
    enforce_limit("license_count", limits.max_fonts, license_rows.len() as u64)?;
    let mut expected =
        BTreeSet::<String>::from(["manifest.json".to_string(), "fonts.conf".to_string()]);
    let mut total_bytes = 0_u64;
    let mut faces = Vec::with_capacity(font_rows.len());
    for row in font_rows {
        let row = row
            .as_object()
            .ok_or(FontPackError::InvalidManifest { reason: "font_row" })?;
        let output = string_field(row, "output")?;
        validate_virtual_member_name(output, limits)?;
        if !expected.insert(output.to_string()) {
            return Err(FontPackError::InvalidManifest {
                reason: "duplicate_output",
            });
        }
        let declared = positive_u64_field(row, "bytes")?;
        enforce_limit("font_bytes", limits.max_font_bytes, declared)?;
        let digest = digest_field(row, "sha256")?;
        let family = string_field(row, "family")?;
        if family.is_empty() || family.len() > 512 || family.chars().any(char::is_control) {
            return Err(FontPackError::InvalidManifest {
                reason: "font_family",
            });
        }
        let italic = match string_field(row, "style")? {
            "normal" => false,
            "italic" => true,
            _ => {
                return Err(FontPackError::InvalidManifest {
                    reason: "font_style",
                });
            }
        };
        let weight = positive_u64_field(row, "weight")?;
        if weight > 1_000 {
            return Err(FontPackError::InvalidManifest {
                reason: "font_weight",
            });
        }
        let bytes = take_memory_member(&mut files, output, declared, digest)?;
        let face = Face::parse(&bytes, 0).map_err(|_| FontPackError::InvalidFont)?;
        validate_face_declaration(&face, family, weight as u16, italic)?;
        let glyph_count = face.number_of_glyphs();
        if BuzzFace::from_slice(&bytes, 0).is_none() {
            return Err(FontPackError::InvalidFont);
        }
        total_bytes = total_bytes
            .checked_add(declared)
            .ok_or(FontPackError::LimitExceeded {
                resource: "total_bytes",
                limit: limits.max_total_bytes,
                actual: u64::MAX,
            })?;
        enforce_limit("total_bytes", limits.max_total_bytes, total_bytes)?;
        faces.push(FontEntry {
            family: family.to_string(),
            normalized_family: normalize_family(family),
            weight: weight as u16,
            italic,
            glyph_count,
            bytes: Arc::from(bytes),
            sha256: digest.to_string(),
            source_pack_sha256: String::new(),
        });
    }

    let alias_rows = optional_array_field(object, "aliases")?;
    let aliases = parse_aliases(alias_rows, &faces)?;

    for row in license_rows {
        let row = row.as_object().ok_or(FontPackError::InvalidManifest {
            reason: "license_row",
        })?;
        let output = string_field(row, "output")?;
        validate_virtual_member_name(output, limits)?;
        if !expected.insert(output.to_string()) {
            return Err(FontPackError::InvalidManifest {
                reason: "duplicate_output",
            });
        }
        let declared = positive_u64_field(row, "bytes")?;
        enforce_limit("auxiliary_bytes", limits.max_auxiliary_bytes, declared)?;
        let digest = digest_field(row, "sha256")?;
        let _ = take_memory_member(&mut files, output, declared, digest)?;
        total_bytes = total_bytes
            .checked_add(declared)
            .ok_or(FontPackError::LimitExceeded {
                resource: "total_bytes",
                limit: limits.max_total_bytes,
                actual: u64::MAX,
            })?;
        enforce_limit("total_bytes", limits.max_total_bytes, total_bytes)?;
    }

    let config_digest = digest_field(object, "fonts_conf_sha256")?;
    let config_declared = files
        .get("fonts.conf")
        .map(|bytes| bytes.len() as u64)
        .ok_or(FontPackError::MissingMember)?;
    enforce_limit(
        "auxiliary_bytes",
        limits.max_auxiliary_bytes,
        config_declared,
    )?;
    let _ = take_memory_member(&mut files, "fonts.conf", config_declared, config_digest)?;
    total_bytes = total_bytes
        .checked_add(config_declared)
        .ok_or(FontPackError::LimitExceeded {
            resource: "total_bytes",
            limit: limits.max_total_bytes,
            actual: u64::MAX,
        })?;
    enforce_limit("total_bytes", limits.max_total_bytes, total_bytes)?;
    if positive_u64_field(object, "total_bytes")? != total_bytes {
        return Err(FontPackError::SizeMismatch);
    }
    if actual != expected || !files.is_empty() {
        return Err(FontPackError::UnexpectedFile);
    }

    let declared_pack_sha = digest_field(object, "pack_sha256")?;
    let mut identity = Map::new();
    if let Some(rows) = alias_rows {
        identity.insert("aliases".to_string(), Value::Array(rows.clone()));
    }
    identity.insert("fonts".to_string(), Value::Array(font_rows.clone()));
    identity.insert(
        "fonts_conf_sha256".to_string(),
        Value::String(config_digest.to_string()),
    );
    identity.insert("licenses".to_string(), Value::Array(license_rows.clone()));
    let mut canonical = serde_json::to_string_pretty(&Value::Object(identity)).map_err(|_| {
        FontPackError::InvalidManifest {
            reason: "identity_json",
        }
    })?;
    canonical.push('\n');
    if sha256_hex(canonical.as_bytes()) != declared_pack_sha {
        return Err(FontPackError::DigestMismatch);
    }
    let default_face = select_default_face(&faces)?;
    for face in &mut faces {
        face.source_pack_sha256 = declared_pack_sha.to_string();
    }
    let face_count = faces.len();
    Ok(FontPack {
        inner: Arc::new(FontPackInner {
            pack_sha256: declared_pack_sha.to_string(),
            faces,
            layers: vec![FontLayer {
                face_start: 0,
                face_end: face_count,
                aliases,
                default_face,
            }],
            default_face,
            limits,
        }),
    })
}

fn validate_virtual_member_name(name: &str, limits: FontPackLimits) -> Result<(), FontPackError> {
    if name.is_empty()
        || name.contains('\0')
        || name.contains('\\')
        || name.starts_with('/')
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(FontPackError::UnsafePath);
    }
    let depth = name.split('/').count().saturating_sub(1) as u64;
    enforce_limit("directory_depth", limits.max_directory_depth, depth)
}

fn take_memory_member(
    files: &mut BTreeMap<String, Vec<u8>>,
    name: &str,
    declared: u64,
    digest: &str,
) -> Result<Vec<u8>, FontPackError> {
    let bytes = files.remove(name).ok_or(FontPackError::MissingMember)?;
    if bytes.len() as u64 != declared {
        return Err(FontPackError::SizeMismatch);
    }
    if sha256_hex(&bytes) != digest {
        return Err(FontPackError::DigestMismatch);
    }
    Ok(bytes)
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    key: &'static str,
) -> Result<&'a str, FontPackError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or(FontPackError::InvalidManifest { reason: key })
}

fn validate_pack_license(
    object: &Map<String, Value>,
    policy: PackLicensePolicy,
) -> Result<(), FontPackError> {
    let license = string_field(object, "license")?;
    let valid_declared = !license.is_empty()
        && license.len() <= 128
        && license == license.trim()
        && license.is_ascii()
        && license.bytes().all(|byte| !byte.is_ascii_control());
    if !valid_declared || (policy == PackLicensePolicy::OflOnly && license != "SIL-OFL-1.1") {
        return Err(FontPackError::InvalidManifest {
            reason: "pack_license",
        });
    }
    Ok(())
}

fn array_field<'a>(
    object: &'a Map<String, Value>,
    key: &'static str,
) -> Result<&'a Vec<Value>, FontPackError> {
    object
        .get(key)
        .and_then(Value::as_array)
        .ok_or(FontPackError::InvalidManifest { reason: key })
}

fn optional_array_field<'a>(
    object: &'a Map<String, Value>,
    key: &'static str,
) -> Result<Option<&'a Vec<Value>>, FontPackError> {
    match object.get(key) {
        Some(value) => value
            .as_array()
            .map(Some)
            .ok_or(FontPackError::InvalidManifest { reason: key }),
        None => Ok(None),
    }
}

fn valid_alias_family(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.trim() == value
        && value.is_ascii()
        && value.bytes().all(|byte| !byte.is_ascii_control())
}

fn parse_aliases(
    rows: Option<&Vec<Value>>,
    faces: &[FontEntry],
) -> Result<BTreeMap<String, String>, FontPackError> {
    let Some(rows) = rows else {
        return Ok(BTreeMap::new());
    };
    enforce_limit("alias_count", MAX_FONT_ALIASES, rows.len() as u64)?;
    let available = faces
        .iter()
        .map(|face| face.normalized_family.as_str())
        .collect::<BTreeSet<_>>();
    let expected_keys = BTreeSet::from(["family", "substitute"]);
    let mut aliases = BTreeMap::new();
    let mut previous: Option<String> = None;
    for row in rows {
        let row = row.as_object().ok_or(FontPackError::InvalidManifest {
            reason: "alias_row",
        })?;
        let keys = row.keys().map(String::as_str).collect::<BTreeSet<_>>();
        if keys != expected_keys {
            return Err(FontPackError::InvalidManifest {
                reason: "alias_row",
            });
        }
        let family = string_field(row, "family")?;
        let substitute = string_field(row, "substitute")?;
        if !valid_alias_family(family) || !valid_alias_family(substitute) {
            return Err(FontPackError::InvalidManifest {
                reason: "alias_family",
            });
        }
        let normalized = normalize_family(family);
        let normalized_substitute = normalize_family(substitute);
        if !available.contains(normalized_substitute.as_str()) {
            return Err(FontPackError::InvalidManifest {
                reason: "alias_substitute",
            });
        }
        if previous.as_ref().is_some_and(|value| value >= &normalized)
            || aliases
                .insert(normalized.clone(), normalized_substitute)
                .is_some()
        {
            return Err(FontPackError::InvalidManifest {
                reason: "alias_order",
            });
        }
        previous = Some(normalized);
    }
    Ok(aliases)
}

fn positive_u64_field(
    object: &Map<String, Value>,
    key: &'static str,
) -> Result<u64, FontPackError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value != 0)
        .ok_or(FontPackError::InvalidManifest { reason: key })
}

fn digest_field<'a>(
    object: &'a Map<String, Value>,
    key: &'static str,
) -> Result<&'a str, FontPackError> {
    let value = string_field(object, key)?;
    if value.len() != SHA256_HEX_LEN
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(FontPackError::InvalidManifest { reason: key });
    }
    Ok(value)
}

fn enforce_limit(resource: &'static str, limit: u64, actual: u64) -> Result<(), FontPackError> {
    if actual > limit {
        Err(FontPackError::LimitExceeded {
            resource,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

fn safe_member(root: &Path, relative: &str) -> Result<PathBuf, FontPackError> {
    if relative.is_empty()
        || relative.contains('\0')
        || relative.contains('\\')
        || relative.starts_with('/')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(FontPackError::UnsafePath);
    }
    let relative_path = Path::new(relative);
    if relative_path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(FontPackError::UnsafePath);
    }
    let candidate = root.join(relative_path);
    let metadata = fs::symlink_metadata(&candidate).map_err(|_| FontPackError::Io {
        operation: "member_metadata",
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(FontPackError::UnsafePath);
    }
    let canonical = candidate.canonicalize().map_err(|_| FontPackError::Io {
        operation: "member_canonicalize",
    })?;
    if !canonical.starts_with(root) {
        return Err(FontPackError::UnsafePath);
    }
    Ok(canonical)
}

fn verify_member(
    path: &Path,
    declared: u64,
    digest: &str,
    read_limit: u64,
    operation: &'static str,
) -> Result<Vec<u8>, FontPackError> {
    let metadata = fs::metadata(path).map_err(|_| FontPackError::Io {
        operation: "member_metadata",
    })?;
    if metadata.len() != declared {
        return Err(FontPackError::SizeMismatch);
    }
    let bytes = read_bounded(path, read_limit, operation)?;
    if bytes.len() as u64 != declared {
        return Err(FontPackError::SizeMismatch);
    }
    if sha256_hex(&bytes) != digest {
        return Err(FontPackError::DigestMismatch);
    }
    Ok(bytes)
}

fn read_bounded(
    path: &Path,
    limit: u64,
    operation: &'static str,
) -> Result<Vec<u8>, FontPackError> {
    let file = File::open(path).map_err(|_| FontPackError::Io { operation })?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| FontPackError::Io { operation })?;
    enforce_limit(operation, limit, bytes.len() as u64)?;
    Ok(bytes)
}

fn collect_pack_files(
    root: &Path,
    limits: FontPackLimits,
) -> Result<BTreeSet<String>, FontPackError> {
    let mut files = BTreeSet::new();
    let mut pending = vec![(root.to_path_buf(), 0_u64)];
    let mut entries = 0_u64;
    while let Some((directory, depth)) = pending.pop() {
        enforce_limit("directory_depth", limits.max_directory_depth, depth)?;
        let iterator = fs::read_dir(&directory).map_err(|_| FontPackError::Io {
            operation: "pack_tree",
        })?;
        for entry in iterator {
            let entry = entry.map_err(|_| FontPackError::Io {
                operation: "pack_tree",
            })?;
            entries = entries.saturating_add(1);
            enforce_limit("file_count", limits.max_files, entries)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|_| FontPackError::Io {
                operation: "pack_tree_metadata",
            })?;
            if metadata.file_type().is_symlink() {
                return Err(FontPackError::UnsafePath);
            }
            if metadata.is_dir() {
                pending.push((entry.path(), depth.saturating_add(1)));
            } else if metadata.is_file() {
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|_| FontPackError::UnsafePath)?
                    .components()
                    .map(|component| component.as_os_str().to_str())
                    .collect::<Option<Vec<_>>>()
                    .ok_or(FontPackError::UnsafePath)?
                    .join("/");
                files.insert(relative);
            } else {
                return Err(FontPackError::UnsafePath);
            }
        }
    }
    Ok(files)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn normalize_family(value: &str) -> String {
    value.trim().to_lowercase()
}

fn validate_face_declaration(
    face: &Face<'_>,
    family: &str,
    weight: u16,
    italic: bool,
) -> Result<(), FontPackError> {
    let names = face.names();
    let mut declared_names = names
        .into_iter()
        .filter(|name| name.name_id == name_id::TYPOGRAPHIC_FAMILY)
        .filter_map(|name| name.to_string())
        .collect::<Vec<_>>();
    if declared_names.is_empty() {
        declared_names = face
            .names()
            .into_iter()
            .filter(|name| name.name_id == name_id::FAMILY)
            .filter_map(|name| name.to_string())
            .collect();
    }
    let normalized = normalize_family(family);
    if declared_names.is_empty()
        || !declared_names
            .iter()
            .any(|name| normalize_family(name) == normalized)
    {
        return Err(FontPackError::InvalidManifest {
            reason: "font_family_identity",
        });
    }
    if face.weight().to_number() != weight {
        return Err(FontPackError::InvalidManifest {
            reason: "font_weight_identity",
        });
    }
    if face.is_italic() != italic {
        return Err(FontPackError::InvalidManifest {
            reason: "font_style_identity",
        });
    }
    Ok(())
}

fn select_default_face(faces: &[FontEntry]) -> Result<FontId, FontPackError> {
    let (index, _) = faces
        .iter()
        .enumerate()
        .max_by_key(|(index, face)| {
            (
                face.glyph_count,
                u8::from(!face.italic),
                u16::MAX - face.weight.abs_diff(400),
                usize::MAX - *index,
            )
        })
        .ok_or(FontPackError::InvalidManifest {
            reason: "empty_faces",
        })?;
    let index = u16::try_from(index).map_err(|_| FontPackError::LimitExceeded {
        resource: "font_count",
        limit: u16::MAX as u64,
        actual: index as u64,
    })?;
    Ok(FontId(index))
}

/// Result of deterministic family/style resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FontResolution {
    pub(crate) id: FontId,
    pub(crate) exact_family: bool,
    pub(crate) exact_style: bool,
}

impl FontPack {
    pub(crate) fn resolve(&self, request: FontRequest<'_>) -> FontResolution {
        let requested = normalize_family(request.family);
        for layer in &self.inner.layers {
            if let Some(id) = self.best_face_in_family_range(
                &requested,
                request,
                layer.face_start..layer.face_end,
            ) {
                let face = &self.inner.faces[usize::from(id.0)];
                return FontResolution {
                    id,
                    exact_family: true,
                    exact_style: face.italic == request.italic && face.weight == request.weight,
                };
            }
        }
        for layer in &self.inner.layers {
            let Some(substitute) = layer.aliases.get(&requested) else {
                continue;
            };
            let Some(id) = self.best_face_in_family_range(
                substitute,
                request,
                layer.face_start..layer.face_end,
            ) else {
                continue;
            };
            let face = &self.inner.faces[usize::from(id.0)];
            return FontResolution {
                id,
                exact_family: false,
                exact_style: face.italic == request.italic && face.weight == request.weight,
            };
        }
        let fallback_family = self.inner.faces[usize::from(self.inner.default_face.0)]
            .normalized_family
            .clone();
        let id = self
            .best_face_in_family(&fallback_family, request)
            .unwrap_or(self.inner.default_face);
        let face = &self.inner.faces[usize::from(id.0)];
        FontResolution {
            id,
            exact_family: false,
            exact_style: face.italic == request.italic && face.weight == request.weight,
        }
    }

    pub(crate) fn is_italic(&self, id: FontId) -> Result<bool, FontPackError> {
        Ok(self.entry(id)?.italic)
    }

    pub(crate) fn weight(&self, id: FontId) -> Result<u16, FontPackError> {
        Ok(self.entry(id)?.weight)
    }

    pub(crate) fn metrics(&self, id: FontId) -> Result<FontMetrics, FontPackError> {
        let face = self.face(id)?;
        let units_per_em = face.units_per_em();
        let underline = face.underline_metrics();
        let strikeout = face.strikeout_metrics();
        Ok(FontMetrics {
            units_per_em,
            ascent: face.ascender(),
            descent: face.descender(),
            line_gap: face.line_gap(),
            underline_position: underline.map_or(-(units_per_em as i16) / 10, |v| v.position),
            underline_thickness: underline
                .map_or((units_per_em / 20).max(1) as i16, |v| v.thickness.max(1)),
            strikeout_position: strikeout.map_or((units_per_em as i16) * 3 / 10, |v| v.position),
            strikeout_thickness: strikeout
                .map_or((units_per_em / 20).max(1) as i16, |v| v.thickness.max(1)),
        })
    }

    pub(crate) fn max_digit_width(
        &self,
        request: FontRequest<'_>,
    ) -> Result<(FontId, u16), FontPackError> {
        let id = self.resolve(request).id;
        let face = self.face(id)?;
        let mut maximum = 0_u16;
        for digit in '0'..='9' {
            let glyph = face.glyph_index(digit).unwrap_or(GlyphId(0));
            maximum = maximum.max(face.glyph_hor_advance(glyph).unwrap_or(0));
        }
        if maximum == 0 {
            return Err(FontPackError::InvalidFont);
        }
        Ok((id, maximum))
    }

    pub(crate) fn outline(
        &self,
        id: FontId,
        glyph: u16,
        max_commands: u64,
    ) -> Result<Vec<FontOutlineCommand>, FontPackError> {
        let limit = max_commands.min(self.inner.limits.max_outline_commands_per_glyph);
        let mut builder = QuantizedOutline::new(limit);
        let _ = self.face(id)?.outline_glyph(GlyphId(glyph), &mut builder);
        if builder.invalid {
            return Err(FontPackError::InvalidFont);
        }
        if builder.exceeded {
            return Err(FontPackError::LimitExceeded {
                resource: "outline_commands",
                limit,
                actual: limit.saturating_add(1),
            });
        }
        Ok(builder.commands)
    }

    pub(crate) fn shape(
        &self,
        text: &str,
        request: FontRequest<'_>,
        options: ShapeOptions,
    ) -> Result<ShapedText, FontPackError> {
        self.shape_styled(
            text,
            &[StyledFontRequest {
                source: 0..text.len(),
                request,
                style_index: 0,
            }],
            options,
        )
    }

    /// Shape a fully covered styled string under one global bidirectional
    /// paragraph analysis. Style boundaries are respected without resetting
    /// neutral resolution or visual ordering at each rich-text run.
    pub(crate) fn shape_styled(
        &self,
        text: &str,
        styles: &[StyledFontRequest<'_>],
        options: ShapeOptions,
    ) -> Result<ShapedText, FontPackError> {
        let scalar_count = text.chars().count();
        if scalar_count > options.max_glyphs {
            return Err(FontPackError::LimitExceeded {
                resource: "shape_glyphs",
                limit: options.max_glyphs as u64,
                actual: scalar_count as u64,
            });
        }
        validate_styled_requests(text, styles)?;
        let mut requested_family_matched = styles
            .iter()
            .all(|style| self.resolve(style.request).exact_family);
        if text.is_empty() {
            return Ok(ShapedText {
                runs: Vec::new(),
                glyph_count: 0,
                missing_glyphs: 0,
                requested_family_matched,
                selected_faces: Vec::new(),
                base_direction: options.direction,
            });
        }
        let default_level = match options.direction {
            BaseDirection::Auto => None,
            BaseDirection::LeftToRight => Some(Level::ltr()),
            BaseDirection::RightToLeft => Some(Level::rtl()),
        };
        let bidi = BidiInfo::new(text, default_level);
        let paragraph = bidi
            .paragraphs
            .first()
            .ok_or(FontPackError::InvalidTextRange)?;
        let base_direction = if paragraph.level.is_rtl() {
            BaseDirection::RightToLeft
        } else {
            BaseDirection::LeftToRight
        };
        let (_, visual_runs) = bidi.visual_runs(paragraph, 0..text.len());
        let mut runs = Vec::new();
        let mut glyph_count = 0_usize;
        let mut missing_glyphs = 0_usize;
        let mut selected_faces = BTreeMap::<FontId, bool>::new();
        for visual in visual_runs {
            let level = bidi
                .levels
                .get(visual.start)
                .copied()
                .ok_or(FontPackError::InvalidTextRange)?;
            let direction = if level.is_rtl() {
                BaseDirection::RightToLeft
            } else {
                BaseDirection::LeftToRight
            };
            let mut styled_ranges = styles
                .iter()
                .filter_map(|style| {
                    let start = style.source.start.max(visual.start);
                    let end = style.source.end.min(visual.end);
                    (start < end).then_some((start..end, style))
                })
                .collect::<Vec<_>>();
            if direction == BaseDirection::RightToLeft {
                styled_ranges.reverse();
            }
            for (styled_source, style) in styled_ranges {
                let resolution = self.resolve(style.request);
                let mut face_ranges =
                    self.face_ranges(text, styled_source, resolution.id, style.request)?;
                if direction == BaseDirection::RightToLeft {
                    face_ranges.reverse();
                }
                for (source, font_id) in face_ranges {
                    let substituted = self.entry(font_id)?.normalized_family
                        != normalize_family(style.request.family);
                    requested_family_matched &= !substituted;
                    selected_faces
                        .entry(font_id)
                        .and_modify(|seen_substitution| *seen_substitution |= substituted)
                        .or_insert(substituted);
                    if runs.len() >= options.max_runs {
                        return Err(FontPackError::LimitExceeded {
                            resource: "shape_runs",
                            limit: options.max_runs as u64,
                            actual: runs.len() as u64 + 1,
                        });
                    }
                    let shaped = self.shape_one_run(text, source.clone(), font_id, direction)?;
                    glyph_count = glyph_count.checked_add(shaped.len()).ok_or(
                        FontPackError::LimitExceeded {
                            resource: "shape_glyphs",
                            limit: options.max_glyphs as u64,
                            actual: u64::MAX,
                        },
                    )?;
                    if glyph_count > options.max_glyphs {
                        return Err(FontPackError::LimitExceeded {
                            resource: "shape_glyphs",
                            limit: options.max_glyphs as u64,
                            actual: glyph_count as u64,
                        });
                    }
                    missing_glyphs = missing_glyphs
                        .saturating_add(shaped.iter().filter(|glyph| glyph.glyph_id == 0).count());
                    runs.push(ShapedRun {
                        font_id,
                        direction,
                        source,
                        style_index: style.style_index,
                        glyphs: shaped,
                    });
                }
            }
        }
        Ok(ShapedText {
            runs,
            glyph_count,
            missing_glyphs,
            requested_family_matched,
            selected_faces: selected_faces
                .into_iter()
                .map(|(font_id, substituted)| ShapedFaceUse {
                    font_id,
                    substituted,
                })
                .collect(),
            base_direction,
        })
    }

    fn best_face_in_family(
        &self,
        normalized_family: &str,
        request: FontRequest<'_>,
    ) -> Option<FontId> {
        self.best_face_in_family_range(normalized_family, request, 0..self.inner.faces.len())
    }

    fn best_face_in_family_range(
        &self,
        normalized_family: &str,
        request: FontRequest<'_>,
        range: Range<usize>,
    ) -> Option<FontId> {
        self.inner
            .faces
            .iter()
            .enumerate()
            .skip(range.start)
            .take(range.end.saturating_sub(range.start))
            .filter(|(_, face)| face.normalized_family == normalized_family)
            .min_by_key(|(index, face)| {
                (
                    u8::from(face.italic != request.italic),
                    face.weight.abs_diff(request.weight),
                    *index,
                )
            })
            .and_then(|(index, _)| u16::try_from(index).ok())
            .map(FontId)
    }

    fn face_ranges(
        &self,
        text: &str,
        range: Range<usize>,
        preferred: FontId,
        request: FontRequest<'_>,
    ) -> Result<Vec<(Range<usize>, FontId)>, FontPackError> {
        let slice = text
            .get(range.clone())
            .ok_or(FontPackError::InvalidTextRange)?;
        let mut out = Vec::<(Range<usize>, FontId)>::new();
        for (offset, grapheme) in slice.grapheme_indices(true) {
            let start = range.start + offset;
            let end = start + grapheme.len();
            let font_id = self.face_for_grapheme(grapheme, preferred, request)?;
            if let Some((last_range, last_font)) = out.last_mut() {
                if *last_font == font_id && last_range.end == start {
                    last_range.end = end;
                    continue;
                }
            }
            out.push((start..end, font_id));
        }
        Ok(out)
    }

    fn face_for_grapheme(
        &self,
        grapheme: &str,
        preferred: FontId,
        request: FontRequest<'_>,
    ) -> Result<FontId, FontPackError> {
        if self.face_supports(preferred, grapheme)? {
            return Ok(preferred);
        }
        let preferred_family = &self.inner.faces[usize::from(preferred.0)].normalized_family;
        let fallback_family =
            &self.inner.faces[usize::from(self.inner.default_face.0)].normalized_family;
        let supported = self
            .inner
            .faces
            .iter()
            .enumerate()
            .filter_map(|(index, face)| {
                let id = FontId(u16::try_from(index).ok()?);
                self.face_supports(id, grapheme)
                    .ok()
                    .filter(|supported| *supported)
                    .map(|_| {
                        let family_rank = if face.normalized_family == *preferred_family {
                            0_u8
                        } else if face.normalized_family == *fallback_family {
                            1
                        } else {
                            2
                        };
                        (
                            (
                                family_rank,
                                u8::from(face.italic != request.italic),
                                face.weight.abs_diff(request.weight),
                                index,
                            ),
                            id,
                        )
                    })
            })
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, id)| id);
        // Preserve the requested face when the pack has no covering fallback.
        // rustybuzz will then produce glyph 0, which the caller reports as a
        // deterministic missing-glyph warning.
        Ok(supported.unwrap_or(preferred))
    }

    fn face_supports(&self, id: FontId, text: &str) -> Result<bool, FontPackError> {
        let face = self.face(id)?;
        Ok(text
            .chars()
            .filter(|character| !is_default_ignorable(*character))
            .all(|character| face.glyph_index(character).is_some()))
    }

    fn shape_one_run(
        &self,
        text: &str,
        source: Range<usize>,
        font_id: FontId,
        direction: BaseDirection,
    ) -> Result<Vec<ShapedGlyph>, FontPackError> {
        let value = text
            .get(source.clone())
            .ok_or(FontPackError::InvalidTextRange)?;
        let face = BuzzFace::from_slice(&self.entry(font_id)?.bytes, 0)
            .ok_or(FontPackError::InvalidFont)?;
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(value);
        buffer.set_direction(match direction {
            BaseDirection::RightToLeft => Direction::RightToLeft,
            BaseDirection::Auto | BaseDirection::LeftToRight => Direction::LeftToRight,
        });
        buffer.guess_segment_properties();
        let glyphs = rustybuzz::shape(&face, &[], buffer);
        let source_start =
            u32::try_from(source.start).map_err(|_| FontPackError::InvalidTextRange)?;
        glyphs
            .glyph_infos()
            .iter()
            .zip(glyphs.glyph_positions())
            .map(|(info, position)| {
                let glyph_id =
                    u16::try_from(info.glyph_id).map_err(|_| FontPackError::InvalidFont)?;
                Ok(ShapedGlyph {
                    glyph_id,
                    cluster: source_start
                        .checked_add(info.cluster)
                        .ok_or(FontPackError::InvalidTextRange)?,
                    x_advance: position.x_advance,
                    y_advance: position.y_advance,
                    x_offset: position.x_offset,
                    y_offset: position.y_offset,
                })
            })
            .collect()
    }

    fn face(&self, id: FontId) -> Result<Face<'_>, FontPackError> {
        Face::parse(&self.entry(id)?.bytes, 0).map_err(|_| FontPackError::InvalidFont)
    }

    fn entry(&self, id: FontId) -> Result<&FontEntry, FontPackError> {
        self.inner
            .faces
            .get(usize::from(id.0))
            .ok_or(FontPackError::InvalidFont)
    }
}

fn is_default_ignorable(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff0}'..='\u{fff8}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e0fff}'
    )
}

struct QuantizedOutline {
    commands: Vec<FontOutlineCommand>,
    limit: u64,
    exceeded: bool,
    invalid: bool,
}

impl QuantizedOutline {
    fn new(limit: u64) -> Self {
        Self {
            commands: Vec::new(),
            limit,
            exceeded: false,
            invalid: false,
        }
    }

    fn push(&mut self, command: FontOutlineCommand) {
        if self.commands.len() as u64 >= self.limit {
            self.exceeded = true;
        } else {
            self.commands.push(command);
        }
    }

    fn coordinate(&mut self, value: f32) -> i32 {
        let scaled = value * OUTLINE_UNITS;
        if !scaled.is_finite() || scaled < i32::MIN as f32 || scaled > i32::MAX as f32 {
            self.invalid = true;
            0
        } else {
            scaled.round() as i32
        }
    }
}

impl OutlineBuilder for QuantizedOutline {
    fn move_to(&mut self, x: f32, y: f32) {
        let x = self.coordinate(x);
        let y = self.coordinate(y);
        self.push(FontOutlineCommand::MoveTo(x, y));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let x = self.coordinate(x);
        let y = self.coordinate(y);
        self.push(FontOutlineCommand::LineTo(x, y));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let x1 = self.coordinate(x1);
        let y1 = self.coordinate(y1);
        let x = self.coordinate(x);
        let y = self.coordinate(y);
        self.push(FontOutlineCommand::QuadraticTo(x1, y1, x, y));
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let x1 = self.coordinate(x1);
        let y1 = self.coordinate(y1);
        let x2 = self.coordinate(x2);
        let y2 = self.coordinate(y2);
        let x = self.coordinate(x);
        let y = self.coordinate(y);
        self.push(FontOutlineCommand::CubicTo(x1, y1, x2, y2, x, y));
    }

    fn close(&mut self) {
        self.push(FontOutlineCommand::Close);
    }
}

#[cfg(test)]
pub(crate) fn synthetic_test_pack() -> FontPack {
    let root = write_synthetic_test_pack();
    let pack = FontPack::load_manifest(root.join("manifest.json")).expect("load synthetic pack");
    fs::remove_dir_all(root).expect("remove synthetic font directory");
    pack
}

#[cfg(test)]
fn write_synthetic_test_pack() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NONCE: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "rxls-render-font-pack-{}-{}",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(root.join("fonts")).expect("create synthetic font directory");
    fs::create_dir_all(root.join("licenses")).expect("create synthetic license directory");
    let wide = synthetic_font(
        "Wide Sans",
        &[
            (0x20, 0x20, 2),
            (0x21, 0x7e, 1),
            (0x3040, 0x30ff, 1),
            (0x4e00, 0x9fff, 1),
            (0xac00, 0xd7a3, 1),
        ],
    );
    let rtl = synthetic_font(
        "RTL Sans",
        &[(0x20, 0x20, 2), (0x0590, 0x05ff, 1), (0x0600, 0x06ff, 1)],
    );
    let license = b"SIL OPEN FONT LICENSE Version 1.1\n";
    let config = b"<fontconfig/>\n";
    fs::write(root.join("fonts/WideSans.ttf"), &wide).expect("write synthetic wide font");
    fs::write(root.join("fonts/RtlSans.ttf"), &rtl).expect("write synthetic RTL font");
    fs::write(root.join("licenses/OFL.txt"), license).expect("write synthetic OFL");
    fs::write(root.join("fonts.conf"), config).expect("write synthetic configuration");

    let fonts = serde_json::json!([
        {
            "bytes": wide.len(),
            "family": "Wide Sans",
            "output": "fonts/WideSans.ttf",
            "sha256": sha256_hex(&wide),
            "style": "normal",
            "weight": 400
        },
        {
            "bytes": rtl.len(),
            "family": "RTL Sans",
            "output": "fonts/RtlSans.ttf",
            "sha256": sha256_hex(&rtl),
            "style": "normal",
            "weight": 400
        }
    ]);
    let licenses = serde_json::json!([{
        "bytes": license.len(),
        "output": "licenses/OFL.txt",
        "sha256": sha256_hex(license)
    }]);
    let config_digest = sha256_hex(config);
    let aliases = serde_json::json!([
        {
            "family": "Legacy Sans",
            "substitute": "Wide Sans"
        },
        {
            "family": "Wide Sans",
            "substitute": "RTL Sans"
        }
    ]);
    let mut identity = Map::new();
    identity.insert("aliases".to_string(), aliases.clone());
    identity.insert("fonts".to_string(), fonts.clone());
    identity.insert(
        "fonts_conf_sha256".to_string(),
        Value::String(config_digest.clone()),
    );
    identity.insert("licenses".to_string(), licenses.clone());
    let mut canonical = serde_json::to_string_pretty(&Value::Object(identity)).unwrap();
    canonical.push('\n');
    let manifest = serde_json::json!({
        "schema": FONT_PACK_SCHEMA,
        "license": "SIL-OFL-1.1",
        "aliases": aliases,
        "fonts": fonts,
        "licenses": licenses,
        "fonts_conf_sha256": config_digest,
        "total_bytes": wide.len() + rtl.len() + license.len() + config.len(),
        "pack_sha256": sha256_hex(canonical.as_bytes())
    });
    let mut manifest_bytes = serde_json::to_string_pretty(&manifest).unwrap();
    manifest_bytes.push('\n');
    fs::write(root.join("manifest.json"), manifest_bytes).expect("write synthetic manifest");
    root
}

#[cfg(test)]
fn synthetic_font(family: &str, groups: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut tables = vec![
        (*b"cmap", synthetic_cmap(groups)),
        (*b"glyf", synthetic_glyf()),
        (*b"head", synthetic_head()),
        (*b"hhea", synthetic_hhea()),
        (*b"hmtx", synthetic_hmtx()),
        (*b"loca", synthetic_loca()),
        (*b"maxp", synthetic_maxp()),
        (*b"name", synthetic_name(family)),
        (*b"post", synthetic_post()),
    ];
    tables.sort_by_key(|(tag, _)| *tag);
    let table_count = tables.len() as u16;
    let directory_bytes = 12 + tables.len() * 16;
    let mut offset = directory_bytes;
    let mut records = Vec::with_capacity(tables.len());
    for (tag, bytes) in &tables {
        records.push((*tag, offset as u32, bytes.len() as u32));
        offset += (bytes.len() + 3) & !3;
    }
    let mut font = Vec::with_capacity(offset);
    be_u32(&mut font, 0x0001_0000);
    be_u16(&mut font, table_count);
    be_u16(&mut font, 0);
    be_u16(&mut font, 0);
    be_u16(&mut font, 0);
    for (tag, offset, length) in &records {
        font.extend_from_slice(tag);
        be_u32(&mut font, 0);
        be_u32(&mut font, *offset);
        be_u32(&mut font, *length);
    }
    for (_, bytes) in tables {
        font.extend_from_slice(&bytes);
        while font.len() % 4 != 0 {
            font.push(0);
        }
    }
    font
}

#[cfg(test)]
fn synthetic_name(family: &str) -> Vec<u8> {
    let encoded = family
        .encode_utf16()
        .flat_map(u16::to_be_bytes)
        .collect::<Vec<_>>();
    let mut table = Vec::with_capacity(18 + encoded.len());
    be_u16(&mut table, 0);
    be_u16(&mut table, 1);
    be_u16(&mut table, 18);
    be_u16(&mut table, 3);
    be_u16(&mut table, 1);
    be_u16(&mut table, 0x0409);
    be_u16(&mut table, name_id::FAMILY);
    be_u16(&mut table, encoded.len() as u16);
    be_u16(&mut table, 0);
    table.extend_from_slice(&encoded);
    table
}

#[cfg(test)]
fn synthetic_cmap(groups: &[(u32, u32, u32)]) -> Vec<u8> {
    let length = 16 + groups.len() * 12;
    let mut table = Vec::with_capacity(12 + length);
    be_u16(&mut table, 0);
    be_u16(&mut table, 1);
    be_u16(&mut table, 0);
    be_u16(&mut table, 6);
    be_u32(&mut table, 12);
    be_u16(&mut table, 13);
    be_u16(&mut table, 0);
    be_u32(&mut table, length as u32);
    be_u32(&mut table, 0);
    be_u32(&mut table, groups.len() as u32);
    for &(start, end, glyph) in groups {
        be_u32(&mut table, start);
        be_u32(&mut table, end);
        be_u32(&mut table, glyph);
    }
    table
}

#[cfg(test)]
fn synthetic_glyf() -> Vec<u8> {
    let glyph = synthetic_rectangle_glyph();
    let mut table = glyph.clone();
    table.extend_from_slice(&glyph);
    table
}

#[cfg(test)]
fn synthetic_rectangle_glyph() -> Vec<u8> {
    let mut glyph = Vec::new();
    be_i16(&mut glyph, 1);
    be_i16(&mut glyph, 0);
    be_i16(&mut glyph, 0);
    be_i16(&mut glyph, 500);
    be_i16(&mut glyph, 700);
    be_u16(&mut glyph, 3);
    be_u16(&mut glyph, 0);
    glyph.extend_from_slice(&[1, 1, 1, 1]);
    for value in [0_i16, 500, 0, -500] {
        be_i16(&mut glyph, value);
    }
    for value in [0_i16, 0, 700, 0] {
        be_i16(&mut glyph, value);
    }
    glyph
}

#[cfg(test)]
fn synthetic_head() -> Vec<u8> {
    let mut table = Vec::new();
    be_u32(&mut table, 0x0001_0000);
    be_u32(&mut table, 0x0001_0000);
    be_u32(&mut table, 0);
    be_u32(&mut table, 0x5f0f_3cf5);
    be_u16(&mut table, 0);
    be_u16(&mut table, 1_000);
    table.extend_from_slice(&[0; 16]);
    for value in [0_i16, 0, 500, 700] {
        be_i16(&mut table, value);
    }
    be_u16(&mut table, 0);
    be_u16(&mut table, 8);
    be_i16(&mut table, 2);
    be_u16(&mut table, 1);
    be_i16(&mut table, 0);
    assert_eq!(table.len(), 54);
    table
}

#[cfg(test)]
fn synthetic_hhea() -> Vec<u8> {
    let mut table = Vec::new();
    be_u32(&mut table, 0x0001_0000);
    be_i16(&mut table, 800);
    be_i16(&mut table, -200);
    be_i16(&mut table, 200);
    table.extend_from_slice(&[0; 24]);
    be_u16(&mut table, 3);
    assert_eq!(table.len(), 36);
    table
}

#[cfg(test)]
fn synthetic_hmtx() -> Vec<u8> {
    let mut table = Vec::new();
    for advance in [600_u16, 600, 300] {
        be_u16(&mut table, advance);
        be_i16(&mut table, 0);
    }
    table
}

#[cfg(test)]
fn synthetic_loca() -> Vec<u8> {
    let glyph_len = synthetic_rectangle_glyph().len() as u32;
    let mut table = Vec::new();
    for offset in [0_u32, glyph_len, glyph_len * 2, glyph_len * 2] {
        be_u32(&mut table, offset);
    }
    table
}

#[cfg(test)]
fn synthetic_maxp() -> Vec<u8> {
    let mut table = Vec::new();
    be_u32(&mut table, 0x0001_0000);
    be_u16(&mut table, 3);
    table
}

#[cfg(test)]
fn synthetic_post() -> Vec<u8> {
    let mut table = Vec::new();
    be_u32(&mut table, 0x0003_0000);
    be_u32(&mut table, 0);
    be_i16(&mut table, -100);
    be_i16(&mut table, 50);
    table.extend_from_slice(&[0; 20]);
    assert_eq!(table.len(), 32);
    table
}

#[cfg(test)]
fn be_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
fn be_i16(output: &mut Vec<u8>, value: i16) {
    output.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
fn be_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_fixture() -> (Vec<u8>, Vec<FontPackMember>) {
        let root = write_synthetic_test_pack();
        let manifest = fs::read(root.join("manifest.json")).unwrap();
        let members = [
            "fonts/WideSans.ttf",
            "fonts/RtlSans.ttf",
            "licenses/OFL.txt",
            "fonts.conf",
        ]
        .into_iter()
        .map(|name| FontPackMember::new(name, fs::read(root.join(name)).unwrap()))
        .collect();
        fs::remove_dir_all(root).unwrap();
        (manifest, members)
    }

    #[test]
    fn synthetic_pack_is_verified_shaped_and_outlined_without_host_fonts() {
        let pack = synthetic_test_pack();
        assert_eq!(pack.font_count(), 2);
        assert_eq!(pack.default_family(), "Wide Sans");
        let shaped = pack
            .shape(
                "Latin 한글 日本 中文 العربية עברית",
                FontRequest {
                    family: "Wide Sans",
                    weight: 400,
                    italic: false,
                },
                ShapeOptions {
                    direction: BaseDirection::Auto,
                    max_glyphs: 1_000,
                    max_runs: 100,
                },
            )
            .unwrap();
        assert_eq!(shaped.missing_glyphs, 0);
        assert!(shaped
            .runs
            .iter()
            .any(|run| run.direction == BaseDirection::RightToLeft));
        assert!(
            shaped
                .runs
                .iter()
                .map(|run| run.font_id)
                .collect::<BTreeSet<_>>()
                .len()
                >= 2
        );
        let outline = pack.outline(FontId(0), 1, 100).unwrap();
        assert!(matches!(
            outline.first(),
            Some(FontOutlineCommand::MoveTo(..))
        ));
        assert!(matches!(outline.last(), Some(FontOutlineCommand::Close)));
    }

    #[test]
    fn styled_shaping_keeps_global_bidi_order_and_exact_style_ranges() {
        let pack = synthetic_test_pack();
        let text = "abאב";
        let styles = [
            StyledFontRequest {
                source: 0..2,
                request: FontRequest {
                    family: "Wide Sans",
                    weight: 400,
                    italic: false,
                },
                style_index: 7,
            },
            StyledFontRequest {
                source: 2..text.len(),
                request: FontRequest {
                    family: "Rtl Sans",
                    weight: 700,
                    italic: true,
                },
                style_index: 11,
            },
        ];
        let first = pack
            .shape_styled(
                text,
                &styles,
                ShapeOptions {
                    direction: BaseDirection::Auto,
                    max_glyphs: 32,
                    max_runs: 32,
                },
            )
            .unwrap();
        let second = pack
            .shape_styled(
                text,
                &styles,
                ShapeOptions {
                    direction: BaseDirection::Auto,
                    max_glyphs: 32,
                    max_runs: 32,
                },
            )
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.base_direction, BaseDirection::LeftToRight);
        assert!(first.runs.iter().any(|run| run.style_index == 7));
        assert!(first
            .runs
            .iter()
            .any(|run| { run.style_index == 11 && run.direction == BaseDirection::RightToLeft }));
        let rtl_clusters = first
            .runs
            .iter()
            .filter(|run| run.style_index == 11)
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.cluster))
            .collect::<Vec<_>>();
        assert!(rtl_clusters.windows(2).all(|pair| pair[0] >= pair[1]));

        let invalid = [StyledFontRequest {
            source: 1..text.len(),
            request: styles[0].request,
            style_index: 0,
        }];
        assert_eq!(
            pack.shape_styled(
                text,
                &invalid,
                ShapeOptions {
                    direction: BaseDirection::Auto,
                    max_glyphs: 32,
                    max_runs: 32,
                },
            ),
            Err(FontPackError::InvalidTextRange)
        );
    }

    #[test]
    fn shaping_and_outline_limits_are_typed() {
        let pack = synthetic_test_pack();
        let error = pack
            .shape(
                "abcdef",
                FontRequest {
                    family: "Wide Sans",
                    weight: 400,
                    italic: false,
                },
                ShapeOptions {
                    direction: BaseDirection::Auto,
                    max_glyphs: 2,
                    max_runs: 2,
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            FontPackError::LimitExceeded {
                resource: "shape_glyphs",
                ..
            }
        ));
        assert!(matches!(
            pack.outline(FontId(0), 1, 1),
            Err(FontPackError::LimitExceeded {
                resource: "outline_commands",
                ..
            })
        ));
    }

    #[test]
    fn pack_identity_is_path_independent_and_owned_after_verification() {
        let first = synthetic_test_pack();
        let second = synthetic_test_pack();
        assert_eq!(first, second);
        assert_eq!(first.pack_sha256(), second.pack_sha256());
        assert!(first.metrics(FontId(0)).is_ok());
    }

    #[test]
    fn memory_loader_matches_filesystem_identity_and_exposes_face_hashes() {
        let filesystem = synthetic_test_pack();
        let (manifest, members) = memory_fixture();
        let memory = FontPack::load_memory(&manifest, members).unwrap();
        assert_eq!(memory, filesystem);
        assert_eq!(memory.default_family(), "Wide Sans");
        let identities = memory.face_identities().collect::<Vec<_>>();
        assert_eq!(identities.len(), 2);
        assert_eq!(identities[0].family, "Wide Sans");
        assert_eq!(identities[0].weight, 400);
        assert!(!identities[0].italic);
        assert_eq!(identities[0].sha256.len(), 64);
        assert!(identities[0]
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        let shaped = memory
            .shape(
                "한글 العربية",
                FontRequest {
                    family: "Wide Sans",
                    weight: 400,
                    italic: false,
                },
                ShapeOptions {
                    direction: BaseDirection::Auto,
                    max_glyphs: 100,
                    max_runs: 10,
                },
            )
            .unwrap();
        assert_eq!(shaped.missing_glyphs, 0);
    }

    #[test]
    fn caller_loader_accepts_declared_license_without_weakening_ofl_loader() {
        let (manifest, members) = memory_fixture();
        let mut caller_manifest: Value = serde_json::from_slice(&manifest).unwrap();
        caller_manifest["license"] = Value::String("LicenseRef-Caller-Supplied".to_string());
        let bytes = serde_json::to_vec(&caller_manifest).unwrap();
        assert!(matches!(
            FontPack::load_memory(&bytes, members.clone()),
            Err(FontPackError::InvalidManifest {
                reason: "pack_license"
            })
        ));
        let caller = FontPack::load_caller_memory(&bytes, members).unwrap();
        assert_eq!(caller.font_count(), 2);
        assert_eq!(caller.default_family(), "Wide Sans");
    }

    #[test]
    fn aliases_are_non_exact_exact_families_win_and_style_fallback_is_stable() {
        let pack = synthetic_test_pack();
        let aliased = pack.resolve(FontRequest {
            family: "Legacy Sans",
            weight: 700,
            italic: true,
        });
        assert_eq!(aliased.id, FontId(0));
        assert!(!aliased.exact_family);
        assert!(!aliased.exact_style);

        // The manifest deliberately also contains Wide Sans -> RTL Sans. The
        // actual Wide Sans family must win before that alias is considered.
        let exact = pack.resolve(FontRequest {
            family: " wide sans ",
            weight: 700,
            italic: true,
        });
        assert_eq!(exact.id, FontId(0));
        assert!(exact.exact_family);
        assert!(!exact.exact_style);

        let shaped = pack
            .shape(
                "abc",
                FontRequest {
                    family: "Legacy Sans",
                    weight: 400,
                    italic: false,
                },
                ShapeOptions {
                    direction: BaseDirection::Auto,
                    max_glyphs: 32,
                    max_runs: 8,
                },
            )
            .unwrap();
        assert!(!shaped.requested_family_matched);
        assert_eq!(
            shaped.selected_faces,
            [ShapedFaceUse {
                font_id: FontId(0),
                substituted: true,
            }]
        );
    }

    #[test]
    fn caller_pack_exact_faces_precede_fallback_and_unknowns_use_final_default() {
        let caller = synthetic_test_pack();
        let mut fallback_faces = caller.inner.faces.clone();
        fallback_faces[0].family = "Legacy Sans".to_string();
        fallback_faces[0].normalized_family = normalize_family("Legacy Sans");
        let fallback = FontPack {
            inner: Arc::new(FontPackInner {
                pack_sha256: sha256_hex(b"distinct-fallback-pack"),
                faces: fallback_faces,
                layers: caller.inner.layers.clone(),
                default_face: caller.inner.default_face,
                limits: caller.inner.limits,
            }),
        };
        let stack = caller.with_fallback(&fallback).unwrap();
        assert_ne!(stack.pack_sha256(), caller.pack_sha256());
        assert_eq!(stack.font_count(), 4);

        // A real family in the second layer beats a first-layer alias.
        let fallback_exact = stack.resolve(FontRequest {
            family: "Legacy Sans",
            weight: 400,
            italic: false,
        });
        assert_eq!(fallback_exact.id, FontId(2));
        assert!(fallback_exact.exact_family);

        // Exact ties remain caller-first.
        assert_eq!(
            stack
                .resolve(FontRequest {
                    family: "RTL Sans",
                    weight: 400,
                    italic: false,
                })
                .id,
            FontId(1)
        );
        // A completely unknown family falls back to the final OFL layer.
        assert_eq!(
            stack
                .resolve(FontRequest {
                    family: "Missing Family",
                    weight: 400,
                    italic: false,
                })
                .id,
            FontId(2)
        );
    }

    #[test]
    fn alias_manifest_tampering_unknown_targets_and_order_fail_closed() {
        let (manifest, members) = memory_fixture();
        let mut legacy: Value = serde_json::from_slice(&manifest).unwrap();
        legacy.as_object_mut().unwrap().remove("aliases");
        let mut legacy_identity = Map::new();
        legacy_identity.insert("fonts".to_string(), legacy["fonts"].clone());
        legacy_identity.insert(
            "fonts_conf_sha256".to_string(),
            legacy["fonts_conf_sha256"].clone(),
        );
        legacy_identity.insert("licenses".to_string(), legacy["licenses"].clone());
        let mut canonical = serde_json::to_string_pretty(&Value::Object(legacy_identity)).unwrap();
        canonical.push('\n');
        legacy["pack_sha256"] = Value::String(sha256_hex(canonical.as_bytes()));
        assert!(
            FontPack::load_memory(&serde_json::to_vec(&legacy).unwrap(), members.clone()).is_ok()
        );

        let mut mislabeled: Value = serde_json::from_slice(&manifest).unwrap();
        mislabeled["fonts"][0]["family"] = Value::String("Invented Sans".to_string());
        assert!(matches!(
            FontPack::load_memory(&serde_json::to_vec(&mislabeled).unwrap(), members.clone()),
            Err(FontPackError::InvalidManifest {
                reason: "font_family_identity"
            })
        ));

        let mut wrong_weight: Value = serde_json::from_slice(&manifest).unwrap();
        wrong_weight["fonts"][0]["weight"] = Value::from(700);
        assert!(matches!(
            FontPack::load_memory(&serde_json::to_vec(&wrong_weight).unwrap(), members.clone()),
            Err(FontPackError::InvalidManifest {
                reason: "font_weight_identity"
            })
        ));

        let mut wrong_style: Value = serde_json::from_slice(&manifest).unwrap();
        wrong_style["fonts"][0]["style"] = Value::String("italic".to_string());
        assert!(matches!(
            FontPack::load_memory(&serde_json::to_vec(&wrong_style).unwrap(), members.clone()),
            Err(FontPackError::InvalidManifest {
                reason: "font_style_identity"
            })
        ));

        let mut unknown: Value = serde_json::from_slice(&manifest).unwrap();
        unknown["aliases"][0]["substitute"] = Value::String("Missing Family".to_string());
        assert!(matches!(
            FontPack::load_memory(&serde_json::to_vec(&unknown).unwrap(), members.clone()),
            Err(FontPackError::InvalidManifest {
                reason: "alias_substitute"
            })
        ));

        let mut reordered: Value = serde_json::from_slice(&manifest).unwrap();
        reordered["aliases"].as_array_mut().unwrap().reverse();
        assert!(matches!(
            FontPack::load_memory(&serde_json::to_vec(&reordered).unwrap(), members.clone()),
            Err(FontPackError::InvalidManifest {
                reason: "alias_order"
            })
        ));

        let mut digest_tamper: Value = serde_json::from_slice(&manifest).unwrap();
        digest_tamper["aliases"][0]["family"] = Value::String("Changed Alias".to_string());
        assert_eq!(
            FontPack::load_memory(&serde_json::to_vec(&digest_tamper).unwrap(), members),
            Err(FontPackError::DigestMismatch)
        );
    }

    #[test]
    fn memory_loader_rejects_missing_extra_duplicate_unsafe_digest_and_limits() {
        let (manifest, mut members) = memory_fixture();
        let missing = members.pop().unwrap();
        assert_eq!(
            FontPack::load_memory(&manifest, members.clone()),
            Err(FontPackError::MissingMember)
        );
        members.push(missing);
        members.push(FontPackMember::new("extra.bin", b"extra".to_vec()));
        assert_eq!(
            FontPack::load_memory(&manifest, members),
            Err(FontPackError::UnexpectedFile)
        );

        let (_, members) = memory_fixture();
        let mut duplicate = members.clone();
        duplicate.push(members[0].clone());
        assert!(matches!(
            FontPack::load_memory(&manifest, duplicate),
            Err(FontPackError::InvalidManifest {
                reason: "duplicate_member"
            })
        ));

        let (_, mut unsafe_members) = memory_fixture();
        unsafe_members[0].name = "../escape.ttf".to_string();
        assert_eq!(
            FontPack::load_memory(&manifest, unsafe_members),
            Err(FontPackError::UnsafePath)
        );

        let (_, mut corrupt) = memory_fixture();
        corrupt[0].bytes[0] ^= 1;
        assert_eq!(
            FontPack::load_memory(&manifest, corrupt),
            Err(FontPackError::DigestMismatch)
        );

        let (_, members) = memory_fixture();
        assert!(matches!(
            FontPack::load_memory_with_limits(
                &manifest,
                members,
                FontPackLimits {
                    max_total_bytes: 1,
                    ..FontPackLimits::default()
                }
            ),
            Err(FontPackError::LimitExceeded {
                resource: "total_bytes",
                limit: 1,
                ..
            })
        ));
    }

    #[test]
    fn loader_rejects_undeclared_traversal_digest_and_size_limit_inputs() {
        let root = write_synthetic_test_pack();
        fs::write(root.join("undeclared.txt"), b"not declared").unwrap();
        assert_eq!(
            FontPack::load_manifest(root.join("manifest.json")),
            Err(FontPackError::UnexpectedFile)
        );
        fs::remove_file(root.join("undeclared.txt")).unwrap();

        let mut manifest: Value =
            serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
        manifest["fonts"][0]["output"] = Value::String("../escape.ttf".to_string());
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        assert_eq!(
            FontPack::load_manifest(root.join("manifest.json")),
            Err(FontPackError::UnsafePath)
        );

        let root_digest = write_synthetic_test_pack();
        let font = root_digest.join("fonts/WideSans.ttf");
        let mut bytes = fs::read(&font).unwrap();
        bytes[0] ^= 1;
        fs::write(&font, bytes).unwrap();
        assert_eq!(
            FontPack::load_manifest(root_digest.join("manifest.json")),
            Err(FontPackError::DigestMismatch)
        );

        let root_limit = write_synthetic_test_pack();
        let limits = FontPackLimits {
            max_font_bytes: 1,
            ..FontPackLimits::default()
        };
        assert!(matches!(
            FontPack::load_manifest_with_limits(root_limit.join("manifest.json"), limits),
            Err(FontPackError::LimitExceeded {
                resource: "font_bytes",
                limit: 1,
                ..
            })
        ));

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(root_digest).unwrap();
        fs::remove_dir_all(root_limit).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn loader_never_follows_pack_symlinks() {
        use std::os::unix::fs::symlink;

        let root = write_synthetic_test_pack();
        let target = root.join("fonts/WideSans.ttf");
        let saved = root.join("WideSans.saved.ttf");
        fs::rename(&target, &saved).unwrap();
        symlink(&saved, &target).unwrap();
        assert_eq!(
            FontPack::load_manifest(root.join("manifest.json")),
            Err(FontPackError::UnsafePath)
        );
        fs::remove_dir_all(root).unwrap();
    }
}
