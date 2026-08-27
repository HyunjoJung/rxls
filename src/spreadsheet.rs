//! Editable spreadsheet wrapper with retained OOXML package bytes.

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use crate::xmltree::{NodeId, XmlTree};
use crate::{package::Package, Error, Result, Workbook};

mod cell_edit;
mod document_edit;
mod save;
mod selection;
mod sheet_layout;
mod sheet_lifecycle;
mod worksheet_features;

const MAX_XLSX_ROW: u32 = 1_048_575;
const MAX_XLSX_COL: u16 = 16_383;
/// Canonical package-relative path of the calculation chain part, used to
/// precisely match `PartName`/`Target` references rather than substring-match
/// the whole element text (a sibling part such as `worksheets/precalcChained.xml`
/// must not be treated as the calc chain merely because it contains the
/// substring "calcChain").
const CALC_CHAIN_PART: &str = "xl/calcChain.xml";

/// Edit/save capability for a [`Spreadsheet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditCapability {
    /// The workbook came from an OOXML package and can be saved without
    /// regenerating unknown parts.
    ReadWrite,
    /// The workbook can be read, but this wrapper cannot preserve edits for its
    /// source format.
    ReadOnly(EditReadOnlyReason),
}

/// Why a [`Spreadsheet`] cannot be edited/saved package-preservingly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditReadOnlyReason {
    /// Legacy OLE2/BIFF `.xls`.
    LegacyBiff,
    /// Binary ZIP package such as `.xlsb`.
    BinaryPackage,
    /// OpenDocument spreadsheet package.
    OpenDocument,
    /// The OOXML package could not be retained losslessly enough for editing.
    PackageMetadataLoss,
}

/// A workbook plus the original package bytes needed for no-loss `.xlsx/.xlsm`
/// save.
#[derive(Debug, Clone)]
pub struct Spreadsheet {
    workbook: Workbook,
    package: Option<Package>,
    capability: EditCapability,
    edited_parts: Vec<String>,
}

impl Spreadsheet {
    /// Open a spreadsheet for read access and, for `.xlsx/.xlsm`, retained-package
    /// save.
    pub fn open(bytes: &[u8]) -> Result<Self> {
        #[cfg(feature = "xlsb")]
        if crate::xlsb::is_xlsb(bytes) {
            return Ok(Self::read_only(
                crate::xlsb::open(bytes)?,
                EditReadOnlyReason::BinaryPackage,
            ));
        }
        #[cfg(feature = "ods")]
        if crate::ods::is_ods(bytes) {
            return Ok(Self::read_only(
                crate::ods::open(bytes)?,
                EditReadOnlyReason::OpenDocument,
            ));
        }

        if crate::xlsx::is_xlsx(bytes) {
            let package = Package::from_bytes(bytes)?;
            let workbook = crate::xlsx::open(bytes)?;
            // Lenient-read / strict-edit asymmetry: an incomplete or
            // metadata-lossy package still opens (and still supports a no-op
            // `save()`, since `Package::to_bytes` never itself consults these
            // flags), but edit methods must refuse rather than risk
            // regenerating OPC metadata lossily.
            let capability = if !package.is_complete() || package.is_meta_lossy() {
                EditCapability::ReadOnly(EditReadOnlyReason::PackageMetadataLoss)
            } else {
                EditCapability::ReadWrite
            };
            return Ok(Self {
                workbook,
                package: Some(package),
                capability,
                edited_parts: Vec::new(),
            });
        }

        Ok(Self::read_only(
            Workbook::open_with_codepage(bytes, None)?,
            EditReadOnlyReason::LegacyBiff,
        ))
    }

    fn read_only(workbook: Workbook, reason: EditReadOnlyReason) -> Self {
        Self {
            workbook,
            package: None,
            capability: EditCapability::ReadOnly(reason),
            edited_parts: Vec::new(),
        }
    }

    /// Parsed workbook view.
    pub fn workbook(&self) -> &Workbook {
        &self.workbook
    }

    /// Whether this spreadsheet can be saved through the retained package path.
    pub fn edit_capability(&self) -> &EditCapability {
        &self.capability
    }

    /// The capability-gate step of the edit recipe: every mutating method
    /// must call this before touching any part, so a read-only-for-edits
    /// spreadsheet (legacy format, or a package that opened with
    /// incomplete/metadata-lossy parts) can never partially apply an edit.
    ///
    /// Takes `&self` (not `&mut self`) so callers can still borrow
    /// `self.package`/`self.edited_parts` disjointly afterward.
    fn ensure_editable(&self) -> Result<()> {
        if self.capability != EditCapability::ReadWrite {
            return Err(Error::Zip(
                "spreadsheet is read-only for package-preserving edit",
            ));
        }
        Ok(())
    }

    /// Package parts edited since open, in deterministic part-name order.
    pub fn edited_parts(&self) -> &[String] {
        &self.edited_parts
    }

    /// Apply a batch of package-preserving edits atomically.
    ///
    /// The closure operates on an isolated clone of this spreadsheet. The
    /// clone is serialized and validated before it replaces `self`; if the
    /// closure or final save returns an error, `self`, its retained package
    /// bytes, and [`Spreadsheet::edited_parts`] remain unchanged.
    ///
    /// This transaction is in-memory. It does not write a filesystem path;
    /// callers can persist the committed bytes returned by [`Spreadsheet::save`].
    pub fn transaction<T>(
        &mut self,
        edit: impl FnOnce(&mut Spreadsheet) -> Result<T>,
    ) -> Result<T> {
        self.mutate_atomic(edit)
    }

    /// Clone-and-swap foundation shared by public transactions and individual
    /// operations that must coordinate several package parts. Serializing the
    /// candidate before commit also runs `Package::to_bytes`'s touched-part and
    /// relationship validation while rollback is still possible.
    fn mutate_atomic<T>(&mut self, edit: impl FnOnce(&mut Spreadsheet) -> Result<T>) -> Result<T> {
        self.ensure_editable()?;
        let mut candidate = self.clone();
        let value = edit(&mut candidate)?;
        candidate.save()?;
        *self = candidate;
        Ok(value)
    }
}

fn remember_edited_part(parts: &mut Vec<String>, part: String) {
    if !parts.iter().any(|p| p == &part) {
        parts.push(part);
        parts.sort();
    }
}

/// Remove any residual calc-chain wiring after a cell edit: the
/// `xl/calcChain.xml` part itself, `[Content_Types].xml`'s `Override` for it,
/// and the workbook `.rels` `Relationship` that points at it. Every removal
/// is exact part-path/target equality (via [`canonical_part_name`] /
/// [`normalize_part_target`]), never a substring match, so a sibling part
/// whose path merely *contains* "calcChain" (e.g.
/// `xl/worksheets/precalcChained.xml`) is never mistaken for the real part.
///
/// Each of `[Content_Types].xml`/the workbook `.rels` part is only promoted
/// (via [`Package::part_tree_mut`]) when a read-only peek first confirms
/// there is actually a matching entry to remove -- so a save with nothing to
/// invalidate leaves those parts completely untouched, exactly as the old
/// string-splicing version did by only calling `replace_part` when its
/// edited XML actually differed from the original.
fn invalidate_calc_chain(package: &mut Package) -> Result<Vec<String>> {
    let before = package.touched_parts();
    package.remove_part(CALC_CHAIN_PART);
    package.remove_content_type(CALC_CHAIN_PART)?;
    let workbook_path = workbook_path(package);
    let relationship_ids: Vec<String> = package
        .relationships_of(&workbook_path)
        .iter()
        .filter(|relationship| {
            !relationship.external
                && Package::try_resolve_rel_target(&workbook_path, &relationship.target)
                    .is_some_and(|target| target == CALC_CHAIN_PART)
        })
        .map(|relationship| relationship.id.clone())
        .collect();
    for id in relationship_ids {
        package.remove_relationship(&workbook_path, &id)?;
    }
    Ok(newly_touched(&before, package))
}

/// Set (or, if `Some(value)` and absent, insert as the last child) a
/// Dublin Core / extended-properties text-only child element by exact
/// qualified tag name -- `Some(value)` sets its text in place when the
/// element already exists, preserving every attribute/sibling it carries;
/// `None` removes the element if present (no-op otherwise). Never rebuilds
/// `root` or any of its other children. Shared by `docProps/core.xml`'s
/// Dublin Core fields and `docProps/app.xml`'s `<Company>`.
fn validate_xml_value(value: &str, message: &'static str) -> Result<()> {
    if value.chars().all(|character| {
        let scalar = character as u32;
        (scalar >= 0x20 || matches!(character, '\t' | '\n' | '\r'))
            && !matches!(scalar, 0xFFFE | 0xFFFF)
    }) {
        Ok(())
    } else {
        Err(Error::Zip(message))
    }
}

fn validate_nonempty_xml_value(value: &str, message: &'static str) -> Result<()> {
    if value.is_empty() {
        Err(Error::Zip(message))
    } else {
        validate_xml_value(value, message)
    }
}

fn validate_edit_cell_text(value: &str, message: &'static str) -> Result<()> {
    if value.encode_utf16().count() > crate::write::MAX_CELL_STRING_UTF16_UNITS {
        return Err(Error::Zip(
            "cell text exceeds Excel's 32,767 UTF-16-unit limit",
        ));
    }
    validate_xml_value(value, message)
}

fn worksheet_path(package: &Package, sheet_name: &str) -> Result<String> {
    let workbook_path = workbook_path(package);
    // `xl/workbook.xml` may already be promoted to an edited tree by an
    // earlier sheet-metadata edit in this session (rename/visibility/active
    // tab/defined name) -- `part_xml_bytes` sees that case too, where a bare
    // `Package::part_bytes` (which only reads still-`Raw` parts) would
    // incorrectly report the part missing.
    let workbook_bytes = part_xml_bytes(package, &workbook_path)?;
    let workbook_xml = std::str::from_utf8(&workbook_bytes).map_err(|_| Error::MissingWorkbook)?;
    let rid = workbook_sheet_rid(workbook_xml, sheet_name).ok_or(Error::MissingWorkbook)?;
    let relationships: Vec<_> = package
        .relationships_of(&workbook_path)
        .iter()
        .filter(|relationship| relationship.id == rid)
        .collect();
    if relationships.len() != 1
        || relationships[0].external
        || !crate::xlsx::relationship_type_matches(&relationships[0].rel_type, "worksheet")
    {
        return Err(Error::MissingWorkbook);
    }
    Package::try_resolve_rel_target(&workbook_path, &relationships[0].target)
        .filter(|path| package.has_part(path))
        .ok_or(Error::MissingWorkbook)
}

/// `path`'s XML bytes regardless of whether the part is still `Raw` or has
/// already been promoted to an edited [`XmlTree`] this session (serializing
/// the tree on demand in that case). Needed anywhere a part might have been
/// promoted by an *earlier* edit in the same session -- at which point
/// [`Package::part_bytes`] (which only sees still-`Raw` parts) would
/// incorrectly report it missing even though it's very much present.
fn part_xml_bytes(package: &Package, path: &str) -> Result<Vec<u8>> {
    if let Some(bytes) = package.part_bytes(path) {
        return Ok(bytes.to_vec());
    }
    if let Some(tree) = package.part_tree_ref(path) {
        return Ok(tree.serialize());
    }
    Err(Error::MissingWorkbook)
}

fn workbook_path(package: &Package) -> String {
    let relationships: Vec<_> = package
        .relationships_of("")
        .iter()
        .filter(|relationship| {
            !relationship.external
                && crate::xlsx::relationship_type_matches(&relationship.rel_type, "officeDocument")
        })
        .collect();
    if relationships.len() == 1 {
        if let Some(path) = Package::try_resolve_rel_target("", &relationships[0].target) {
            return path;
        }
    }
    "xl/workbook.xml".to_string()
}

fn workbook_sheet_rid(xml: &str, sheet_name: &str) -> Option<String> {
    if !crate::xml_reference_work_within_budget(xml) {
        return None;
    }
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if local(e.name().as_ref()) == b"sheet"
                    && attr(&e, b"name").as_deref() == Some(sheet_name) =>
            {
                return attr(&e, b"id");
            }
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
    }
}

/// Read `path`'s tree without promoting an unpromoted part: reuses the
/// already-promoted tree if `path` was already promoted this session (so an
/// earlier edit in the same session is visible to the peek), else parses a
/// throwaway `XmlTree` from its raw bytes; `missing` is returned verbatim if
/// the part doesn't exist at all. Lets a caller validate something (e.g. the
/// next append row, a sheet-name uniqueness/visibility invariant, bounds --
/// checked *before* any mutation) without leaving a spurious
/// `touched`/re-serialized part behind if the validation then fails.
fn peek_part_tree<T>(
    package: &Package,
    path: &str,
    missing: Error,
    read: impl FnOnce(&XmlTree) -> Result<T>,
) -> Result<T> {
    if let Some(tree) = package.part_tree_ref(path) {
        return read(tree);
    }
    let bytes = package.part_bytes(path).ok_or(missing)?;
    let tree = XmlTree::parse(bytes)?;
    read(&tree)
}

/// Parts in `package.touched_parts()` now that weren't in `before` -- used to
/// recover the actual canonical stored key a `part_tree_mut` call just
/// touched (its resolved key may differ in case/leading-slash form from the
/// name it was looked up with), mirroring how `replace_part`'s return value
/// used to be recorded directly.
fn newly_touched(before: &[String], package: &Package) -> Vec<String> {
    package
        .touched_parts()
        .into_iter()
        .filter(|n| !before.contains(n))
        .collect()
}

fn canonical_part_name(name: &str) -> String {
    name.replace('\\', "/").trim_start_matches('/').to_string()
}

fn canonical_part_key(name: &str) -> String {
    canonical_part_name(name).to_ascii_lowercase()
}

fn direct_elements_by_local_name(tree: &XmlTree, parent: NodeId, name: &[u8]) -> Vec<NodeId> {
    tree.children_of(parent)
        .iter()
        .copied()
        .filter(|&node| {
            tree.element_name(node)
                .is_some_and(|element| local(element) == name)
        })
        .collect()
}

fn workbook_sheet_index(tree: &XmlTree, name: &str) -> Option<usize> {
    let workbook = tree.root_element()?;
    let sheets = tree.child_by_name(workbook, b"sheets")?;
    tree.children_of(sheets)
        .iter()
        .filter(|&&child| tree.element_name(child) == Some(b"sheet"))
        .position(|&child| {
            tree.attr_value(child, b"name")
                .and_then(|value| std::str::from_utf8(value).ok())
                == Some(name)
        })
}

fn attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (local(a.key.as_ref()) == key).then(|| {
            a.decoded_and_normalized_value_with(
                XmlVersion::Implicit1_0,
                e.decoder(),
                1,
                quick_xml::escape::resolve_xml_entity,
            )
            .ok()
            .map(|value| value.into_owned())
            .unwrap_or_default()
        })
    })
}

fn local(name: &[u8]) -> &[u8] {
    name.rsplit(|&b| b == b':').next().unwrap_or(name)
}

#[cfg(test)]
mod tests;
