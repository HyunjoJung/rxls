//! Editable spreadsheet wrapper with retained OOXML package bytes.

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use crate::write::xml::{a1, esc_attr, esc_text, num_str};
use crate::xmltree::{NodeId, XmlTree};
use crate::{package::Package, Cell, Color, DocProperties, Error, Result, SheetVisible, Workbook};

const MAX_EDIT_RANGE_CELLS: u64 = 10_000;
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

    /// Set a worksheet cell in the retained OOXML package.
    ///
    /// The parsed [`Workbook`] view is intentionally not mutated; reopen the
    /// saved bytes to observe edited values through read APIs.
    pub fn set_cell_value(
        &mut self,
        sheet_name: &str,
        row: u32,
        col: u16,
        value: Cell,
    ) -> Result<()> {
        if row > 1_048_575 || col > 16_383 {
            return Err(Error::Zip("cell is outside the Excel grid"));
        }
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        sml_edit_cell(tree, row, col, &value)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        for touched in invalidate_calc_chain(package)? {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Set a worksheet formula and cached value in the retained OOXML package.
    pub fn set_cell_formula(
        &mut self,
        sheet_name: &str,
        row: u32,
        col: u16,
        formula: impl AsRef<str>,
        cached: impl Into<Cell>,
    ) -> Result<()> {
        self.set_cell_value(
            sheet_name,
            row,
            col,
            Cell::Formula {
                formula: formula.as_ref().trim_start_matches('=').to_string(),
                cached: Box::new(cached.into()),
            },
        )
    }

    /// Append one row of cells to the target worksheet XML part.
    ///
    /// Returns the appended zero-based row index. Text is written as inline
    /// strings, matching [`Spreadsheet::set_cell_value`].
    pub fn append_row<I>(&mut self, sheet_name: &str, values: I) -> Result<u32>
    where
        I: IntoIterator<Item = Cell>,
    {
        let values: Vec<Cell> = values.into_iter().collect();
        if values.len() > 16_384 {
            return Err(Error::Zip("row is outside the Excel grid"));
        }
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        // Compute the append row from a read-only peek *before* promoting the
        // part for editing: if the bounds check below fails, the part must
        // stay completely untouched (no promotion, no `touched`/re-serialize),
        // matching the read-then-validate-then-mutate ordering the old
        // string-splicing code got for free by only calling `replace_part`
        // after every fallible step succeeded.
        let row = peek_part_tree(
            package,
            &path,
            Error::Zip("worksheet XML is missing"),
            |tree| Ok(sml_next_append_row(tree)),
        )?;
        if row > 1_048_575 {
            return Err(Error::Zip("row is outside the Excel grid"));
        }
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        for (col, value) in values.into_iter().enumerate() {
            sml_edit_cell(tree, row, col as u16, &value)?;
        }
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        for touched in invalidate_calc_chain(package)? {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(row)
    }

    /// Clear cells in an inclusive target range.
    pub fn clear_range(
        &mut self,
        sheet_name: &str,
        start_row: u32,
        start_col: u16,
        end_row: u32,
        end_col: u16,
    ) -> Result<()> {
        let row0 = start_row.min(end_row);
        let row1 = start_row.max(end_row);
        let col0 = start_col.min(end_col);
        let col1 = start_col.max(end_col);
        if row1 > 1_048_575 || col1 > 16_383 {
            return Err(Error::Zip("range is outside the Excel grid"));
        }
        let row_count = row1.saturating_sub(row0).saturating_add(1) as u64;
        let col_count = u64::from(col1.saturating_sub(col0).saturating_add(1));
        if row_count.saturating_mul(col_count) > MAX_EDIT_RANGE_CELLS {
            return Err(Error::Zip("range is too large for package-preserving edit"));
        }

        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        for row in row0..=row1 {
            for col in col0..=col1 {
                sml_clear_cell(tree, row, col)?;
            }
        }
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        for touched in invalidate_calc_chain(package)? {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Set workbook document properties in the retained OOXML package.
    ///
    /// Core properties are edited in place in `docProps/core.xml` (only the
    /// `Some` fields are written -- the rest are left as-is if present, or
    /// removed if `None` and previously present); the extended company
    /// property is updated the same way in `docProps/app.xml` when that part
    /// exists, and only actually touched if the company value changes. The
    /// parsed [`Workbook`] view is intentionally not mutated.
    pub fn set_document_properties(&mut self, properties: DocProperties) -> Result<()> {
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;

        let before = package.touched_parts();
        let tree = package.part_tree_mut("docProps/core.xml")?;
        core_set_properties(tree, &properties)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }

        if package.has_part("docProps/app.xml") {
            let changed = peek_part_tree(
                package,
                "docProps/app.xml",
                Error::Zip("docProps/app.xml is missing"),
                |tree| Ok(app_company_changed(tree, properties.company.as_deref())),
            )?;
            if changed {
                let before = package.touched_parts();
                let tree = package.part_tree_mut("docProps/app.xml")?;
                app_set_company(tree, properties.company.as_deref())?;
                for touched in newly_touched(&before, package) {
                    remember_edited_part(&mut self.edited_parts, touched);
                }
            }
        }

        Ok(())
    }

    /// Set or replace a workbook-global defined name in `xl/workbook.xml`.
    ///
    /// Sheet-local and built-in `_xlnm.*` names are left untouched.
    pub fn set_defined_name(
        &mut self,
        name: impl AsRef<str>,
        refers_to: impl AsRef<str>,
    ) -> Result<()> {
        let name = name.as_ref();
        if name.trim().is_empty() || name.starts_with("_xlnm.") {
            return Err(Error::Zip("defined name is not editable"));
        }
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let workbook_path = workbook_path(package);
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&workbook_path)?;
        sml_set_global_defined_name(tree, name, refers_to.as_ref())?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Rename a worksheet in `xl/workbook.xml`.
    pub fn rename_sheet(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        validate_sheet_name(new_name)?;
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let workbook_path = workbook_path(package);
        let exists = peek_part_tree(package, &workbook_path, Error::MissingWorkbook, |tree| {
            Ok(workbook_sheet_index(tree, new_name).is_some())
        })?;
        if exists {
            return Err(Error::Zip("sheet name already exists"));
        }
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&workbook_path)?;
        let sheet = sml_find_sheet_by_name(tree, old_name).ok_or(Error::MissingWorkbook)?;
        tree.set_attr(sheet, b"name", new_name.as_bytes())?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Set a worksheet visibility state in `xl/workbook.xml`.
    pub fn set_sheet_visibility(&mut self, sheet_name: &str, visible: SheetVisible) -> Result<()> {
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let workbook_path = workbook_path(package);
        // Validate (existence + "at least one visible sheet") against a
        // read-only peek *before* promoting the part, so a rejected edit
        // leaves `xl/workbook.xml` completely untouched.
        peek_part_tree(package, &workbook_path, Error::MissingWorkbook, |tree| {
            let sheet = sml_find_sheet_by_name(tree, sheet_name).ok_or(Error::MissingWorkbook)?;
            if visible != SheetVisible::Visible
                && sheet_visibility_of(tree, sheet) == SheetVisible::Visible
                && visible_sheet_count(tree) <= 1
            {
                return Err(Error::Zip("cannot hide the last visible sheet"));
            }
            Ok(())
        })?;

        let before = package.touched_parts();
        let tree = package.part_tree_mut(&workbook_path)?;
        let sheet = sml_find_sheet_by_name(tree, sheet_name).ok_or(Error::MissingWorkbook)?;
        match visible {
            SheetVisible::Visible => tree.remove_attr(sheet, b"state"),
            SheetVisible::Hidden => tree.set_attr(sheet, b"state", b"hidden")?,
            SheetVisible::VeryHidden => tree.set_attr(sheet, b"state", b"veryHidden")?,
        }
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Set the active worksheet by name in `xl/workbook.xml`.
    pub fn set_active_sheet(&mut self, sheet_name: &str) -> Result<()> {
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let workbook_path = workbook_path(package);
        let index = peek_part_tree(package, &workbook_path, Error::MissingWorkbook, |tree| {
            workbook_sheet_index(tree, sheet_name).ok_or(Error::MissingWorkbook)
        })?;
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&workbook_path)?;
        sml_set_active_tab(tree, index)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Set worksheet tab color in the target worksheet XML part.
    pub fn set_sheet_tab_color(&mut self, sheet_name: &str, color: impl Into<Color>) -> Result<()> {
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        sml_set_tab_color(tree, color.into())?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Save the retained OOXML package.
    pub fn save(&self) -> Result<Vec<u8>> {
        match &self.package {
            Some(package) => package.to_bytes(),
            None => Err(Error::Zip(
                "spreadsheet is read-only for package-preserving save",
            )),
        }
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
    let mut touched = Vec::new();
    if let Some(name) = package.remove_part(CALC_CHAIN_PART) {
        touched.push(name);
    }

    if let Some(name) = remove_matching_child(package, "[Content_Types].xml", b"PartName", &|v| {
        canonical_part_name(v) == CALC_CHAIN_PART
    })? {
        touched.push(name);
    }

    let workbook_path = workbook_path(package);
    let rels_path = rels_path_for(&workbook_path);
    if let Some(name) = remove_matching_child(package, &rels_path, b"Target", &|v| {
        normalize_part_target(&workbook_path, v) == CALC_CHAIN_PART
    })? {
        touched.push(name);
    }

    touched.sort();
    touched.dedup();
    Ok(touched)
}

/// Set (or, if `Some(value)` and absent, insert as the last child) a
/// Dublin Core / extended-properties text-only child element by exact
/// qualified tag name -- `Some(value)` sets its text in place when the
/// element already exists, preserving every attribute/sibling it carries;
/// `None` removes the element if present (no-op otherwise). Never rebuilds
/// `root` or any of its other children. Shared by `docProps/core.xml`'s
/// Dublin Core fields and `docProps/app.xml`'s `<Company>`.
fn set_or_remove_child_text(
    tree: &mut XmlTree,
    root: NodeId,
    tag: &str,
    value: Option<&str>,
) -> Result<()> {
    match (tree.child_by_name(root, tag.as_bytes()), value) {
        (Some(id), Some(v)) => tree.set_element_text(id, v),
        (Some(id), None) => tree.remove_child(root, id),
        (None, Some(v)) => {
            let frag = format!("<{tag}>{}</{tag}>", esc_text(v));
            let idx = tree.children_of(root).len();
            tree.insert_fragment_at(root, idx, frag.as_bytes())?;
            Ok(())
        }
        (None, None) => Ok(()),
    }
}

/// Set (or remove) `docProps/core.xml`'s `dcterms:created`/`dcterms:modified`
/// pair together, both carrying `xsi:type="dcterms:W3CDTF"`. `created` must
/// already be validated (see [`crate::write::is_w3cdtf`]) -- `None` removes
/// both elements if present, matching how an invalid/absent timestamp is
/// simply omitted.
fn core_set_timestamp_pair(tree: &mut XmlTree, root: NodeId, ts: Option<&str>) -> Result<()> {
    for tag in ["dcterms:created", "dcterms:modified"] {
        match (tree.child_by_name(root, tag.as_bytes()), ts) {
            (Some(id), Some(v)) => {
                tree.set_attr(id, b"xsi:type", b"dcterms:W3CDTF")?;
                tree.set_element_text(id, v)?;
            }
            (Some(id), None) => tree.remove_child(root, id)?,
            (None, Some(v)) => {
                let frag = format!(
                    r#"<{tag} xsi:type="dcterms:W3CDTF">{}</{tag}>"#,
                    esc_text(v)
                );
                let idx = tree.children_of(root).len();
                tree.insert_fragment_at(root, idx, frag.as_bytes())?;
            }
            (None, None) => {}
        }
    }
    Ok(())
}

/// Apply every `Some` field of `p` onto `docProps/core.xml`'s
/// `<cp:coreProperties>` children, in place: each field is written if
/// `Some`, or its existing element (if any) removed if `None` -- exactly
/// mirroring the old whole-part-regeneration's "only `Some` fields survive"
/// semantics, but without rebuilding anything this module doesn't model
/// (unknown elements, attributes, namespace decls, comments all ride along
/// untouched).
fn core_set_properties(tree: &mut XmlTree, p: &DocProperties) -> Result<()> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("docProps/core.xml is malformed"))?;
    set_or_remove_child_text(tree, root, "dc:title", p.title.as_deref())?;
    set_or_remove_child_text(tree, root, "dc:subject", p.subject.as_deref())?;
    set_or_remove_child_text(tree, root, "dc:creator", p.creator.as_deref())?;
    set_or_remove_child_text(tree, root, "cp:keywords", p.keywords.as_deref())?;
    set_or_remove_child_text(tree, root, "dc:description", p.description.as_deref())?;
    set_or_remove_child_text(
        tree,
        root,
        "cp:lastModifiedBy",
        p.last_modified_by.as_deref(),
    )?;
    // Only a value shaped like W3CDTF may carry `xsi:type="dcterms:W3CDTF"`,
    // matching `core_xml_with_budget`'s validation (a malformed timestamp
    // would otherwise make the part schema-invalid).
    let ts = p
        .created
        .as_deref()
        .filter(|ts| crate::write::is_w3cdtf(ts));
    core_set_timestamp_pair(tree, root, ts)?;
    Ok(())
}

/// The current text of `docProps/app.xml`'s `<Company>`, if present.
fn app_company_text(tree: &XmlTree) -> Option<String> {
    let root = tree.root_element()?;
    let company = tree.child_by_name(root, b"Company")?;
    Some(tree.text_of(company))
}

/// Whether setting the company to `desired` would actually change
/// `docProps/app.xml` -- lets [`Spreadsheet::set_document_properties`] only
/// promote/touch that part when the company value genuinely differs, exactly
/// matching the old `edited != app_xml` no-op check.
fn app_company_changed(tree: &XmlTree, desired: Option<&str>) -> bool {
    app_company_text(tree).as_deref() != desired
}

/// Set (or remove) `docProps/app.xml`'s `<Company>` in place.
fn app_set_company(tree: &mut XmlTree, company: Option<&str>) -> Result<()> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("docProps/app.xml is malformed"))?;
    set_or_remove_child_text(tree, root, "Company", company)
}

fn validate_sheet_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.chars().count() > 31
        || name
            .chars()
            .any(|ch| matches!(ch, ':' | '\\' | '/' | '?' | '*' | '[' | ']'))
    {
        return Err(Error::Zip("invalid sheet name"));
    }
    Ok(())
}

/// Find the `<sheet name="...">` child of `xl/workbook.xml`'s `<sheets>`
/// element by exact name match.
fn sml_find_sheet_by_name(tree: &XmlTree, name: &str) -> Option<NodeId> {
    let workbook = tree.root_element()?;
    let sheets = tree.child_by_name(workbook, b"sheets")?;
    tree.children_of(sheets).iter().copied().find(|&c| {
        tree.attr_value(c, b"name")
            .and_then(|v| std::str::from_utf8(v).ok())
            == Some(name)
    })
}

/// 0-based ordinal of the `<sheet name="...">` among `<sheets>`'s `<sheet>`
/// children (document order), or `None` if no sheet has that name. Filters
/// to actual `<sheet>` elements via [`XmlTree::element_name`] -- not just a
/// raw child-list position -- so a pretty-printed part with whitespace `Text`
/// nodes interleaved between `<sheet>` elements still yields the correct
/// sheet ordinal.
fn workbook_sheet_index(tree: &XmlTree, name: &str) -> Option<usize> {
    let workbook = tree.root_element()?;
    let sheets = tree.child_by_name(workbook, b"sheets")?;
    tree.children_of(sheets)
        .iter()
        .filter(|&&c| tree.element_name(c) == Some(b"sheet"))
        .position(|&c| {
            tree.attr_value(c, b"name")
                .and_then(|v| std::str::from_utf8(v).ok())
                == Some(name)
        })
}

/// A `<sheet>` node's visibility, read from its `state` attribute (absent ⇒
/// visible).
fn sheet_visibility_of(tree: &XmlTree, sheet: NodeId) -> SheetVisible {
    match tree
        .attr_value(sheet, b"state")
        .and_then(|v| std::str::from_utf8(v).ok())
    {
        Some("hidden") => SheetVisible::Hidden,
        Some("veryHidden") => SheetVisible::VeryHidden,
        _ => SheetVisible::Visible,
    }
}

/// Count of `<sheets>`'s `<sheet>` children that are not `hidden`/`veryHidden`
/// -- see [`workbook_sheet_index`] for why this filters to actual `<sheet>`
/// elements rather than a raw child count.
fn visible_sheet_count(tree: &XmlTree) -> usize {
    let Some(workbook) = tree.root_element() else {
        return 0;
    };
    let Some(sheets) = tree.child_by_name(workbook, b"sheets") else {
        return 0;
    };
    tree.children_of(sheets)
        .iter()
        .filter(|&&c| tree.element_name(c) == Some(b"sheet"))
        .filter(|&&c| sheet_visibility_of(tree, c) == SheetVisible::Visible)
        .count()
}

/// Find the workbook-global (non-sheet-local) `<definedName name="...">`
/// child of `xl/workbook.xml`'s `<definedNames>` element, if any.
fn sml_defined_name_node(tree: &XmlTree, workbook: NodeId, name: &str) -> Option<NodeId> {
    let defined_names = tree.child_by_name(workbook, b"definedNames")?;
    tree.children_of(defined_names).iter().copied().find(|&c| {
        tree.attr_value(c, b"localSheetId").is_none()
            && tree
                .attr_value(c, b"name")
                .and_then(|v| std::str::from_utf8(v).ok())
                == Some(name)
    })
}

/// Insert-or-replace-by-name a workbook-global defined name: if a global
/// `<definedName name="X">` already exists, only its text is replaced
/// (preserving any other attribute it carries); otherwise a new element is
/// appended to `<definedNames>` (creating that element, as the workbook's
/// last child, if it doesn't exist yet either).
fn sml_set_global_defined_name(tree: &mut XmlTree, name: &str, refers_to: &str) -> Result<()> {
    let workbook = tree.root_element().ok_or(Error::MissingWorkbook)?;
    if let Some(existing) = sml_defined_name_node(tree, workbook, name) {
        return tree.set_element_text(existing, refers_to);
    }
    let frag = format!(
        r#"<definedName name="{}">{}</definedName>"#,
        esc_attr(name),
        esc_text(refers_to)
    );
    if let Some(defined_names) = tree.child_by_name(workbook, b"definedNames") {
        let idx = tree.children_of(defined_names).len();
        tree.insert_fragment_at(defined_names, idx, frag.as_bytes())?;
        return Ok(());
    }
    let idx = tree.children_of(workbook).len();
    let wrapped = format!("<definedNames>{frag}</definedNames>");
    tree.insert_fragment_at(workbook, idx, wrapped.as_bytes())?;
    Ok(())
}

/// Set (or create) `xl/workbook.xml`'s
/// `<bookViews><workbookView activeTab="N"/></bookViews>`, preserving any
/// other attribute an existing `<workbookView>` carries and inserting a
/// missing `<bookViews>` in `CT_Workbook` order (right before `<sheets>`).
fn sml_set_active_tab(tree: &mut XmlTree, index: usize) -> Result<()> {
    let workbook = tree.root_element().ok_or(Error::MissingWorkbook)?;
    let index = index.to_string();
    let book_views = match tree.child_by_name(workbook, b"bookViews") {
        Some(id) => id,
        None => {
            let sheets = tree.child_by_name(workbook, b"sheets");
            let insert_idx = sheets
                .and_then(|s| tree.children_of(workbook).iter().position(|&c| c == s))
                .unwrap_or_else(|| tree.children_of(workbook).len());
            tree.insert_fragment_at(workbook, insert_idx, b"<bookViews></bookViews>")?
        }
    };
    match tree.child_by_name(book_views, b"workbookView") {
        Some(view) => tree.set_attr(view, b"activeTab", index.as_bytes())?,
        None => {
            let frag = format!(r#"<workbookView activeTab="{index}"/>"#);
            let idx = tree.children_of(book_views).len();
            tree.insert_fragment_at(book_views, idx, frag.as_bytes())?;
        }
    }
    Ok(())
}

/// Set (or create) the worksheet's `<sheetPr><tabColor rgb="..."/></sheetPr>`.
/// An existing `tabColor` is edited in place but reduced to just `rgb`
/// (matching the previous string-splicing output shape exactly: any
/// `indexed`/`theme`/`tint`/`auto` color encoding is cleared), while every
/// other `sheetPr` child/attribute -- and everything else in the part --
/// rides along untouched.
fn sml_set_tab_color(tree: &mut XmlTree, color: Color) -> Result<()> {
    let worksheet = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let rgb = color_hex(color);
    let Some(sheet_pr) = tree.child_by_name(worksheet, b"sheetPr") else {
        let frag = format!(r#"<sheetPr><tabColor rgb="{rgb}"/></sheetPr>"#);
        tree.insert_fragment_at(worksheet, 0, frag.as_bytes())?;
        return Ok(());
    };
    match tree.child_by_name(sheet_pr, b"tabColor") {
        Some(tab_color) => {
            for attr in [b"indexed".as_slice(), b"theme", b"tint", b"auto"] {
                tree.remove_attr(tab_color, attr);
            }
            tree.set_attr(tab_color, b"rgb", rgb.as_bytes())?;
        }
        None => {
            // `tabColor` is CT_SheetPr's first child (before `outlinePr`,
            // `pageSetUpPr`, ...) -- prepend, matching the old
            // string-splicing insertion right after `<sheetPr ...>`'s open
            // tag, ahead of any existing children.
            let frag = format!(r#"<tabColor rgb="{rgb}"/>"#);
            tree.insert_fragment_at(sheet_pr, 0, frag.as_bytes())?;
        }
    }
    Ok(())
}

fn color_hex(color: Color) -> String {
    format!("FF{:02X}{:02X}{:02X}", color.0[0], color.0[1], color.0[2])
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
    let rels_path = rels_path_for(&workbook_path);
    // Same promotion hazard as `workbook_bytes` above: the workbook `.rels`
    // part is promoted to a live `XmlTree` the moment an earlier edit's
    // `invalidate_calc_chain` removes a calc-chain `Relationship` from it --
    // at which point a bare `Package::part_bytes` (raw-only) would
    // incorrectly report it missing even though it's still fully present.
    let rels_bytes = part_xml_bytes(package, &rels_path)?;
    let rels_xml = std::str::from_utf8(&rels_bytes).map_err(|_| Error::MissingWorkbook)?;
    let rels = crate::xlsx::parse_rels(rels_xml);
    let target = rels.get(&rid).ok_or(Error::MissingWorkbook)?;
    Ok(normalize_part_target(&workbook_path, target))
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
    let Some(root_rels) = package
        .part_bytes("_rels/.rels")
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
    else {
        return "xl/workbook.xml".to_string();
    };
    let rels = crate::xlsx::parse_rels(root_rels);
    let types = crate::xlsx::parse_rel_types(root_rels);
    types
        .into_iter()
        .find_map(|(id, ty)| {
            (ty.rsplit('/').next() == Some("officeDocument"))
                .then(|| rels.get(&id).map(|target| canonical_part_name(target)))
                .flatten()
        })
        .unwrap_or_else(|| "xl/workbook.xml".to_string())
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

// --- SpreadsheetML tree finders/builders (cell-editing path) ---
//
// The functions below are the SpreadsheetML-specific layer above the
// format-agnostic `XmlTree`: they know what a `<row>`/`<c>` looks like and
// how a `Cell` value encodes onto one, but all structural mutation goes
// through `XmlTree`'s generic node operations. Because there is no manual
// `>`-scanning anywhere in this layer -- `XmlTree::parse` already rejects
// malformed/adversarial XML up front, never panics -- the whole "quoted `>`
// after a multibyte character" bug class from the old string-splicing
// implementation cannot recur here by construction.

/// Find (or create) the target worksheet's `<sheetData>` element. A
/// worksheet missing it entirely (unusual, but the old string-splicing code
/// tolerated it) gets one appended as the last child of `<worksheet>`,
/// mirroring that same fallback.
fn sml_sheet_data(tree: &mut XmlTree) -> Result<NodeId> {
    let worksheet = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    if let Some(sheet_data) = tree.child_by_name(worksheet, b"sheetData") {
        return Ok(sheet_data);
    }
    let idx = tree.children_of(worksheet).len();
    tree.insert_fragment_at(worksheet, idx, b"<sheetData></sheetData>")
}

/// A `<row>` child's parsed `r=` (1-based row number), or `None` if absent or
/// non-numeric.
fn sml_row_ref(tree: &XmlTree, child: NodeId) -> Option<u32> {
    tree.attr_value(child, b"r")
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|s| s.parse::<u32>().ok())
}

/// Find (or create, inserted in ascending `r=` order) the `<row r="N">`
/// child of `sheet_data` for 0-based `row`.
///
/// Two separate passes, deliberately not fused into one early-exiting scan:
/// `XmlTree::parse` is schema-agnostic and does not enforce ascending `r=`
/// order, so a worksheet with out-of-order rows (valid XML, just not Excel's
/// usual convention) must not have an existing row missed merely because a
/// higher-numbered row happens to appear earlier in document order.
fn sml_row_node(tree: &mut XmlTree, sheet_data: NodeId, row: u32) -> Result<NodeId> {
    let row_ref = row + 1;
    // Pass 1: full linear scan for an EXACT match across ALL children -- no
    // early break, so it cannot miss an out-of-order sibling.
    for &child in tree.children_of(sheet_data) {
        if sml_row_ref(tree, child) == Some(row_ref) {
            return Ok(child);
        }
    }
    // Pass 2 (only reached when no exact match exists): compute the
    // ascending-order insertion index for a NEW row. Early-breaking on the
    // first larger `r=` is safe here -- this pass only locates where a new
    // element belongs, it no longer needs to detect an existing match.
    let mut insert_idx = tree.children_of(sheet_data).len();
    for (i, &child) in tree.children_of(sheet_data).iter().enumerate() {
        if sml_row_ref(tree, child).is_some_and(|r| r > row_ref) {
            insert_idx = i;
            break;
        }
    }
    let frag = format!(r#"<row r="{row_ref}"></row>"#);
    tree.insert_fragment_at(sheet_data, insert_idx, frag.as_bytes())
}

/// Find (or create, inserted in ascending column order) the `<c r="A1">`
/// child of `row_node` for 0-based `(row, col)`. A newly created cell carries
/// only its `r` attribute -- no `s` (style), matching the old create-path
/// behavior of `inline_cell_xml(.., style: None)`; an existing cell's `s` (or
/// any other attribute this module doesn't know about) is left untouched by
/// construction, since it's never rebuilt from scratch.
///
/// Same two-pass shape as [`sml_row_node`] and for the same reason: a `<row>`
/// with out-of-order `<c>` children (valid XML, non-conforming OOXML) must
/// not have an existing cell missed by an early-break scan.
fn sml_cell_node(tree: &mut XmlTree, row_node: NodeId, row: u32, col: u16) -> Result<NodeId> {
    let cell_ref = a1(row, col);
    // Pass 1: full linear scan for an EXACT match across ALL children -- no
    // early break, so it cannot miss an out-of-order sibling.
    for &child in tree.children_of(row_node) {
        if tree.attr_value(child, b"r") == Some(cell_ref.as_bytes()) {
            return Ok(child);
        }
    }
    // Pass 2 (only reached when no exact match exists): compute the
    // ascending-column insertion index for a NEW cell. Early-breaking on the
    // first larger column is safe here -- this pass only locates where a new
    // element belongs, it no longer needs to detect an existing match.
    let mut insert_idx = tree.children_of(row_node).len();
    for (i, &child) in tree.children_of(row_node).iter().enumerate() {
        let Some(r) = tree.attr_value(child, b"r") else {
            continue;
        };
        if let Some(existing_col) = sml_col_of_ref(r) {
            if existing_col > u32::from(col) {
                insert_idx = i;
                break;
            }
        }
    }
    let frag = format!(r#"<c r="{cell_ref}"></c>"#);
    tree.insert_fragment_at(row_node, insert_idx, frag.as_bytes())
}

/// Parse a `<c r="...">` reference's leading column letters into a 0-based
/// column number, for insertion-order comparisons only: a malformed/absent
/// column just returns `None` (such a sibling is left in its current
/// position, never causing a panic or a wrong-but-crashing comparison).
fn sml_col_of_ref(r: &[u8]) -> Option<u32> {
    let mut col: u32 = 0;
    for &b in r {
        if b.is_ascii_alphabetic() {
            col = col
                .checked_mul(26)?
                .checked_add(u32::from(b.to_ascii_uppercase() - b'A') + 1)?;
        } else {
            break;
        }
    }
    col.checked_sub(1)
}

/// Whether -- and how -- `sml_set_cell_value` must change `cell`'s `t`
/// attribute for a given value. Decided up front (before any mutation) so it
/// can be preflighted via [`XmlTree::can_set_attr`].
#[derive(Clone, Copy)]
enum CellTypeAttr {
    Set(&'static [u8]),
    Remove,
}

/// Apply `value`'s SpreadsheetML encoding onto `cell` -- ports
/// `inline_cell_xml`'s value-encoding decisions (text -> inline string,
/// number/date -> plain `<v>`, bool -> `t="b"`, error -> `t="e"`, formula ->
/// `<f>` plus a cached `<v>` typed from the cached value's shape) onto tree
/// mutation. Only ever touches the value-carrying `t` attribute and the
/// `<v>`/`<f>`/`<is>` children: an existing `s` (style) attribute -- or any
/// other attribute/child this function doesn't model -- rides along
/// untouched, because the `<c>` tag is never rebuilt from scratch.
///
/// Every fallible step (the attribute write's budget, and the value
/// fragment's node budget) is preflighted BEFORE the old `<v>`/`<f>`/`<is>`
/// child is removed, so an `Err` return always means "nothing changed" --
/// never "old value gone, new value never written." This mirrors the
/// canonical edit recipe's "preflight on a throwaway parse" + "budget
/// preflight" steps: `XmlTree::insert_fragment_at` itself first parses the
/// fragment, then checks the combined node count against the budget before
/// committing anything, so redoing that same check here first (with the
/// tree untouched) is exact, not approximate -- `XmlTree::remove_child` only
/// ever shrinks a parent's child list, never the arena `node_count()` counts
/// against the budget, and neither it nor `remove_attr` can change whether
/// `can_set_attr` would answer differently later.
fn sml_set_cell_value(tree: &mut XmlTree, cell: NodeId, value: &Cell) -> Result<()> {
    let (type_attr, frag): (CellTypeAttr, String) = match value {
        // ponytail: edited text uses inline strings; rewrite sharedStrings when
        // SST index preservation becomes necessary.
        Cell::Text(t) => (
            CellTypeAttr::Set(b"inlineStr"),
            format!(r#"<is><t xml:space="preserve">{}</t></is>"#, esc_text(t)),
        ),
        Cell::Number(n) | Cell::Date(n) => {
            (CellTypeAttr::Remove, format!("<v>{}</v>", num_str(*n)))
        }
        Cell::Bool(b) => (
            CellTypeAttr::Set(b"b"),
            format!("<v>{}</v>", if *b { 1 } else { 0 }),
        ),
        Cell::Error(e) => (CellTypeAttr::Set(b"e"), format!("<v>{}</v>", esc_text(e))),
        Cell::Formula { formula, cached } => {
            let (t_attr, v): (Option<&'static [u8]>, String) = match cached.as_ref() {
                Cell::Text(t) => (Some(b"str"), esc_text(t)),
                Cell::Bool(b) => (Some(b"b"), if *b { "1" } else { "0" }.to_string()),
                Cell::Error(e) => (Some(b"e"), esc_text(e)),
                Cell::Number(n) | Cell::Date(n) => (None, num_str(*n)),
                Cell::Formula { .. } => (None, "0".to_string()),
            };
            let type_attr = match t_attr {
                Some(t) => CellTypeAttr::Set(t),
                None => CellTypeAttr::Remove,
            };
            (type_attr, format!("<f>{}</f><v>{v}</v>", esc_text(formula)))
        }
    };

    // Preflight 1: the value fragment must fit under the node budget. Parse
    // it as a throwaway tree (exactly what `insert_fragment_at` does
    // internally) and compare against `tree`'s CURRENT node count -- valid
    // both now and after the upcoming `remove_child` calls, since removal
    // never shrinks `node_count()`.
    let frag_tree = XmlTree::parse(frag.as_bytes())?;
    if tree.node_count().saturating_add(frag_tree.node_count()) > crate::xmltree::node_budget() {
        return Err(Error::Xml("edit would exceed the node budget"));
    }
    // Preflight 2: a new `t` attribute value must fit under the attribute
    // budget (replacing an existing `t` always succeeds, so this only
    // rejects the "adding a brand-new attribute" case).
    if let CellTypeAttr::Set(_) = type_attr {
        if !tree.can_set_attr(cell, b"t") {
            return Err(Error::Xml("element has too many attributes to add another"));
        }
    }

    // Both preflights passed: it is now safe to drop the old value before
    // writing the new one.
    for name in [b"v".as_slice(), b"f".as_slice(), b"is".as_slice()] {
        if let Some(child) = tree.child_by_name(cell, name) {
            tree.remove_child(cell, child)?;
        }
    }
    match type_attr {
        CellTypeAttr::Set(val) => tree.set_attr(cell, b"t", val)?,
        CellTypeAttr::Remove => tree.remove_attr(cell, b"t"),
    }
    let idx = tree.children_of(cell).len();
    tree.insert_fragment_at(cell, idx, frag.as_bytes())?;
    Ok(())
}

/// Find-or-create the `<c>` for 0-based `(row, col)` in `tree`'s worksheet
/// and apply `value`'s encoding to it. The single entry point
/// `set_cell_value`/`append_row` both drive.
fn sml_edit_cell(tree: &mut XmlTree, row: u32, col: u16, value: &Cell) -> Result<()> {
    let sheet_data = sml_sheet_data(tree)?;
    let row_node = sml_row_node(tree, sheet_data, row)?;
    let cell = sml_cell_node(tree, row_node, row, col)?;
    sml_set_cell_value(tree, cell, value)
}

/// Remove the `<c>` for 0-based `(row, col)` entirely (not just its value),
/// if present -- a no-op if the row or cell doesn't exist. Mirrors the old
/// string-splicing `clear_range`'s `find_cell_bounds` + whole-span removal.
fn sml_clear_cell(tree: &mut XmlTree, row: u32, col: u16) -> Result<()> {
    let Some(worksheet) = tree.root_element() else {
        return Ok(());
    };
    let Some(sheet_data) = tree.child_by_name(worksheet, b"sheetData") else {
        return Ok(());
    };
    let row_ref = row + 1;
    let row_node = tree.children_of(sheet_data).iter().copied().find(|&c| {
        tree.attr_value(c, b"r")
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|s| s.parse::<u32>().ok())
            == Some(row_ref)
    });
    let Some(row_node) = row_node else {
        return Ok(());
    };
    let cell_ref = a1(row, col);
    let cell_node = tree
        .children_of(row_node)
        .iter()
        .copied()
        .find(|&c| tree.attr_value(c, b"r") == Some(cell_ref.as_bytes()));
    let Some(cell_node) = cell_node else {
        return Ok(());
    };
    tree.remove_child(row_node, cell_node)
}

/// The 0-based row `append_row` should target: one past the highest existing
/// `<row r=N>` under `<sheetData>` (0 if the sheet has no rows yet).
fn sml_next_append_row(tree: &XmlTree) -> u32 {
    let Some(worksheet) = tree.root_element() else {
        return 0;
    };
    let Some(sheet_data) = tree.child_by_name(worksheet, b"sheetData") else {
        return 0;
    };
    tree.children_of(sheet_data)
        .iter()
        .filter_map(|&c| tree.attr_value(c, b"r"))
        .filter_map(|r| std::str::from_utf8(r).ok())
        .filter_map(|s| s.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
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

/// The child of `root` whose `attr` attribute, read as UTF-8, satisfies
/// `resolve` -- used to locate a `[Content_Types].xml` `<Override>` or
/// `.rels` `<Relationship>` by exact resolved-target equality (never a
/// substring match).
fn find_child_with_attr(
    tree: &XmlTree,
    root: NodeId,
    attr: &[u8],
    resolve: &dyn Fn(&str) -> bool,
) -> Option<NodeId> {
    tree.children_of(root).iter().copied().find(|&c| {
        tree.attr_value(c, attr)
            .and_then(|v| std::str::from_utf8(v).ok())
            .is_some_and(resolve)
    })
}

/// Read-only peek: does `part` (already-promoted tree, or a throwaway parse
/// of its raw bytes) have a child matching `find_child_with_attr`? Absent
/// parts, or parts whose bytes fail to parse, report `false` rather than
/// erroring -- matching the old code's `if let Some(xml) = ... { .. }`
/// skip-if-missing behavior.
fn has_child_with_attr(
    package: &Package,
    part: &str,
    attr: &[u8],
    resolve: &dyn Fn(&str) -> bool,
) -> bool {
    if let Some(tree) = package.part_tree_ref(part) {
        return match tree.root_element() {
            Some(root) => find_child_with_attr(tree, root, attr, resolve).is_some(),
            None => false,
        };
    }
    let Some(bytes) = package.part_bytes(part) else {
        return false;
    };
    let Ok(tree) = XmlTree::parse(bytes) else {
        return false;
    };
    match tree.root_element() {
        Some(root) => find_child_with_attr(&tree, root, attr, resolve).is_some(),
        None => false,
    }
}

/// Remove the child of `part`'s root element whose `attr` attribute resolves
/// to a match under `resolve` (exact equality, never substring), returning
/// the touched part name on an actual removal. A cheap read-only peek
/// ([`has_child_with_attr`]) runs first so a part with nothing to remove is
/// never promoted (and thus never re-serialized/marked touched) -- only a
/// genuine match promotes `part` via [`Package::part_tree_mut`].
fn remove_matching_child(
    package: &mut Package,
    part: &str,
    attr: &[u8],
    resolve: &dyn Fn(&str) -> bool,
) -> Result<Option<String>> {
    if !has_child_with_attr(package, part, attr, resolve) {
        return Ok(None);
    }
    let before = package.touched_parts();
    let tree = package.part_tree_mut(part)?;
    let Some(root) = tree.root_element() else {
        return Ok(None);
    };
    let Some(node) = find_child_with_attr(tree, root, attr, resolve) else {
        return Ok(None);
    };
    tree.remove_child(root, node)?;
    Ok(newly_touched(&before, package).into_iter().next())
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

fn rels_path_for(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => format!("{}/_rels/{}.rels", &path[..i], &path[i + 1..]),
        None => format!("_rels/{path}.rels"),
    }
}

fn normalize_part_target(base: &str, target: &str) -> String {
    let target = target.replace('\\', "/");
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = base
        .rsplit_once('/')
        .map(|(dir, _)| dir.split('/').collect())
        .unwrap_or_default();
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(segment),
        }
    }
    parts.join("/")
}

fn canonical_part_name(name: &str) -> String {
    name.replace('\\', "/").trim_start_matches('/').to_string()
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
mod tests {
    use super::*;
    use crate::xmltree::{reset_test_node_budget, set_test_node_budget};

    /// A minimal worksheet part with exactly one row, one valued cell.
    /// Shared by the narrow (`sml_set_cell_value`) and broad
    /// (`Spreadsheet::set_cell_value`) node-budget regression tests below, so
    /// the pinned budget and the fixture can never drift out of sync.
    const MINIMAL_WORKSHEET_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#;

    /// Regression test for a `sml_set_cell_value` bug: it unconditionally
    /// removed the cell's existing `<v>`/`<f>`/`<is>` child FIRST, and only
    /// afterward performed the fallible write of the new value (`set_attr`
    /// and/or `insert_fragment_at`, either of which can fail under node/attr
    /// budget pressure). A failure at that point returned `Err` with the old
    /// value already gone and the new value never written -- silent data
    /// loss reported as "nothing happened."
    ///
    /// This is the narrowest possible reproduction: it calls
    /// `sml_set_cell_value` directly on a tree pinned to exactly its current
    /// node count (zero room for even one more node), so the value-insert
    /// step is guaranteed to fail.
    #[test]
    fn sml_set_cell_value_leaves_original_value_intact_when_node_budget_write_fails() {
        let mut tree = XmlTree::parse(MINIMAL_WORKSHEET_XML).expect("parse minimal worksheet");
        let budget = tree.node_count();
        let worksheet = tree.root_element().expect("root element");
        let sheet_data = tree
            .child_by_name(worksheet, b"sheetData")
            .expect("sheetData");
        let row = tree.child_by_name(sheet_data, b"row").expect("row");
        let cell = tree.child_by_name(row, b"c").expect("cell");

        set_test_node_budget(budget);
        let result = sml_set_cell_value(&mut tree, cell, &Cell::Number(999.0));
        reset_test_node_budget();

        assert!(
            result.is_err(),
            "overwriting the value must fail under a zero-room node budget"
        );
        let v = tree
            .child_by_name(cell, b"v")
            .expect("the ORIGINAL <v> child must survive a failed write");
        assert_eq!(tree.text_of(v), "1", "original value must be untouched");
        assert!(
            tree.child_by_name(cell, b"f").is_none(),
            "no half-written <f> child should appear"
        );
        assert!(
            tree.child_by_name(cell, b"is").is_none(),
            "no half-written <is> child should appear"
        );
        assert_eq!(
            tree.serialize(),
            XmlTree::parse(MINIMAL_WORKSHEET_XML)
                .expect("re-parse fixture")
                .serialize(),
            "tree must be byte-for-byte unchanged after a failed write"
        );
    }

    /// Builds a minimal single-sheet `.xlsx` ZIP whose `xl/worksheets/sheet1.xml`
    /// is exactly [`MINIMAL_WORKSHEET_XML`], for the broader
    /// `Spreadsheet::set_cell_value` end-to-end regression test below.
    fn minimal_xlsx_with_one_valued_cell() -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        fn add(
            zip: &mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>,
            opt: SimpleFileOptions,
            name: &str,
            bytes: &[u8],
        ) {
            zip.start_file(name, opt).unwrap();
            zip.write_all(bytes).unwrap();
        }

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        add(
            &mut zip,
            opt,
            "[Content_Types].xml",
            br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
        );
        add(
            &mut zip,
            opt,
            "_rels/.rels",
            br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        );
        add(
            &mut zip,
            opt,
            "xl/workbook.xml",
            br#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        );
        add(
            &mut zip,
            opt,
            "xl/_rels/workbook.xml.rels",
            br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        );
        add(
            &mut zip,
            opt,
            "xl/worksheets/sheet1.xml",
            MINIMAL_WORKSHEET_XML,
        );
        zip.finish().unwrap().into_inner()
    }

    /// Broader end-to-end confirmation through the public API: the same
    /// budget-failure scenario, driven through `Spreadsheet::set_cell_value`,
    /// must report `Err` while leaving the cell's original value intact and
    /// `edited_parts()` empty (the `?` in `set_cell_value` must short-circuit
    /// before the edited-parts bookkeeping runs).
    #[test]
    fn set_cell_value_leaves_cell_untouched_when_node_budget_write_fails() {
        let input = minimal_xlsx_with_one_valued_cell();
        let budget = XmlTree::parse(MINIMAL_WORKSHEET_XML)
            .expect("parse fixture")
            .node_count();

        set_test_node_budget(budget);
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
        let result = spreadsheet.set_cell_value("Data", 0, 0, Cell::Number(999.0));
        reset_test_node_budget();

        assert!(
            result.is_err(),
            "write must fail under a zero-room node budget"
        );
        assert!(
            spreadsheet.edited_parts().is_empty(),
            "a failed edit must not be recorded as an edited part"
        );

        let saved = spreadsheet.save().expect("save must still succeed");
        let reopened = Workbook::open(&saved).expect("reopen saved package");
        assert_eq!(
            reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
            Some(&Cell::Number(1.0)),
            "original cell value must survive a failed edit"
        );
    }
}
