//! Raw OOXML package preservation for editable `.xlsx`/`.xlsm` wrappers.
//!
//! [`Package`] models an OPC ZIP container as a name-keyed store of [`Part`]s
//! plus the parsed views of its two metadata kinds — `[Content_Types].xml`
//! ([`ContentTypes`]) and every `_rels/*.rels` part ([`Rel`]) — needed to keep
//! those consistent as parts are added, replaced, or removed.
//!
//! **Lazy promotion**: a part starts as `Part::Raw` bytes and stays that way
//! until [`Package::part_tree_mut`] promotes it to a parsed [`XmlTree`] for
//! structural editing. Only a part actually promoted is ever re-serialized on
//! [`Package::to_bytes`]; every other part round-trips byte-for-byte. This
//! mirrors [`crate::xmltree`]'s own "only an edited part pays the
//! parse/serialize cost" design, one layer up.
//!
//! **Three integrity flags** record what happened while opening the ZIP so a
//! higher-level caller can decide whether preservation-editing is safe:
//! [`Package::is_complete`] (some ZIP entry could not be read at all — a
//! preservation-editing save could not honor every original part) and
//! [`Package::is_meta_lossy`] (`[Content_Types].xml` or a `.rels` part parsed
//! lossily, or a duplicate part name differed only by case — regenerating
//! metadata could drop something the source had). A third, internal-only
//! bookkeeping flag (`ct_rels_injected`) tracks a narrower, recoverable case: a
//! `[Content_Types].xml` that parsed fine but was missing the mandatory `rels`
//! `Default` — nothing is lost on a no-op save, but the moment any edit writes
//! a `.rels` part, `[Content_Types].xml` must be regenerated too so the newly
//! written `.rels` part is actually typed in the output.
//!
//! Part-name lookup (`part_bytes`/`replace_part`/`remove_part`/`has_part`/…) is
//! layered: an exact match first, then rxls's own long-standing
//! backslash-normalized/leading-slash-stripped canonical match (real-world
//! non-conformant packages apparently need this), then a case-insensitive
//! match on top of that canonicalization (some producers emit inconsistent
//! part-name casing between `[Content_Types].xml`/`.rels` references and the
//! actual ZIP entry). Whichever spelling was first stored in the package wins
//! for output, so `to_bytes` never silently renames an entry.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::write::SimpleFileOptions;

use crate::write::xml::esc_attr;
use crate::xmltree::{NodeId, XmlTree};
use crate::{Error, Result};

/// Largest accepted decompressed size for a single package part.
const MAX_PART: usize = 64 << 20;
/// Whole-archive decompressed size budget.
const MAX_TOTAL: usize = 512 << 20;
/// Maximum ZIP entries (files + directories) accepted.
const MAX_ENTRIES: usize = 65_536;
/// Maximum accepted part-name (ZIP entry name) length.
const MAX_NAME_LEN: usize = 4096;

/// Canonical package-relative name of the OPC content-types stream.
const CONTENT_TYPES: &str = "[Content_Types].xml";

// Test-lowerable copies of `MAX_PART`/`MAX_ENTRIES`, so the over-budget paths
// can be exercised on tiny fixtures instead of multi-megabyte/many-entry
// ones. Production always uses the const. Same seam pattern as
// `crate::xmltree`'s `TEST_NODE_BUDGET`.
#[cfg(test)]
thread_local! {
    static TEST_MAX_PART: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_PART) };
}

#[cfg(test)]
pub(crate) fn set_test_max_part(n: usize) {
    TEST_MAX_PART.with(|c| c.set(n));
}

#[cfg(test)]
pub(crate) fn reset_test_max_part() {
    TEST_MAX_PART.with(|c| c.set(MAX_PART));
}

fn max_part() -> usize {
    #[cfg(test)]
    {
        TEST_MAX_PART.with(|c| c.get())
    }
    #[cfg(not(test))]
    {
        MAX_PART
    }
}

#[cfg(test)]
thread_local! {
    static TEST_MAX_ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_ENTRIES) };
}

#[cfg(test)]
pub(crate) fn set_test_max_entries(n: usize) {
    TEST_MAX_ENTRIES.with(|c| c.set(n));
}

#[cfg(test)]
pub(crate) fn reset_test_max_entries() {
    TEST_MAX_ENTRIES.with(|c| c.set(MAX_ENTRIES));
}

fn max_entries() -> usize {
    #[cfg(test)]
    {
        TEST_MAX_ENTRIES.with(|c| c.get())
    }
    #[cfg(not(test))]
    {
        MAX_ENTRIES
    }
}

/// A package part's content: raw bytes by default, or a parsed [`XmlTree`]
/// once promoted for structural editing (see [`Package::part_tree_mut`]).
#[derive(Debug, Clone)]
enum Part {
    Raw(Vec<u8>),
    Xml(XmlTree),
}

impl Part {
    /// The part's serialized bytes — borrowed for `Raw`, freshly re-serialized
    /// for `Xml` (no caching of the serialized form: only a promoted part ever
    /// pays this cost, and it pays it fresh every call).
    fn bytes(&self) -> Cow<'_, [u8]> {
        match self {
            Part::Raw(b) => Cow::Borrowed(b.as_slice()),
            Part::Xml(t) => Cow::Owned(t.serialize()),
        }
    }
}

/// One `<Relationship>` entry from a `.rels` part.
#[derive(Debug, Clone)]
pub(crate) struct Rel {
    pub(crate) id: String,
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    pub(crate) rel_type: String,
    pub(crate) target: String,
    pub(crate) external: bool,
}

/// Parsed view of `[Content_Types].xml`: extension-keyed defaults plus
/// part-name-keyed overrides, matching OPC's own two-tier content-type
/// resolution.
#[derive(Debug, Clone)]
struct ContentTypes {
    /// `(extension without the leading '.', content type)`, e.g. `("xml",
    /// "application/xml")`. Matched case-insensitively.
    defaults: Vec<(String, String)>,
    /// `(part name incl. leading '/', content type)`. Matched
    /// case-insensitively on the part name (OPC part names are
    /// case-insensitive).
    overrides: Vec<(String, String)>,
}

impl ContentTypes {
    /// A bare-minimum fallback used when `[Content_Types].xml` is absent or
    /// fails to parse: just enough for the package to still open and a no-op
    /// save to reproduce the original (untouched) bytes.
    fn fallback() -> ContentTypes {
        ContentTypes {
            defaults: vec![("rels".to_string(), crate::write::xml::CT_RELS.to_string())],
            overrides: Vec::new(),
        }
    }

    /// Whether `part_name` resolves to *some* content type: an `Override` for
    /// this exact part name, or (failing that) a `Default` for its extension.
    fn resolves(&self, part_name: &str) -> bool {
        let pn = override_part_name(part_name);
        if self
            .overrides
            .iter()
            .any(|(p, _)| p.eq_ignore_ascii_case(&pn))
        {
            return true;
        }
        let Some((_, ext)) = part_name.rsplit_once('.') else {
            return false;
        };
        self.defaults
            .iter()
            .any(|(e, _)| e.eq_ignore_ascii_case(ext))
    }
}

/// `part_name` (bare, e.g. `xl/workbook.xml`) as an `Override`'s absolute
/// `PartName` (e.g. `/xl/workbook.xml`) — OPC `Override` part names are always
/// `/`-rooted.
fn override_part_name(part_name: &str) -> String {
    format!("/{}", canonical_part_name(part_name))
}

/// A retained OOXML (`.xlsx`/`.xlsm`) package: every part from the source ZIP,
/// kept as raw bytes unless promoted for editing, plus enough parsed metadata
/// ([Content_Types].xml, every `.rels` part) to keep edits internally
/// consistent.
#[derive(Debug, Clone)]
pub(crate) struct Package {
    /// Part/directory names in original ZIP order, for deterministic re-emit.
    /// Includes `[Content_Types].xml` and every `_rels/*.rels` part. A part
    /// removed from `parts` (see [`Package::remove_part`]) may leave a stale
    /// entry here; [`Package::to_bytes`] skips names no longer in `parts`.
    order: Vec<String>,
    /// Part name (canonical casing as first seen) → content. The
    /// authoritative store.
    parts: HashMap<String, Part>,
    /// Parsed view of `[Content_Types].xml`, regenerated into `parts` when an
    /// edit calls [`Package::ensure_content_type`] or writes a `.rels` part
    /// while `ct_rels_injected` is set.
    ctypes: ContentTypes,
    /// Parsed view of every `_rels/*.rels` part, keyed by the rels part's own
    /// package name (e.g. `xl/_rels/workbook.xml.rels`).
    #[cfg_attr(not(test), allow(dead_code))]
    // consumed by upcoming roadmap slices; unit-tested
    rels: HashMap<String, Vec<Rel>>,
    /// Next relationship-id ordinal to allocate, seeded above every existing
    /// `rIdN` in the source package. `u64` so seeding past a hostile
    /// `rId18446744073709551615` still yields a fresh, non-colliding id.
    #[cfg_attr(not(test), allow(dead_code))]
    // consumed by upcoming roadmap slices; unit-tested
    rid_next: u64,
    /// Parts added/replaced this session (via [`Package::replace_part`] or
    /// [`Package::set_part`]/[`Package::part_tree_mut`]). Only these are
    /// content-type/relationship validated on [`Package::to_bytes`] — an
    /// original passthrough part keeps its own typing even if the source
    /// package is itself non-conformant, so editing never *rejects* a file it
    /// could otherwise preserve.
    touched: HashSet<String>,
    /// `false` if [`Package::from_bytes`] skipped any unreadable ZIP entry —
    /// i.e. not every original part was retained. A higher-level caller
    /// should refuse preservation-editing saves in that case (checked here
    /// only as data; the actual gate lives in `crate::spreadsheet`).
    complete: bool,
    /// `true` if `[Content_Types].xml` or a `.rels` part failed to parse
    /// cleanly (or was entirely absent, or a duplicate part name differed
    /// only by case). Read and a no-op save still reproduce the original
    /// bytes; regenerating metadata from the (necessarily partial) parsed
    /// view would be lossy.
    meta_lossy: bool,
    /// `true` when `[Content_Types].xml` parsed but was missing the mandatory
    /// `Default Extension="rels"` entry, which was injected into the
    /// in-memory [`ContentTypes`] view. No effect on read or a no-op save
    /// (the original bytes are unaffected); the moment a `.rels` part is
    /// (re)written, `[Content_Types].xml` is force-regenerated too so the
    /// injected default actually reaches the output.
    #[cfg_attr(not(test), allow(dead_code))]
    // consumed by upcoming roadmap slices; unit-tested
    ct_rels_injected: bool,
}

impl Package {
    /// Parse an OOXML ZIP package. Lenient per-entry: an unreadable entry
    /// clears [`Package::is_complete`] rather than failing the whole parse;
    /// malformed `[Content_Types].xml`/`.rels` metadata clears
    /// [`Package::is_meta_lossy`] rather than failing. Hard errors only for
    /// budget violations (too many/too-large/too-long-named entries) and a ZIP
    /// the `zip` crate cannot open at all.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Package> {
        // Best-effort pre-flight, before `ZipArchive::new` parses (and
        // allocates per-entry state for) the central directory.
        check_zip_entry_budget(bytes)?;

        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|_| Error::Zip("not a valid spreadsheet ZIP container"))?;
        if zip.len() > max_entries() {
            return Err(Error::Zip("OOXML package has too many entries"));
        }

        let mut order: Vec<String> = Vec::new();
        let mut parts: HashMap<String, Part> = HashMap::new();
        let mut seen_dirs: HashSet<String> = HashSet::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut seen_ci: HashSet<String> = HashSet::new();
        // Canonical-form (backslash-to-slash, leading-slash-stripped) and
        // canonical-then-lowercased collision trackers. `seen`/`seen_ci`
        // above compare *raw* entry names (exact, and lowercased-exact), so
        // two entries whose raw names differ only by separator style (e.g.
        // `xl/Sheet.XML` vs `xl\SHEET.xml`) pass both of those unflagged even
        // though `find_part_key`'s canonical-matching tiers treat them as
        // ambiguous. These two trackers close that gap so `meta_lossy` fires
        // for every case `find_part_key` itself considers ambiguous.
        let mut seen_canon: HashSet<String> = HashSet::new();
        let mut seen_canon_ci: HashSet<String> = HashSet::new();
        let mut complete = true;
        let mut meta_lossy = false;
        let mut total: usize = 0;

        for idx in 0..zip.len() {
            let mut file = match zip.by_index(idx) {
                Ok(f) => f,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            let name = file.name().to_string();
            if name.len() > MAX_NAME_LEN {
                return Err(Error::Zip("OOXML package entry name is too long"));
            }
            if name.ends_with('/') {
                if seen_dirs.insert(name.clone()) {
                    order.push(name);
                }
                continue;
            }
            if !file.is_file() {
                // Not a directory (handled above) and not a regular file --
                // e.g. a symlink entry (`ZipFile::is_file()` is `false` for
                // both). The original part's bytes are unavailable, exactly
                // like the two sibling skip paths below (a `by_index` error,
                // a `read_to_end` error), so this must clear `complete` too.
                complete = false;
                continue;
            }
            if file.size() > max_part() as u64 {
                return Err(Error::Zip("OOXML package entry is too large"));
            }

            let mut data = Vec::new();
            let read = file
                .by_ref()
                .take(max_part() as u64 + 1)
                .read_to_end(&mut data);
            if read.is_err() {
                complete = false;
                continue;
            }
            if data.len() > max_part() {
                return Err(Error::Zip("OOXML package entry is too large"));
            }
            total = total
                .checked_add(data.len())
                .ok_or(Error::Zip("OOXML package is too large"))?;
            if total > MAX_TOTAL {
                return Err(Error::Zip("OOXML package is too large"));
            }

            if !seen.insert(name.clone()) {
                // Exact duplicate name: keep the *last* entry's data (matching
                // the `zip` crate's own last-entry-wins `by_name` lookup
                // semantics) but don't add a second `order`/`parts` slot.
                parts.insert(name, Part::Raw(data));
                continue;
            }
            if !seen_ci.insert(name.to_ascii_lowercase()) {
                // A different exact name that collides case-insensitively
                // with one already seen: both are retained individually (real
                // content is not dropped), but content-type/relationship
                // resolution can no longer disambiguate them safely.
                meta_lossy = true;
            }
            let canon = canonical_part_name(&name);
            if !seen_canon.insert(canon.clone()) {
                // Same canonical form (case-sensitive) as one already seen,
                // via a raw name that differs only by separator/leading-slash
                // style: exactly the ambiguity `find_part_key`'s tier-2
                // (canonical, case-sensitive) match papers over.
                meta_lossy = true;
            }
            if !seen_canon_ci.insert(canon.to_ascii_lowercase()) {
                // Same canonical form once also lowercased: the ambiguity
                // `find_part_key`'s tier-3 (case-insensitive canonical) match
                // papers over.
                meta_lossy = true;
            }
            order.push(name.clone());
            parts.insert(name, Part::Raw(data));
        }

        let (ctypes, ct_meta_lossy, ct_rels_injected) = match parts.get(CONTENT_TYPES) {
            Some(Part::Raw(bytes)) => match parse_content_types(bytes) {
                Some((ctypes, injected)) => (ctypes, false, injected),
                None => (ContentTypes::fallback(), true, false),
            },
            _ => (ContentTypes::fallback(), true, false),
        };
        meta_lossy = meta_lossy || ct_meta_lossy;

        let mut rels: HashMap<String, Vec<Rel>> = HashMap::new();
        for name in order.iter().filter(|n| is_rels_part(n)) {
            let Some(Part::Raw(bytes)) = parts.get(name) else {
                continue;
            };
            match parse_rels(bytes) {
                Some(entries) => {
                    rels.insert(name.clone(), entries);
                }
                None => meta_lossy = true,
            }
        }

        let rid_next = rels
            .values()
            .flatten()
            .filter_map(|r| r.id.strip_prefix("rId").and_then(|n| n.parse::<u64>().ok()))
            .max()
            .map_or(1, |m| m.saturating_add(1));

        Ok(Package {
            order,
            parts,
            ctypes,
            rels,
            rid_next,
            touched: HashSet::new(),
            complete,
            meta_lossy,
            ct_rels_injected,
        })
    }

    /// Serialize back to ZIP bytes. Validates only *touched* parts (see
    /// [`Package::touched_parts`]): every touched, still-present part must
    /// resolve a content type, and every touched `.rels` part's internal
    /// (non-external) relationship targets must resolve to a part that
    /// actually exists. Re-checks the entry/name/size/total budgets against
    /// the regenerated output. Emits `order` first (directories and files, in
    /// original ZIP order), then any part not in `order` (added this
    /// session), sorted alphabetically.
    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut touched: Vec<&String> = self.touched.iter().collect();
        touched.sort();

        // Validate against *live* content-types/relationship views derived
        // from each part's current bytes, not the `self.ctypes`/`self.rels`
        // caches parsed at `from_bytes` time: `replace_part` is a raw
        // passthrough (spreadsheet.rs's string-splicing edits go through it,
        // including on `[Content_Types].xml`/`.rels` parts themselves) that
        // intentionally does not keep those caches in sync, so validating
        // against the cache here could both false-positive (a stale rels
        // entry the raw edit already removed) and false-negative (a stale
        // content-types cache missing an override the raw edit just added).
        let live_ctypes = self.live_content_types();

        for name in &touched {
            if name.ends_with('/') || name.as_str() == CONTENT_TYPES {
                continue;
            }
            if !self.parts.contains_key(name.as_str()) {
                continue; // removed this session -- nothing to validate/emit
            }
            if !live_ctypes.resolves(name) {
                return Err(Error::Zip(
                    "OOXML package part has no resolvable content type",
                ));
            }
        }

        for name in &touched {
            if !is_rels_part(name) {
                continue;
            }
            let Some(part) = self.parts.get(name.as_str()) else {
                continue; // removed this session
            };
            let bytes = part.bytes();
            let entries =
                parse_rels(&bytes).ok_or(Error::Zip("touched .rels part is not well-formed"))?;
            let source_part = source_part_of_rels_path(name).ok_or(Error::Zip(
                "relationships part path is not a valid .rels path",
            ))?;
            for rel in &entries {
                if rel.external {
                    continue;
                }
                let target = Package::resolve_rel_target(&source_part, &rel.target);
                if !self.has_part(&target) {
                    return Err(Error::Zip(
                        "relationship in a touched .rels part targets a missing part",
                    ));
                }
            }
        }

        let in_order: HashSet<&str> = self.order.iter().map(String::as_str).collect();
        let mut extra: Vec<&String> = self
            .parts
            .keys()
            .filter(|k| !in_order.contains(k.as_str()))
            .collect();
        extra.sort();
        if self.order.len().saturating_add(extra.len()) > max_entries() {
            return Err(Error::Zip("OOXML package has too many entries"));
        }

        let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let mut total: usize = 0;

        for name in &self.order {
            if name.ends_with('/') {
                if name.len() > MAX_NAME_LEN {
                    return Err(Error::Zip("OOXML package entry name is too long"));
                }
                out.add_directory(name.as_str(), opt)
                    .map_err(|_| Error::Zip("failed to write OOXML package entry"))?;
            } else if let Some(part) = self.parts.get(name) {
                let data = part.bytes();
                write_part(&mut out, &mut total, name, &data, opt)?;
            }
        }
        for name in extra {
            if let Some(part) = self.parts.get(name) {
                let data = part.bytes();
                write_part(&mut out, &mut total, name, &data, opt)?;
            }
        }

        out.finish()
            .map(|c| c.into_inner())
            .map_err(|_| Error::Zip("failed to finish OOXML package"))
    }

    /// A part's current bytes, if present. Only returns `Some` when the part
    /// is still `Raw` (the common case for every part this batch's callers
    /// touch): a part promoted via [`Package::part_tree_mut`] would need a
    /// fresh serialization on every call to return owned bytes, which this
    /// signature — preserved unchanged from before `Package` grew tree-based
    /// editing — cannot express. Callers that may be looking at a promoted
    /// part should use [`Package::part_tree_ref`] instead.
    pub(crate) fn part_bytes(&self, name: &str) -> Option<&[u8]> {
        let key = self.find_part_key(name)?;
        match self.parts.get(key)?.bytes() {
            Cow::Borrowed(b) => Some(b),
            Cow::Owned(_) => None,
        }
    }

    /// Every part/directory name, in original ZIP order.
    pub(crate) fn part_names(&self) -> impl Iterator<Item = &str> {
        self.order.iter().map(String::as_str)
    }

    /// Replace an *existing* part's bytes wholesale (a raw passthrough — does
    /// not go through [`Package::part_tree_mut`] promotion). Errors if no part
    /// matches `name`. Returns the part's actual stored name, which may differ
    /// from `name` in case or leading-slash form.
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    pub(crate) fn replace_part(&mut self, name: &str, data: Vec<u8>) -> Result<String> {
        if data.len() > max_part() {
            return Err(Error::Zip("OOXML package entry is too large"));
        }
        let key = self
            .find_part_key(name)
            .cloned()
            .ok_or(Error::Zip("OOXML package part is missing"))?;
        self.touched.insert(key.clone());
        self.parts.insert(key.clone(), Part::Raw(data));
        Ok(key)
    }

    /// Remove a part if present, returning its actual stored name
    /// (case-preserved). The name may linger in the original-order list (see
    /// the `order` field doc); [`Package::to_bytes`] skips it there.
    pub(crate) fn remove_part(&mut self, name: &str) -> Option<String> {
        let key = self.find_part_key(name)?.clone();
        self.parts.remove(&key);
        Some(key)
    }

    /// Add or replace a part (unlike [`Package::replace_part`], this also
    /// *creates* a new part if `name` doesn't already exist), with
    /// case-insensitive existing-name lookup that preserves whatever casing
    /// the part was first stored under. Marks the part touched. When
    /// `content_type` is `Some`, also ensures `[Content_Types].xml` resolves
    /// `name` to it (see [`Package::ensure_content_type`]).
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    pub(crate) fn set_part(&mut self, name: &str, bytes: Vec<u8>, content_type: Option<&str>) {
        self.set_part_raw(name, bytes);
        if let Some(ct) = content_type {
            self.ensure_content_type(name, ct);
        }
    }

    /// Case-insensitive (and backslash/leading-slash-canonicalizing) existence
    /// check.
    pub(crate) fn has_part(&self, name: &str) -> bool {
        self.find_part_key(name).is_some()
    }

    /// Parts touched (added/replaced/promoted-and-edited) since this package
    /// was opened, sorted for deterministic reporting.
    pub(crate) fn touched_parts(&self) -> Vec<String> {
        let mut v: Vec<String> = self.touched.iter().cloned().collect();
        v.sort();
        v
    }

    /// `false` if [`Package::from_bytes`] had to skip an unreadable ZIP entry.
    pub(crate) fn is_complete(&self) -> bool {
        self.complete
    }

    /// `true` if `[Content_Types].xml`/a `.rels` part parsed lossily (or was
    /// absent, or a duplicate part name differed only by case).
    pub(crate) fn is_meta_lossy(&self) -> bool {
        self.meta_lossy
    }

    /// Promote `name` to an editable [`XmlTree`] (lazy: parsed on first call,
    /// cached as `Part::Xml` from then on), returning a mutable handle.
    /// Subsequent [`Package::to_bytes`]/[`Package::part_bytes`] reflect the
    /// edited tree for this part; every other part stays raw. Marks the part
    /// touched only *after* a successful parse, so a failed promotion (e.g.
    /// the part's bytes are not well-formed XML) leaves `touched` unchanged.
    pub(crate) fn part_tree_mut(&mut self, name: &str) -> Result<&mut XmlTree> {
        let key = self
            .find_part_key(name)
            .cloned()
            .ok_or(Error::Zip("OOXML package part is missing"))?;
        let entry = self
            .parts
            .get_mut(&key)
            .expect("key resolved by find_part_key must exist in parts");
        if let Part::Raw(bytes) = entry {
            *entry = Part::Xml(XmlTree::parse(bytes)?);
        }
        self.touched.insert(key);
        match entry {
            Part::Xml(t) => Ok(t),
            Part::Raw(_) => unreachable!("just promoted to Xml"),
        }
    }

    /// A non-promoting read of an already-promoted part's tree. `None` if the
    /// part doesn't exist or is still `Raw` — a caller wanting to peek at
    /// un-promoted content without forcing promotion should parse a throwaway
    /// [`XmlTree`] itself via [`Package::part_bytes`] + `XmlTree::parse`.
    pub(crate) fn part_tree_ref(&self, name: &str) -> Option<&XmlTree> {
        let key = self.find_part_key(name)?;
        match self.parts.get(key)? {
            Part::Xml(t) => Some(t),
            Part::Raw(_) => None,
        }
    }

    /// Ensure `[Content_Types].xml` resolves `part` to exactly
    /// `content_type`: a no-op if an `Override` for `part` already carries
    /// that exact content type, otherwise adds (replacing any existing
    /// `Override` for `part`, since OPC requires unique `PartName`s) one and
    /// regenerates `[Content_Types].xml`, marking it touched.
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    pub(crate) fn ensure_content_type(&mut self, part: &str, content_type: &str) {
        let pn = override_part_name(part);
        let already = self
            .ctypes
            .overrides
            .iter()
            .any(|(p, ct)| p.eq_ignore_ascii_case(&pn) && ct == content_type);
        if already {
            return;
        }
        self.ctypes
            .overrides
            .retain(|(p, _)| !p.eq_ignore_ascii_case(&pn));
        self.ctypes.overrides.push((pn, content_type.to_string()));
        self.regen_content_types();
    }

    /// Resolve an internal relationship target against `src_part`'s directory
    /// and normalize dot segments to a canonical package part name (no
    /// leading slash) suitable for [`Package::has_part`] lookup.
    pub(crate) fn resolve_rel_target(src_part: &str, target: &str) -> String {
        let base: Vec<&str> = if target.starts_with('/') {
            Vec::new()
        } else {
            src_part
                .rsplit_once('/')
                .map(|(dir, _)| dir.split('/').filter(|s| !s.is_empty()).collect())
                .unwrap_or_default()
        };
        let mut segs = base;
        for seg in target.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    segs.pop();
                }
                s => segs.push(s),
            }
        }
        segs.join("/")
    }

    /// The `Target` string to write into `src_part`'s `.rels` XML when adding
    /// a relationship to `new_part`: relative to `src_part`'s directory when
    /// `new_part` lives under it, else an absolute (`/`-rooted) path. The
    /// mirror-image write-direction companion of
    /// [`Package::resolve_rel_target`].
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    pub(crate) fn rel_target(src_part: &str, new_part: &str) -> String {
        let src_dir = src_part.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        if src_dir.is_empty() {
            return new_part.to_string();
        }
        if let Some(rest) = new_part.strip_prefix(&format!("{src_dir}/")) {
            return rest.to_string();
        }
        format!("/{new_part}")
    }

    /// The `.rels` package part name that stores `part`'s relationships
    /// (`xl/workbook.xml` -> `xl/_rels/workbook.xml.rels`; the package root
    /// (`""`) -> `_rels/.rels`).
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    pub(crate) fn rels_path_of(part: &str) -> String {
        match part.rsplit_once('/') {
            Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
            None if part.is_empty() => "_rels/.rels".to_string(),
            None => format!("_rels/{part}.rels"),
        }
    }

    /// `part`'s parsed relationships (empty if it has no `.rels` part, or that
    /// part failed to parse).
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    pub(crate) fn relationships_of(&self, part: &str) -> &[Rel] {
        self.rels
            .get(&Self::rels_path_of(part))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Add a new relationship from `src_part` to `target` (an internal
    /// package part name, or an external URI when `external` is `true`),
    /// allocating a fresh `rId`. Regenerates `src_part`'s `.rels` XML and
    /// marks it touched; if `[Content_Types].xml` was missing the mandatory
    /// `rels` default (see [`Package::ct_rels_injected`] in the struct docs),
    /// also regenerates `[Content_Types].xml` so that injected default
    /// actually reaches the output. Returns the allocated relationship id.
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    pub(crate) fn add_relationship(
        &mut self,
        src_part: &str,
        rel_type: &str,
        target: &str,
        external: bool,
    ) -> String {
        let rid = self.alloc_rid();
        let rels_path = Self::rels_path_of(src_part);
        self.rels.entry(rels_path.clone()).or_default().push(Rel {
            id: rid.clone(),
            rel_type: rel_type.to_string(),
            target: target.to_string(),
            external,
        });
        self.regen_rels(&rels_path);
        rid
    }

    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    fn alloc_rid(&mut self) -> String {
        let id = format!("rId{}", self.rid_next);
        self.rid_next = self.rid_next.saturating_add(1);
        id
    }

    /// Regenerate `[Content_Types].xml` from `self.ctypes` and store it,
    /// marking it touched. When `[Content_Types].xml` is already promoted to
    /// a live [`XmlTree`] (edited directly through [`Package::part_tree_mut`]
    /// -- e.g. `src/spreadsheet.rs`'s calc-chain-removal code promotes and
    /// edits `.rels`/Content-Types parts this same way), *merges*
    /// `self.ctypes`'s entries into that live tree in place instead of
    /// overwriting it wholesale: an unconditional raw rebuild here would
    /// silently discard any edit made through the tree API, with no error,
    /// the moment this function's caller next ran. See
    /// [`Package::merge_content_types_into_tree`].
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    fn regen_content_types(&mut self) {
        if let Some(key) = self.find_part_key(CONTENT_TYPES).cloned() {
            if matches!(self.parts.get(&key), Some(Part::Xml(_))) {
                self.merge_content_types_into_tree(&key);
                return;
            }
        }
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
        );
        for (ext, ct) in &self.ctypes.defaults {
            xml.push_str(&format!(
                r#"<Default Extension="{}" ContentType="{}"/>"#,
                esc_attr(ext),
                esc_attr(ct)
            ));
        }
        for (part, ct) in &self.ctypes.overrides {
            xml.push_str(&format!(
                r#"<Override PartName="{}" ContentType="{}"/>"#,
                esc_attr(part),
                esc_attr(ct)
            ));
        }
        xml.push_str("</Types>");
        self.set_part_raw(CONTENT_TYPES, xml.into_bytes());
    }

    /// Merge `self.ctypes`'s current defaults/overrides into `key`'s *live*
    /// tree in place: an existing `<Default>`/`<Override>` matching an
    /// entry's key (case-insensitively, matching OPC's own
    /// extension/part-name resolution -- see [`ContentTypes::resolves`]) has
    /// its `ContentType` updated if it differs; a missing one is appended as
    /// a new last child of the root `<Types>` element. Every other child
    /// already in the tree -- including one added directly through
    /// `part_tree_mut`, which is exactly what the old unconditional
    /// `set_part_raw` overwrite here used to silently discard -- is left
    /// untouched. A merge that cannot apply (node-budget/attribute-cap
    /// overflow from appending one small element -- effectively unreachable
    /// in practice) is swallowed rather than surfaced: `ensure_content_type`/
    /// `add_relationship` are an established infallible API, and adding a
    /// `Result` return here would ripple across every caller in the crate
    /// for a case this narrow.
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    fn merge_content_types_into_tree(&mut self, key: &str) {
        let defaults = self.ctypes.defaults.clone();
        let overrides = self.ctypes.overrides.clone();
        let Some(Part::Xml(tree)) = self.parts.get_mut(key) else {
            return;
        };
        let Some(root) = tree.root_element() else {
            return;
        };
        for (ext, ct) in &defaults {
            merge_child_element(tree, root, "Default", "Extension", ext, "ContentType", ct);
        }
        for (part_name, ct) in &overrides {
            merge_child_element(
                tree,
                root,
                "Override",
                "PartName",
                part_name,
                "ContentType",
                ct,
            );
        }
        self.touched.insert(key.to_string());
    }

    /// Regenerate `rels_path`'s XML from `self.rels[rels_path]` and store it,
    /// marking it touched. Merges into an already-promoted live [`XmlTree`]
    /// instead of overwriting it wholesale -- see
    /// [`Package::regen_content_types`]'s doc comment (same rationale, same
    /// hazard) and [`Package::merge_rels_into_tree`].
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    fn regen_rels(&mut self, rels_path: &str) {
        if let Some(key) = self.find_part_key(rels_path).cloned() {
            if matches!(self.parts.get(&key), Some(Part::Xml(_))) {
                self.merge_rels_into_tree(&key, rels_path);
                if self.ct_rels_injected {
                    self.regen_content_types();
                }
                return;
            }
        }
        let entries = self.rels.get(rels_path).cloned().unwrap_or_default();
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
        );
        for rel in &entries {
            xml.push_str(&format!(
                r#"<Relationship Id="{}" Type="{}" Target="{}""#,
                esc_attr(&rel.id),
                esc_attr(&rel.rel_type),
                esc_attr(&rel.target)
            ));
            if rel.external {
                xml.push_str(r#" TargetMode="External""#);
            }
            xml.push_str("/>");
        }
        xml.push_str("</Relationships>");
        self.set_part_raw(rels_path, xml.into_bytes());
        if self.ct_rels_injected {
            self.regen_content_types();
        }
    }

    /// Merge `self.rels[rels_path]`'s current entries into `key`'s *live*
    /// tree in place, matched by `Id` (relationship ids are opaque
    /// exact-match tokens this crate itself allocates via
    /// [`Package::alloc_rid`] -- unlike part names/extensions, no
    /// case-insensitive resolution applies): an existing `<Relationship>`
    /// with the same `Id` has its `Type`/`Target`/`TargetMode` updated in
    /// place if they differ; a missing one is appended as a new last child
    /// of the root `<Relationships>` element. Mirrors
    /// [`Package::merge_content_types_into_tree`]'s rationale and
    /// error-swallowing behavior.
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    fn merge_rels_into_tree(&mut self, key: &str, rels_path: &str) {
        let entries = self.rels.get(rels_path).cloned().unwrap_or_default();
        let Some(Part::Xml(tree)) = self.parts.get_mut(key) else {
            return;
        };
        let Some(root) = tree.root_element() else {
            return;
        };
        for rel in &entries {
            merge_relationship_element(tree, root, rel);
        }
        self.touched.insert(key.to_string());
    }

    /// Shared implementation of [`Package::set_part`] (minus the
    /// content-type-ensuring step): case-insensitive/canonical existing-name
    /// lookup preserving original casing, push into `order` only when the
    /// part is genuinely new, mark touched, store as `Part::Raw`. Returns the
    /// stored key.
    #[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
    fn set_part_raw(&mut self, name: &str, bytes: Vec<u8>) -> String {
        let key = self.find_part_key(name).cloned().unwrap_or_else(|| {
            self.order.push(name.to_string());
            name.to_string()
        });
        self.touched.insert(key.clone());
        self.parts.insert(key.clone(), Part::Raw(bytes));
        key
    }

    /// Resolve `name` to its actual stored key: an exact match first, then
    /// rxls's existing backslash-normalized/leading-slash-stripped canonical
    /// match, then a case-insensitive match on top of that canonicalization.
    ///
    /// The exact-match tier is inherently deterministic (a `HashMap` key can
    /// equal `name` at most once). The two canonical tiers are not: when
    /// *multiple* stored keys share the same canonical/lowercased-canonical
    /// form, the ambiguous-match tie-break must stay a pure function of the
    /// input bytes, so both walk `self.order` (source-order, a `Vec`)
    /// instead of `self.parts.keys()` (a `HashMap`, whose iteration order
    /// depends on the process's randomized hasher state, not on the input).
    fn find_part_key(&self, name: &str) -> Option<&String> {
        if let Some(k) = self.parts.keys().find(|k| k.as_str() == name) {
            return Some(k);
        }
        let canon = canonical_part_name(name);
        if let Some(k) = self.first_matching_key_in_order(|k| canonical_part_name(k) == canon) {
            return Some(k);
        }
        let canon_lower = canon.to_ascii_lowercase();
        self.first_matching_key_in_order(|k| {
            canonical_part_name(k).to_ascii_lowercase() == canon_lower
        })
    }

    /// Deterministic ambiguous-match helper for [`Package::find_part_key`]:
    /// the first key in `self.order` (source order) satisfying `pred` that
    /// still has a live entry in `self.parts` (a name may linger in `order`
    /// after [`Package::remove_part`]). Never consults `HashMap` iteration
    /// order.
    fn first_matching_key_in_order(&self, pred: impl Fn(&str) -> bool) -> Option<&String> {
        self.order
            .iter()
            .filter(|name| pred(name.as_str()))
            .find_map(|name| self.parts.get_key_value(name.as_str()))
            .map(|(k, _)| k)
    }

    /// A `[Content_Types].xml` view derived from that part's *current* bytes,
    /// used for validation (see [`Package::to_bytes`]'s doc comment on why
    /// this can't just be `self.ctypes`). Falls back to the cached
    /// `self.ctypes` if the current bytes fail to parse (so a validation pass
    /// still has a usable, if slightly stale, view rather than none at all),
    /// or to [`ContentTypes::fallback`] if the part is absent entirely.
    fn live_content_types(&self) -> ContentTypes {
        match self.parts.get(CONTENT_TYPES) {
            Some(part) => {
                let bytes = part.bytes();
                match parse_content_types(&bytes) {
                    Some((ctypes, _)) => ctypes,
                    None => self.ctypes.clone(),
                }
            }
            None => ContentTypes::fallback(),
        }
    }
}

fn write_part(
    out: &mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>,
    total: &mut usize,
    name: &str,
    data: &[u8],
    opt: SimpleFileOptions,
) -> Result<()> {
    if name.len() > MAX_NAME_LEN {
        return Err(Error::Zip("OOXML package entry name is too long"));
    }
    if data.len() > max_part() {
        return Err(Error::Zip("OOXML package entry is too large"));
    }
    *total = total
        .checked_add(data.len())
        .ok_or(Error::Zip("OOXML package is too large"))?;
    if *total > MAX_TOTAL {
        return Err(Error::Zip("OOXML package is too large"));
    }
    out.start_file(name, opt)
        .map_err(|_| Error::Zip("failed to write OOXML package entry"))?;
    out.write_all(data)
        .map_err(|_| Error::Zip("failed to write OOXML package entry"))?;
    Ok(())
}

fn canonical_part_name(name: &str) -> String {
    name.replace('\\', "/").trim_start_matches('/').to_string()
}

/// Ensure a `<tag key_attr="key_val" val_attr="val"/>`-shaped child of
/// `parent` exists: updates `val_attr` in place on the first existing child
/// named `tag` whose `key_attr` value matches `key_val` case-insensitively,
/// else appends a freshly built element as `parent`'s last child. Used by
/// [`Package::merge_content_types_into_tree`] for both `<Default
/// Extension=.. ContentType=..>` and `<Override PartName=.. ContentType=..>`
/// entries. See that function's doc comment for why a failure here (the
/// underlying tree edit erroring) is swallowed rather than propagated.
#[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
fn merge_child_element(
    tree: &mut XmlTree,
    parent: NodeId,
    tag: &str,
    key_attr: &str,
    key_val: &str,
    val_attr: &str,
    val: &str,
) {
    let children: Vec<NodeId> = tree.children_of(parent).to_vec();
    for id in children {
        if tree.element_name(id) == Some(tag.as_bytes()) {
            let matches = tree
                .attr_value(id, key_attr.as_bytes())
                .map(|v| String::from_utf8_lossy(v).eq_ignore_ascii_case(key_val))
                .unwrap_or(false);
            if matches {
                let _ = tree.set_attr(id, val_attr.as_bytes(), val.as_bytes());
                return;
            }
        }
    }
    let frag = format!(
        r#"<{tag} {key_attr}="{}" {val_attr}="{}"/>"#,
        esc_attr(key_val),
        esc_attr(val)
    );
    let idx = tree.children_of(parent).len();
    let _ = tree.insert_fragment_at(parent, idx, frag.as_bytes());
}

/// Ensure a `<Relationship Id="rel.id" .../>` child of `parent` exists
/// matching `rel`'s `Type`/`Target`/`TargetMode`: updates those attributes
/// in place on the first existing `<Relationship>` with the same `Id`, else
/// appends a freshly built element. Used by
/// [`Package::merge_rels_into_tree`]; see
/// [`merge_child_element`]/[`Package::merge_content_types_into_tree`]'s doc
/// comments for the shared error-swallowing rationale.
#[cfg_attr(not(test), allow(dead_code))] // consumed by upcoming roadmap slices; unit-tested
fn merge_relationship_element(tree: &mut XmlTree, parent: NodeId, rel: &Rel) {
    let children: Vec<NodeId> = tree.children_of(parent).to_vec();
    for id in children {
        if tree.element_name(id) == Some(b"Relationship")
            && tree.attr_value(id, b"Id") == Some(rel.id.as_bytes())
        {
            let _ = tree.set_attr(id, b"Type", rel.rel_type.as_bytes());
            let _ = tree.set_attr(id, b"Target", rel.target.as_bytes());
            if rel.external {
                let _ = tree.set_attr(id, b"TargetMode", b"External");
            } else {
                tree.remove_attr(id, b"TargetMode");
            }
            return;
        }
    }
    let mut frag = format!(
        r#"<Relationship Id="{}" Type="{}" Target="{}""#,
        esc_attr(&rel.id),
        esc_attr(&rel.rel_type),
        esc_attr(&rel.target)
    );
    if rel.external {
        frag.push_str(r#" TargetMode="External""#);
    }
    frag.push_str("/>");
    let idx = tree.children_of(parent).len();
    let _ = tree.insert_fragment_at(parent, idx, frag.as_bytes());
}

/// Whether `name` is a `.rels` part path: its filename ends with `.rels` and
/// its containing directory is (or ends with) `_rels`.
fn is_rels_part(name: &str) -> bool {
    match name.rsplit_once('/') {
        Some((dir, file)) => file.ends_with(".rels") && (dir == "_rels" || dir.ends_with("/_rels")),
        None => false,
    }
}

/// The package part a `.rels` path describes the relationships *of* — the
/// inverse of the `.rels`-path-construction convention: `_rels/.rels` -> `""`
/// (the package root); `xl/_rels/workbook.xml.rels` -> `xl/workbook.xml`.
fn source_part_of_rels_path(rels_path: &str) -> Option<String> {
    let (dir, file) = rels_path.rsplit_once('/')?;
    let parent_dir = dir.strip_suffix("_rels")?.trim_end_matches('/');
    let part_name = file.strip_suffix(".rels")?;
    if part_name.is_empty() {
        Some(parent_dir.to_string())
    } else if parent_dir.is_empty() {
        Some(part_name.to_string())
    } else {
        Some(format!("{parent_dir}/{part_name}"))
    }
}

/// Local (namespace-prefix-stripped) name of a qualified XML name.
fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|&b| b == b':').next().unwrap_or(name)
}

fn attr_value(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if local_name(a.key.as_ref()) == key {
            a.unescape_value().ok().map(|v| v.into_owned())
        } else {
            None
        }
    })
}

/// Leniently parse `[Content_Types].xml` bytes. `None` on any malformed XML
/// or a `Default`/`Override` missing a required attribute (the caller treats
/// that as `meta_lossy`, not a hard error). On success, also reports whether
/// the mandatory `Default Extension="rels"` was missing and had to be
/// injected (see the `ct_rels_injected` field doc on [`Package`]).
fn parse_content_types(bytes: &[u8]) -> Option<(ContentTypes, bool)> {
    let mut reader = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut defaults = Vec::new();
    let mut overrides = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) => match local_name(e.name().as_ref()) {
                b"Default" => {
                    let ext = attr_value(&e, b"Extension")?;
                    let ct = attr_value(&e, b"ContentType")?;
                    defaults.push((ext.to_ascii_lowercase(), ct));
                }
                b"Override" => {
                    let pn = attr_value(&e, b"PartName")?;
                    let ct = attr_value(&e, b"ContentType")?;
                    overrides.push((pn, ct));
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => return None,
        }
        buf.clear();
    }
    let injected = if defaults.iter().any(|(ext, _)| ext == "rels") {
        false
    } else {
        defaults.push(("rels".to_string(), crate::write::xml::CT_RELS.to_string()));
        true
    };
    Some((
        ContentTypes {
            defaults,
            overrides,
        },
        injected,
    ))
}

/// Leniently parse a `.rels` part's `<Relationship>` entries. `None` on any
/// malformed XML or a `Relationship` missing a required attribute.
fn parse_rels(bytes: &[u8]) -> Option<Vec<Rel>> {
    let mut reader = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut rels = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) => {
                if local_name(e.name().as_ref()) == b"Relationship" {
                    let id = attr_value(&e, b"Id")?;
                    let rel_type = attr_value(&e, b"Type")?;
                    let target = attr_value(&e, b"Target")?;
                    let external = attr_value(&e, b"TargetMode").as_deref() == Some("External");
                    rels.push(Rel {
                        id,
                        rel_type,
                        target,
                        external,
                    });
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
        buf.clear();
    }
    Some(rels)
}

/// Best-effort pre-flight: reject a ZIP whose End-Of-Central-Directory
/// declares more entries than [`max_entries`] *before* `ZipArchive::new`
/// parses (and allocates per-entry state for) the central directory. If the
/// EOCD can't be located/parsed, the check is silently skipped — it's
/// backstopped by the post-construction `zip.len() > max_entries()` check.
fn check_zip_entry_budget(bytes: &[u8]) -> Result<()> {
    if let Some(count) = eocd_entry_count(bytes) {
        if count > max_entries() as u64 {
            return Err(Error::Zip("OOXML package has too many entries"));
        }
    }
    Ok(())
}

/// Hand-parse the ZIP End-Of-Central-Directory record (falling back to the
/// ZIP64 EOCD when the classic record signals `0xFFFF`, meaning "see ZIP64")
/// to read the declared total entry count, without using the `zip` crate
/// (which would itself allocate central-directory state for every entry).
fn eocd_entry_count(bytes: &[u8]) -> Option<u64> {
    const EOCD_SIG: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
    const EOCD_MIN: usize = 22;
    if bytes.len() < EOCD_MIN {
        return None;
    }
    let max_comment = 65_535usize;
    let search_start = bytes.len().saturating_sub(EOCD_MIN + max_comment);
    let window = &bytes[search_start..];
    let sig_pos = window.windows(4).rposition(|w| w == EOCD_SIG)?;
    let eocd_off = search_start + sig_pos;
    if eocd_off + EOCD_MIN > bytes.len() {
        return None;
    }
    let entries16 = u16::from_le_bytes([bytes[eocd_off + 10], bytes[eocd_off + 11]]) as u64;
    if entries16 != 0xFFFF {
        return Some(entries16);
    }

    // ZIP64: the classic EOCD is a stub; the real count lives in the ZIP64
    // EOCD record, located via the ZIP64 EOCD Locator immediately preceding
    // the classic EOCD.
    const ZIP64_LOC_SIG: [u8; 4] = [0x50, 0x4B, 0x06, 0x07];
    const ZIP64_LOC_LEN: usize = 20;
    if eocd_off < ZIP64_LOC_LEN {
        return None;
    }
    let loc_off = eocd_off - ZIP64_LOC_LEN;
    if bytes.get(loc_off..loc_off + 4)? != ZIP64_LOC_SIG {
        return None;
    }
    let zip64_eocd_off = u64::from_le_bytes(bytes.get(loc_off + 8..loc_off + 16)?.try_into().ok()?);
    let zip64_eocd_off = usize::try_from(zip64_eocd_off).ok()?;

    const ZIP64_EOCD_SIG: [u8; 4] = [0x50, 0x4B, 0x06, 0x06];
    if zip64_eocd_off.checked_add(56)? > bytes.len() {
        return None;
    }
    if bytes.get(zip64_eocd_off..zip64_eocd_off + 4)? != ZIP64_EOCD_SIG {
        return None;
    }
    let entries64 = u64::from_le_bytes(
        bytes
            .get(zip64_eocd_off + 32..zip64_eocd_off + 40)?
            .try_into()
            .ok()?,
    );
    Some(entries64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::xml::{CT_CORE_PROPS, CT_SST, CT_STYLES, CT_WORKBOOK};

    fn zip_bytes(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = zip::write::SimpleFileOptions::default();
        for (name, bytes) in parts {
            zip.start_file(*name, opt).unwrap();
            std::io::Write::write_all(&mut zip, bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn minimal_xlsx() -> Vec<u8> {
        zip_bytes(&[
            (
                "[Content_Types].xml",
                br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                br#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#,
            ),
        ])
    }

    // --- Preserved public surface (spreadsheet.rs / report.rs call sites) ---

    #[test]
    fn raw_package_roundtrip_preserves_part_payloads() {
        let input = zip_bytes(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("xl/workbook.xml", b"<workbook/>"),
            ("xl/vbaProject.bin", b"macro bytes"),
        ]);

        let package = Package::from_bytes(&input).unwrap();
        let output = package.to_bytes().unwrap();
        let reread = Package::from_bytes(&output).unwrap();

        assert_eq!(
            reread.part_bytes("xl/vbaProject.bin"),
            Some(&b"macro bytes"[..])
        );
        assert_eq!(
            reread.part_bytes("/xl/workbook.xml"),
            Some(&b"<workbook/>"[..])
        );
    }

    #[test]
    fn raw_package_rejects_oversized_part_names() {
        let name = format!("{}.xml", "x".repeat(4096));
        let input = zip_bytes(&[(&name, b"x")]);

        assert!(Package::from_bytes(&input).is_err());
    }

    #[test]
    fn part_names_returns_original_zip_order() {
        let input = zip_bytes(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("_rels/.rels", b"<Relationships/>"),
            ("xl/workbook.xml", b"<workbook/>"),
        ]);
        let package = Package::from_bytes(&input).unwrap();
        assert_eq!(
            package.part_names().collect::<Vec<_>>(),
            vec!["[Content_Types].xml", "_rels/.rels", "xl/workbook.xml"]
        );
    }

    #[test]
    fn replace_part_errors_when_part_missing() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        assert!(package
            .replace_part("xl/missing.xml", b"x".to_vec())
            .is_err());
    }

    #[test]
    fn replace_part_is_case_insensitive_and_returns_stored_name() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        let stored = package
            .replace_part("XL/WORKBOOK.XML", b"<workbook edited=\"1\"/>".to_vec())
            .unwrap();
        assert_eq!(stored, "xl/workbook.xml");
        assert_eq!(
            package.part_bytes("xl/workbook.xml"),
            Some(&b"<workbook edited=\"1\"/>"[..])
        );
    }

    #[test]
    fn remove_part_returns_stored_name_case_preserved_and_drops_from_output() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        let removed = package.remove_part("XL/WORKBOOK.XML").unwrap();
        assert_eq!(removed, "xl/workbook.xml");
        assert!(package.remove_part("xl/workbook.xml").is_none());

        let bytes = package.to_bytes().unwrap();
        let reread = Package::from_bytes(&bytes).unwrap();
        assert!(reread.part_bytes("xl/workbook.xml").is_none());
    }

    // --- from_bytes: budgets ---

    #[test]
    fn from_bytes_rejects_over_budget_part_size() {
        set_test_max_part(4);
        let input = zip_bytes(&[("xl/workbook.xml", b"toolong")]);
        let result = Package::from_bytes(&input);
        reset_test_max_part();
        assert!(result.is_err());
    }

    #[test]
    fn from_bytes_rejects_over_budget_entry_count() {
        set_test_max_entries(1);
        let input = zip_bytes(&[("a.xml", b"1"), ("b.xml", b"2")]);
        let result = Package::from_bytes(&input);
        reset_test_max_entries();
        assert!(result.is_err());
    }

    #[test]
    fn eocd_entry_count_reads_a_real_small_archive() {
        let bytes = zip_bytes(&[("a.xml", b"1"), ("b.xml", b"2"), ("c.xml", b"3")]);
        assert_eq!(eocd_entry_count(&bytes), Some(3));
    }

    #[test]
    fn eocd_preflight_rejects_before_zip_archive_parses() {
        set_test_max_entries(0);
        let input = zip_bytes(&[("a.xml", b"1")]);
        let result = Package::from_bytes(&input);
        reset_test_max_entries();
        assert!(result.is_err());
    }

    // --- from_bytes: entry handling ---

    #[test]
    fn directory_entries_round_trip() {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = zip::write::SimpleFileOptions::default();
        zip.add_directory("xl/", opt).unwrap();
        zip.start_file("xl/workbook.xml", opt).unwrap();
        std::io::Write::write_all(&mut zip, b"<workbook/>").unwrap();
        let input = zip.finish().unwrap().into_inner();

        let package = Package::from_bytes(&input).unwrap();
        assert!(package.part_names().any(|n| n == "xl/"));
        let output = package.to_bytes().unwrap();
        let reread = Package::from_bytes(&output).unwrap();
        assert!(reread.part_names().any(|n| n == "xl/"));
        assert_eq!(
            reread.part_bytes("xl/workbook.xml"),
            Some(&b"<workbook/>"[..])
        );
    }

    // Note: a genuine byte-level exact-duplicate ZIP entry name (two central
    // directory records with the identical name) can't be produced through
    // `zip::ZipWriter` — `start_file` itself rejects a repeated name
    // ("Duplicate filename") — so the from_bytes dedup branch that keeps the
    // last entry's data under one `order`/`parts` slot is defensive code for
    // hostile/non-conformant archives this crate's own writer can't create,
    // and isn't covered by a fixture-based test here.

    #[test]
    fn case_colliding_part_names_set_meta_lossy_but_keep_both() {
        let input = zip_bytes(&[("xl/Sheet.xml", b"upper"), ("xl/sheet.xml", b"lower")]);
        let package = Package::from_bytes(&input).unwrap();
        assert!(package.is_meta_lossy());
        assert_eq!(package.part_bytes("xl/Sheet.xml"), Some(&b"upper"[..]));
        assert_eq!(package.part_bytes("xl/sheet.xml"), Some(&b"lower"[..]));
    }

    #[test]
    fn canonically_colliding_raw_distinct_part_names_set_meta_lossy() {
        // Two raw entry names that differ ONLY by separator style
        // (`/` vs `\`) canonicalize to the exact same name
        // ("xl/Sheet.xml"), which is exactly the ambiguity
        // `find_part_key`'s tier-2 (canonical, case-sensitive) match papers
        // over -- neither `seen` (exact raw name) nor `seen_ci` (lowercased
        // raw name, separators untouched) catches this, since the raw
        // strings never collide even after lowercasing. `is_meta_lossy` must
        // fire anyway, since a package with this ambiguity can produce
        // different output bytes across runs. A well-formed
        // `[Content_Types].xml` is included so `meta_lossy` can't come from
        // that unrelated (missing-content-types) path instead -- this
        // isolates the canonical-collision signal specifically.
        let input = zip_bytes(&[
            (
                "[Content_Types].xml",
                br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/></Types>"#,
            ),
            ("xl/Sheet.xml", b"first"),
            (r"xl\Sheet.xml", b"second"),
        ]);
        let package = Package::from_bytes(&input).unwrap();
        assert!(package.is_meta_lossy());
        // Both entries are still individually retained (real content is not
        // dropped) -- only the safety signal changes.
        assert!(package.parts.contains_key("xl/Sheet.xml"));
        assert!(package.parts.contains_key(r"xl\Sheet.xml"));
    }

    #[test]
    fn find_part_key_ambiguous_tier_resolves_the_same_key_every_time() {
        // Same fixture as `canonically_colliding_raw_distinct_part_names_set_meta_lossy`:
        // both raw names canonicalize to "xl/Sheet.xml", so a query that
        // doesn't exact-match either raw key (tier 1) falls into the
        // ambiguous tier-2 (canonical, case-sensitive) match, where more than
        // one stored key qualifies. Building a *fresh* `Package` from the
        // byte-identical input on every iteration exercises a fresh
        // `HashMap` (and thus a fresh randomized hasher instance) each time;
        // before the fix, which physical part won was a function of that
        // hasher state, not of the input bytes, so this loop could observe
        // different winners across iterations of the same process. After the
        // fix (walking `self.order`, not `self.parts.keys()`), the first
        // entry in source order ("xl/Sheet.xml") must win every time.
        let input = zip_bytes(&[("xl/Sheet.xml", b"first"), (r"xl\Sheet.xml", b"second")]);

        let mut winners: HashSet<String> = HashSet::new();
        for _ in 0..500 {
            let package = Package::from_bytes(&input).unwrap();
            // "/xl/Sheet.xml" exact-matches neither raw key, so this query
            // only resolves via the ambiguous canonical tier.
            let key = package.find_part_key("/xl/Sheet.xml").unwrap().clone();
            winners.insert(key);
        }
        assert_eq!(
            winners,
            HashSet::from(["xl/Sheet.xml".to_string()]),
            "ambiguous canonical match must deterministically resolve to the \
             first key in source order every time, not vary by hasher state"
        );
    }

    #[test]
    fn from_bytes_marks_incomplete_when_an_original_part_is_a_symlink() {
        // `ZipFile::is_file()` is `false` for both directories and symlinks;
        // by the time `from_bytes`'s loop reaches its `!file.is_file()`
        // branch for a non-directory-named entry, a symlink is the only way
        // to hit it. That branch must clear `complete` (like its two sibling
        // skip paths) so a package missing an original part -- because it
        // was a symlink, not a regular file -- doesn't falsely report
        // `is_complete() == true` and get treated as safe to
        // preservation-edit.
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = zip::write::SimpleFileOptions::default();
        zip.add_symlink("xl/workbook.xml", "somewhere/else.xml", opt)
            .unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let package = Package::from_bytes(&bytes).unwrap();
        assert!(!package.is_complete());
        assert!(!package.has_part("xl/workbook.xml"));
    }

    #[test]
    fn from_zip_marks_incomplete_when_entry_data_is_corrupted() {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("[Content_Types].xml", opt).unwrap();
        std::io::Write::write_all(&mut zip, b"<Types/>").unwrap();
        zip.start_file("xl/workbook.xml", opt).unwrap();
        std::io::Write::write_all(&mut zip, b"CORRUPTME").unwrap();
        let mut bytes = zip.finish().unwrap().into_inner();

        // Flip one payload byte of the second entry's stored (uncompressed)
        // content so its CRC32 no longer matches the central-directory
        // checksum, without touching ZIP structure.
        let pos = bytes
            .windows(b"CORRUPTME".len())
            .rposition(|w| w == b"CORRUPTME")
            .unwrap();
        bytes[pos] ^= 0xFF;

        let package = Package::from_bytes(&bytes).unwrap();
        assert!(!package.is_complete());
        assert_eq!(
            package.part_bytes("[Content_Types].xml"),
            Some(&b"<Types/>"[..])
        );
    }

    // --- [Content_Types].xml / .rels leniency ---

    #[test]
    fn content_types_missing_entirely_sets_meta_lossy_and_still_opens() {
        let input = zip_bytes(&[("xl/workbook.xml", b"<workbook/>")]);
        let package = Package::from_bytes(&input).unwrap();
        assert!(package.is_meta_lossy());
        assert_eq!(
            package.part_bytes("xl/workbook.xml"),
            Some(&b"<workbook/>"[..])
        );
    }

    #[test]
    fn content_types_malformed_sets_meta_lossy_but_still_opens() {
        let input = zip_bytes(&[
            ("[Content_Types].xml", b"<Types><Default Extension"), // truncated/malformed
            ("xl/workbook.xml", b"<workbook/>"),
        ]);
        let package = Package::from_bytes(&input).unwrap();
        assert!(package.is_meta_lossy());
    }

    #[test]
    fn rels_malformed_sets_meta_lossy_but_still_opens() {
        let input = zip_bytes(&[
            ("[Content_Types].xml", b"<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/></Types>"),
            ("_rels/.rels", b"<Relationships><Relationship"), // truncated
        ]);
        let package = Package::from_bytes(&input).unwrap();
        assert!(package.is_meta_lossy());
    }

    #[test]
    fn ct_rels_default_missing_is_injected_not_meta_lossy() {
        let input = zip_bytes(&[(
            "[Content_Types].xml",
            br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#,
        )]);
        let package = Package::from_bytes(&input).unwrap();
        assert!(!package.is_meta_lossy());
        assert!(package.ct_rels_injected);
    }

    #[test]
    fn rid_next_seeds_above_existing_rids() {
        let input = zip_bytes(&[(
            "xl/_rels/workbook.xml.rels",
            br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId5" Type="t" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="t" Target="styles.xml"/></Relationships>"#,
        )]);
        let package = Package::from_bytes(&input).unwrap();
        assert_eq!(package.rid_next, 6);
    }

    // --- part_tree_mut / part_tree_ref (lazy promotion) ---

    #[test]
    fn part_tree_mut_promotes_raw_to_xml_and_marks_touched() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        assert!(package.touched_parts().is_empty());
        assert!(package.part_tree_ref("xl/workbook.xml").is_none());

        let tree = package.part_tree_mut("xl/workbook.xml").unwrap();
        let root = tree.root_element().unwrap();
        tree.set_attr(root, b"edited", b"1").unwrap();

        assert_eq!(package.touched_parts(), vec!["xl/workbook.xml".to_string()]);
        assert!(package.part_tree_ref("xl/workbook.xml").is_some());
        let bytes = package.part_bytes("xl/workbook.xml");
        assert!(bytes.is_none(), "a promoted part cannot borrow fresh bytes");
    }

    #[test]
    fn part_tree_mut_failed_parse_leaves_touched_unchanged() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        package.set_part("xl/broken.xml", b"<a><b>".to_vec(), None);
        // `set_part` itself marks touched; clear it to isolate what
        // `part_tree_mut` does on a failed promotion.
        package.touched.clear();

        let err = package.part_tree_mut("xl/broken.xml");
        assert!(err.is_err());
        assert!(package.touched_parts().is_empty());
        // Still raw and readable.
        assert_eq!(package.part_bytes("xl/broken.xml"), Some(&b"<a><b>"[..]));
    }

    #[test]
    fn part_tree_ref_never_promotes() {
        let package = Package::from_bytes(&minimal_xlsx()).unwrap();
        assert!(package.part_tree_ref("xl/workbook.xml").is_none());
        assert!(package.touched_parts().is_empty());
    }

    #[test]
    fn part_tree_mut_missing_part_errors() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        assert!(package.part_tree_mut("xl/missing.xml").is_err());
    }

    // --- set_part / has_part / touched_parts ---

    #[test]
    fn set_part_adds_new_part_and_ensures_content_type() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        package.set_part("xl/styles.xml", b"<styleSheet/>".to_vec(), Some(CT_STYLES));

        assert!(package.has_part("xl/styles.xml"));
        assert_eq!(
            package.part_bytes("xl/styles.xml"),
            Some(&b"<styleSheet/>"[..])
        );
        assert!(package
            .touched_parts()
            .contains(&"xl/styles.xml".to_string()));
        assert!(package
            .touched_parts()
            .contains(&"[Content_Types].xml".to_string()));

        let saved = package.to_bytes().unwrap();
        let ct = Package::from_bytes(&saved)
            .unwrap()
            .part_bytes(CONTENT_TYPES)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap();
        assert!(ct.contains(CT_STYLES));
        assert!(ct.contains("/xl/styles.xml"));
    }

    #[test]
    fn set_part_case_insensitive_preserves_original_casing() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        package.set_part(
            "XL/Workbook.XML",
            b"<workbook edited=\"1\"/>".to_vec(),
            None,
        );
        // Original casing ("xl/workbook.xml") must win, not the caller's spelling.
        assert_eq!(package.touched_parts(), vec!["xl/workbook.xml".to_string()]);
        assert_eq!(
            package.part_bytes("xl/workbook.xml"),
            Some(&b"<workbook edited=\"1\"/>"[..])
        );
    }

    #[test]
    fn has_part_is_case_and_slash_insensitive() {
        let package = Package::from_bytes(&minimal_xlsx()).unwrap();
        assert!(package.has_part("xl/workbook.xml"));
        assert!(package.has_part("/xl/workbook.xml"));
        assert!(package.has_part("XL/WORKBOOK.XML"));
        assert!(package.has_part(r"xl\workbook.xml"));
        assert!(!package.has_part("xl/missing.xml"));
    }

    #[test]
    fn touched_parts_returns_sorted_touched_set() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        package.set_part("xl/z.xml", b"<a/>".to_vec(), None);
        package.set_part("xl/a.xml", b"<a/>".to_vec(), None);
        assert_eq!(
            package.touched_parts(),
            vec!["xl/a.xml".to_string(), "xl/z.xml".to_string()]
        );
    }

    // --- ensure_content_type ---

    #[test]
    fn ensure_content_type_is_idempotent_and_marks_content_types_touched() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        // Already resolved (an Override for this exact part+type exists).
        package.ensure_content_type("xl/workbook.xml", CT_WORKBOOK);
        assert!(
            package.touched_parts().is_empty(),
            "no-op must not touch anything"
        );

        package.ensure_content_type("xl/sharedStrings.xml", CT_SST);
        assert_eq!(
            package.touched_parts(),
            vec!["[Content_Types].xml".to_string()]
        );

        // Calling again with the same (part, type) is a no-op the second time.
        let before = package.to_bytes().unwrap();
        package.ensure_content_type("xl/sharedStrings.xml", CT_SST);
        let after = package.to_bytes().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn ensure_content_type_replaces_a_conflicting_existing_override() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        package.ensure_content_type("xl/workbook.xml", CT_CORE_PROPS); // deliberately wrong type
        let bytes = package.to_bytes().unwrap();
        let ct = Package::from_bytes(&bytes)
            .unwrap()
            .part_bytes(CONTENT_TYPES)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap();
        assert!(ct.contains(CT_CORE_PROPS));
        assert!(!ct.contains(CT_WORKBOOK));
        assert_eq!(ct.matches("PartName=\"/xl/workbook.xml\"").count(), 1);
    }

    // --- resolve_rel_target / rel_target ---

    #[test]
    fn resolve_rel_target_handles_relative_dotdot_and_absolute() {
        assert_eq!(
            Package::resolve_rel_target("xl/workbook.xml", "worksheets/sheet1.xml"),
            "xl/worksheets/sheet1.xml"
        );
        assert_eq!(
            Package::resolve_rel_target("xl/worksheets/sheet1.xml", "../styles.xml"),
            "xl/styles.xml"
        );
        assert_eq!(
            Package::resolve_rel_target("xl/workbook.xml", "/xl/styles.xml"),
            "xl/styles.xml"
        );
        assert_eq!(
            Package::resolve_rel_target("", "xl/workbook.xml"),
            "xl/workbook.xml"
        );
    }

    #[test]
    fn rel_target_computes_relative_or_absolute_path() {
        assert_eq!(
            Package::rel_target("xl/workbook.xml", "xl/worksheets/sheet1.xml"),
            "worksheets/sheet1.xml"
        );
        assert_eq!(
            Package::rel_target("xl/workbook.xml", "docProps/core.xml"),
            "/docProps/core.xml"
        );
        assert_eq!(
            Package::rel_target("", "xl/workbook.xml"),
            "xl/workbook.xml"
        );
    }

    #[test]
    fn resolve_and_rel_target_round_trip() {
        let target_str = Package::rel_target("xl/workbook.xml", "xl/worksheets/sheet1.xml");
        assert_eq!(
            Package::resolve_rel_target("xl/workbook.xml", &target_str),
            "xl/worksheets/sheet1.xml"
        );
    }

    // --- relationships_of / add_relationship ---

    #[test]
    fn add_relationship_allocates_rid_and_regenerates_rels_part() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        package.set_part("xl/styles.xml", b"<styleSheet/>".to_vec(), None);
        let rid = package.add_relationship(
            "xl/workbook.xml",
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles",
            "styles.xml",
            false,
        );
        // rId1 is already used by `_rels/.rels`'s officeDocument relationship,
        // so `rid_next` (seeded above every existing rId in the package) starts at 2.
        assert_eq!(rid, "rId2");
        let rels = package.relationships_of("xl/workbook.xml");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].id, rid);
        assert_eq!(rels[0].target, "styles.xml");
        assert!(!rels[0].external);
        assert!(package
            .touched_parts()
            .contains(&"xl/_rels/workbook.xml.rels".to_string()));

        let bytes = package.to_bytes().unwrap();
        let reread = Package::from_bytes(&bytes).unwrap();
        assert_eq!(reread.relationships_of("xl/workbook.xml").len(), 1);
    }

    #[test]
    fn add_relationship_forces_content_types_regen_when_rels_default_was_injected() {
        let input = zip_bytes(&[(
            "[Content_Types].xml",
            br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#,
        )]);
        let mut package = Package::from_bytes(&input).unwrap();
        assert!(package.ct_rels_injected);
        package.set_part("xl/styles.xml", b"<styleSheet/>".to_vec(), None);
        package.add_relationship("xl/workbook.xml", "t", "styles.xml", false);
        assert!(package
            .touched_parts()
            .contains(&"[Content_Types].xml".to_string()));
        let bytes = package.to_bytes().unwrap();
        let ct = Package::from_bytes(&bytes)
            .unwrap()
            .part_bytes(CONTENT_TYPES)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap();
        assert!(ct.contains("Extension=\"rels\""));
    }

    #[test]
    fn relationships_of_root_package_reads_dot_rels() {
        let package = Package::from_bytes(&minimal_xlsx()).unwrap();
        let rels = package.relationships_of("");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].target, "xl/workbook.xml");
    }

    #[test]
    fn add_relationship_after_direct_tree_edit_does_not_clobber_the_tree_edit() {
        // Reproduces the exact failure scenario: `_rels/.rels` is promoted
        // and edited directly via `part_tree_mut` (a real, already-used
        // pattern -- `src/spreadsheet.rs`'s calc-chain-removal code promotes
        // and edits `.rels`/Content-Types parts this same way), inserting a
        // new `<Relationship>` child straight onto the live tree. Then
        // `add_relationship` is called for that SAME rels path. Before the
        // fix, `add_relationship` -> `regen_rels` rebuilt the part purely
        // from the stale `self.rels` cache and overwrote `self.parts[key]`
        // wholesale via `set_part_raw`, silently discarding the
        // tree-inserted relationship with no error.
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();

        let tree = package.part_tree_mut("_rels/.rels").unwrap();
        let root = tree.root_element().unwrap();
        let idx = tree.children_of(root).len();
        tree.insert_fragment_at(
            root,
            idx,
            br#"<Relationship Id="rIdTreeEdit" Type="t" Target="xl/workbook.xml"/>"#,
        )
        .unwrap();

        // Confirm it's live before `add_relationship` is ever called.
        let live = package.part_tree_ref("_rels/.rels").unwrap().serialize();
        assert!(
            String::from_utf8_lossy(&live).contains("rIdTreeEdit"),
            "tree edit must be live before add_relationship is called"
        );

        let rid = package.add_relationship("", "t2", "xl/workbook.xml", false);

        let bytes = package.to_bytes().unwrap();
        let reread = Package::from_bytes(&bytes).unwrap();
        let rels = reread.relationships_of("");
        assert!(
            rels.iter().any(|r| r.id == "rIdTreeEdit"),
            "the direct tree edit must survive to_bytes() output, got {rels:?}"
        );
        assert!(
            rels.iter().any(|r| r.id == rid),
            "the add_relationship-added relationship must also be present, got {rels:?}"
        );
    }

    #[test]
    fn ensure_content_type_after_direct_tree_edit_does_not_clobber_the_tree_edit() {
        // Same scenario as
        // `add_relationship_after_direct_tree_edit_does_not_clobber_the_tree_edit`,
        // for the `[Content_Types].xml` / `ensure_content_type` sibling call
        // path (`ensure_content_type` -> `regen_content_types`).
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();

        let tree = package.part_tree_mut(CONTENT_TYPES).unwrap();
        let root = tree.root_element().unwrap();
        let idx = tree.children_of(root).len();
        tree.insert_fragment_at(
            root,
            idx,
            br#"<Override PartName="/xl/treeEdit.xml" ContentType="application/tree-edit"/>"#,
        )
        .unwrap();

        let live = package.part_tree_ref(CONTENT_TYPES).unwrap().serialize();
        assert!(
            String::from_utf8_lossy(&live).contains("treeEdit"),
            "tree edit must be live before ensure_content_type is called"
        );

        package.ensure_content_type("xl/sharedStrings.xml", CT_SST);

        let bytes = package.to_bytes().unwrap();
        let ct = Package::from_bytes(&bytes)
            .unwrap()
            .part_bytes(CONTENT_TYPES)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap();
        assert!(
            ct.contains("treeEdit"),
            "the direct tree edit must survive to_bytes() output, got {ct:?}"
        );
        assert!(
            ct.contains("/xl/sharedStrings.xml") && ct.contains(CT_SST),
            "the ensure_content_type-added override must also be present, got {ct:?}"
        );
    }

    // --- to_bytes validation ---

    #[test]
    fn to_bytes_rejects_touched_part_with_unresolvable_content_type() {
        let mut package = Package::from_bytes(&zip_bytes(&[(
            "[Content_Types].xml",
            br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/></Types>"#,
        )]))
        .unwrap();
        // No `Default Extension="xml"`/Override, so a touched `.bin` (no dot
        // matches nothing either) part has no resolvable content type.
        package.replace_part_or_add("xl/unknownpart", b"data".to_vec());
        assert!(package.to_bytes().is_err());
    }

    #[test]
    fn to_bytes_allows_untouched_part_with_unresolvable_content_type() {
        let input = zip_bytes(&[
            (
                "[Content_Types].xml",
                br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/></Types>"#,
            ),
            ("xl/mystery", b"untouched original data"),
        ]);
        let package = Package::from_bytes(&input).unwrap();
        // Untouched -- passthrough trust, even though `[Content_Types].xml`
        // has no Default/Override that resolves it.
        let bytes = package.to_bytes().unwrap();
        let reread = Package::from_bytes(&bytes).unwrap();
        assert_eq!(
            reread.part_bytes("xl/mystery"),
            Some(&b"untouched original data"[..])
        );
    }

    #[test]
    fn to_bytes_rejects_touched_rels_with_dangling_target() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        package.add_relationship("xl/workbook.xml", "t", "worksheets/missing.xml", false);
        assert!(package.to_bytes().is_err());
    }

    #[test]
    fn to_bytes_allows_untouched_rels_with_preexisting_dangling_target() {
        let input = zip_bytes(&[
            (
                "[Content_Types].xml",
                br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/></Types>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="t" Target="worksheets/missing.xml"/></Relationships>"#,
            ),
            ("xl/workbook.xml", b"<workbook/>"),
        ]);
        let package = Package::from_bytes(&input).unwrap();
        // Untouched .rels with an already-dangling target is trusted as-is.
        assert!(package.to_bytes().is_ok());
    }

    #[test]
    fn to_bytes_rejects_over_budget_entry_count_on_regenerated_output() {
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        set_test_max_entries(3); // exactly the current count in minimal_xlsx()
        package.set_part("xl/extra.xml", b"<a/>".to_vec(), None);
        let result = package.to_bytes();
        reset_test_max_entries();
        assert!(result.is_err());
    }

    #[test]
    fn to_bytes_parts_not_in_order_are_appended_alphabetically() {
        // Every real mutation path (`set_part`, `regen_content_types`,
        // `regen_rels`) pushes a genuinely new part name into `order`
        // immediately, so "a part present in `parts` but missing from
        // `order`" cannot happen through the public API — it's a defensive
        // fallback (see the `to_bytes` doc comment). Exercise it directly by
        // bypassing `order` the way that fallback is meant to guard against.
        let mut package = Package::from_bytes(&minimal_xlsx()).unwrap();
        package
            .parts
            .insert("xl/z.xml".to_string(), Part::Raw(b"<a/>".to_vec()));
        package
            .parts
            .insert("xl/a.xml".to_string(), Part::Raw(b"<a/>".to_vec()));

        let bytes = package.to_bytes().unwrap();
        let names: Vec<String> = {
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
            (0..zip.len())
                .map(|i| zip.by_index(i).unwrap().name().to_string())
                .collect()
        };
        let a_pos = names.iter().position(|n| n == "xl/a.xml").unwrap();
        let z_pos = names.iter().position(|n| n == "xl/z.xml").unwrap();
        assert!(
            a_pos < z_pos,
            "extra parts must be appended alphabetically: {names:?}"
        );
    }

    // Small helper used only by `to_bytes_rejects_touched_part_with_unresolvable_content_type`
    // above -- a bare `set_part` wrapper for a part with a genuinely unresolvable
    // extension (no dot in the name at all).
    impl Package {
        fn replace_part_or_add(&mut self, name: &str, bytes: Vec<u8>) {
            self.set_part(name, bytes, None);
        }
    }
}
