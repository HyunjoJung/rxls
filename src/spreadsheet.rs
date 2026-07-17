//! Editable spreadsheet wrapper with retained OOXML package bytes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use crate::write::xml::{
    a1, esc_attr, esc_text, num_str, CT_COMMENTS, CT_VML, CT_WORKSHEET, NS_MAIN, NS_R,
    REL_COMMENTS, REL_HYPERLINK, REL_VML_DRAWING, REL_WORKSHEET,
};
use crate::xmltree::{NodeId, XmlTree};
use crate::{
    package::Package, Cell, Color, Comment, DataValidation, DocProperties, DvKind, DvOp, Error,
    Result, SheetVisible, Workbook,
};

const MAX_EDIT_RANGE_CELLS: u64 = 10_000;
const MAX_XLSX_ROW: u32 = 1_048_575;
const MAX_XLSX_COL: u16 = 16_383;
static SAVE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
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
        validate_edit_cell_value(&value)?;
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
        for value in &values {
            validate_edit_cell_value(value)?;
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
        validate_document_properties_for_edit(&properties)?;
        self.mutate_atomic(move |candidate| candidate.set_document_properties_in_place(&properties))
    }

    /// Multi-part implementation for [`Spreadsheet::set_document_properties`].
    /// The public method wraps this in [`Spreadsheet::mutate_atomic`] so a
    /// failure while updating `docProps/app.xml` cannot leave an already-edited
    /// `docProps/core.xml` committed.
    fn set_document_properties_in_place(&mut self, properties: &DocProperties) -> Result<()> {
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;

        let before = package.touched_parts();
        let tree = package.part_tree_mut("docProps/core.xml")?;
        core_set_properties(tree, properties)?;
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
        let refers_to = refers_to.as_ref();
        if !crate::write::is_valid_defined_name(name)
            || name
                .get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("_xlnm."))
        {
            return Err(Error::Zip("defined name is not editable"));
        }
        validate_xml_value(refers_to, "defined name formula contains invalid XML text")?;
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let workbook_path = workbook_path(package);
        peek_part_tree(package, &workbook_path, Error::MissingWorkbook, |tree| {
            validate_global_defined_name_target(tree, name)
        })?;
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&workbook_path)?;
        sml_set_global_defined_name(tree, name, refers_to)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Rename a worksheet and its direct sheet-qualified references.
    ///
    /// Formula text in the workbook, worksheets, charts, tables, and related
    /// formula-bearing parts is rewritten together with workbook/global/local
    /// defined names (including print-area/title built-ins). Internal hyperlink
    /// locations and pivot-cache worksheet-source attributes are also updated.
    /// External-workbook qualifiers such as `[Book.xlsx]Data!A1` are left
    /// unchanged. The whole operation is atomic: an unsupported or malformed
    /// touched part, a write-budget failure, or final package-validation error
    /// leaves this [`Spreadsheet`] unchanged.
    pub fn rename_sheet(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        validate_sheet_name(new_name)?;
        let old_name = old_name.to_string();
        let new_name = new_name.to_string();
        self.mutate_atomic(move |candidate| candidate.rename_sheet_in_place(&old_name, &new_name))
    }

    fn rename_sheet_in_place(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let workbook_path = workbook_path(package);
        peek_part_tree(package, &workbook_path, Error::MissingWorkbook, |tree| {
            let target = sml_find_sheet_by_name(tree, old_name).ok_or(Error::MissingWorkbook)?;
            if workbook_has_other_sheet_named(tree, target, new_name) {
                return Err(Error::Zip("sheet name already exists"));
            }
            Ok(())
        })?;

        if old_name == new_name {
            return Ok(());
        }

        let formula_parts = formula_bearing_parts(package, &workbook_path);
        for path in formula_parts {
            let rewrites = peek_part_tree(
                package,
                &path,
                Error::Zip("formula-bearing OOXML part is missing"),
                |tree| Ok(collect_sheet_reference_rewrites(tree, old_name, new_name)),
            )?;
            if rewrites.is_empty() {
                continue;
            }
            let before = package.touched_parts();
            let tree = package.part_tree_mut(&path)?;
            apply_sheet_reference_rewrites(tree, &rewrites)?;
            for touched in newly_touched(&before, package) {
                remember_edited_part(&mut self.edited_parts, touched);
            }
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

    /// Append a new empty worksheet to the retained OOXML package.
    ///
    /// The worksheet part, content-type override, workbook relationship, and
    /// `<sheet>` entry are created as one atomic operation. Names are unique
    /// case-insensitively, while relationship ids, sheet ids, and worksheet
    /// part names are allocated deterministically without renumbering any
    /// existing package component.
    pub fn add_sheet(&mut self, name: &str) -> Result<()> {
        validate_sheet_name(name)?;
        let name = name.to_string();
        self.mutate_atomic(move |candidate| candidate.add_sheet_in_place(&name))
    }

    fn add_sheet_in_place(&mut self, name: &str) -> Result<()> {
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let workbook_path = workbook_path(package);
        let (sheet_id, was_empty) =
            peek_part_tree(package, &workbook_path, Error::MissingWorkbook, |tree| {
                if workbook_has_sheet_named(tree, name) {
                    return Err(Error::Zip("sheet name already exists"));
                }
                Ok((next_sheet_id(tree)?, workbook_sheet_count(tree) == 0))
            })?;
        let worksheet_path = next_worksheet_part_name(package)?;
        let relationship_target = Package::rel_target(&workbook_path, &worksheet_path);
        let before = package.touched_parts();
        let rid =
            package.add_relationship(&workbook_path, REL_WORKSHEET, &relationship_target, false);
        package.set_part(&worksheet_path, empty_worksheet_xml(), Some(CT_WORKSHEET));

        let tree = package.part_tree_mut(&workbook_path)?;
        sml_append_sheet(tree, name, sheet_id, &rid)?;
        if was_empty {
            sml_set_active_tab(tree, 0)?;
        }
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Delete a worksheet and repair its known package dependencies atomically.
    ///
    /// A workbook must retain at least one worksheet and one visible
    /// worksheet. Deleting the active sheet selects the adjacent surviving
    /// tab; local defined names owned by the deleted sheet are removed, later
    /// local-sheet indexes are shifted, and surviving formulas/names that
    /// directly qualify the deleted sheet are changed to `#REF!`. Exclusively
    /// owned standard worksheet dependencies are garbage-collected without
    /// renumbering surviving parts. Ambiguous relationships and structural
    /// dependency kinds that cannot be repaired safely are rejected.
    pub fn delete_sheet(&mut self, name: &str) -> Result<()> {
        let name = name.to_string();
        self.mutate_atomic(move |candidate| candidate.delete_sheet_in_place(&name))
    }

    fn delete_sheet_in_place(&mut self, name: &str) -> Result<()> {
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let workbook_path = workbook_path(package);
        let plan = peek_part_tree(package, &workbook_path, Error::MissingWorkbook, |tree| {
            delete_sheet_plan(tree, name)
        })?;
        let workbook_relationships = package.relationships_of(&workbook_path);
        let mut worksheet_count = 0usize;
        for sheet_rid in &plan.sheet_rids {
            let matches: Vec<_> = workbook_relationships
                .iter()
                .filter(|relationship| relationship.id == *sheet_rid)
                .collect();
            if matches.len() != 1 || matches[0].external {
                return Err(Error::Zip(
                    "workbook sheet relationships are missing or ambiguous",
                ));
            }
            if matches[0].rel_type.rsplit('/').next() == Some("worksheet") {
                worksheet_count += 1;
            }
        }
        if worksheet_count <= 1 {
            return Err(Error::Zip("cannot delete the last worksheet"));
        }
        let relationship_matches: Vec<_> = workbook_relationships
            .iter()
            .filter(|relationship| relationship.id == plan.rid)
            .collect();
        if relationship_matches.len() != 1 {
            return Err(Error::Zip("worksheet relationship is missing or ambiguous"));
        }
        let relationship = relationship_matches[0];
        if relationship.external || relationship.rel_type.rsplit('/').next() != Some("worksheet") {
            return Err(Error::MissingWorkbook);
        }
        let worksheet_path = Package::resolve_rel_target(&workbook_path, &relationship.target);
        if !package.has_part(&worksheet_path) {
            return Err(Error::MissingWorkbook);
        }

        let owned_parts = plan_sheet_owned_parts(package, &worksheet_path)?;
        let removed_keys: BTreeSet<_> = owned_parts
            .iter()
            .chain(std::iter::once(&worksheet_path))
            .map(|path| canonical_part_key(path))
            .collect();
        let mut reference_repairs = Vec::new();
        for path in formula_bearing_parts(package, &workbook_path) {
            if removed_keys.contains(&canonical_part_key(&path)) {
                continue;
            }
            let rewrites = peek_part_tree(
                package,
                &path,
                Error::Zip("formula-bearing OOXML part is missing"),
                |tree| collect_deleted_sheet_reference_rewrites(tree, name),
            )?;
            if !rewrites.is_empty() {
                reference_repairs.push((path, rewrites));
            }
        }
        let app_repair = if package.has_part("docProps/app.xml") {
            peek_part_tree(
                package,
                "docProps/app.xml",
                Error::Zip("docProps/app.xml is missing"),
                |tree| plan_app_sheet_title_repair(tree, name, worksheet_count),
            )?
        } else {
            None
        };

        let before = package.touched_parts();
        for (path, rewrites) in reference_repairs {
            let tree = package.part_tree_mut(&path)?;
            apply_sheet_reference_rewrites(tree, &rewrites)?;
        }
        if let Some(repair) = app_repair {
            let tree = package.part_tree_mut("docProps/app.xml")?;
            apply_app_sheet_title_repair(tree, repair)?;
        }
        let tree = package.part_tree_mut(&workbook_path)?;
        sml_delete_sheet(tree, name, plan.sheet_index, plan.new_active_tab)?;
        if !package.remove_relationship(&workbook_path, &plan.rid)? {
            return Err(Error::MissingWorkbook);
        }
        package.remove_content_type(&worksheet_path)?;
        package
            .remove_part(&worksheet_path)
            .ok_or(Error::MissingWorkbook)?;
        let worksheet_rels = Package::rels_path_of(&worksheet_path);
        package.remove_part(&worksheet_rels);
        for path in owned_parts {
            package.remove_content_type(&path)?;
            package.remove_part(&path);
            package.remove_part(&Package::rels_path_of(&path));
        }
        invalidate_calc_chain(package)?;

        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Merge an inclusive rectangular cell range atomically.
    ///
    /// The range must be ordered, inside Excel's worksheet grid, span at
    /// least two cells, and not overlap any existing merged range.
    pub fn merge_cells(
        &mut self,
        sheet_name: &str,
        first_row: u32,
        first_col: u16,
        last_row: u32,
        last_col: u16,
    ) -> Result<()> {
        validate_layout_range(first_row, first_col, last_row, last_col)?;
        if first_row == last_row && first_col == last_col {
            return Err(Error::Zip("merged range must contain at least two cells"));
        }
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.merge_cells_in_place(&sheet_name, first_row, first_col, last_row, last_col)
        })
    }

    fn merge_cells_in_place(
        &mut self,
        sheet_name: &str,
        first_row: u32,
        first_col: u16,
        last_row: u32,
        last_col: u16,
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        peek_part_tree(
            package,
            &path,
            Error::Zip("worksheet XML is missing"),
            |tree| {
                validate_merge_does_not_overlap(tree, (first_row, first_col, last_row, last_col))
            },
        )?;
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        sml_add_merge(tree, (first_row, first_col, last_row, last_col))?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Remove an exact inclusive merged-cell range atomically.
    pub fn unmerge_cells(
        &mut self,
        sheet_name: &str,
        first_row: u32,
        first_col: u16,
        last_row: u32,
        last_col: u16,
    ) -> Result<()> {
        validate_layout_range(first_row, first_col, last_row, last_col)?;
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.unmerge_cells_in_place(&sheet_name, first_row, first_col, last_row, last_col)
        })
    }

    fn unmerge_cells_in_place(
        &mut self,
        sheet_name: &str,
        first_row: u32,
        first_col: u16,
        last_row: u32,
        last_col: u16,
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        let target = (first_row, first_col, last_row, last_col);
        let exists = peek_part_tree(
            package,
            &path,
            Error::Zip("worksheet XML is missing"),
            |tree| Ok(find_exact_merge(tree, target).is_some()),
        )?;
        if !exists {
            return Err(Error::Zip("merged range does not exist"));
        }
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        sml_remove_merge(tree, target)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Set a row's explicit height in points atomically.
    pub fn set_row_height(&mut self, sheet_name: &str, row: u32, points: f32) -> Result<()> {
        validate_row(row)?;
        validate_layout_measure(points, 409.5, "row height is invalid")?;
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.set_row_layout_in_place(&sheet_name, row, RowLayoutEdit::Height(points))
        })
    }

    /// Hide or unhide a row atomically.
    pub fn set_row_hidden(&mut self, sheet_name: &str, row: u32, hidden: bool) -> Result<()> {
        validate_row(row)?;
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.set_row_layout_in_place(&sheet_name, row, RowLayoutEdit::Hidden(hidden))
        })
    }

    fn set_row_layout_in_place(
        &mut self,
        sheet_name: &str,
        row: u32,
        edit: RowLayoutEdit,
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        if matches!(edit, RowLayoutEdit::Hidden(false)) {
            let needs_edit = peek_part_tree(
                package,
                &path,
                Error::Zip("worksheet XML is missing"),
                |tree| Ok(row_is_hidden(tree, row)),
            )?;
            if !needs_edit {
                return Ok(());
            }
        }
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        sml_set_row_layout(tree, row, edit)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Set a column's explicit width in character units atomically.
    pub fn set_column_width(&mut self, sheet_name: &str, col: u16, width: f32) -> Result<()> {
        validate_col(col)?;
        validate_layout_measure(width, 255.0, "column width is invalid")?;
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.set_column_layout_in_place(&sheet_name, col, ColumnLayoutEdit::Width(width))
        })
    }

    /// Hide or unhide a column atomically.
    pub fn set_column_hidden(&mut self, sheet_name: &str, col: u16, hidden: bool) -> Result<()> {
        validate_col(col)?;
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.set_column_layout_in_place(&sheet_name, col, ColumnLayoutEdit::Hidden(hidden))
        })
    }

    fn set_column_layout_in_place(
        &mut self,
        sheet_name: &str,
        col: u16,
        edit: ColumnLayoutEdit,
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        if matches!(edit, ColumnLayoutEdit::Hidden(false)) {
            let needs_edit = peek_part_tree(
                package,
                &path,
                Error::Zip("worksheet XML is missing"),
                |tree| Ok(column_is_hidden(tree, col)),
            )?;
            if !needs_edit {
                return Ok(());
            }
        }
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        sml_set_column_layout(tree, col, edit)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Freeze panes above `row` and to the left of `col` atomically.
    pub fn set_freeze_panes(&mut self, sheet_name: &str, row: u32, col: u16) -> Result<()> {
        validate_row(row)?;
        validate_col(col)?;
        let sheet_name = sheet_name.to_string();
        let freeze = (row > 0 || col > 0).then_some((row, col));
        self.mutate_atomic(move |candidate| {
            candidate.set_freeze_panes_in_place(&sheet_name, freeze)
        })
    }

    /// Remove a worksheet's frozen panes atomically.
    pub fn clear_freeze_panes(&mut self, sheet_name: &str) -> Result<()> {
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| candidate.set_freeze_panes_in_place(&sheet_name, None))
    }

    fn set_freeze_panes_in_place(
        &mut self,
        sheet_name: &str,
        freeze: Option<(u32, u16)>,
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        if freeze.is_none() {
            let needs_edit = peek_part_tree(
                package,
                &path,
                Error::Zip("worksheet XML is missing"),
                |tree| Ok(find_frozen_pane(tree).is_some()),
            )?;
            if !needs_edit {
                return Ok(());
            }
        }
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        sml_set_freeze_panes(tree, freeze)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Set or clear the local worksheet print area atomically.
    pub fn set_print_area(
        &mut self,
        sheet_name: &str,
        area: Option<(u32, u16, u32, u16)>,
    ) -> Result<()> {
        if let Some((first_row, first_col, last_row, last_col)) = area {
            validate_layout_range(first_row, first_col, last_row, last_col)?;
        }
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| candidate.set_print_area_in_place(&sheet_name, area))
    }

    fn set_print_area_in_place(
        &mut self,
        sheet_name: &str,
        area: Option<(u32, u16, u32, u16)>,
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let workbook_path = workbook_path(package);
        let sheet_index =
            peek_part_tree(package, &workbook_path, Error::MissingWorkbook, |tree| {
                workbook_sheet_index(tree, sheet_name).ok_or(Error::MissingWorkbook)
            })?;
        if area.is_none() {
            let exists = peek_part_tree(package, &workbook_path, Error::MissingWorkbook, |tree| {
                Ok(find_local_defined_name(tree, "_xlnm.Print_Area", sheet_index).is_some())
            })?;
            if !exists {
                return Ok(());
            }
        }
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&workbook_path)?;
        sml_set_print_area(tree, sheet_name, sheet_index, area)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Create or replace a legacy cell comment (Excel note) atomically.
    pub fn set_comment(
        &mut self,
        sheet_name: &str,
        row: u32,
        col: u16,
        text: &str,
        author: Option<&str>,
    ) -> Result<()> {
        validate_row(row)?;
        validate_col(col)?;
        validate_xml_value(text, "comment text is not valid XML text")?;
        if let Some(author) = author {
            validate_xml_value(author, "comment author is not valid XML text")?;
        }
        let sheet_name = sheet_name.to_string();
        let text = text.to_string();
        let author = author.map(str::to_string);
        self.mutate_atomic(move |candidate| {
            candidate.set_comment_in_place(&sheet_name, row, col, &text, author.as_deref())
        })
    }

    fn set_comment_in_place(
        &mut self,
        sheet_name: &str,
        row: u32,
        col: u16,
        text: &str,
        author: Option<&str>,
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let worksheet_path = worksheet_path(package, sheet_name)?;
        let comment_relation = unique_related_part(package, &worksheet_path, "comments")?;
        let vml_relation = unique_related_part(package, &worksheet_path, "vmldrawing")?;
        let existing = if let Some(relation) = &comment_relation {
            if !package.has_part(&relation.path) {
                return Err(Error::Zip("comment relationship target is missing"));
            }
            peek_part_tree(
                package,
                &relation.path,
                Error::Zip("comments XML part is missing"),
                |tree| comment_exists_exactly_once(tree, row, col),
            )?
        } else {
            false
        };
        if existing {
            let relation = vml_relation
                .as_ref()
                .ok_or(Error::Zip("legacy comment VML relationship is missing"))?;
            if !package.has_part(&relation.path) {
                return Err(Error::Zip("legacy comment VML part is missing"));
            }
        }

        let before = package.touched_parts();
        match comment_relation {
            Some(relation) => {
                let tree = package.part_tree_mut(&relation.path)?;
                sml_set_comment(tree, row, col, text, author)?;
            }
            None => {
                let path = next_comment_part_name(package)?;
                let comment = Comment {
                    row,
                    col,
                    text: text.to_string(),
                    author: author.map(str::to_string),
                };
                package.set_part(
                    &path,
                    crate::write::editable_comments_xml(&[comment]).into_bytes(),
                    Some(CT_COMMENTS),
                );
                let target = Package::rel_target(&worksheet_path, &path);
                package.add_relationship(&worksheet_path, REL_COMMENTS, &target, false);
            }
        }

        if !existing {
            let vml_relation = match vml_relation {
                Some(relation) => {
                    if !package.has_part(&relation.path) {
                        return Err(Error::Zip("legacy comment VML part is missing"));
                    }
                    peek_part_tree(
                        package,
                        &relation.path,
                        Error::Zip("legacy comment VML part is missing"),
                        |tree| validate_vml_note_target_available(tree, row, col),
                    )?;
                    let tree = package.part_tree_mut(&relation.path)?;
                    sml_add_vml_note(tree, row, col)?;
                    relation
                }
                None => {
                    let path = next_vml_part_name(package)?;
                    let comment = Comment {
                        row,
                        col,
                        text: text.to_string(),
                        author: author.map(str::to_string),
                    };
                    package.set_part(
                        &path,
                        crate::write::editable_vml_drawing_xml(&[comment]).into_bytes(),
                        Some(CT_VML),
                    );
                    let target = Package::rel_target(&worksheet_path, &path);
                    let id =
                        package.add_relationship(&worksheet_path, REL_VML_DRAWING, &target, false);
                    RelatedPart { id, path }
                }
            };
            let tree = package.part_tree_mut(&worksheet_path)?;
            sml_ensure_legacy_drawing(tree, &vml_relation.id)?;
        }

        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Delete a legacy cell comment (Excel note) atomically.
    pub fn delete_comment(&mut self, sheet_name: &str, row: u32, col: u16) -> Result<()> {
        validate_row(row)?;
        validate_col(col)?;
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.delete_comment_in_place(&sheet_name, row, col)
        })
    }

    fn delete_comment_in_place(&mut self, sheet_name: &str, row: u32, col: u16) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let worksheet_path = worksheet_path(package, sheet_name)?;
        let comment_relation = unique_related_part(package, &worksheet_path, "comments")?
            .ok_or(Error::Zip("comment does not exist"))?;
        let vml_relation = unique_related_part(package, &worksheet_path, "vmldrawing")?
            .ok_or(Error::Zip("legacy comment VML relationship is missing"))?;
        if !package.has_part(&comment_relation.path) || !package.has_part(&vml_relation.path) {
            return Err(Error::Zip("legacy comment package part is missing"));
        }
        peek_part_tree(
            package,
            &comment_relation.path,
            Error::Zip("comments XML part is missing"),
            |tree| {
                if comment_exists_exactly_once(tree, row, col)? {
                    Ok(())
                } else {
                    Err(Error::Zip("comment does not exist"))
                }
            },
        )?;
        peek_part_tree(
            package,
            &vml_relation.path,
            Error::Zip("legacy comment VML part is missing"),
            |tree| validate_single_vml_note_shape(tree, row, col),
        )?;

        let before = package.touched_parts();
        let comments_remaining = {
            let tree = package.part_tree_mut(&comment_relation.path)?;
            sml_delete_comment(tree, row, col)?;
            comment_count(tree)
        };
        let vml_shapes_remaining = {
            let tree = package.part_tree_mut(&vml_relation.path)?;
            sml_delete_vml_note(tree, row, col)?;
            vml_shape_count(tree)
        };

        if comments_remaining == 0 {
            package.remove_relationship(&worksheet_path, &comment_relation.id)?;
            package.remove_content_type(&comment_relation.path)?;
            package.remove_part(&comment_relation.path);
            if vml_shapes_remaining == 0 {
                package.remove_relationship(&worksheet_path, &vml_relation.id)?;
                package.remove_content_type(&vml_relation.path)?;
                package.remove_part(&vml_relation.path);
                let tree = package.part_tree_mut(&worksheet_path)?;
                sml_remove_legacy_drawing(tree, &vml_relation.id)?;
            }
        }
        if package.relationships_of(&worksheet_path).is_empty() {
            package.remove_part(&Package::rels_path_of(&worksheet_path));
        }
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Create or replace an external hyperlink on a cell atomically.
    pub fn set_external_hyperlink(
        &mut self,
        sheet_name: &str,
        row: u32,
        col: u16,
        target: &str,
    ) -> Result<()> {
        validate_row(row)?;
        validate_col(col)?;
        validate_nonempty_xml_value(target, "external hyperlink target is invalid")?;
        let sheet_name = sheet_name.to_string();
        let target = target.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.set_hyperlink_in_place(
                &sheet_name,
                row,
                col,
                HyperlinkEdit::External(&target),
            )
        })
    }

    /// Create or replace an internal workbook hyperlink on a cell atomically.
    pub fn set_internal_hyperlink(
        &mut self,
        sheet_name: &str,
        row: u32,
        col: u16,
        location: &str,
    ) -> Result<()> {
        validate_row(row)?;
        validate_col(col)?;
        validate_nonempty_xml_value(location, "internal hyperlink location is invalid")?;
        let sheet_name = sheet_name.to_string();
        let location = location.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.set_hyperlink_in_place(
                &sheet_name,
                row,
                col,
                HyperlinkEdit::Internal(&location),
            )
        })
    }

    fn set_hyperlink_in_place(
        &mut self,
        sheet_name: &str,
        row: u32,
        col: u16,
        edit: HyperlinkEdit<'_>,
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let worksheet_path = worksheet_path(package, sheet_name)?;
        let record = peek_part_tree(
            package,
            &worksheet_path,
            Error::Zip("worksheet XML is missing"),
            |tree| hyperlink_record(tree, row, col),
        )?;
        let old_relationship = record
            .rid
            .as_deref()
            .map(|id| validate_hyperlink_relationship(package, &worksheet_path, id))
            .transpose()?;
        let before = package.touched_parts();

        if let (HyperlinkEdit::External(target), Some(relationship)) =
            (edit, old_relationship.as_ref())
        {
            if record.rid_uses == 1 {
                if !package.update_relationship_target(
                    &worksheet_path,
                    &relationship.id,
                    target,
                    true,
                )? {
                    return Err(Error::Zip("hyperlink relationship is missing"));
                }
                for touched in newly_touched(&before, package) {
                    remember_edited_part(&mut self.edited_parts, touched);
                }
                return Ok(());
            }
        }

        let new_rid = match edit {
            HyperlinkEdit::External(target) => {
                Some(package.add_relationship(&worksheet_path, REL_HYPERLINK, target, true))
            }
            HyperlinkEdit::Internal(_) => None,
        };
        {
            let tree = package.part_tree_mut(&worksheet_path)?;
            sml_set_hyperlink(tree, row, col, edit, new_rid.as_deref())?;
        }
        if let Some(relationship) = old_relationship {
            if record.rid_uses == 1 {
                package.remove_relationship(&worksheet_path, &relationship.id)?;
            }
        }
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Delete an external or internal hyperlink from a cell atomically.
    pub fn delete_hyperlink(&mut self, sheet_name: &str, row: u32, col: u16) -> Result<()> {
        validate_row(row)?;
        validate_col(col)?;
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.delete_hyperlink_in_place(&sheet_name, row, col)
        })
    }

    fn delete_hyperlink_in_place(&mut self, sheet_name: &str, row: u32, col: u16) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let worksheet_path = worksheet_path(package, sheet_name)?;
        let record = peek_part_tree(
            package,
            &worksheet_path,
            Error::Zip("worksheet XML is missing"),
            |tree| hyperlink_record(tree, row, col),
        )?;
        if !record.exists {
            return Err(Error::Zip("hyperlink does not exist"));
        }
        let relationship = record
            .rid
            .as_deref()
            .map(|id| validate_hyperlink_relationship(package, &worksheet_path, id))
            .transpose()?;
        let before = package.touched_parts();
        {
            let tree = package.part_tree_mut(&worksheet_path)?;
            sml_delete_hyperlink(tree, row, col)?;
        }
        if let Some(relationship) = relationship {
            if record.rid_uses == 1 {
                package.remove_relationship(&worksheet_path, &relationship.id)?;
            }
        }
        if package.relationships_of(&worksheet_path).is_empty() {
            package.remove_part(&Package::rels_path_of(&worksheet_path));
        }
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Create or replace one worksheet data-validation rule atomically.
    ///
    /// A rule is identified by its exact inclusive [`DataValidation::sqref`]
    /// range. Replacing an existing single-range rule updates only modeled
    /// attributes and formula children, preserving unknown OOXML attributes
    /// and child elements. Overlapping rules and multi-range `sqref` records
    /// are rejected rather than merged ambiguously.
    pub fn set_data_validation(
        &mut self,
        sheet_name: &str,
        validation: DataValidation,
    ) -> Result<()> {
        let (r0, c0, r1, c1) = validation.sqref;
        validate_layout_range(r0, c0, r1, c1)?;
        crate::write::validate_data_validation_rule(&validation)
            .map_err(|_| Error::Zip("invalid data-validation rule"))?;
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.set_data_validation_in_place(&sheet_name, &validation)
        })
    }

    fn set_data_validation_in_place(
        &mut self,
        sheet_name: &str,
        validation: &DataValidation,
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let worksheet_path = worksheet_path(package, sheet_name)?;
        let existing = peek_part_tree(
            package,
            &worksheet_path,
            Error::Zip("worksheet XML is missing"),
            |tree| data_validation_target(tree, validation.sqref),
        )?;
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&worksheet_path)?;
        sml_set_data_validation(tree, validation, existing)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Delete the data-validation rule at one exact inclusive range atomically.
    ///
    /// The operation rejects overlapping or multi-range validation records;
    /// it never edits one token inside an ambiguous space-separated `sqref`.
    pub fn delete_data_validation(
        &mut self,
        sheet_name: &str,
        sqref: (u32, u16, u32, u16),
    ) -> Result<()> {
        validate_layout_range(sqref.0, sqref.1, sqref.2, sqref.3)?;
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.delete_data_validation_in_place(&sheet_name, sqref)
        })
    }

    fn delete_data_validation_in_place(
        &mut self,
        sheet_name: &str,
        sqref: (u32, u16, u32, u16),
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let worksheet_path = worksheet_path(package, sheet_name)?;
        let existing = peek_part_tree(
            package,
            &worksheet_path,
            Error::Zip("worksheet XML is missing"),
            |tree| data_validation_target(tree, sqref),
        )?
        .ok_or(Error::Zip("data-validation rule does not exist"))?;
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&worksheet_path)?;
        sml_delete_data_validation(tree, existing)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }

    /// Resize or move an existing worksheet table atomically.
    ///
    /// The requested inclusive range must stay within the Excel grid and keep
    /// exactly the table's existing header-column width. The table part and
    /// its `autoFilter` range are updated in place, preserving unknown OOXML.
    /// Table creation/deletion and structural row/column insertion are not
    /// performed by this API.
    pub fn set_table_range(
        &mut self,
        sheet_name: &str,
        table_name: &str,
        range: (u32, u16, u32, u16),
    ) -> Result<()> {
        validate_layout_range(range.0, range.1, range.2, range.3)?;
        if table_name.is_empty() {
            return Err(Error::Zip("table name is empty"));
        }
        let sheet_name = sheet_name.to_string();
        let table_name = table_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.set_table_range_in_place(&sheet_name, &table_name, range)
        })
    }

    fn set_table_range_in_place(
        &mut self,
        sheet_name: &str,
        table_name: &str,
        range: (u32, u16, u32, u16),
    ) -> Result<()> {
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let worksheet_path = worksheet_path(package, sheet_name)?;
        let table_parts = worksheet_table_parts(package, &worksheet_path)?;
        let mut plans = Vec::new();
        for path in table_parts {
            let plan = peek_part_tree(
                package,
                &path,
                Error::Zip("table XML part is missing"),
                inspect_table_part,
            )?;
            plans.push((path, plan));
        }
        let matches: Vec<_> = plans
            .iter()
            .enumerate()
            .filter(|(_, (_, plan))| plan.name.eq_ignore_ascii_case(table_name))
            .map(|(index, _)| index)
            .collect();
        if matches.len() != 1 {
            return Err(Error::Zip("table name is missing or ambiguous"));
        }
        let target_index = matches[0];
        let target = &plans[target_index].1;
        let width = u32::from(range.3 - range.1) + 1;
        if width != target.column_count as u32 {
            return Err(Error::Zip(
                "table range width does not match its header-column count",
            ));
        }
        if (range.0, range.1, range.3) != (target.range.0, target.range.1, target.range.3) {
            return Err(Error::Zip(
                "moving or changing a table header range is unsupported",
            ));
        }
        if range.2 < range.0.saturating_add(target.filter_tail_rows) {
            return Err(Error::Zip(
                "table range is too short for its existing totals-row layout",
            ));
        }
        if range != target.range && target.has_sort_state {
            return Err(Error::Zip(
                "resizing a table with an active sort state is unsupported",
            ));
        }
        if plans
            .iter()
            .enumerate()
            .any(|(index, (_, plan))| index != target_index && ranges_overlap(range, plan.range))
        {
            return Err(Error::Zip("table range overlaps another table"));
        }
        if range == target.range {
            return Ok(());
        }

        let before = package.touched_parts();
        let path = plans[target_index].0.clone();
        let tree = package.part_tree_mut(&path)?;
        sml_set_table_range(tree, target, range)?;
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

    /// Persist the retained package through a sibling temporary file.
    ///
    /// The complete candidate bytes are serialized before the destination is
    /// touched. A uniquely created sibling file is then written, flushed with
    /// `fsync`, and atomically renamed over `path`; every pre-rename failure
    /// removes the temporary file and leaves an existing destination intact.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let bytes = self.save()?;
        atomic_write_sibling(path.as_ref(), &bytes)
    }
}

fn atomic_write_sibling(path: &Path, bytes: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .ok_or(Error::Zip("atomic save destination has no file name"))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut opened: Option<(PathBuf, File)> = None;
    for _ in 0..128 {
        let ordinal = SAVE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".rxls-tmp-{}-{ordinal}", std::process::id()));
        let temp_path = parent.join(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => {
                opened = Some((temp_path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(Error::Zip("failed to create atomic save temporary file")),
        }
    }
    let (temp_path, mut temp_file) = opened.ok_or(Error::Zip(
        "could not allocate a unique atomic save temporary file",
    ))?;

    if temp_file
        .write_all(bytes)
        .and_then(|_| temp_file.sync_all())
        .is_err()
    {
        drop(temp_file);
        let _ = fs::remove_file(&temp_path);
        return Err(Error::Zip("failed to write atomic save temporary file"));
    }
    drop(temp_file);

    if fs::rename(&temp_path, path).is_err() {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::Zip("failed to atomically replace spreadsheet file"));
    }

    #[cfg(unix)]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| Error::Zip("failed to sync atomic save directory"))?;
    }
    Ok(())
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
                && Package::resolve_rel_target(&workbook_path, &relationship.target)
                    == CALC_CHAIN_PART
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AppSheetTitleRepair {
    worksheet_count_node: NodeId,
    titles_vector: NodeId,
    title_node: NodeId,
    new_worksheet_count: usize,
    new_titles_size: usize,
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

fn only_element_child(tree: &XmlTree, parent: NodeId) -> Option<NodeId> {
    let children: Vec<_> = tree
        .children_of(parent)
        .iter()
        .copied()
        .filter(|&node| tree.element_name(node).is_some())
        .collect();
    (children.len() == 1).then_some(children[0])
}

fn parse_vector_size(tree: &XmlTree, vector: NodeId) -> Result<usize> {
    tree.attr_value(vector, b"size")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or(Error::Zip("docProps/app.xml vector size is malformed"))
}

fn plan_app_sheet_title_repair(
    tree: &XmlTree,
    deleted_name: &str,
    worksheet_count: usize,
) -> Result<Option<AppSheetTitleRepair>> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("docProps/app.xml is malformed"))?;
    let heading_pairs = direct_elements_by_local_name(tree, root, b"HeadingPairs");
    let titles = direct_elements_by_local_name(tree, root, b"TitlesOfParts");
    if heading_pairs.is_empty() && titles.is_empty() {
        return Ok(None);
    }
    if heading_pairs.len() != 1 || titles.len() != 1 {
        return Err(Error::Zip(
            "docProps/app.xml sheet-title metadata is missing or ambiguous",
        ));
    }
    let heading_vectors = direct_elements_by_local_name(tree, heading_pairs[0], b"vector");
    let title_vectors = direct_elements_by_local_name(tree, titles[0], b"vector");
    if heading_vectors.len() != 1 || title_vectors.len() != 1 {
        return Err(Error::Zip(
            "docProps/app.xml title vectors are missing or ambiguous",
        ));
    }
    let heading_vector = heading_vectors[0];
    let titles_vector = title_vectors[0];
    let variants = direct_elements_by_local_name(tree, heading_vector, b"variant");
    if variants.len() % 2 != 0 || parse_vector_size(tree, heading_vector)? != variants.len() {
        return Err(Error::Zip("docProps/app.xml heading pairs are malformed"));
    }

    let mut worksheet_counts = Vec::new();
    let mut titles_accounted_for = 0usize;
    for pair in variants.chunks_exact(2) {
        let label = only_element_child(tree, pair[0])
            .ok_or(Error::Zip("docProps/app.xml heading label is malformed"))?;
        let count = only_element_child(tree, pair[1])
            .ok_or(Error::Zip("docProps/app.xml heading count is malformed"))?;
        let value = tree
            .text_of(count)
            .parse::<usize>()
            .map_err(|_| Error::Zip("docProps/app.xml heading count is malformed"))?;
        if tree.text_of(label).eq_ignore_ascii_case("Worksheets") {
            worksheet_counts.push((count, value, titles_accounted_for));
        }
        titles_accounted_for = titles_accounted_for
            .checked_add(value)
            .ok_or(Error::Zip("docProps/app.xml heading counts overflow"))?;
    }
    if worksheet_counts.len() != 1 || worksheet_counts[0].1 != worksheet_count {
        return Err(Error::Zip(
            "docProps/app.xml worksheet count does not match the workbook",
        ));
    }

    let title_nodes: Vec<_> = tree
        .children_of(titles_vector)
        .iter()
        .copied()
        .filter(|&node| tree.element_name(node).is_some())
        .collect();
    if parse_vector_size(tree, titles_vector)? != title_nodes.len()
        || titles_accounted_for != title_nodes.len()
    {
        return Err(Error::Zip("docProps/app.xml sheet titles are malformed"));
    }
    let title_start = worksheet_counts[0].2;
    let title_end = title_start
        .checked_add(worksheet_count)
        .filter(|&end| end <= title_nodes.len())
        .ok_or(Error::Zip("docProps/app.xml sheet titles are malformed"))?;
    let matches: Vec<_> = title_nodes[title_start..title_end]
        .iter()
        .copied()
        .filter(|&node| tree.text_of(node) == deleted_name)
        .collect();
    if matches.len() != 1 {
        return Err(Error::Zip(
            "docProps/app.xml deleted sheet title is missing or ambiguous",
        ));
    }
    Ok(Some(AppSheetTitleRepair {
        worksheet_count_node: worksheet_counts[0].0,
        titles_vector,
        title_node: matches[0],
        new_worksheet_count: worksheet_count - 1,
        new_titles_size: title_nodes.len() - 1,
    }))
}

fn apply_app_sheet_title_repair(tree: &mut XmlTree, repair: AppSheetTitleRepair) -> Result<()> {
    tree.set_element_text(
        repair.worksheet_count_node,
        &repair.new_worksheet_count.to_string(),
    )?;
    tree.remove_child(repair.titles_vector, repair.title_node)?;
    tree.set_attr(
        repair.titles_vector,
        b"size",
        repair.new_titles_size.to_string().as_bytes(),
    )?;
    Ok(())
}

fn validate_sheet_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.trim() != name
        || name.chars().count() > 31
        || !name.chars().all(|ch| {
            let scalar = ch as u32;
            (scalar >= 0x20 || matches!(ch, '\t' | '\n' | '\r'))
                && !matches!(scalar, 0xFFFE | 0xFFFF)
        })
        || name
            .chars()
            .any(|ch| matches!(ch, ':' | '\\' | '/' | '?' | '*' | '[' | ']'))
    {
        return Err(Error::Zip("invalid sheet name"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum RowLayoutEdit {
    Height(f32),
    Hidden(bool),
}

#[derive(Clone, Copy)]
enum ColumnLayoutEdit {
    Width(f32),
    Hidden(bool),
}

type XmlAttributes = Vec<(Vec<u8>, Vec<u8>)>;

struct ColumnSpan {
    node: NodeId,
    first: u32,
    last: u32,
    attributes: XmlAttributes,
}

fn validate_row(row: u32) -> Result<()> {
    if row <= MAX_XLSX_ROW {
        Ok(())
    } else {
        Err(Error::Zip("row is outside the XLSX worksheet grid"))
    }
}

fn validate_col(col: u16) -> Result<()> {
    if col <= MAX_XLSX_COL {
        Ok(())
    } else {
        Err(Error::Zip("column is outside the XLSX worksheet grid"))
    }
}

fn validate_layout_measure(value: f32, maximum: f32, message: &'static str) -> Result<()> {
    if value.is_finite() && (0.0..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(Error::Zip(message))
    }
}

fn validate_layout_range(r0: u32, c0: u16, r1: u32, c1: u16) -> Result<()> {
    validate_row(r0)?;
    validate_row(r1)?;
    validate_col(c0)?;
    validate_col(c1)?;
    if r0 <= r1 && c0 <= c1 {
        Ok(())
    } else {
        Err(Error::Zip("worksheet range endpoints are reversed"))
    }
}

fn parse_a1_cell(reference: &str) -> Option<(u32, u16)> {
    let bytes = reference.as_bytes();
    let mut i = usize::from(bytes.first() == Some(&b'$'));
    let mut column = 0u32;
    let mut letters = 0usize;
    while let Some(&byte) = bytes.get(i) {
        if !byte.is_ascii_alphabetic() {
            break;
        }
        column = column
            .checked_mul(26)?
            .checked_add(u32::from(byte.to_ascii_uppercase() - b'A') + 1)?;
        letters += 1;
        i += 1;
    }
    if letters == 0 || column == 0 || column > u32::from(MAX_XLSX_COL) + 1 {
        return None;
    }
    if bytes.get(i) == Some(&b'$') {
        i += 1;
    }
    let digits_start = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if digits_start == i || i != bytes.len() {
        return None;
    }
    let row = reference[digits_start..].parse::<u32>().ok()?;
    if row == 0 || row > MAX_XLSX_ROW + 1 {
        return None;
    }
    Some((row - 1, u16::try_from(column - 1).ok()?))
}

fn parse_a1_range(reference: &str) -> Option<(u32, u16, u32, u16)> {
    let (first, last) = reference.split_once(':').unwrap_or((reference, reference));
    let (r0, c0) = parse_a1_cell(first)?;
    let (r1, c1) = parse_a1_cell(last)?;
    (r0 <= r1 && c0 <= c1).then_some((r0, c0, r1, c1))
}

fn ranges_overlap(left: (u32, u16, u32, u16), right: (u32, u16, u32, u16)) -> bool {
    left.0 <= right.2 && right.0 <= left.2 && left.1 <= right.3 && right.1 <= left.3
}

fn merge_cells_node(tree: &XmlTree) -> Option<NodeId> {
    let worksheet = tree.root_element()?;
    tree.child_by_name(worksheet, b"mergeCells")
}

fn merge_range_of(tree: &XmlTree, node: NodeId) -> Option<(u32, u16, u32, u16)> {
    tree.attr_value(node, b"ref")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(parse_a1_range)
}

fn validate_merge_does_not_overlap(tree: &XmlTree, requested: (u32, u16, u32, u16)) -> Result<()> {
    let Some(merges) = merge_cells_node(tree) else {
        return Ok(());
    };
    if tree
        .children_of(merges)
        .iter()
        .filter_map(|&node| merge_range_of(tree, node))
        .any(|existing| ranges_overlap(existing, requested))
    {
        Err(Error::Zip("merged range overlaps an existing merge"))
    } else {
        Ok(())
    }
}

fn find_exact_merge(tree: &XmlTree, requested: (u32, u16, u32, u16)) -> Option<(NodeId, NodeId)> {
    let merges = merge_cells_node(tree)?;
    tree.children_of(merges)
        .iter()
        .copied()
        .find(|&node| merge_range_of(tree, node) == Some(requested))
        .map(|node| (merges, node))
}

fn range_ref(range: (u32, u16, u32, u16)) -> String {
    format!("{}:{}", a1(range.0, range.1), a1(range.2, range.3))
}

fn worksheet_child_rank(name: &[u8]) -> u8 {
    match local(name) {
        b"sheetPr" => 0,
        b"dimension" => 1,
        b"sheetViews" => 2,
        b"sheetFormatPr" => 3,
        b"cols" => 4,
        b"sheetData" => 5,
        b"sheetCalcPr" => 6,
        b"sheetProtection" => 7,
        b"protectedRanges" => 8,
        b"scenarios" => 9,
        b"autoFilter" => 10,
        b"sortState" => 11,
        b"dataConsolidate" => 12,
        b"customSheetViews" => 13,
        b"mergeCells" => 14,
        b"phoneticPr" => 15,
        b"conditionalFormatting" => 16,
        b"dataValidations" => 17,
        b"hyperlinks" => 18,
        b"printOptions" => 19,
        b"pageMargins" => 20,
        b"pageSetup" => 21,
        b"headerFooter" => 22,
        b"rowBreaks" => 23,
        b"colBreaks" => 24,
        b"customProperties" => 25,
        b"cellWatches" => 26,
        b"ignoredErrors" => 27,
        b"smartTags" => 28,
        b"drawing" => 29,
        b"legacyDrawing" => 30,
        b"legacyDrawingHF" => 31,
        b"picture" => 32,
        b"oleObjects" => 33,
        b"controls" => 34,
        b"webPublishItems" => 35,
        b"tableParts" => 36,
        b"extLst" => 37,
        _ => 38,
    }
}

fn insert_worksheet_fragment(
    tree: &mut XmlTree,
    worksheet: NodeId,
    rank: u8,
    fragment: &[u8],
) -> Result<NodeId> {
    let index = tree
        .children_of(worksheet)
        .iter()
        .position(|&node| {
            tree.element_name(node)
                .is_some_and(|name| worksheet_child_rank(name) > rank)
        })
        .unwrap_or_else(|| tree.children_of(worksheet).len());
    tree.insert_fragment_at(worksheet, index, fragment)
}

fn merge_count(tree: &XmlTree, merges: NodeId) -> usize {
    tree.children_of(merges)
        .iter()
        .filter(|&&node| {
            tree.element_name(node)
                .is_some_and(|name| local(name) == b"mergeCell")
        })
        .count()
}

fn sml_add_merge(tree: &mut XmlTree, range: (u32, u16, u32, u16)) -> Result<()> {
    let worksheet = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let merges = match tree.child_by_name(worksheet, b"mergeCells") {
        Some(node) => node,
        None => insert_worksheet_fragment(tree, worksheet, 14, b"<mergeCells></mergeCells>")?,
    };
    let fragment = format!(r#"<mergeCell ref="{}"/>"#, range_ref(range));
    let index = tree.children_of(merges).len();
    tree.insert_fragment_at(merges, index, fragment.as_bytes())?;
    tree.set_attr(
        merges,
        b"count",
        merge_count(tree, merges).to_string().as_bytes(),
    )?;
    Ok(())
}

fn sml_remove_merge(tree: &mut XmlTree, range: (u32, u16, u32, u16)) -> Result<()> {
    let worksheet = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let (merges, node) =
        find_exact_merge(tree, range).ok_or(Error::Zip("merged range does not exist"))?;
    tree.remove_child(merges, node)?;
    let count = merge_count(tree, merges);
    if count == 0 {
        tree.remove_child(worksheet, merges)?;
    } else {
        tree.set_attr(merges, b"count", count.to_string().as_bytes())?;
    }
    Ok(())
}

fn row_is_hidden(tree: &XmlTree, row: u32) -> bool {
    let Some(worksheet) = tree.root_element() else {
        return false;
    };
    let Some(sheet_data) = tree.child_by_name(worksheet, b"sheetData") else {
        return false;
    };
    tree.children_of(sheet_data).iter().copied().any(|node| {
        sml_row_ref(tree, node) == Some(row + 1)
            && tree
                .attr_value(node, b"hidden")
                .is_some_and(attr_true_bytes)
    })
}

fn attr_true_bytes(value: &[u8]) -> bool {
    matches!(value, b"1" | b"true" | b"TRUE")
}

fn sml_set_row_layout(tree: &mut XmlTree, row: u32, edit: RowLayoutEdit) -> Result<()> {
    let sheet_data = sml_sheet_data(tree)?;
    let row_node = sml_row_node(tree, sheet_data, row)?;
    match edit {
        RowLayoutEdit::Height(points) => {
            tree.set_attr(row_node, b"ht", num_str(f64::from(points)).as_bytes())?;
            tree.set_attr(row_node, b"customHeight", b"1")?;
        }
        RowLayoutEdit::Hidden(true) => tree.set_attr(row_node, b"hidden", b"1")?,
        RowLayoutEdit::Hidden(false) => tree.remove_attr(row_node, b"hidden"),
    }
    Ok(())
}

fn column_bounds(tree: &XmlTree, node: NodeId) -> Option<(u32, u32)> {
    if tree
        .element_name(node)
        .is_none_or(|name| local(name) != b"col")
    {
        return None;
    }
    let first = tree
        .attr_value(node, b"min")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<u32>().ok())?;
    let last = tree
        .attr_value(node, b"max")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<u32>().ok())?;
    (first >= 1 && first <= last && last <= u32::from(MAX_XLSX_COL) + 1).then_some((first, last))
}

fn column_is_hidden(tree: &XmlTree, col: u16) -> bool {
    let Some(worksheet) = tree.root_element() else {
        return false;
    };
    let Some(cols) = tree.child_by_name(worksheet, b"cols") else {
        return false;
    };
    let target = u32::from(col) + 1;
    tree.children_of(cols).iter().copied().any(|node| {
        column_bounds(tree, node).is_some_and(|(first, last)| first <= target && target <= last)
            && tree
                .attr_value(node, b"hidden")
                .is_some_and(attr_true_bytes)
    })
}

fn set_attribute(attributes: &mut XmlAttributes, name: &[u8], value: impl Into<Vec<u8>>) {
    let value = value.into();
    if let Some((_, existing)) = attributes
        .iter_mut()
        .find(|(existing, _)| existing.as_slice() == name)
    {
        *existing = value;
    } else {
        attributes.push((name.to_vec(), value));
    }
}

fn remove_attribute(attributes: &mut XmlAttributes, name: &[u8]) {
    attributes.retain(|(existing, _)| existing.as_slice() != name);
}

fn column_fragment(mut attributes: XmlAttributes, first: u32, last: u32) -> Result<Vec<u8>> {
    set_attribute(&mut attributes, b"min", first.to_string().into_bytes());
    set_attribute(&mut attributes, b"max", last.to_string().into_bytes());
    let mut fragment = String::from("<col");
    for (name, value) in attributes {
        let name = std::str::from_utf8(&name)
            .map_err(|_| Error::Xml("column attribute name is not UTF-8"))?;
        let value = std::str::from_utf8(&value)
            .map_err(|_| Error::Xml("column attribute value is not UTF-8"))?;
        fragment.push(' ');
        fragment.push_str(name);
        fragment.push_str("=\"");
        fragment.push_str(&esc_attr(value));
        fragment.push('"');
    }
    fragment.push_str("/>");
    Ok(fragment.into_bytes())
}

fn sml_set_column_layout(tree: &mut XmlTree, col: u16, edit: ColumnLayoutEdit) -> Result<()> {
    let worksheet = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let cols = match tree.child_by_name(worksheet, b"cols") {
        Some(node) => node,
        None => insert_worksheet_fragment(tree, worksheet, 4, b"<cols></cols>")?,
    };
    let target = u32::from(col) + 1;
    let matches: Vec<ColumnSpan> = tree
        .children_of(cols)
        .iter()
        .copied()
        .filter_map(|node| {
            let (first, last) = column_bounds(tree, node)?;
            (first <= target && target <= last).then(|| ColumnSpan {
                node,
                first,
                last,
                attributes: tree.attributes(node).unwrap_or_default().to_vec(),
            })
        })
        .collect();
    let mut target_attributes = matches
        .last()
        .map(|span| span.attributes.clone())
        .unwrap_or_default();

    for span in matches.iter().rev() {
        let index = tree
            .children_of(cols)
            .iter()
            .position(|candidate| candidate == &span.node)
            .ok_or(Error::Xml("column node is detached"))?;
        tree.remove_child(cols, span.node)?;
        let mut offset = 0usize;
        if span.first < target {
            let fragment = column_fragment(span.attributes.clone(), span.first, target - 1)?;
            tree.insert_fragment_at(cols, index, &fragment)?;
            offset += 1;
        }
        if target < span.last {
            let fragment = column_fragment(span.attributes.clone(), target + 1, span.last)?;
            tree.insert_fragment_at(cols, index + offset, &fragment)?;
        }
    }

    match edit {
        ColumnLayoutEdit::Width(width) => {
            set_attribute(
                &mut target_attributes,
                b"width",
                num_str(f64::from(width)).into_bytes(),
            );
            set_attribute(&mut target_attributes, b"customWidth", b"1".to_vec());
        }
        ColumnLayoutEdit::Hidden(true) => {
            set_attribute(&mut target_attributes, b"hidden", b"1".to_vec());
        }
        ColumnLayoutEdit::Hidden(false) => {
            remove_attribute(&mut target_attributes, b"hidden");
        }
    }
    let fragment = column_fragment(target_attributes, target, target)?;
    let index = tree
        .children_of(cols)
        .iter()
        .position(|&node| column_bounds(tree, node).is_some_and(|(first, _)| first > target))
        .unwrap_or_else(|| tree.children_of(cols).len());
    tree.insert_fragment_at(cols, index, &fragment)?;
    Ok(())
}

fn selected_sheet_view(tree: &XmlTree) -> Option<NodeId> {
    let worksheet = tree.root_element()?;
    let views = tree.child_by_name(worksheet, b"sheetViews")?;
    tree.children_of(views).iter().copied().find(|&node| {
        tree.element_name(node) == Some(b"sheetView")
            && tree
                .attr_value(node, b"workbookViewId")
                .map(|value| value == b"0")
                .unwrap_or(true)
    })
}

fn find_frozen_pane(tree: &XmlTree) -> Option<(NodeId, NodeId)> {
    let view = selected_sheet_view(tree)?;
    tree.children_of(view)
        .iter()
        .copied()
        .find(|&node| {
            tree.element_name(node) == Some(b"pane")
                && tree
                    .attr_value(node, b"state")
                    .is_some_and(|state| matches!(state, b"frozen" | b"frozenSplit"))
        })
        .map(|pane| (view, pane))
}

fn sml_set_freeze_panes(tree: &mut XmlTree, freeze: Option<(u32, u16)>) -> Result<()> {
    let freeze = freeze.filter(|&(row, col)| row > 0 || col > 0);
    if freeze.is_none() {
        while let Some((view, pane)) = find_frozen_pane(tree) {
            tree.remove_child(view, pane)?;
        }
        return Ok(());
    }

    let worksheet = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let views = match tree.child_by_name(worksheet, b"sheetViews") {
        Some(node) => node,
        None => insert_worksheet_fragment(tree, worksheet, 2, b"<sheetViews></sheetViews>")?,
    };
    let view = match selected_sheet_view(tree) {
        Some(node) => node,
        None => {
            let index = tree.children_of(views).len();
            tree.insert_fragment_at(
                views,
                index,
                b"<sheetView workbookViewId=\"0\"></sheetView>",
            )?
        }
    };
    let pane = tree
        .children_of(view)
        .iter()
        .copied()
        .find(|&node| tree.element_name(node) == Some(b"pane"))
        .map(Ok)
        .unwrap_or_else(|| tree.insert_fragment_at(view, 0, b"<pane/>"))?;
    for attribute in [
        b"xSplit".as_slice(),
        b"ySplit",
        b"topLeftCell",
        b"activePane",
    ] {
        tree.remove_attr(pane, attribute);
    }
    let (row, col) = freeze.expect("filtered above");
    if col > 0 {
        tree.set_attr(pane, b"xSplit", col.to_string().as_bytes())?;
    }
    if row > 0 {
        tree.set_attr(pane, b"ySplit", row.to_string().as_bytes())?;
    }
    tree.set_attr(pane, b"topLeftCell", a1(row, col).as_bytes())?;
    let active_pane = match (row > 0, col > 0) {
        (true, true) => b"bottomRight".as_slice(),
        (true, false) => b"bottomLeft".as_slice(),
        (false, true) => b"topRight".as_slice(),
        (false, false) => unreachable!(),
    };
    tree.set_attr(pane, b"activePane", active_pane)?;
    tree.set_attr(pane, b"state", b"frozen")?;
    Ok(())
}

fn find_local_defined_name(tree: &XmlTree, name: &str, sheet_index: usize) -> Option<NodeId> {
    let workbook = tree.root_element()?;
    let names = tree.child_by_name(workbook, b"definedNames")?;
    let sheet_index = sheet_index.to_string();
    tree.children_of(names).iter().copied().find(|&node| {
        tree.element_name(node) == Some(b"definedName")
            && tree.attr_value(node, b"name") == Some(name.as_bytes())
            && tree.attr_value(node, b"localSheetId") == Some(sheet_index.as_bytes())
    })
}

fn absolute_a1(row: u32, col: u16) -> String {
    let cell = a1(row, col);
    let split = cell
        .find(|character: char| character.is_ascii_digit())
        .unwrap_or(cell.len());
    format!("${}${}", &cell[..split], &cell[split..])
}

fn workbook_child_rank(name: &[u8]) -> u8 {
    match local(name) {
        b"fileVersion" => 0,
        b"fileSharing" => 1,
        b"workbookPr" => 2,
        b"workbookProtection" => 3,
        b"bookViews" => 4,
        b"sheets" => 5,
        b"functionGroups" => 6,
        b"externalReferences" => 7,
        b"definedNames" => 8,
        b"calcPr" => 9,
        b"oleSize" => 10,
        b"customWorkbookViews" => 11,
        b"pivotCaches" => 12,
        b"smartTagPr" => 13,
        b"smartTagTypes" => 14,
        b"webPublishing" => 15,
        b"fileRecoveryPr" => 16,
        b"webPublishObjects" => 17,
        b"extLst" => 18,
        _ => 19,
    }
}

fn sml_set_print_area(
    tree: &mut XmlTree,
    sheet_name: &str,
    sheet_index: usize,
    area: Option<(u32, u16, u32, u16)>,
) -> Result<()> {
    let workbook = tree.root_element().ok_or(Error::MissingWorkbook)?;
    let existing = find_local_defined_name(tree, "_xlnm.Print_Area", sheet_index);
    match (existing, area) {
        (Some(node), Some((r0, c0, r1, c1))) => {
            let quoted = sheet_name.replace('\'', "''");
            let formula = format!("'{quoted}'!{}:{}", absolute_a1(r0, c0), absolute_a1(r1, c1));
            tree.set_element_text(node, &formula)?;
        }
        (None, Some((r0, c0, r1, c1))) => {
            let quoted = sheet_name.replace('\'', "''");
            let formula = format!("'{quoted}'!{}:{}", absolute_a1(r0, c0), absolute_a1(r1, c1));
            let fragment = format!(
                r#"<definedName name="_xlnm.Print_Area" localSheetId="{sheet_index}">{}</definedName>"#,
                esc_text(&formula)
            );
            if let Some(names) = tree.child_by_name(workbook, b"definedNames") {
                let index = tree.children_of(names).len();
                tree.insert_fragment_at(names, index, fragment.as_bytes())?;
            } else {
                let wrapped = format!("<definedNames>{fragment}</definedNames>");
                let index = tree
                    .children_of(workbook)
                    .iter()
                    .position(|&node| {
                        tree.element_name(node)
                            .is_some_and(|name| workbook_child_rank(name) > 8)
                    })
                    .unwrap_or_else(|| tree.children_of(workbook).len());
                tree.insert_fragment_at(workbook, index, wrapped.as_bytes())?;
            }
        }
        (Some(node), None) => {
            let names = tree
                .child_by_name(workbook, b"definedNames")
                .ok_or(Error::MissingWorkbook)?;
            tree.remove_child(names, node)?;
            if !tree
                .children_of(names)
                .iter()
                .any(|&child| tree.element_name(child) == Some(b"definedName"))
            {
                tree.remove_child(workbook, names)?;
            }
        }
        (None, None) => {}
    }
    Ok(())
}

#[derive(Clone)]
struct RelatedPart {
    id: String,
    path: String,
}

#[derive(Clone, Copy)]
enum HyperlinkEdit<'a> {
    External(&'a str),
    Internal(&'a str),
}

#[derive(Default)]
struct HyperlinkRecord {
    exists: bool,
    rid: Option<String>,
    rid_uses: usize,
}

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

fn validate_formula_cached_value(value: &Cell) -> Result<()> {
    match value {
        Cell::Text(text) => {
            validate_edit_cell_text(text, "formula cached text contains invalid XML characters")
        }
        Cell::Error(error) => validate_xml_value(
            error,
            "formula cached error contains invalid XML characters",
        ),
        Cell::Number(number) | Cell::Date(number) if !number.is_finite() => {
            Err(Error::Zip("formula cached numeric value must be finite"))
        }
        Cell::Formula { .. } => Err(Error::Zip(
            "formula cached value cannot contain another formula",
        )),
        Cell::Number(_) | Cell::Date(_) | Cell::Bool(_) => Ok(()),
    }
}

fn validate_edit_cell_value(value: &Cell) -> Result<()> {
    match value {
        Cell::Text(text) => {
            validate_edit_cell_text(text, "cell text contains invalid XML characters")
        }
        Cell::Error(error) => {
            validate_xml_value(error, "cell error contains invalid XML characters")
        }
        Cell::Number(number) | Cell::Date(number) if !number.is_finite() => {
            Err(Error::Zip("cell numeric value must be finite"))
        }
        Cell::Formula { formula, cached } => {
            validate_xml_value(formula, "formula contains invalid XML characters")?;
            validate_formula_cached_value(cached)
        }
        Cell::Number(_) | Cell::Date(_) | Cell::Bool(_) => Ok(()),
    }
}

fn validate_document_properties_for_edit(properties: &DocProperties) -> Result<()> {
    for value in [
        properties.title.as_deref(),
        properties.subject.as_deref(),
        properties.creator.as_deref(),
        properties.keywords.as_deref(),
        properties.description.as_deref(),
        properties.last_modified_by.as_deref(),
        properties.company.as_deref(),
        properties.created.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_xml_value(value, "document property contains invalid XML characters")?;
    }
    if properties
        .created
        .as_deref()
        .is_some_and(|timestamp| !crate::write::is_w3cdtf(timestamp))
    {
        return Err(Error::Zip("document property timestamp is invalid"));
    }
    Ok(())
}

fn unique_related_part(
    package: &Package,
    source: &str,
    relationship_kind: &str,
) -> Result<Option<RelatedPart>> {
    let matches: Vec<_> = package
        .relationships_of(source)
        .iter()
        .filter(|relationship| {
            !relationship.external
                && relationship
                    .rel_type
                    .rsplit('/')
                    .next()
                    .is_some_and(|kind| kind.eq_ignore_ascii_case(relationship_kind))
        })
        .collect();
    if matches.len() > 1 {
        return Err(Error::Zip(
            "multiple relationships of the requested type are unsupported",
        ));
    }
    Ok(matches.first().map(|relationship| RelatedPart {
        id: relationship.id.clone(),
        path: Package::resolve_rel_target(source, &relationship.target),
    }))
}

fn next_numbered_part_name(package: &Package, prefix: &str, extension: &str) -> Result<String> {
    let used: BTreeSet<String> = package
        .part_names()
        .map(canonical_part_name)
        .map(|name| name.to_ascii_lowercase())
        .collect();
    for ordinal in 1..=u32::MAX {
        let candidate = format!("{prefix}{ordinal}{extension}");
        if !used.contains(&candidate.to_ascii_lowercase()) {
            return Ok(candidate);
        }
    }
    Err(Error::Zip("numbered OOXML part-name space is exhausted"))
}

fn workbook_directory(package: &Package) -> String {
    workbook_path(package)
        .rsplit_once('/')
        .map(|(directory, _)| directory.to_string())
        .unwrap_or_default()
}

fn next_comment_part_name(package: &Package) -> Result<String> {
    let directory = workbook_directory(package);
    let prefix = if directory.is_empty() {
        "comments".to_string()
    } else {
        format!("{directory}/comments")
    };
    next_numbered_part_name(package, &prefix, ".xml")
}

fn next_vml_part_name(package: &Package) -> Result<String> {
    let directory = workbook_directory(package);
    let prefix = if directory.is_empty() {
        "drawings/vmlDrawing".to_string()
    } else {
        format!("{directory}/drawings/vmlDrawing")
    };
    next_numbered_part_name(package, &prefix, ".vml")
}

fn child_by_local_name(tree: &XmlTree, parent: NodeId, name: &[u8]) -> Option<NodeId> {
    tree.children_of(parent).iter().copied().find(|&node| {
        tree.element_name(node)
            .is_some_and(|element| local(element) == name)
    })
}

fn comment_list_node(tree: &XmlTree) -> Option<NodeId> {
    let root = tree.root_element()?;
    (tree
        .element_name(root)
        .is_some_and(|name| local(name) == b"comments"))
    .then(|| child_by_local_name(tree, root, b"commentList"))
    .flatten()
}

fn comment_nodes_at(tree: &XmlTree, row: u32, col: u16) -> Vec<NodeId> {
    let Some(list) = comment_list_node(tree) else {
        return Vec::new();
    };
    let reference = a1(row, col);
    tree.children_of(list)
        .iter()
        .copied()
        .filter(|&node| {
            tree.element_name(node)
                .is_some_and(|name| local(name) == b"comment")
                && tree.attr_value(node, b"ref") == Some(reference.as_bytes())
        })
        .collect()
}

fn comment_exists_exactly_once(tree: &XmlTree, row: u32, col: u16) -> Result<bool> {
    let comments = comment_nodes_at(tree, row, col);
    if comments.len() > 1 {
        Err(Error::Zip("duplicate comments at one cell are unsupported"))
    } else {
        Ok(comments.len() == 1)
    }
}

fn comment_count(tree: &XmlTree) -> usize {
    comment_list_node(tree)
        .map(|list| {
            tree.children_of(list)
                .iter()
                .filter(|&&node| {
                    tree.element_name(node)
                        .is_some_and(|name| local(name) == b"comment")
                })
                .count()
        })
        .unwrap_or(0)
}

fn sml_set_comment(
    tree: &mut XmlTree,
    row: u32,
    col: u16,
    text: &str,
    author: Option<&str>,
) -> Result<()> {
    let root = tree
        .root_element()
        .filter(|&root| {
            tree.element_name(root)
                .is_some_and(|name| local(name) == b"comments")
        })
        .ok_or(Error::Zip("comments XML root is malformed"))?;
    let authors = match child_by_local_name(tree, root, b"authors") {
        Some(node) => node,
        None => tree.insert_fragment_at(root, 0, b"<authors></authors>")?,
    };
    let author = author.unwrap_or("");
    let author_id = tree
        .children_of(authors)
        .iter()
        .copied()
        .filter(|&node| {
            tree.element_name(node)
                .is_some_and(|name| local(name) == b"author")
        })
        .position(|node| tree.text_of(node) == author)
        .unwrap_or_else(|| {
            tree.children_of(authors)
                .iter()
                .filter(|&&node| {
                    tree.element_name(node)
                        .is_some_and(|name| local(name) == b"author")
                })
                .count()
        });
    let has_author = tree
        .children_of(authors)
        .iter()
        .copied()
        .filter(|&node| {
            tree.element_name(node)
                .is_some_and(|name| local(name) == b"author")
        })
        .any(|node| tree.text_of(node) == author);
    if !has_author {
        let fragment = format!("<author>{}</author>", esc_text(author));
        let index = tree.children_of(authors).len();
        tree.insert_fragment_at(authors, index, fragment.as_bytes())?;
    }
    let list = match child_by_local_name(tree, root, b"commentList") {
        Some(node) => node,
        None => {
            let author_position = tree
                .children_of(root)
                .iter()
                .position(|&node| node == authors)
                .unwrap_or(0);
            tree.insert_fragment_at(root, author_position + 1, b"<commentList></commentList>")?
        }
    };
    let existing = comment_nodes_at(tree, row, col);
    if existing.len() > 1 {
        return Err(Error::Zip("duplicate comments at one cell are unsupported"));
    }
    let text_fragment = format!(
        r#"<text><t xml:space="preserve">{}</t></text>"#,
        esc_text(text)
    );
    if let Some(comment) = existing.first().copied() {
        tree.set_attr(comment, b"authorId", author_id.to_string().as_bytes())?;
        let old_texts: Vec<NodeId> = tree
            .children_of(comment)
            .iter()
            .copied()
            .filter(|&node| {
                tree.element_name(node)
                    .is_some_and(|name| local(name) == b"text")
            })
            .collect();
        for old_text in old_texts {
            tree.remove_child(comment, old_text)?;
        }
        tree.insert_fragment_at(comment, 0, text_fragment.as_bytes())?;
    } else {
        let fragment = format!(
            r#"<comment ref="{}" authorId="{author_id}">{text_fragment}</comment>"#,
            a1(row, col)
        );
        let index = tree.children_of(list).len();
        tree.insert_fragment_at(list, index, fragment.as_bytes())?;
    }
    Ok(())
}

fn sml_delete_comment(tree: &mut XmlTree, row: u32, col: u16) -> Result<()> {
    let list = comment_list_node(tree).ok_or(Error::Zip("comment does not exist"))?;
    let comments = comment_nodes_at(tree, row, col);
    if comments.len() != 1 {
        return Err(Error::Zip("comment does not exist or is duplicated"));
    }
    tree.remove_child(list, comments[0])
}

fn descendant_by_local_name(tree: &XmlTree, parent: NodeId, name: &[u8]) -> Option<NodeId> {
    let mut stack: Vec<NodeId> = tree.children_of(parent).iter().rev().copied().collect();
    while let Some(node) = stack.pop() {
        if tree
            .element_name(node)
            .is_some_and(|element| local(element) == name)
        {
            return Some(node);
        }
        stack.extend(tree.children_of(node).iter().rev().copied());
    }
    None
}

fn vml_note_coordinates(tree: &XmlTree, shape: NodeId) -> Option<(u32, u16)> {
    let client_data = descendant_by_local_name(tree, shape, b"ClientData")?;
    if !tree
        .attr_value(client_data, b"ObjectType")
        .and_then(|value| std::str::from_utf8(value).ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("Note"))
    {
        return None;
    }
    let row = child_by_local_name(tree, client_data, b"Row")
        .map(|node| tree.text_of(node))?
        .parse::<u32>()
        .ok()?;
    let col = child_by_local_name(tree, client_data, b"Column")
        .map(|node| tree.text_of(node))?
        .parse::<u16>()
        .ok()?;
    Some((row, col))
}

fn vml_shapes(tree: &XmlTree) -> Vec<NodeId> {
    let Some(root) = tree.root_element() else {
        return Vec::new();
    };
    let mut shapes = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node != root
            && tree
                .element_name(node)
                .is_some_and(|name| local(name) == b"shape")
        {
            shapes.push(node);
        }
        stack.extend(tree.children_of(node).iter().rev().copied());
    }
    shapes
}

fn vml_shape_count(tree: &XmlTree) -> usize {
    vml_shapes(tree).len()
}

fn validate_vml_note_target_available(tree: &XmlTree, row: u32, col: u16) -> Result<()> {
    if vml_shapes(tree)
        .into_iter()
        .any(|shape| vml_note_coordinates(tree, shape) == Some((row, col)))
    {
        Err(Error::Zip("legacy VML note shape already exists at cell"))
    } else {
        Ok(())
    }
}

fn validate_single_vml_note_shape(tree: &XmlTree, row: u32, col: u16) -> Result<()> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("legacy VML root is malformed"))?;
    let matches: Vec<_> = vml_shapes(tree)
        .into_iter()
        .filter(|&shape| vml_note_coordinates(tree, shape) == Some((row, col)))
        .collect();
    if matches.len() != 1 {
        return Err(Error::Zip("legacy VML note shape is missing or duplicated"));
    }
    if !tree.children_of(root).contains(&matches[0]) {
        return Err(Error::Zip("nested legacy VML note shapes are unsupported"));
    }
    Ok(())
}

fn next_vml_shape_id(tree: &XmlTree) -> u32 {
    vml_shapes(tree)
        .into_iter()
        .filter_map(|shape| tree.attr_value(shape, b"id"))
        .filter_map(|value| std::str::from_utf8(value).ok())
        .filter_map(|value| value.strip_prefix("_x0000_s"))
        .filter_map(|value| value.parse::<u32>().ok())
        .max()
        .unwrap_or(1024)
        .saturating_add(1)
        .max(1025)
}

fn vml_note_shape_fragment(row: u32, col: u16, shape_id: u32) -> String {
    let anchor = format!(
        "{left}, 15, {row}, 2, {right}, 15, {bottom}, 16",
        left = u32::from(col) + 1,
        right = u32::from(col) + 3,
        bottom = row.saturating_add(4),
    );
    format!(
        r##"<v:shape id="_x0000_s{shape_id}" type="#_x0000_t202" style="position:absolute;visibility:hidden" fillcolor="#ffffe1" o:insetmode="auto"><v:fill color2="#ffffe1"/><v:shadow on="t" color="black" obscured="t"/><v:path o:connecttype="none"/><v:textbox style="mso-direction-alt:auto"><div style="text-align:left"></div></v:textbox><x:ClientData ObjectType="Note"><x:MoveWithCells/><x:SizeWithCells/><x:Anchor>{anchor}</x:Anchor><x:AutoFill>False</x:AutoFill><x:Row>{row}</x:Row><x:Column>{col}</x:Column></x:ClientData></v:shape>"##
    )
}

fn sml_add_vml_note(tree: &mut XmlTree, row: u32, col: u16) -> Result<()> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("legacy VML root is malformed"))?;
    for (name, value) in [
        (
            b"xmlns:v".as_slice(),
            b"urn:schemas-microsoft-com:vml".as_slice(),
        ),
        (
            b"xmlns:o".as_slice(),
            b"urn:schemas-microsoft-com:office:office".as_slice(),
        ),
        (
            b"xmlns:x".as_slice(),
            b"urn:schemas-microsoft-com:office:excel".as_slice(),
        ),
    ] {
        if tree.attr_value(root, name).is_none() {
            tree.set_attr(root, name, value)?;
        }
    }
    let has_note_type = tree.children_of(root).iter().copied().any(|node| {
        tree.element_name(node)
            .is_some_and(|name| local(name) == b"shapetype")
            && tree.attr_value(node, b"id") == Some(b"_x0000_t202")
    });
    if !has_note_type {
        let fragment = br##"<v:shapetype id="_x0000_t202" coordsize="21600,21600" o:spt="202" path="m,l,21600r21600,l21600,xe"><v:stroke joinstyle="miter"/><v:path gradientshapeok="t" o:connecttype="rect"/></v:shapetype>"##;
        let index = tree
            .children_of(root)
            .iter()
            .position(|&node| {
                tree.element_name(node)
                    .is_some_and(|name| local(name) == b"shape")
            })
            .unwrap_or_else(|| tree.children_of(root).len());
        tree.insert_fragment_at(root, index, fragment)?;
    }
    let fragment = vml_note_shape_fragment(row, col, next_vml_shape_id(tree));
    let index = tree.children_of(root).len();
    tree.insert_fragment_at(root, index, fragment.as_bytes())?;
    Ok(())
}

fn sml_delete_vml_note(tree: &mut XmlTree, row: u32, col: u16) -> Result<()> {
    validate_single_vml_note_shape(tree, row, col)?;
    let root = tree
        .root_element()
        .ok_or(Error::Zip("legacy VML root is malformed"))?;
    let shape = tree
        .children_of(root)
        .iter()
        .copied()
        .find(|&shape| vml_note_coordinates(tree, shape) == Some((row, col)))
        .ok_or(Error::Zip("legacy VML note shape is missing"))?;
    tree.remove_child(root, shape)
}

fn legacy_drawing_nodes(tree: &XmlTree) -> Vec<NodeId> {
    let Some(root) = tree.root_element() else {
        return Vec::new();
    };
    tree.children_of(root)
        .iter()
        .copied()
        .filter(|&node| {
            tree.element_name(node)
                .is_some_and(|name| local(name) == b"legacyDrawing")
        })
        .collect()
}

fn sml_ensure_legacy_drawing(tree: &mut XmlTree, rid: &str) -> Result<()> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let drawings = legacy_drawing_nodes(tree);
    if drawings.len() > 1 {
        return Err(Error::Zip(
            "multiple legacyDrawing elements are unsupported",
        ));
    }
    if let Some(drawing) = drawings.first().copied() {
        if tree.attr_value(drawing, b"r:id") == Some(rid.as_bytes()) {
            return Ok(());
        }
        return Err(Error::Zip(
            "worksheet already references a different legacy drawing",
        ));
    }
    if tree.attr_value(root, b"xmlns:r").is_none() {
        tree.set_attr(root, b"xmlns:r", NS_R.as_bytes())?;
    }
    let fragment = format!(r#"<legacyDrawing r:id="{}"/>"#, esc_attr(rid));
    insert_worksheet_fragment(tree, root, 30, fragment.as_bytes())?;
    Ok(())
}

fn sml_remove_legacy_drawing(tree: &mut XmlTree, rid: &str) -> Result<()> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let matches: Vec<_> = legacy_drawing_nodes(tree)
        .into_iter()
        .filter(|&node| tree.attr_value(node, b"r:id") == Some(rid.as_bytes()))
        .collect();
    if matches.len() != 1 {
        return Err(Error::Zip(
            "legacyDrawing relationship is missing or duplicated",
        ));
    }
    tree.remove_child(root, matches[0])
}

fn hyperlink_record(tree: &XmlTree, row: u32, col: u16) -> Result<HyperlinkRecord> {
    let Some(root) = tree.root_element() else {
        return Err(Error::Zip("worksheet XML is malformed"));
    };
    let Some(hyperlinks) = child_by_local_name(tree, root, b"hyperlinks") else {
        return Ok(HyperlinkRecord::default());
    };
    let target = (row, col, row, col);
    let mut exact = Vec::new();
    for node in tree
        .children_of(hyperlinks)
        .iter()
        .copied()
        .filter(|&node| {
            tree.element_name(node)
                .is_some_and(|name| local(name) == b"hyperlink")
        })
    {
        let Some(range) = tree
            .attr_value(node, b"ref")
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(parse_a1_range)
        else {
            continue;
        };
        if ranges_overlap(range, target) {
            if range != target {
                return Err(Error::Zip(
                    "editing one cell inside a range hyperlink is unsupported",
                ));
            }
            exact.push(node);
        }
    }
    if exact.len() > 1 {
        return Err(Error::Zip(
            "duplicate hyperlinks at one cell are unsupported",
        ));
    }
    let Some(node) = exact.first().copied() else {
        return Ok(HyperlinkRecord::default());
    };
    let rid = tree
        .attr_value(node, b"r:id")
        .and_then(|value| std::str::from_utf8(value).ok())
        .map(str::to_string);
    let rid_uses = rid.as_deref().map_or(0, |rid| {
        tree.children_of(hyperlinks)
            .iter()
            .filter(|&&candidate| tree.attr_value(candidate, b"r:id") == Some(rid.as_bytes()))
            .count()
    });
    Ok(HyperlinkRecord {
        exists: true,
        rid,
        rid_uses,
    })
}

fn validate_hyperlink_relationship(
    package: &Package,
    worksheet_path: &str,
    rid: &str,
) -> Result<RelatedPart> {
    let matches: Vec<_> = package
        .relationships_of(worksheet_path)
        .iter()
        .filter(|relationship| relationship.id == rid)
        .collect();
    if matches.len() != 1 {
        return Err(Error::Zip(
            "hyperlink relationship is missing or duplicated",
        ));
    }
    let relationship = matches[0];
    if !relationship.external || relationship.rel_type.rsplit('/').next() != Some("hyperlink") {
        return Err(Error::Zip(
            "cell r:id is not an external hyperlink relationship",
        ));
    }
    Ok(RelatedPart {
        id: relationship.id.clone(),
        path: relationship.target.clone(),
    })
}

fn exact_hyperlink_node(tree: &XmlTree, row: u32, col: u16) -> Option<(NodeId, NodeId)> {
    let root = tree.root_element()?;
    let hyperlinks = child_by_local_name(tree, root, b"hyperlinks")?;
    let reference = a1(row, col);
    tree.children_of(hyperlinks)
        .iter()
        .copied()
        .find(|&node| {
            tree.element_name(node)
                .is_some_and(|name| local(name) == b"hyperlink")
                && tree.attr_value(node, b"ref") == Some(reference.as_bytes())
        })
        .map(|node| (hyperlinks, node))
}

fn sml_set_hyperlink(
    tree: &mut XmlTree,
    row: u32,
    col: u16,
    edit: HyperlinkEdit<'_>,
    new_rid: Option<&str>,
) -> Result<()> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let hyperlinks = match child_by_local_name(tree, root, b"hyperlinks") {
        Some(node) => node,
        None => insert_worksheet_fragment(tree, root, 18, b"<hyperlinks></hyperlinks>")?,
    };
    let hyperlink = match exact_hyperlink_node(tree, row, col) {
        Some((_, node)) => node,
        None => {
            let fragment = format!(r#"<hyperlink ref="{}"/>"#, a1(row, col));
            let index = tree.children_of(hyperlinks).len();
            tree.insert_fragment_at(hyperlinks, index, fragment.as_bytes())?
        }
    };
    match edit {
        HyperlinkEdit::External(_) => {
            let rid = new_rid.ok_or(Error::Zip("new hyperlink relationship id is missing"))?;
            if tree.attr_value(root, b"xmlns:r").is_none() {
                tree.set_attr(root, b"xmlns:r", NS_R.as_bytes())?;
            }
            tree.set_attr(hyperlink, b"r:id", rid.as_bytes())?;
            tree.remove_attr(hyperlink, b"location");
        }
        HyperlinkEdit::Internal(location) => {
            tree.set_attr(hyperlink, b"location", location.as_bytes())?;
            tree.remove_attr(hyperlink, b"r:id");
        }
    }
    Ok(())
}

fn sml_delete_hyperlink(tree: &mut XmlTree, row: u32, col: u16) -> Result<()> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let (hyperlinks, hyperlink) =
        exact_hyperlink_node(tree, row, col).ok_or(Error::Zip("hyperlink does not exist"))?;
    tree.remove_child(hyperlinks, hyperlink)?;
    let any_remaining = tree.children_of(hyperlinks).iter().any(|&node| {
        tree.element_name(node)
            .is_some_and(|name| local(name) == b"hyperlink")
    });
    if !any_remaining {
        tree.remove_child(root, hyperlinks)?;
    }
    Ok(())
}

fn data_validation_wrappers(tree: &XmlTree, root: NodeId) -> Vec<NodeId> {
    direct_elements_by_local_name(tree, root, b"dataValidations")
}

fn data_validation_nodes(tree: &XmlTree, wrapper: NodeId) -> Vec<NodeId> {
    direct_elements_by_local_name(tree, wrapper, b"dataValidation")
}

fn data_validation_ranges(tree: &XmlTree, node: NodeId) -> Result<Vec<(u32, u16, u32, u16)>> {
    let sqref = tree
        .attr_value(node, b"sqref")
        .and_then(|value| std::str::from_utf8(value).ok())
        .ok_or(Error::Zip("data-validation sqref is malformed"))?;
    let ranges: Vec<_> = sqref.split_whitespace().map(parse_a1_range).collect();
    if ranges.is_empty() || ranges.iter().any(Option::is_none) {
        return Err(Error::Zip("data-validation sqref is malformed"));
    }
    Ok(ranges.into_iter().flatten().collect())
}

fn validate_data_validation_formula_children(tree: &XmlTree, node: NodeId) -> Result<()> {
    for name in [b"formula1".as_slice(), b"formula2"] {
        if direct_elements_by_local_name(tree, node, name).len() > 1 {
            return Err(Error::Zip("data-validation formula children are ambiguous"));
        }
    }
    Ok(())
}

fn data_validation_target(tree: &XmlTree, target: (u32, u16, u32, u16)) -> Result<Option<NodeId>> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let wrappers = data_validation_wrappers(tree, root);
    if wrappers.len() > 1 {
        return Err(Error::Zip(
            "multiple dataValidations elements are unsupported",
        ));
    }
    let Some(wrapper) = wrappers.first().copied() else {
        return Ok(None);
    };
    let mut exact = Vec::new();
    for node in data_validation_nodes(tree, wrapper) {
        let ranges = data_validation_ranges(tree, node)?;
        if ranges.iter().any(|&range| ranges_overlap(range, target)) {
            if ranges.len() == 1 && ranges[0] == target {
                validate_data_validation_formula_children(tree, node)?;
                exact.push(node);
            } else {
                return Err(Error::Zip(
                    "overlapping or multi-range data validation is unsupported",
                ));
            }
        }
    }
    if exact.len() > 1 {
        return Err(Error::Zip(
            "duplicate data validations at one range are unsupported",
        ));
    }
    Ok(exact.first().copied())
}

fn dv_kind_name(kind: DvKind) -> &'static str {
    match kind {
        DvKind::List => "list",
        DvKind::Whole => "whole",
        DvKind::Decimal => "decimal",
        DvKind::Date => "date",
        DvKind::Time => "time",
        DvKind::TextLength => "textLength",
        DvKind::Custom => "custom",
    }
}

fn dv_op_name(operator: DvOp) -> &'static str {
    match operator {
        DvOp::Between => "between",
        DvOp::NotBetween => "notBetween",
        DvOp::Equal => "equal",
        DvOp::NotEqual => "notEqual",
        DvOp::GreaterThan => "greaterThan",
        DvOp::LessThan => "lessThan",
        DvOp::GreaterThanOrEqual => "greaterThanOrEqual",
        DvOp::LessThanOrEqual => "lessThanOrEqual",
    }
}

fn set_optional_attr(
    tree: &mut XmlTree,
    node: NodeId,
    name: &[u8],
    value: Option<&str>,
) -> Result<()> {
    if let Some(value) = value {
        tree.set_attr(node, name, value.as_bytes())?;
    } else {
        tree.remove_attr(node, name);
    }
    Ok(())
}

fn data_validation_formula_node(tree: &XmlTree, validation: NodeId, name: &[u8]) -> Option<NodeId> {
    direct_elements_by_local_name(tree, validation, name)
        .first()
        .copied()
}

fn sml_set_data_validation_formula(
    tree: &mut XmlTree,
    validation: NodeId,
    name: &'static str,
    value: Option<&str>,
) -> Result<Option<NodeId>> {
    let existing = data_validation_formula_node(tree, validation, name.as_bytes());
    match (existing, value) {
        (Some(node), Some(value)) => {
            tree.set_element_text(node, value)?;
            Ok(Some(node))
        }
        (Some(node), None) => {
            tree.remove_child(validation, node)?;
            Ok(None)
        }
        (None, Some(value)) => {
            let fragment = format!("<{name}>{}</{name}>", esc_text(value));
            let children = tree.children_of(validation);
            let index = if name == "formula1" {
                children
                    .iter()
                    .position(|&child| {
                        tree.element_name(child).is_some_and(|element| {
                            matches!(local(element), b"formula2" | b"extLst")
                        })
                    })
                    .unwrap_or(children.len())
            } else if let Some(formula1) =
                data_validation_formula_node(tree, validation, b"formula1")
            {
                children
                    .iter()
                    .position(|&child| child == formula1)
                    .map(|index| index + 1)
                    .unwrap_or(children.len())
            } else {
                children.len()
            };
            let node = tree.insert_fragment_at(validation, index, fragment.as_bytes())?;
            Ok(Some(node))
        }
        (None, None) => Ok(None),
    }
}

fn repair_data_validation_count(tree: &mut XmlTree, wrapper: NodeId) -> Result<usize> {
    let count = data_validation_nodes(tree, wrapper).len();
    tree.set_attr(wrapper, b"count", count.to_string().as_bytes())?;
    Ok(count)
}

fn sml_set_data_validation(
    tree: &mut XmlTree,
    validation: &DataValidation,
    existing: Option<NodeId>,
) -> Result<()> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let wrappers = data_validation_wrappers(tree, root);
    if wrappers.len() > 1 {
        return Err(Error::Zip(
            "multiple dataValidations elements are unsupported",
        ));
    }
    let wrapper = match wrappers.first().copied() {
        Some(wrapper) => wrapper,
        None => insert_worksheet_fragment(tree, root, 17, b"<dataValidations count=\"0\"/>")?,
    };
    let node = if let Some(node) = existing {
        node
    } else {
        let fragment = crate::write::editable_data_validation_xml(validation);
        let index = tree.children_of(wrapper).len();
        tree.insert_fragment_at(wrapper, index, fragment.as_bytes())?
    };
    if existing.is_some() {
        tree.set_attr(node, b"type", dv_kind_name(validation.kind).as_bytes())?;
        if matches!(validation.kind, DvKind::List | DvKind::Custom) {
            tree.remove_attr(node, b"operator");
        } else {
            tree.set_attr(
                node,
                b"operator",
                dv_op_name(validation.operator).as_bytes(),
            )?;
        }
        tree.set_attr(
            node,
            b"allowBlank",
            if validation.allow_blank { b"1" } else { b"0" },
        )?;
        tree.set_attr(
            node,
            b"showInputMessage",
            if validation.show_input_message {
                b"1"
            } else {
                b"0"
            },
        )?;
        tree.set_attr(
            node,
            b"showErrorMessage",
            if validation.show_error_message {
                b"1"
            } else {
                b"0"
            },
        )?;
        tree.set_attr(node, b"sqref", range_ref(validation.sqref).as_bytes())?;
        set_optional_attr(
            tree,
            node,
            b"promptTitle",
            validation.prompt.as_ref().map(|(title, _)| title.as_str()),
        )?;
        set_optional_attr(
            tree,
            node,
            b"prompt",
            validation
                .prompt
                .as_ref()
                .map(|(_, message)| message.as_str()),
        )?;
        set_optional_attr(
            tree,
            node,
            b"errorTitle",
            validation.error.as_ref().map(|(title, _)| title.as_str()),
        )?;
        set_optional_attr(
            tree,
            node,
            b"error",
            validation
                .error
                .as_ref()
                .map(|(_, message)| message.as_str()),
        )?;
        sml_set_data_validation_formula(tree, node, "formula1", Some(&validation.formula1))?;
        sml_set_data_validation_formula(tree, node, "formula2", validation.formula2.as_deref())?;
    }
    repair_data_validation_count(tree, wrapper)?;
    Ok(())
}

fn sml_delete_data_validation(tree: &mut XmlTree, validation: NodeId) -> Result<()> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let wrappers = data_validation_wrappers(tree, root);
    if wrappers.len() != 1 {
        return Err(Error::Zip(
            "dataValidations element is missing or ambiguous",
        ));
    }
    let wrapper = wrappers[0];
    tree.remove_child(wrapper, validation)?;
    let count = data_validation_nodes(tree, wrapper).len();
    let has_unknown_elements = tree
        .children_of(wrapper)
        .iter()
        .any(|&node| tree.element_name(node).is_some());
    if count == 0 && !has_unknown_elements {
        tree.remove_child(root, wrapper)?;
    } else {
        repair_data_validation_count(tree, wrapper)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TablePartPlan {
    name: String,
    range: (u32, u16, u32, u16),
    root: NodeId,
    auto_filter: Option<NodeId>,
    filter_tail_rows: u32,
    column_count: usize,
    has_sort_state: bool,
}

fn worksheet_table_part_rids(tree: &XmlTree) -> Result<Vec<String>> {
    let root = tree
        .root_element()
        .ok_or(Error::Zip("worksheet XML is malformed"))?;
    let wrappers = direct_elements_by_local_name(tree, root, b"tableParts");
    if wrappers.len() > 1 {
        return Err(Error::Zip("multiple tableParts elements are unsupported"));
    }
    let Some(wrapper) = wrappers.first().copied() else {
        return Ok(Vec::new());
    };
    let parts = direct_elements_by_local_name(tree, wrapper, b"tablePart");
    let declared_count = tree
        .attr_value(wrapper, b"count")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or(Error::Zip("tableParts count is malformed"))?;
    if declared_count != parts.len() {
        return Err(Error::Zip("tableParts count does not match its entries"));
    }
    let rids: Vec<String> = parts
        .iter()
        .map(|&part| {
            tree.attr_value(part, b"r:id")
                .and_then(|value| std::str::from_utf8(value).ok())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or(Error::Zip("tablePart relationship id is malformed"))
        })
        .collect::<Result<_>>()?;
    let unique: BTreeSet<_> = rids.iter().collect();
    if unique.len() != rids.len() {
        return Err(Error::Zip("tablePart relationship ids are ambiguous"));
    }
    Ok(rids)
}

fn worksheet_table_parts(package: &Package, worksheet_path: &str) -> Result<Vec<String>> {
    let rids = peek_part_tree(
        package,
        worksheet_path,
        Error::Zip("worksheet XML is missing"),
        worksheet_table_part_rids,
    )?;
    let relationships = package.relationships_of(worksheet_path);
    let mut paths = Vec::new();
    for rid in rids {
        let matches: Vec<_> = relationships
            .iter()
            .filter(|relationship| relationship.id == rid)
            .collect();
        if matches.len() != 1
            || matches[0].external
            || matches[0].rel_type.rsplit('/').next() != Some("table")
        {
            return Err(Error::Zip("table relationship is missing or ambiguous"));
        }
        let path = Package::resolve_rel_target(worksheet_path, &matches[0].target);
        if !package.has_part(&path) {
            return Err(Error::Zip("table relationship target is missing"));
        }
        paths.push(canonical_part_name(&path));
    }
    let unique: BTreeSet<_> = paths.iter().map(|path| canonical_part_key(path)).collect();
    if unique.len() != paths.len() {
        return Err(Error::Zip("table relationship targets are ambiguous"));
    }
    Ok(paths)
}

fn inspect_table_part(tree: &XmlTree) -> Result<TablePartPlan> {
    let root = tree
        .root_element()
        .filter(|&node| {
            tree.element_name(node)
                .is_some_and(|name| local(name) == b"table")
        })
        .ok_or(Error::Zip("table XML is malformed"))?;
    let name = tree
        .attr_value(root, b"name")
        .and_then(|value| std::str::from_utf8(value).ok())
        .filter(|value| !value.is_empty());
    let display_name = tree
        .attr_value(root, b"displayName")
        .and_then(|value| std::str::from_utf8(value).ok())
        .filter(|value| !value.is_empty());
    if name
        .zip(display_name)
        .is_some_and(|(name, display)| !name.eq_ignore_ascii_case(display))
    {
        return Err(Error::Zip("table name metadata is ambiguous"));
    }
    let name = display_name
        .or(name)
        .ok_or(Error::Zip("table name is missing"))?
        .to_string();
    let range = tree
        .attr_value(root, b"ref")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(parse_a1_range)
        .ok_or(Error::Zip("table range is malformed"))?;
    if tree
        .attr_value(root, b"headerRowCount")
        .and_then(|value| std::str::from_utf8(value).ok())
        .is_some_and(|value| value != "1")
    {
        return Err(Error::Zip("headerless tables cannot be resized safely"));
    }
    let column_wrappers = direct_elements_by_local_name(tree, root, b"tableColumns");
    if column_wrappers.len() != 1 {
        return Err(Error::Zip("tableColumns element is missing or ambiguous"));
    }
    let columns = direct_elements_by_local_name(tree, column_wrappers[0], b"tableColumn");
    let declared_count = tree
        .attr_value(column_wrappers[0], b"count")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or(Error::Zip("tableColumns count is malformed"))?;
    let width = u32::from(range.3 - range.1) + 1;
    if columns.is_empty() || declared_count != columns.len() || width != columns.len() as u32 {
        return Err(Error::Zip(
            "table range width does not match its header-column count",
        ));
    }
    if columns.iter().any(|&column| {
        tree.attr_value(column, b"name")
            .and_then(|value| std::str::from_utf8(value).ok())
            .is_none_or(str::is_empty)
    }) {
        return Err(Error::Zip("table header-column metadata is malformed"));
    }

    let auto_filters = direct_elements_by_local_name(tree, root, b"autoFilter");
    if auto_filters.len() > 1 {
        return Err(Error::Zip("table autoFilter is ambiguous"));
    }
    let (auto_filter, filter_tail_rows) = if let Some(auto_filter) = auto_filters.first().copied() {
        let filter_range = tree
            .attr_value(auto_filter, b"ref")
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(parse_a1_range)
            .ok_or(Error::Zip("table autoFilter range is malformed"))?;
        if (filter_range.0, filter_range.1, filter_range.3) != (range.0, range.1, range.3)
            || filter_range.2 > range.2
            || range.2 - filter_range.2 > 1
        {
            return Err(Error::Zip(
                "table autoFilter range is inconsistent with the table",
            ));
        }
        (Some(auto_filter), range.2 - filter_range.2)
    } else {
        (None, 0)
    };
    let mut has_sort_state = false;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        has_sort_state |= tree
            .element_name(node)
            .is_some_and(|name| local(name) == b"sortState");
        stack.extend(tree.children_of(node).iter().copied());
    }
    Ok(TablePartPlan {
        name,
        range,
        root,
        auto_filter,
        filter_tail_rows,
        column_count: columns.len(),
        has_sort_state,
    })
}

fn sml_set_table_range(
    tree: &mut XmlTree,
    plan: &TablePartPlan,
    range: (u32, u16, u32, u16),
) -> Result<()> {
    let table_ref = range_ref(range);
    tree.set_attr(plan.root, b"ref", table_ref.as_bytes())?;
    let filter_range = (range.0, range.1, range.2 - plan.filter_tail_rows, range.3);
    let filter_ref = range_ref(filter_range);
    if let Some(auto_filter) = plan.auto_filter {
        tree.set_attr(auto_filter, b"ref", filter_ref.as_bytes())?;
    } else {
        let children = tree.children_of(plan.root);
        let index = children
            .iter()
            .position(|&node| {
                tree.element_name(node).is_some_and(|name| {
                    matches!(
                        local(name),
                        b"sortState" | b"tableColumns" | b"tableStyleInfo" | b"extLst"
                    )
                })
            })
            .unwrap_or(children.len());
        let fragment = format!(r#"<autoFilter ref="{}"/>"#, esc_attr(&filter_ref));
        tree.insert_fragment_at(plan.root, index, fragment.as_bytes())?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SheetReferenceRewrite {
    Text(NodeId, String),
    Attribute(NodeId, &'static [u8], Vec<u8>),
}

fn formula_bearing_parts(package: &Package, workbook_path: &str) -> Vec<String> {
    let mut parts = BTreeSet::from([workbook_path.to_string()]);

    // Known OOXML formula-bearing locations are included even when a producer
    // omits relationship Type metadata or stores a part under an unusual rid.
    for name in package.part_names() {
        let canonical = canonical_part_name(name);
        let lower = canonical.to_ascii_lowercase();
        let known = lower.ends_with(".xml")
            && [
                "/worksheets/",
                "/chartsheets/",
                "/dialogsheets/",
                "/macrosheets/",
                "/charts/",
                "/tables/",
                "/pivotcache/",
                "/pivottables/",
            ]
            .iter()
            .any(|segment| lower.contains(segment));
        if known && package.has_part(&canonical) {
            parts.insert(canonical);
        }
    }

    // Follow typed relationships as well so non-canonical but valid part paths
    // are covered. Drawing parts are traversed to reach their chart children;
    // only trees with an actual matching reference are promoted later.
    let mut queue = VecDeque::from([workbook_path.to_string()]);
    let mut visited = BTreeSet::new();
    while let Some(source) = queue.pop_front() {
        if !visited.insert(source.clone()) {
            continue;
        }
        for rel in package.relationships_of(&source) {
            if rel.external || !formula_relationship_type(&rel.rel_type) {
                continue;
            }
            let target = Package::resolve_rel_target(&source, &rel.target);
            if !package.has_part(&target) {
                continue;
            }
            parts.insert(target.clone());
            queue.push_back(target);
        }
    }

    parts.into_iter().collect()
}

fn formula_relationship_type(rel_type: &str) -> bool {
    let kind = rel_type.rsplit('/').next().unwrap_or(rel_type);
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "worksheet"
            | "chartsheet"
            | "dialogsheet"
            | "macrosheet"
            | "xlmacrosheet"
            | "xlintlmacrosheet"
            | "drawing"
            | "chart"
            | "table"
            | "pivottable"
            | "pivotcachedefinition"
    )
}

fn canonical_part_key(name: &str) -> String {
    canonical_part_name(name).to_ascii_lowercase()
}

fn sheet_owned_relationship_kind(kind: &str) -> bool {
    matches!(
        kind,
        "drawing" | "comments" | "vmldrawing" | "table" | "printersettings"
    )
}

fn nested_owned_relationship_kind(kind: &str) -> bool {
    matches!(
        kind,
        "chart"
            | "image"
            | "diagramdata"
            | "diagramlayout"
            | "diagramcolors"
            | "diagramquickstyle"
            | "chartstyle"
            | "chartcolorstyle"
    )
}

fn unsafe_sheet_dependency_kind(kind: &str) -> bool {
    matches!(
        kind,
        "pivottable"
            | "pivotcachedefinition"
            | "querytable"
            | "oleobject"
            | "control"
            | "ctrlprop"
            | "threadedcomment"
            | "threadedcomments"
            | "slicer"
            | "slicercache"
            | "timeline"
            | "timelinecache"
            | "connections"
            | "externallink"
            | "hyperlink"
    )
}

/// Find standard package parts exclusively owned by a worksheet. Unknown
/// relationship types are deliberately left alone: dropping a known
/// worksheet must not guess that an extension/custom target is disposable.
/// Known complex structures whose workbook-level repair is not implemented
/// are rejected before mutation.
fn plan_sheet_owned_parts(package: &Package, worksheet_path: &str) -> Result<Vec<String>> {
    let relationships = package.relationship_entries();
    let worksheet_key = canonical_part_key(worksheet_path);
    let mut candidates = BTreeMap::<String, String>::new();
    let mut queue = VecDeque::from([canonical_part_name(worksheet_path)]);
    let mut visited = BTreeSet::new();

    while let Some(source) = queue.pop_front() {
        let source_key = canonical_part_key(&source);
        if !visited.insert(source_key.clone()) {
            continue;
        }
        let source_relationships: Vec<_> = relationships
            .iter()
            .filter(|(candidate, _)| canonical_part_key(candidate) == source_key)
            .collect();
        let unique_ids: BTreeSet<_> = source_relationships
            .iter()
            .map(|(_, relationship)| relationship.id.as_str())
            .collect();
        if unique_ids.len() != source_relationships.len() {
            return Err(Error::Zip("sheet dependency relationships are ambiguous"));
        }

        for (relationship_source, relationship) in source_relationships {
            if relationship.external {
                continue;
            }
            let target = Package::resolve_rel_target(relationship_source, &relationship.target);
            if !package.has_part(&target) {
                return Err(Error::Zip(
                    "sheet dependency relationship target is missing",
                ));
            }
            let kind = relationship
                .rel_type
                .rsplit('/')
                .next()
                .unwrap_or(&relationship.rel_type)
                .to_ascii_lowercase();
            if unsafe_sheet_dependency_kind(&kind) {
                return Err(Error::Zip(
                    "worksheet has a structural dependency that cannot be repaired safely",
                ));
            }
            let owned = if source_key == worksheet_key {
                sheet_owned_relationship_kind(&kind)
            } else {
                nested_owned_relationship_kind(&kind)
            };
            if !owned {
                continue;
            }
            let target = canonical_part_name(&target);
            let target_key = canonical_part_key(&target);
            if target_key == worksheet_key {
                return Err(Error::Zip("worksheet dependency graph is cyclic"));
            }
            if candidates.insert(target_key, target.clone()).is_none() {
                queue.push_back(target);
            }
        }
    }

    // An otherwise-owned chart/image/etc. can be shared by a surviving part.
    // Repeatedly prune anything with an incoming edge from outside the removal
    // set; pruning its children on the next iteration preserves the full
    // shared branch without relying on relationship traversal order.
    let mut removable: BTreeSet<String> = candidates.keys().cloned().collect();
    loop {
        let blocked: Vec<_> = removable
            .iter()
            .filter(|target_key| {
                relationships.iter().any(|(source, relationship)| {
                    if relationship.external {
                        return false;
                    }
                    let relationship_target =
                        Package::resolve_rel_target(source, &relationship.target);
                    canonical_part_key(&relationship_target) == target_key.as_str()
                        && canonical_part_key(source) != worksheet_key
                        && !removable.contains(&canonical_part_key(source))
                })
            })
            .cloned()
            .collect();
        if blocked.is_empty() {
            break;
        }
        for key in blocked {
            removable.remove(&key);
        }
    }

    let mut parts: Vec<_> = removable
        .into_iter()
        .filter_map(|key| candidates.remove(&key))
        .collect();
    parts.sort();
    Ok(parts)
}

fn collect_sheet_reference_rewrites(
    tree: &XmlTree,
    old_name: &str,
    new_name: &str,
) -> Vec<SheetReferenceRewrite> {
    let Some(root) = tree.root_element() else {
        return Vec::new();
    };
    let mut rewrites = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let Some(name) = tree.element_name(node) else {
            continue;
        };
        let tag = local(name);
        if matches!(
            tag,
            b"f" | b"formula"
                | b"formula1"
                | b"formula2"
                | b"definedName"
                | b"calculatedColumnFormula"
                | b"totalsRowFormula"
        ) {
            let text = tree.text_of(node);
            let rewritten = rewrite_sheet_qualifiers(&text, old_name, new_name);
            if rewritten != text {
                rewrites.push(SheetReferenceRewrite::Text(node, rewritten));
            }
        }
        if tag == b"hyperlink" {
            if let Some(location) = tree
                .attr_value(node, b"location")
                .and_then(|value| std::str::from_utf8(value).ok())
            {
                let rewritten = rewrite_sheet_qualifiers(location, old_name, new_name);
                if rewritten != location {
                    rewrites.push(SheetReferenceRewrite::Attribute(
                        node,
                        b"location",
                        rewritten.into_bytes(),
                    ));
                }
            }
        }
        if tag == b"worksheetSource" {
            if let Some(sheet) = tree.attr_value(node, b"sheet") {
                if std::str::from_utf8(sheet)
                    .ok()
                    .is_some_and(|sheet| formula_sheet_name_eq(sheet, old_name))
                {
                    rewrites.push(SheetReferenceRewrite::Attribute(
                        node,
                        b"sheet",
                        new_name.as_bytes().to_vec(),
                    ));
                }
            }
        }
        stack.extend(tree.children_of(node).iter().rev().copied());
    }
    rewrites
}

fn collect_deleted_sheet_reference_rewrites(
    tree: &XmlTree,
    deleted_name: &str,
) -> Result<Vec<SheetReferenceRewrite>> {
    let Some(root) = tree.root_element() else {
        return Err(Error::Zip("formula-bearing OOXML part is malformed"));
    };
    let mut rewrites = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let Some(name) = tree.element_name(node) else {
            continue;
        };
        let tag = local(name);
        if matches!(
            tag,
            b"f" | b"formula"
                | b"formula1"
                | b"formula2"
                | b"definedName"
                | b"calculatedColumnFormula"
                | b"totalsRowFormula"
        ) {
            let text = tree.text_of(node);
            let rewritten = rewrite_deleted_sheet_qualifiers(&text, deleted_name);
            if rewritten != text {
                rewrites.push(SheetReferenceRewrite::Text(node, rewritten));
            }
        }
        if tag == b"hyperlink" {
            if let Some(location) = tree
                .attr_value(node, b"location")
                .and_then(|value| std::str::from_utf8(value).ok())
            {
                let rewritten = rewrite_deleted_sheet_qualifiers(location, deleted_name);
                if rewritten != location {
                    rewrites.push(SheetReferenceRewrite::Attribute(
                        node,
                        b"location",
                        rewritten.into_bytes(),
                    ));
                }
            }
        }
        if tag == b"worksheetSource"
            && tree
                .attr_value(node, b"sheet")
                .and_then(|value| std::str::from_utf8(value).ok())
                .is_some_and(|sheet| formula_sheet_name_eq(sheet, deleted_name))
        {
            return Err(Error::Zip(
                "pivot cache source on the deleted worksheet cannot be repaired safely",
            ));
        }
        stack.extend(tree.children_of(node).iter().rev().copied());
    }
    Ok(rewrites)
}

fn apply_sheet_reference_rewrites(
    tree: &mut XmlTree,
    rewrites: &[SheetReferenceRewrite],
) -> Result<()> {
    for rewrite in rewrites {
        match rewrite {
            SheetReferenceRewrite::Text(node, text) => tree.set_element_text(*node, text)?,
            SheetReferenceRewrite::Attribute(node, name, value) => {
                tree.set_attr(*node, name, value)?;
            }
        }
    }
    Ok(())
}

fn rewrite_sheet_qualifiers(formula: &str, old_name: &str, new_name: &str) -> String {
    rewrite_sheet_qualifiers_impl(formula, old_name, Some(new_name))
}

fn rewrite_deleted_sheet_qualifiers(formula: &str, deleted_name: &str) -> String {
    rewrite_sheet_qualifiers_impl(formula, deleted_name, None)
}

fn rewrite_sheet_qualifiers_impl(formula: &str, old_name: &str, new_name: Option<&str>) -> String {
    let bytes = formula.as_bytes();
    let mut out = String::with_capacity(
        formula
            .len()
            .saturating_add(new_name.map(str::len).unwrap_or(5)),
    );
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if bytes.get(i + 1) == Some(&b'"') {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += formula[i..].chars().next().map(char::len_utf8).unwrap_or(1);
                }
            }
            out.push_str(&formula[start..i]);
            continue;
        }

        if bytes[i] == b'\'' {
            if let Some((end, qualifier)) = quoted_sheet_qualifier(formula, i) {
                if let Some(rewritten) = rewrite_sheet_span(&qualifier, old_name, new_name) {
                    push_sheet_span_rewrite(&mut out, &rewritten, true);
                } else {
                    out.push_str(&formula[i..end]);
                }
                i = end;
                continue;
            }
        }

        let ch = formula[i..].chars().next().expect("i is in bounds");
        let previous = formula[..i].chars().next_back();
        if formula_sheet_token_char(ch)
            && !previous.is_some_and(|ch| formula_sheet_token_char(ch) || ch == ']')
        {
            let mut end = i;
            for (offset, ch) in formula[i..].char_indices() {
                if !formula_sheet_token_char(ch) {
                    break;
                }
                end = i + offset + ch.len_utf8();
            }
            if bytes.get(end) == Some(&b'!') {
                let qualifier = &formula[i..end];
                if let Some(rewritten) = rewrite_sheet_span(qualifier, old_name, new_name) {
                    push_sheet_span_rewrite(&mut out, &rewritten, false);
                    i = end + 1;
                    continue;
                }
            }
        }

        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SheetSpanRewrite {
    Name(String),
    RefError,
}

fn push_sheet_span_rewrite(out: &mut String, rewrite: &SheetSpanRewrite, preserve_quotes: bool) {
    match rewrite {
        SheetSpanRewrite::RefError => out.push_str("#REF!"),
        SheetSpanRewrite::Name(name) => {
            if !preserve_quotes && formula_sheet_span_can_be_unquoted(name) {
                out.push_str(name);
            } else {
                out.push('\'');
                out.push_str(&name.replace('\'', "''"));
                out.push('\'');
            }
            out.push('!');
        }
    }
}

fn quoted_sheet_qualifier(formula: &str, start: usize) -> Option<(usize, String)> {
    let bytes = formula.as_bytes();
    let mut qualifier = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                qualifier.push('\'');
                i += 2;
                continue;
            }
            if bytes.get(i + 1) == Some(&b'!') {
                return Some((i + 2, qualifier));
            }
            return None;
        }
        let ch = formula[i..].chars().next()?;
        qualifier.push(ch);
        i += ch.len_utf8();
    }
    None
}

fn rewrite_sheet_span(
    span: &str,
    old_name: &str,
    new_name: Option<&str>,
) -> Option<SheetSpanRewrite> {
    if span.contains('[') || span.contains(']') {
        return None;
    }
    let mut names: Vec<&str> = span.split(':').collect();
    if names.is_empty() || names.len() > 2 || names.iter().any(|name| name.is_empty()) {
        return None;
    }
    if !names
        .iter()
        .any(|name| formula_sheet_name_eq(name, old_name))
    {
        return None;
    }
    let Some(new_name) = new_name else {
        return Some(SheetSpanRewrite::RefError);
    };
    let mut changed = false;
    for name in &mut names {
        if formula_sheet_name_eq(name, old_name) {
            *name = new_name;
            changed = true;
        }
    }
    changed.then(|| SheetSpanRewrite::Name(names.join(":")))
}

fn formula_sheet_name_eq(left: &str, right: &str) -> bool {
    left.to_lowercase() == right.to_lowercase()
}

fn formula_sheet_token_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '.' | ':')
}

fn formula_sheet_span_can_be_unquoted(span: &str) -> bool {
    span.split(':').all(|name| {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|ch| ch.is_alphabetic() || ch == '_')
            && chars.all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '.'))
    })
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

fn workbook_has_other_sheet_named(tree: &XmlTree, target: NodeId, name: &str) -> bool {
    let Some(workbook) = tree.root_element() else {
        return false;
    };
    let Some(sheets) = tree.child_by_name(workbook, b"sheets") else {
        return false;
    };
    tree.children_of(sheets).iter().copied().any(|sheet| {
        sheet != target
            && tree
                .attr_value(sheet, b"name")
                .and_then(|value| std::str::from_utf8(value).ok())
                .is_some_and(|existing| formula_sheet_name_eq(existing, name))
    })
}

fn workbook_has_sheet_named(tree: &XmlTree, name: &str) -> bool {
    let Some(workbook) = tree.root_element() else {
        return false;
    };
    let Some(sheets) = tree.child_by_name(workbook, b"sheets") else {
        return false;
    };
    tree.children_of(sheets).iter().copied().any(|sheet| {
        tree.element_name(sheet) == Some(b"sheet")
            && tree
                .attr_value(sheet, b"name")
                .and_then(|value| std::str::from_utf8(value).ok())
                .is_some_and(|existing| formula_sheet_name_eq(existing, name))
    })
}

fn workbook_sheet_count(tree: &XmlTree) -> usize {
    let Some(workbook) = tree.root_element() else {
        return 0;
    };
    tree.child_by_name(workbook, b"sheets")
        .map(|sheets| {
            tree.children_of(sheets)
                .iter()
                .filter(|&&node| tree.element_name(node) == Some(b"sheet"))
                .count()
        })
        .unwrap_or(0)
}

fn next_sheet_id(tree: &XmlTree) -> Result<u32> {
    let workbook = tree.root_element().ok_or(Error::MissingWorkbook)?;
    let sheets = tree
        .child_by_name(workbook, b"sheets")
        .ok_or(Error::MissingWorkbook)?;
    let max_id = tree
        .children_of(sheets)
        .iter()
        .filter(|&&node| tree.element_name(node) == Some(b"sheet"))
        .filter_map(|&node| tree.attr_value(node, b"sheetId"))
        .filter_map(|value| std::str::from_utf8(value).ok())
        .filter_map(|value| value.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    max_id
        .checked_add(1)
        .ok_or(Error::Zip("worksheet sheetId space is exhausted"))
}

fn next_worksheet_part_name(package: &Package) -> Result<String> {
    let workbook_path = workbook_path(package);
    let workbook_dir = workbook_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    let prefix = if workbook_dir.is_empty() {
        "worksheets/sheet".to_string()
    } else {
        format!("{workbook_dir}/worksheets/sheet")
    };
    let used: BTreeSet<String> = package
        .part_names()
        .map(canonical_part_name)
        .map(|name| name.to_ascii_lowercase())
        .collect();
    for ordinal in 1..=u32::MAX {
        let candidate = format!("{prefix}{ordinal}.xml");
        if !used.contains(&candidate.to_ascii_lowercase()) {
            return Ok(candidate);
        }
    }
    Err(Error::Zip("worksheet part-name space is exhausted"))
}

fn empty_worksheet_xml() -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="{NS_MAIN}"><sheetData/></worksheet>"#
    )
    .into_bytes()
}

fn sml_append_sheet(tree: &mut XmlTree, name: &str, sheet_id: u32, rid: &str) -> Result<()> {
    let workbook = tree.root_element().ok_or(Error::MissingWorkbook)?;
    let sheets = tree
        .child_by_name(workbook, b"sheets")
        .ok_or(Error::MissingWorkbook)?;
    if tree.attr_value(workbook, b"xmlns:r").is_none() {
        tree.set_attr(workbook, b"xmlns:r", NS_R.as_bytes())?;
    }
    let fragment = format!(
        r#"<sheet name="{}" sheetId="{sheet_id}" r:id="{}"/>"#,
        esc_attr(name),
        esc_attr(rid)
    );
    let index = tree.children_of(sheets).len();
    tree.insert_fragment_at(sheets, index, fragment.as_bytes())?;
    Ok(())
}

fn workbook_active_tab(tree: &XmlTree) -> usize {
    let Some(workbook) = tree.root_element() else {
        return 0;
    };
    tree.child_by_name(workbook, b"bookViews")
        .and_then(|views| tree.child_by_name(views, b"workbookView"))
        .and_then(|view| tree.attr_value(view, b"activeTab"))
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SheetDeletePlan {
    sheet_index: usize,
    new_active_tab: usize,
    rid: String,
    sheet_rids: Vec<String>,
}

fn delete_sheet_plan(tree: &XmlTree, name: &str) -> Result<SheetDeletePlan> {
    let workbook = tree.root_element().ok_or(Error::MissingWorkbook)?;
    let sheets = tree
        .child_by_name(workbook, b"sheets")
        .ok_or(Error::MissingWorkbook)?;
    let sheet_nodes: Vec<NodeId> = tree
        .children_of(sheets)
        .iter()
        .copied()
        .filter(|&node| tree.element_name(node) == Some(b"sheet"))
        .collect();
    let matching_indices: Vec<_> = sheet_nodes
        .iter()
        .enumerate()
        .filter_map(|(index, &sheet)| {
            (tree
                .attr_value(sheet, b"name")
                .and_then(|value| std::str::from_utf8(value).ok())
                == Some(name))
            .then_some(index)
        })
        .collect();
    if matching_indices.len() != 1 {
        return Err(Error::MissingWorkbook);
    }
    let sheet_index = matching_indices[0];
    let sheet_rids: Vec<String> = sheet_nodes
        .iter()
        .map(|&sheet| {
            tree.attr_value(sheet, b"r:id")
                .and_then(|value| std::str::from_utf8(value).ok())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or(Error::Zip("workbook sheet relationship id is malformed"))
        })
        .collect::<Result<_>>()?;
    let unique_rids: BTreeSet<_> = sheet_rids.iter().collect();
    if unique_rids.len() != sheet_rids.len() {
        return Err(Error::Zip("workbook sheet relationship ids are ambiguous"));
    }
    if let Some(defined_names) = tree.child_by_name(workbook, b"definedNames") {
        for defined_name in tree
            .children_of(defined_names)
            .iter()
            .copied()
            .filter(|&node| tree.element_name(node) == Some(b"definedName"))
        {
            let Some(local_index) = tree.attr_value(defined_name, b"localSheetId") else {
                continue;
            };
            let valid = std::str::from_utf8(local_index)
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|index| index < sheet_nodes.len());
            if !valid {
                return Err(Error::Zip("defined-name sheet scope is malformed"));
            }
        }
    }
    let rid = sheet_rids
        .get(sheet_index)
        .cloned()
        .ok_or(Error::MissingWorkbook)?;
    if sheet_nodes.len() <= 1 {
        return Err(Error::Zip("cannot delete the last worksheet"));
    }
    if sheet_visibility_of(tree, sheet_nodes[sheet_index]) == SheetVisible::Visible
        && visible_sheet_count(tree) <= 1
    {
        return Err(Error::Zip("cannot delete the last visible sheet"));
    }

    let active = workbook_active_tab(tree).min(sheet_nodes.len() - 1);
    let new_active = match active.cmp(&sheet_index) {
        std::cmp::Ordering::Greater => active - 1,
        std::cmp::Ordering::Equal => sheet_index.min(sheet_nodes.len() - 2),
        std::cmp::Ordering::Less => active,
    };
    Ok(SheetDeletePlan {
        sheet_index,
        new_active_tab: new_active,
        rid,
        sheet_rids,
    })
}

fn sml_delete_sheet(
    tree: &mut XmlTree,
    name: &str,
    sheet_index: usize,
    new_active_tab: usize,
) -> Result<()> {
    let workbook = tree.root_element().ok_or(Error::MissingWorkbook)?;
    let sheets = tree
        .child_by_name(workbook, b"sheets")
        .ok_or(Error::MissingWorkbook)?;
    let sheet = sml_find_sheet_by_name(tree, name).ok_or(Error::MissingWorkbook)?;
    tree.remove_child(sheets, sheet)?;
    sml_repair_local_defined_names_after_delete(tree, workbook, sheet_index)?;
    sml_repair_workbook_view_after_delete(tree, sheet_index, new_active_tab)?;
    Ok(())
}

fn sml_repair_local_defined_names_after_delete(
    tree: &mut XmlTree,
    workbook: NodeId,
    deleted_index: usize,
) -> Result<()> {
    let Some(defined_names) = tree.child_by_name(workbook, b"definedNames") else {
        return Ok(());
    };
    let names: Vec<NodeId> = tree
        .children_of(defined_names)
        .iter()
        .copied()
        .filter(|&node| tree.element_name(node) == Some(b"definedName"))
        .collect();
    for name in names {
        let Some(local_index) = tree
            .attr_value(name, b"localSheetId")
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.parse::<usize>().ok())
        else {
            continue;
        };
        match local_index.cmp(&deleted_index) {
            std::cmp::Ordering::Equal => tree.remove_child(defined_names, name)?,
            std::cmp::Ordering::Greater => tree.set_attr(
                name,
                b"localSheetId",
                (local_index - 1).to_string().as_bytes(),
            )?,
            std::cmp::Ordering::Less => {}
        }
    }
    let has_names = tree
        .children_of(defined_names)
        .iter()
        .any(|&node| tree.element_name(node) == Some(b"definedName"));
    if !has_names {
        tree.remove_child(workbook, defined_names)?;
    }
    Ok(())
}

fn sml_repair_workbook_view_after_delete(
    tree: &mut XmlTree,
    deleted_index: usize,
    new_active_tab: usize,
) -> Result<()> {
    sml_set_active_tab(tree, new_active_tab)?;
    let Some(workbook) = tree.root_element() else {
        return Err(Error::MissingWorkbook);
    };
    let Some(view) = tree
        .child_by_name(workbook, b"bookViews")
        .and_then(|views| tree.child_by_name(views, b"workbookView"))
    else {
        return Ok(());
    };
    let Some(first_sheet) = tree
        .attr_value(view, b"firstSheet")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return Ok(());
    };
    let repaired = match first_sheet.cmp(&deleted_index) {
        std::cmp::Ordering::Greater => first_sheet - 1,
        std::cmp::Ordering::Equal => new_active_tab,
        std::cmp::Ordering::Less => first_sheet,
    };
    tree.set_attr(view, b"firstSheet", repaired.to_string().as_bytes())?;
    Ok(())
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

fn validate_global_defined_name_target(tree: &XmlTree, name: &str) -> Result<()> {
    let workbook = tree.root_element().ok_or(Error::MissingWorkbook)?;
    let Some(defined_names) = tree.child_by_name(workbook, b"definedNames") else {
        return Ok(());
    };
    let folded = name.to_lowercase();
    let mut case_insensitive_matches = 0usize;
    let mut exact_matches = 0usize;
    for node in tree
        .children_of(defined_names)
        .iter()
        .copied()
        .filter(|&node| {
            tree.element_name(node) == Some(b"definedName")
                && tree.attr_value(node, b"localSheetId").is_none()
        })
    {
        let existing = tree
            .attr_value(node, b"name")
            .and_then(|value| std::str::from_utf8(value).ok())
            .ok_or(Error::Zip("defined name is malformed"))?;
        if existing.to_lowercase() == folded {
            case_insensitive_matches += 1;
            exact_matches += usize::from(existing == name);
        }
    }
    if case_insensitive_matches > 1 || (case_insensitive_matches == 1 && exact_matches == 0) {
        return Err(Error::Zip("defined name collides case-insensitively"));
    }
    Ok(())
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
    use crate::xmltree::{
        reset_test_fail_commit, reset_test_node_budget, set_test_fail_commit_after,
        set_test_node_budget,
    };

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

    fn assert_rejected_edit_is_unchanged(spreadsheet: &Spreadsheet, before: &[u8]) {
        assert!(
            spreadsheet.edited_parts().is_empty(),
            "a rejected edit must not record an edited package part"
        );
        assert_eq!(
            spreadsheet.save().expect("serialize rejected edit"),
            before,
            "a rejected edit must preserve the exact package bytes"
        );
    }

    #[test]
    fn cell_input_validation_rejects_without_mutating_the_package() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, "original");
        let input = workbook.to_xlsx();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
        let before = spreadsheet.save().expect("serialize original package");
        let nested_formula = Cell::Formula {
            formula: "INNER()".to_string(),
            cached: Box::new(Cell::Number(1.0)),
        };
        let invalid_values = vec![
            Cell::Number(f64::NAN),
            Cell::Date(f64::INFINITY),
            Cell::Text("illegal\u{1}text".to_string()),
            Cell::Error("#BAD\u{1}!".to_string()),
            Cell::Text("😀".repeat(16_384)),
            Cell::Formula {
                formula: "SUM(\u{1})".to_string(),
                cached: Box::new(Cell::Number(1.0)),
            },
            Cell::Formula {
                formula: "TEXT()".to_string(),
                cached: Box::new(Cell::Text("illegal\u{1}cache".to_string())),
            },
            Cell::Formula {
                formula: "ERROR()".to_string(),
                cached: Box::new(Cell::Error("#BAD\u{1}!".to_string())),
            },
            Cell::Formula {
                formula: "LONG()".to_string(),
                cached: Box::new(Cell::Text("x".repeat(32_768))),
            },
            Cell::Formula {
                formula: "OUTER()".to_string(),
                cached: Box::new(nested_formula),
            },
        ];

        for value in invalid_values {
            assert!(spreadsheet.set_cell_value("Data", 0, 0, value).is_err());
            assert_rejected_edit_is_unchanged(&spreadsheet, &before);
        }

        assert!(spreadsheet
            .set_cell_formula("Data", 0, 0, "=SUM(\u{1})", Cell::Number(1.0))
            .is_err());
        assert_rejected_edit_is_unchanged(&spreadsheet, &before);

        assert!(spreadsheet
            .append_row(
                "Data",
                [
                    Cell::Text("would be partial".into()),
                    Cell::Number(f64::NAN)
                ],
            )
            .is_err());
        assert_rejected_edit_is_unchanged(&spreadsheet, &before);
    }

    #[test]
    fn defined_name_validation_rejects_invalid_or_colliding_names_without_mutation() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, 1.0);
        workbook.define_name("Rate", "Data!$A$1");
        let input = workbook.to_xlsx();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
        let before = spreadsheet.save().expect("serialize original package");

        for name in [
            "",
            "A1",
            "R1C1",
            "2024",
            "bad name",
            "bad-name",
            "_xlnm.Print_Area",
            "_XLNM.Print_Area",
        ] {
            assert!(spreadsheet.set_defined_name(name, "Data!$A$1").is_err());
            assert_rejected_edit_is_unchanged(&spreadsheet, &before);
        }

        assert!(spreadsheet.set_defined_name("rate", "Data!$A$2").is_err());
        assert_rejected_edit_is_unchanged(&spreadsheet, &before);

        assert!(spreadsheet
            .set_defined_name("ValidName", "Data!$A$1\u{1}")
            .is_err());
        assert_rejected_edit_is_unchanged(&spreadsheet, &before);
    }

    #[test]
    fn document_property_validation_rejects_without_removing_existing_timestamps() {
        let original_timestamp = "2024-01-01T00:00:00Z";
        let mut workbook = Workbook::new();
        workbook.set_properties(
            DocProperties::new()
                .with_title("Original title")
                .with_company("Original company")
                .with_created(original_timestamp),
        );
        workbook.add_sheet("Data").write(0, 0, "value");
        let input = workbook.to_xlsx();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
        let before = spreadsheet.save().expect("serialize original package");
        let invalid_xml = "illegal\u{1}property";
        let invalid_properties = vec![
            DocProperties::new().with_title(invalid_xml),
            DocProperties::new().with_subject(invalid_xml),
            DocProperties::new().with_creator(invalid_xml),
            DocProperties::new().with_keywords(invalid_xml),
            DocProperties::new().with_description(invalid_xml),
            DocProperties::new().with_last_modified_by(invalid_xml),
            DocProperties::new().with_company(invalid_xml),
            DocProperties::new().with_created(invalid_xml),
        ];

        for properties in invalid_properties {
            assert!(spreadsheet.set_document_properties(properties).is_err());
            assert_rejected_edit_is_unchanged(&spreadsheet, &before);
        }

        assert!(spreadsheet
            .set_document_properties(
                DocProperties::new()
                    .with_title("Candidate title")
                    .with_created("2024-02-31T00:00:00Z"),
            )
            .is_err());
        assert_rejected_edit_is_unchanged(&spreadsheet, &before);

        let reopened = Workbook::open(&before).expect("reopen original package");
        assert_eq!(
            reopened.properties.created.as_deref(),
            Some(original_timestamp),
            "an invalid timestamp edit must not remove the existing timestamp"
        );
    }

    #[test]
    fn transaction_rolls_back_every_edit_when_the_closure_fails() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, "original");
        let input = workbook.to_xlsx();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
        let before = spreadsheet.save().expect("serialize original package");
        let before_parts = spreadsheet.edited_parts().to_vec();

        let result: Result<()> = spreadsheet.transaction(|draft| {
            draft.set_cell_value("Data", 0, 0, Cell::Text("candidate".into()))?;
            draft.set_sheet_tab_color("Data", Color::rgb(0x12, 0x34, 0x56))?;
            Err(Error::Zip("abort test transaction"))
        });

        assert!(matches!(result, Err(Error::Zip("abort test transaction"))));
        assert_eq!(spreadsheet.edited_parts(), before_parts);
        assert_eq!(
            spreadsheet.save().expect("serialize rolled-back package"),
            before,
            "a failed transaction must preserve the exact pre-transaction package bytes"
        );

        let reopened = Workbook::open(&before).expect("reopen original package");
        let sheet = reopened.sheet_by_name("Data").expect("Data sheet");
        assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("original".into())));
        assert_eq!(sheet.tab_color(), None);
    }

    #[test]
    fn transaction_commits_a_successful_batch_and_returns_its_value() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, "original");
        let input = workbook.to_xlsx();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

        let value = spreadsheet
            .transaction(|draft| {
                draft.set_cell_value("Data", 0, 0, Cell::Text("committed".into()))?;
                draft.set_defined_name("Answer", "Data!$A$1")?;
                Ok(42_u8)
            })
            .expect("commit transaction");

        assert_eq!(value, 42);
        assert_eq!(
            spreadsheet.edited_parts(),
            &["xl/workbook.xml", "xl/worksheets/sheet1.xml"]
        );
        let saved = spreadsheet.save().expect("save committed package");
        let reopened = Workbook::open(&saved).expect("reopen committed package");
        assert_eq!(
            reopened.sheet_by_name("Data").and_then(|s| s.cell(0, 0)),
            Some(&Cell::Text("committed".into()))
        );
        assert_eq!(
            reopened.defined_names(),
            &[("Answer".to_string(), "Data!$A$1".to_string())]
        );
    }

    #[test]
    fn sheet_qualifier_rewriter_handles_quotes_3d_strings_and_external_books() {
        let formula =
            r#"Old!A1+'Old'!B2+'Old:Other'!C3+"Old!D4"+'[Book.xlsx]Old'!E5+[Book.xlsx]Old!F6"#;
        assert_eq!(
            rewrite_sheet_qualifiers(formula, "Old", "New Data"),
            r#"'New Data'!A1+'New Data'!B2+'New Data:Other'!C3+"Old!D4"+'[Book.xlsx]Old'!E5+[Book.xlsx]Old!F6"#
        );
        assert_eq!(
            rewrite_sheet_qualifiers("'O''Brien'!A1", "O'Brien", "Renamed"),
            "'Renamed'!A1"
        );
        assert_eq!(
            rewrite_deleted_sheet_qualifiers(formula, "Old"),
            r#"#REF!A1+#REF!B2+#REF!C3+"Old!D4"+'[Book.xlsx]Old'!E5+[Book.xlsx]Old!F6"#
        );

        let mut tree = XmlTree::parse(
            br#"<root><hyperlink location="'Old'!A1"/><worksheetSource sheet="Old"/></root>"#,
        )
        .expect("parse reference attributes");
        let rewrites = collect_sheet_reference_rewrites(&tree, "Old", "New Data");
        assert_eq!(rewrites.len(), 2);
        apply_sheet_reference_rewrites(&mut tree, &rewrites).expect("rewrite attributes");
        let xml = String::from_utf8(tree.serialize()).expect("serialized XML");
        assert!(xml.contains(r#"location="'New Data'!A1""#));
        assert!(xml.contains(r#"sheet="New Data""#));
    }

    fn zip_member(bytes: &[u8], name: &str) -> Vec<u8> {
        use std::io::Read;

        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("open zip");
        let mut part = zip.by_name(name).expect("zip member");
        let mut out = Vec::new();
        part.read_to_end(&mut out).expect("read zip member");
        out
    }

    fn zip_has_member(bytes: &[u8], name: &str) -> bool {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("open zip");
        let exists = zip.by_name(name).is_ok();
        exists
    }

    fn sheet_delete_dependency_fixture() -> Vec<u8> {
        use crate::Table;

        let mut workbook = Workbook::new();
        {
            let deleted = workbook.add_sheet("Delete");
            deleted.write(0, 0, "Value");
            deleted.write(1, 0, 2.0);
            deleted.add_comment(1, 0, "remove me", Some("author"));
            deleted.add_table(Table {
                range: (0, 0, 1, 0),
                name: "DeletedTable".into(),
                columns: vec!["Value".into()],
                style: None,
            });
        }
        workbook
            .add_sheet("Keep")
            .write_formula(0, 0, "Delete!A2+1", 3.0);
        workbook.define_name("DeletedGlobal", "Delete!$A$2");
        workbook.define_name("SafeGlobal", "Keep!$A$1");
        workbook.define_local_name("Delete", "DeletedLocal", "Delete!$A$2");
        workbook.define_local_name("Keep", "CrossLocal", "Delete!$A$2");
        workbook.define_local_name("Keep", "SafeLocal", "Keep!$A$1");

        let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open delete fixture");
        let package = seed.package.as_mut().expect("editable package");
        package
            .replace_part(
                "docProps/app.xml",
                br#"<?xml version="1.0" encoding="UTF-8"?><Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes" custom="preserve"><HeadingPairs keep="yes"><vt:vector size="4" baseType="variant"><vt:variant><vt:lpstr>Worksheets</vt:lpstr></vt:variant><vt:variant><vt:i4>2</vt:i4></vt:variant><vt:variant><vt:lpstr>Named Ranges</vt:lpstr></vt:variant><vt:variant><vt:i4>1</vt:i4></vt:variant></vt:vector></HeadingPairs><TitlesOfParts keep="yes"><vt:vector size="3" baseType="lpstr"><vt:lpstr>Delete</vt:lpstr><vt:lpstr>Keep</vt:lpstr><vt:lpstr>Unrelated title</vt:lpstr></vt:vector></TitlesOfParts><Extension keep="untouched"/></Properties>"#.to_vec(),
            )
            .expect("replace app metadata");
        package.set_part(
            "xl/custom/keep.bin",
            b"unknown worksheet extension payload".to_vec(),
            Some("application/octet-stream"),
        );
        package.add_relationship(
            "xl/worksheets/sheet1.xml",
            "http://example.com/relationships/customWorksheetExtension",
            "../custom/keep.bin",
            false,
        );
        seed.save().expect("save delete dependency fixture")
    }

    #[test]
    fn add_sheet_wires_a_deterministic_part_and_preserves_existing_parts() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, "original");
        let input = workbook.to_xlsx();
        let original_sheet = zip_member(&input, "xl/worksheets/sheet1.xml");
        let original_styles = zip_member(&input, "xl/styles.xml");
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

        spreadsheet.add_sheet("Added").expect("add sheet");

        assert_eq!(
            spreadsheet.edited_parts(),
            &[
                "[Content_Types].xml",
                "xl/_rels/workbook.xml.rels",
                "xl/workbook.xml",
                "xl/worksheets/sheet2.xml",
            ]
        );
        let saved = spreadsheet.save().expect("save added sheet");
        assert!(zip_has_member(&saved, "xl/worksheets/sheet2.xml"));
        assert_eq!(
            zip_member(&saved, "xl/worksheets/sheet1.xml"),
            original_sheet
        );
        assert_eq!(zip_member(&saved, "xl/styles.xml"), original_styles);
        let rels = String::from_utf8(zip_member(&saved, "xl/_rels/workbook.xml.rels"))
            .expect("UTF-8 rels");
        assert!(rels.contains(r#"Id="rId4""#));
        assert!(rels.contains(r#"Target="worksheets/sheet2.xml""#));

        let reopened = Workbook::open(&saved).expect("reopen added sheet");
        assert_eq!(reopened.sheet_names(), vec!["Data", "Added"]);
        assert_eq!(reopened.active_sheet_name(), Some("Data"));
    }

    #[test]
    fn delete_active_sheet_repairs_local_names_and_preserves_surviving_parts() {
        use crate::PageSetup;

        let mut workbook = Workbook::new();
        workbook.add_sheet("First").write(0, 0, "first");
        workbook.add_sheet("Middle").write(0, 0, "middle");
        workbook
            .add_sheet("Last")
            .set_page_setup(PageSetup::new().with_print_area((0, 0, 1, 1)));
        workbook.set_active_sheet(1);
        let input = workbook.to_xlsx();
        let original_first = zip_member(&input, "xl/worksheets/sheet1.xml");
        let original_last = zip_member(&input, "xl/worksheets/sheet3.xml");
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

        spreadsheet
            .delete_sheet("Middle")
            .expect("delete active sheet");

        assert_eq!(
            spreadsheet.edited_parts(),
            &[
                "[Content_Types].xml",
                "xl/_rels/workbook.xml.rels",
                "xl/workbook.xml",
                "xl/worksheets/sheet2.xml",
            ]
        );
        let saved = spreadsheet.save().expect("save deleted sheet");
        assert!(!zip_has_member(&saved, "xl/worksheets/sheet2.xml"));
        assert_eq!(
            zip_member(&saved, "xl/worksheets/sheet1.xml"),
            original_first
        );
        assert_eq!(
            zip_member(&saved, "xl/worksheets/sheet3.xml"),
            original_last
        );

        let reopened = Workbook::open(&saved).expect("reopen deleted sheet");
        assert_eq!(reopened.sheet_names(), vec!["First", "Last"]);
        assert_eq!(reopened.active_sheet_name(), Some("Last"));
        assert_eq!(
            reopened
                .sheet_by_name("Last")
                .and_then(|sheet| sheet.page_setup())
                .and_then(|setup| setup.print_area),
            Some((0, 0, 1, 1))
        );
    }

    #[test]
    fn delete_sheet_repairs_references_app_titles_and_owned_orphans() {
        let input = sheet_delete_dependency_fixture();
        let original_styles = zip_member(&input, "xl/styles.xml");
        let unknown = zip_member(&input, "xl/custom/keep.bin");
        let mut spreadsheet = Spreadsheet::open(&input).expect("open dependency fixture");

        spreadsheet
            .delete_sheet("Delete")
            .expect("delete sheet with repairable dependencies");
        let saved = spreadsheet.save().expect("save repaired deletion");

        for removed in [
            "xl/worksheets/sheet1.xml",
            "xl/worksheets/_rels/sheet1.xml.rels",
            "xl/comments1.xml",
            "xl/drawings/vmlDrawing1.vml",
            "xl/tables/table1.xml",
        ] {
            assert!(
                !zip_has_member(&saved, removed),
                "orphan survived: {removed}"
            );
        }
        assert_eq!(zip_member(&saved, "xl/custom/keep.bin"), unknown);
        assert_eq!(zip_member(&saved, "xl/styles.xml"), original_styles);

        let workbook_xml =
            String::from_utf8(zip_member(&saved, "xl/workbook.xml")).expect("workbook UTF-8");
        assert!(workbook_xml.contains("#REF!$A$2"));
        assert!(!workbook_xml.contains("DeletedLocal"));
        assert!(workbook_xml.contains(r#"name="CrossLocal" localSheetId="0">#REF!$A$2"#));
        assert!(workbook_xml.contains(r#"name="SafeLocal" localSheetId="0">Keep!$A$1"#));
        let keep_sheet = String::from_utf8(zip_member(&saved, "xl/worksheets/sheet2.xml"))
            .expect("worksheet UTF-8");
        assert!(keep_sheet.contains("<f>#REF!A2+1</f>"));

        let app = String::from_utf8(zip_member(&saved, "docProps/app.xml"))
            .expect("app properties UTF-8");
        assert!(app.contains(r#"custom="preserve""#));
        assert!(app.contains(r#"keep="untouched""#));
        assert!(app.contains(r#"<vt:i4>1</vt:i4>"#));
        assert!(app.contains(r#"<vt:vector size="2" baseType="lpstr">"#));
        assert!(!app.contains("<vt:lpstr>Delete</vt:lpstr>"));
        assert!(app.contains("<vt:lpstr>Keep</vt:lpstr>"));
        assert!(app.contains("<vt:lpstr>Unrelated title</vt:lpstr>"));

        let reopened = Workbook::open(&saved).expect("reopen repaired deletion");
        assert_eq!(reopened.sheet_names(), vec!["Keep"]);
        assert_eq!(
            reopened
                .sheet_by_name("Keep")
                .and_then(|sheet| sheet.cell(0, 0)),
            Some(&Cell::Formula {
                formula: "#REF!A2+1".into(),
                cached: Box::new(Cell::Number(3.0)),
            })
        );
        assert!(reopened
            .defined_names()
            .iter()
            .any(|(name, value)| name == "DeletedGlobal" && value == "#REF!$A$2"));
    }

    #[test]
    fn delete_sheet_dependency_repair_rolls_back_after_an_earlier_rewrite() {
        let input = sheet_delete_dependency_fixture();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open rollback fixture");
        let before = spreadsheet.save().expect("serialize rollback fixture");

        set_test_fail_commit_after(1);
        let result = spreadsheet.delete_sheet("Delete");
        reset_test_fail_commit();

        assert!(result.is_err(), "injected later tree edit must fail");
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(
            spreadsheet.save().expect("save rolled-back fixture"),
            before
        );
    }

    #[test]
    fn delete_sheet_rejects_ambiguous_and_unsafe_dependency_graphs() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("First");
        workbook.add_sheet("Second");
        let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open relationship fixture");
        let package = seed.package.as_mut().expect("editable package");
        let tree = package
            .part_tree_mut("xl/_rels/workbook.xml.rels")
            .expect("promote workbook relationships");
        let root = tree.root_element().expect("relationships root");
        let index = tree.children_of(root).len();
        tree.insert_fragment_at(
            root,
            index,
            br#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>"#,
        )
        .expect("insert duplicate relationship id");
        let ambiguous = seed.save().expect("save ambiguous relationship fixture");
        let mut spreadsheet = Spreadsheet::open(&ambiguous).expect("reopen ambiguous fixture");
        let before = spreadsheet.save().expect("serialize ambiguous fixture");
        assert!(spreadsheet.delete_sheet("First").is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(spreadsheet.save().expect("save rejected deletion"), before);

        let mut workbook = Workbook::new();
        workbook.add_sheet("First");
        workbook.add_sheet("Second");
        let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open pivot fixture");
        let package = seed.package.as_mut().expect("editable package");
        package.set_part(
            "xl/pivotTables/pivotTable1.xml",
            b"<pivotTableDefinition/>".to_vec(),
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.pivotTable+xml"),
        );
        package.add_relationship(
            "xl/worksheets/sheet1.xml",
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotTable",
            "../pivotTables/pivotTable1.xml",
            false,
        );
        let unsafe_graph = seed.save().expect("save pivot dependency fixture");
        let mut spreadsheet = Spreadsheet::open(&unsafe_graph).expect("reopen pivot fixture");
        let before = spreadsheet.save().expect("serialize pivot fixture");
        assert!(spreadsheet.delete_sheet("First").is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(
            spreadsheet.save().expect("save rejected pivot deletion"),
            before
        );
    }

    #[test]
    fn add_delete_rejections_and_late_delete_failure_roll_back_exactly() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data");
        let mut spreadsheet = Spreadsheet::open(&workbook.to_xlsx()).expect("open one sheet");
        let before = spreadsheet.save().expect("save one sheet");
        assert!(spreadsheet.add_sheet("data").is_err());
        assert!(spreadsheet.delete_sheet("Data").is_err());
        assert_eq!(spreadsheet.save().expect("save rejected edits"), before);
        assert!(spreadsheet.edited_parts().is_empty());

        let mut workbook = Workbook::new();
        workbook.add_sheet("First");
        workbook.add_sheet("Second");
        let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open two sheets");
        let package = seed.package.as_mut().expect("editable package");
        let tree = package
            .part_tree_mut("xl/workbook.xml")
            .expect("promote workbook");
        let root = tree.root_element().expect("workbook root");
        let views = tree.child_by_name(root, b"bookViews").expect("book views");
        tree.remove_child(root, views).expect("remove book views");
        let input = seed.save().expect("save workbook without book views");
        let mut spreadsheet = Spreadsheet::open(&input).expect("reopen custom workbook");
        let before = spreadsheet.save().expect("serialize custom workbook");

        set_test_fail_commit_after(0);
        let result = spreadsheet.delete_sheet("First");
        reset_test_fail_commit();

        assert!(result.is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(spreadsheet.save().expect("save rolled back delete"), before);
        assert_eq!(
            Workbook::open(&before)
                .expect("reopen original")
                .sheet_names(),
            vec!["First", "Second"]
        );
    }

    #[test]
    fn merge_and_common_layout_edits_round_trip_and_clear() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, "anchor");
        let input = workbook.to_xlsx();
        let original_styles = zip_member(&input, "xl/styles.xml");
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

        spreadsheet
            .merge_cells("Data", 0, 0, 1, 1)
            .expect("merge cells");
        spreadsheet
            .set_row_height("Data", 2, 24.5)
            .expect("set row height");
        spreadsheet
            .set_row_hidden("Data", 2, true)
            .expect("hide row");
        spreadsheet
            .set_column_width("Data", 2, 18.25)
            .expect("set column width");
        spreadsheet
            .set_column_hidden("Data", 2, true)
            .expect("hide column");
        spreadsheet
            .set_freeze_panes("Data", 1, 2)
            .expect("freeze panes");
        spreadsheet
            .set_print_area("Data", Some((0, 0, 9, 3)))
            .expect("set print area");

        assert_eq!(
            spreadsheet.edited_parts(),
            &["xl/workbook.xml", "xl/worksheets/sheet1.xml"]
        );
        let saved = spreadsheet.save().expect("save layout edits");
        assert_eq!(zip_member(&saved, "xl/styles.xml"), original_styles);
        let reopened = Workbook::open(&saved).expect("reopen layout edits");
        let sheet = reopened.sheet_by_name("Data").expect("Data sheet");
        assert_eq!(sheet.merged_ranges(), &[(0, 0, 1, 1)]);
        assert_eq!(sheet.row_heights().get(&2), Some(&24.5));
        assert!(sheet.hidden_rows().contains(&2));
        assert_eq!(sheet.column_widths().get(&2), Some(&18.25));
        assert!(sheet.hidden_columns().contains(&2));
        assert_eq!(sheet.sheet_view().freeze, Some((1, 2)));
        assert_eq!(
            sheet.page_setup().and_then(|setup| setup.print_area),
            Some((0, 0, 9, 3))
        );

        spreadsheet
            .unmerge_cells("Data", 0, 0, 1, 1)
            .expect("unmerge cells");
        spreadsheet
            .set_row_hidden("Data", 2, false)
            .expect("unhide row");
        spreadsheet
            .set_column_hidden("Data", 2, false)
            .expect("unhide column");
        spreadsheet
            .clear_freeze_panes("Data")
            .expect("clear freeze panes");
        spreadsheet
            .set_print_area("Data", None)
            .expect("clear print area");
        let cleared = Workbook::open(&spreadsheet.save().expect("save cleared layout"))
            .expect("reopen cleared layout");
        let sheet = cleared.sheet_by_name("Data").expect("Data sheet");
        assert!(sheet.merged_ranges().is_empty());
        assert!(!sheet.hidden_rows().contains(&2));
        assert!(!sheet.hidden_columns().contains(&2));
        assert_eq!(sheet.sheet_view().freeze, None);
        assert_eq!(sheet.page_setup().and_then(|setup| setup.print_area), None);
    }

    #[test]
    fn column_range_split_preserves_neighbor_layout_attributes() {
        let input = minimal_xlsx_with_one_valued_cell();
        let mut seed = Spreadsheet::open(&input).expect("open minimal xlsx");
        seed.package
            .as_mut()
            .expect("editable package")
            .replace_part(
                "xl/worksheets/sheet1.xml",
                br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><cols><col min="1" max="3" width="12" customWidth="1" hidden="1" outlineLevel="2" bestFit="1"/></cols><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#.to_vec(),
            )
            .expect("replace worksheet fixture");
        let custom = seed.save().expect("serialize custom fixture");
        let mut spreadsheet = Spreadsheet::open(&custom).expect("reopen custom fixture");

        spreadsheet
            .set_column_hidden("Data", 1, false)
            .expect("unhide middle column");
        spreadsheet
            .set_column_width("Data", 1, 20.0)
            .expect("resize middle column");

        let saved = spreadsheet.save().expect("save split columns");
        let xml = String::from_utf8(zip_member(&saved, "xl/worksheets/sheet1.xml"))
            .expect("worksheet UTF-8");
        assert!(xml.contains(
            r#"min="1" max="1" width="12" customWidth="1" hidden="1" outlineLevel="2" bestFit="1""#
        ));
        assert!(xml.contains(
            r#"min="2" max="2" width="20" customWidth="1" outlineLevel="2" bestFit="1""#
        ));
        assert!(xml.contains(
            r#"min="3" max="3" width="12" customWidth="1" hidden="1" outlineLevel="2" bestFit="1""#
        ));
        let reopened = Workbook::open(&saved).expect("reopen split columns");
        let sheet = reopened.sheet_by_name("Data").expect("Data sheet");
        assert_eq!(sheet.column_widths().get(&0), Some(&12.0));
        assert_eq!(sheet.column_widths().get(&1), Some(&20.0));
        assert_eq!(sheet.column_widths().get(&2), Some(&12.0));
        assert!(sheet.hidden_columns().contains(&0));
        assert!(!sheet.hidden_columns().contains(&1));
        assert!(sheet.hidden_columns().contains(&2));
        assert_eq!(sheet.col_outline_levels().get(&1), Some(&2));
    }

    #[test]
    fn merge_overlap_validation_and_late_failure_roll_back_exactly() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").merge(0, 0, 1, 1);
        let mut spreadsheet = Spreadsheet::open(&workbook.to_xlsx()).expect("open merged xlsx");
        let before = spreadsheet.save().expect("serialize original merge");

        assert!(spreadsheet.merge_cells("Data", 1, 1, 2, 2).is_err());
        assert!(spreadsheet.unmerge_cells("Data", 3, 3, 4, 4).is_err());
        assert!(spreadsheet.set_row_height("Data", 0, f32::NAN).is_err());
        assert!(spreadsheet
            .set_column_width("Data", u16::MAX, 10.0)
            .is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(spreadsheet.save().expect("save rejected edits"), before);

        set_test_fail_commit_after(0);
        let result = spreadsheet.merge_cells("Data", 0, 2, 0, 3);
        reset_test_fail_commit();
        assert!(result.is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(spreadsheet.save().expect("save rolled back merge"), before);
    }

    #[test]
    fn save_to_path_atomically_replaces_and_cleans_failed_temporary_files() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, "persisted");
        let spreadsheet = Spreadsheet::open(&workbook.to_xlsx()).expect("open editable xlsx");
        let unique = format!(
            "rxls-save-test-{}-{}",
            std::process::id(),
            SAVE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(unique);
        fs::create_dir(&root).expect("create test directory");
        let destination = root.join("book.xlsx");
        fs::write(&destination, b"old destination").expect("write old destination");

        spreadsheet
            .save_to_path(&destination)
            .expect("atomic save succeeds");
        let persisted = fs::read(&destination).expect("read atomic destination");
        assert_eq!(
            Workbook::open(&persisted)
                .expect("reopen atomic destination")
                .sheet_by_name("Data")
                .and_then(|sheet| sheet.cell(0, 0)),
            Some(&Cell::Text("persisted".into()))
        );

        let blocked = root.join("blocked.xlsx");
        fs::create_dir(&blocked).expect("create blocking destination directory");
        fs::write(blocked.join("marker"), b"unchanged").expect("write marker");
        assert!(spreadsheet.save_to_path(&blocked).is_err());
        assert_eq!(
            fs::read(blocked.join("marker")).expect("read marker"),
            b"unchanged"
        );
        let leftovers: Vec<_> = fs::read_dir(&root)
            .expect("list test directory")
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".rxls-tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files leaked: {leftovers:?}"
        );
        fs::remove_dir_all(&root).expect("clean test directory");
    }

    #[test]
    fn legacy_comment_create_update_delete_round_trips_and_preserves_parts() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, "anchor");
        let input = workbook.to_xlsx();
        let original_styles = zip_member(&input, "xl/styles.xml");
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

        spreadsheet
            .set_comment("Data", 0, 0, "first note", Some("Alice"))
            .expect("create comment");
        assert_eq!(
            spreadsheet.edited_parts(),
            &[
                "[Content_Types].xml",
                "xl/comments1.xml",
                "xl/drawings/vmlDrawing1.vml",
                "xl/worksheets/_rels/sheet1.xml.rels",
                "xl/worksheets/sheet1.xml",
            ]
        );
        let created = spreadsheet.save().expect("save created comment");
        let rels = String::from_utf8(zip_member(&created, "xl/worksheets/_rels/sheet1.xml.rels"))
            .expect("worksheet rels UTF-8");
        assert!(rels.contains(r#"Id="rId4""#) && rels.contains("/comments\""));
        assert!(rels.contains(r#"Id="rId5""#) && rels.contains("/vmlDrawing\""));
        let reopened = Workbook::open(&created).expect("reopen created comment");
        assert_eq!(
            reopened.sheet_by_name("Data").expect("Data").comments(),
            &[Comment {
                row: 0,
                col: 0,
                text: "first note".into(),
                author: Some("Alice".into()),
            }]
        );

        let original_sheet = zip_member(&created, "xl/worksheets/sheet1.xml");
        let original_vml = zip_member(&created, "xl/drawings/vmlDrawing1.vml");
        let mut spreadsheet = Spreadsheet::open(&created).expect("reopen for comment update");
        spreadsheet
            .set_comment("Data", 0, 0, "updated note", Some("Bob"))
            .expect("update comment");
        assert_eq!(spreadsheet.edited_parts(), &["xl/comments1.xml"]);
        let updated = spreadsheet.save().expect("save updated comment");
        assert_eq!(
            zip_member(&updated, "xl/worksheets/sheet1.xml"),
            original_sheet
        );
        assert_eq!(
            zip_member(&updated, "xl/drawings/vmlDrawing1.vml"),
            original_vml
        );
        let reopened = Workbook::open(&updated).expect("reopen updated comment");
        assert_eq!(
            reopened.sheet_by_name("Data").expect("Data").comments(),
            &[Comment {
                row: 0,
                col: 0,
                text: "updated note".into(),
                author: Some("Bob".into()),
            }]
        );

        let mut spreadsheet = Spreadsheet::open(&updated).expect("reopen for comment delete");
        spreadsheet
            .delete_comment("Data", 0, 0)
            .expect("delete comment");
        let deleted = spreadsheet.save().expect("save deleted comment");
        assert!(!zip_has_member(&deleted, "xl/comments1.xml"));
        assert!(!zip_has_member(&deleted, "xl/drawings/vmlDrawing1.vml"));
        assert!(!zip_has_member(
            &deleted,
            "xl/worksheets/_rels/sheet1.xml.rels"
        ));
        assert_eq!(zip_member(&deleted, "xl/styles.xml"), original_styles);
        assert!(Workbook::open(&deleted)
            .expect("reopen deleted comment")
            .sheet_by_name("Data")
            .expect("Data")
            .comments()
            .is_empty());
    }

    #[test]
    fn comment_delete_preserves_other_vml_shapes_and_malformed_vml_rolls_back() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data");
        let mut spreadsheet = Spreadsheet::open(&workbook.to_xlsx()).expect("open xlsx");
        spreadsheet
            .set_comment("Data", 0, 0, "note", Some("Alice"))
            .expect("create comment");
        let created = spreadsheet.save().expect("save comment");

        let mut seed = Spreadsheet::open(&created).expect("open VML seed");
        let package = seed.package.as_mut().expect("editable package");
        let tree = package
            .part_tree_mut("xl/drawings/vmlDrawing1.vml")
            .expect("promote VML");
        let root = tree.root_element().expect("VML root");
        let index = tree.children_of(root).len();
        tree.insert_fragment_at(
            root,
            index,
            br##"<v:shape id="_x0000_s2048" type="#_x0000_t201"/>"##,
        )
        .expect("insert control-like shape");
        let with_control = seed.save().expect("save VML control fixture");
        let mut spreadsheet = Spreadsheet::open(&with_control).expect("reopen VML fixture");
        spreadsheet
            .delete_comment("Data", 0, 0)
            .expect("delete note but preserve control VML");
        let preserved = spreadsheet.save().expect("save preserved VML");
        assert!(!zip_has_member(&preserved, "xl/comments1.xml"));
        assert!(zip_has_member(&preserved, "xl/drawings/vmlDrawing1.vml"));
        let vml = String::from_utf8(zip_member(&preserved, "xl/drawings/vmlDrawing1.vml"))
            .expect("VML UTF-8");
        assert!(vml.contains("_x0000_s2048"));

        let mut seed = Spreadsheet::open(&created).expect("open malformed VML seed");
        seed.package
            .as_mut()
            .expect("editable package")
            .replace_part(
                "xl/drawings/vmlDrawing1.vml",
                b"not well-formed VML".to_vec(),
            )
            .expect("replace VML");
        let malformed = seed.save().expect("save malformed VML fixture");
        let mut spreadsheet = Spreadsheet::open(&malformed).expect("open malformed VML fixture");
        let before = spreadsheet.save().expect("serialize malformed fixture");
        assert!(spreadsheet.delete_comment("Data", 0, 0).is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(spreadsheet.save().expect("save rolled back delete"), before);
    }

    #[test]
    fn external_and_internal_hyperlink_crud_reuses_relationship_ids() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "external");
        sheet.write(0, 1, "internal");
        let input = workbook.to_xlsx();
        let original_styles = zip_member(&input, "xl/styles.xml");
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
        spreadsheet
            .set_external_hyperlink("Data", 0, 0, "https://example.com/one")
            .expect("create external hyperlink");
        spreadsheet
            .set_internal_hyperlink("Data", 0, 1, "Data!A1")
            .expect("create internal hyperlink");
        let created = spreadsheet.save().expect("save hyperlinks");
        let created_sheet = zip_member(&created, "xl/worksheets/sheet1.xml");
        let rels = String::from_utf8(zip_member(&created, "xl/worksheets/_rels/sheet1.xml.rels"))
            .expect("worksheet rels UTF-8");
        assert!(rels.contains(r#"Id="rId4""#));
        assert!(rels.contains(r#"Target="https://example.com/one""#));
        let sheet_xml = String::from_utf8(created_sheet.clone()).expect("worksheet UTF-8");
        assert!(sheet_xml.contains(r#"ref="A1" r:id="rId4""#));
        assert!(sheet_xml.contains(r#"ref="B1" location="Data!A1""#));
        assert_eq!(
            Workbook::open(&created)
                .expect("reopen hyperlinks")
                .sheet_by_name("Data")
                .expect("Data")
                .hyperlinks(),
            &[(0, 0, "https://example.com/one".into())]
        );

        let mut spreadsheet = Spreadsheet::open(&created).expect("reopen external update");
        spreadsheet
            .set_external_hyperlink("Data", 0, 0, "https://example.com/two")
            .expect("update external hyperlink");
        assert_eq!(
            spreadsheet.edited_parts(),
            &["xl/worksheets/_rels/sheet1.xml.rels"]
        );
        let external_updated = spreadsheet.save().expect("save external update");
        assert_eq!(
            zip_member(&external_updated, "xl/worksheets/sheet1.xml"),
            created_sheet
        );
        let rels = String::from_utf8(zip_member(
            &external_updated,
            "xl/worksheets/_rels/sheet1.xml.rels",
        ))
        .expect("updated rels UTF-8");
        assert!(rels.contains(r#"Id="rId4""#));
        assert!(rels.contains(r#"Target="https://example.com/two""#));

        let mut spreadsheet = Spreadsheet::open(&external_updated).expect("reopen internal update");
        let original_rels = zip_member(&external_updated, "xl/worksheets/_rels/sheet1.xml.rels");
        spreadsheet
            .set_internal_hyperlink("Data", 0, 1, "Data!B2")
            .expect("update internal hyperlink");
        assert_eq!(spreadsheet.edited_parts(), &["xl/worksheets/sheet1.xml"]);
        let internal_updated = spreadsheet.save().expect("save internal update");
        assert_eq!(
            zip_member(&internal_updated, "xl/worksheets/_rels/sheet1.xml.rels"),
            original_rels
        );

        let mut spreadsheet = Spreadsheet::open(&internal_updated).expect("reopen link deletes");
        spreadsheet
            .delete_hyperlink("Data", 0, 1)
            .expect("delete internal hyperlink");
        spreadsheet
            .delete_hyperlink("Data", 0, 0)
            .expect("delete external hyperlink");
        let deleted = spreadsheet.save().expect("save deleted hyperlinks");
        assert!(!zip_has_member(
            &deleted,
            "xl/worksheets/_rels/sheet1.xml.rels"
        ));
        let sheet_xml = String::from_utf8(zip_member(&deleted, "xl/worksheets/sheet1.xml"))
            .expect("deleted worksheet UTF-8");
        assert!(!sheet_xml.contains("<hyperlinks"));
        assert_eq!(zip_member(&deleted, "xl/styles.xml"), original_styles);
        let reopened = Workbook::open(&deleted).expect("reopen deleted hyperlinks");
        let sheet = reopened.sheet_by_name("Data").expect("Data");
        assert!(sheet.hyperlinks().is_empty());
        assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("external".into())));
        assert_eq!(sheet.cell(0, 1), Some(&Cell::Text("internal".into())));
    }

    #[test]
    fn retargeting_one_of_two_shared_external_links_splits_the_relationship() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "first");
        sheet.write(0, 1, "second");
        let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open xlsx");
        seed.set_external_hyperlink("Data", 0, 0, "https://example.com/shared")
            .expect("create external hyperlink");

        let worksheet_path = worksheet_path(seed.package.as_ref().expect("package"), "Data")
            .expect("worksheet path");
        let tree = seed
            .package
            .as_mut()
            .expect("editable package")
            .part_tree_mut(&worksheet_path)
            .expect("promote worksheet");
        sml_set_hyperlink(
            tree,
            0,
            1,
            HyperlinkEdit::External("https://example.com/shared"),
            Some("rId4"),
        )
        .expect("share relationship with second cell");
        let shared = seed.save().expect("save shared relationship fixture");

        let mut spreadsheet = Spreadsheet::open(&shared).expect("reopen shared fixture");
        spreadsheet
            .set_external_hyperlink("Data", 0, 0, "https://example.com/first")
            .expect("retarget only the first hyperlink");
        let updated = spreadsheet.save().expect("save split relationships");
        let sheet_xml = String::from_utf8(zip_member(&updated, "xl/worksheets/sheet1.xml"))
            .expect("worksheet UTF-8");
        assert!(sheet_xml.contains(r#"ref="A1" r:id="rId5""#));
        assert!(sheet_xml.contains(r#"ref="B1" r:id="rId4""#));
        let rels = String::from_utf8(zip_member(&updated, "xl/worksheets/_rels/sheet1.xml.rels"))
            .expect("relationships UTF-8");
        assert!(rels.contains(r#"Id="rId4""#));
        assert!(rels.contains(r#"Target="https://example.com/shared""#));
        assert!(rels.contains(r#"Id="rId5""#));
        assert!(rels.contains(r#"Target="https://example.com/first""#));
        assert_eq!(
            Workbook::open(&updated)
                .expect("reopen split hyperlinks")
                .sheet_by_name("Data")
                .expect("Data")
                .hyperlinks(),
            &[
                (0, 0, "https://example.com/first".into()),
                (0, 1, "https://example.com/shared".into()),
            ]
        );
    }

    #[test]
    fn comment_and_hyperlink_late_failures_roll_back_exactly() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data");
        let input = workbook.to_xlsx();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
        let before_link = spreadsheet.save().expect("serialize link fixture");

        set_test_fail_commit_after(0);
        let result = spreadsheet.set_external_hyperlink("Data", 0, 0, "https://example.com");
        reset_test_fail_commit();
        assert!(result.is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(
            spreadsheet.save().expect("save rolled back link"),
            before_link
        );

        spreadsheet
            .set_comment("Data", 0, 0, "original", Some("Alice"))
            .expect("create rollback comment fixture");
        let with_comment = spreadsheet.save().expect("save rollback fixture");
        let mut spreadsheet = Spreadsheet::open(&with_comment).expect("reopen rollback fixture");
        let before = spreadsheet.save().expect("serialize rollback fixture");
        set_test_fail_commit_after(0);
        let result = spreadsheet.set_comment("Data", 0, 0, "candidate", Some("Alice"));
        reset_test_fail_commit();
        assert!(result.is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(
            spreadsheet.save().expect("save rolled back comment"),
            before
        );
    }

    #[test]
    fn data_validation_create_update_delete_round_trips_and_preserves_unknown_xml() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, "value");
        let input = workbook.to_xlsx();
        let original_styles = zip_member(&input, "xl/styles.xml");
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

        spreadsheet
            .set_data_validation(
                "Data",
                DataValidation::list((0, 0, 2, 0), "\"Yes,No\"").with_prompt("Pick", "Choose one"),
            )
            .expect("create data validation");
        assert_eq!(spreadsheet.edited_parts(), &["xl/worksheets/sheet1.xml"]);
        let created = spreadsheet.save().expect("save created validation");
        let reopened = Workbook::open(&created).expect("reopen created validation");
        let validations = reopened
            .sheet_by_name("Data")
            .expect("Data")
            .data_validations();
        assert_eq!(validations.len(), 1);
        assert_eq!(validations[0].sqref, (0, 0, 2, 0));
        assert_eq!(validations[0].kind, DvKind::List);

        let mut seed = Spreadsheet::open(&created).expect("open unknown XML seed");
        let tree = seed
            .package
            .as_mut()
            .expect("editable package")
            .part_tree_mut("xl/worksheets/sheet1.xml")
            .expect("promote worksheet");
        let root = tree.root_element().expect("worksheet root");
        let wrapper = data_validation_wrappers(tree, root)[0];
        tree.set_attr(wrapper, b"customWrapper", b"preserve")
            .expect("set wrapper extension attr");
        let validation = data_validation_nodes(tree, wrapper)[0];
        tree.set_attr(validation, b"errorStyle", b"warning")
            .expect("set unknown modeled-adjacent attr");
        tree.set_attr(validation, b"customRule", b"preserve")
            .expect("set custom rule attr");
        let index = tree.children_of(validation).len();
        tree.insert_fragment_at(
            validation,
            index,
            br#"<extLst><ext uri="custom"><custom keep="yes"/></ext></extLst>"#,
        )
        .expect("insert unknown child");
        let seeded = seed.save().expect("save unknown XML seed");

        let mut spreadsheet = Spreadsheet::open(&seeded).expect("reopen validation update");
        spreadsheet
            .set_data_validation(
                "Data",
                DataValidation::new((0, 0, 2, 0), DvKind::Whole, DvOp::Between, "1")
                    .with_formula2("10")
                    .with_error("Bounds", "Use 1 through 10"),
            )
            .expect("replace data validation");
        spreadsheet
            .set_data_validation(
                "Data",
                DataValidation::new((0, 2, 0, 2), DvKind::Custom, DvOp::Equal, "ISNUMBER(C1)"),
            )
            .expect("append second validation");
        let updated = spreadsheet.save().expect("save updated validations");
        let xml = String::from_utf8(zip_member(&updated, "xl/worksheets/sheet1.xml"))
            .expect("worksheet UTF-8");
        assert!(xml.contains(r#"<dataValidations count="2" customWrapper="preserve">"#));
        assert!(xml.contains(r#"errorStyle="warning" customRule="preserve""#));
        assert!(xml.contains(r#"<custom keep="yes"/>"#));
        assert!(xml.contains(r#"type="whole""#));
        assert!(xml.contains(r#"operator="between""#));
        assert!(xml.contains("<formula1>1</formula1><formula2>10</formula2>"));
        let reopened = Workbook::open(&updated).expect("reopen updated validations");
        assert_eq!(
            reopened
                .sheet_by_name("Data")
                .expect("Data")
                .data_validations()
                .len(),
            2
        );

        let mut spreadsheet = Spreadsheet::open(&updated).expect("reopen validation delete");
        spreadsheet
            .delete_data_validation("Data", (0, 0, 2, 0))
            .expect("delete first validation");
        let one_left = spreadsheet.save().expect("save one validation");
        let xml = String::from_utf8(zip_member(&one_left, "xl/worksheets/sheet1.xml"))
            .expect("worksheet UTF-8");
        assert!(xml.contains(r#"<dataValidations count="1" customWrapper="preserve">"#));
        let mut spreadsheet = Spreadsheet::open(&one_left).expect("reopen last validation delete");
        spreadsheet
            .delete_data_validation("Data", (0, 2, 0, 2))
            .expect("delete last validation");
        let deleted = spreadsheet.save().expect("save deleted validations");
        let xml = String::from_utf8(zip_member(&deleted, "xl/worksheets/sheet1.xml"))
            .expect("worksheet UTF-8");
        assert!(!xml.contains("<dataValidations"));
        assert_eq!(zip_member(&deleted, "xl/styles.xml"), original_styles);
    }

    #[test]
    fn data_validation_rejections_and_late_failure_roll_back_exactly() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data");
        let mut spreadsheet = Spreadsheet::open(&workbook.to_xlsx()).expect("open xlsx");
        spreadsheet
            .set_data_validation("Data", DataValidation::list((0, 0, 2, 0), "\"A,B\""))
            .expect("seed validation");
        let seeded = spreadsheet.save().expect("save seeded validation");

        let mut spreadsheet = Spreadsheet::open(&seeded).expect("reopen rejection fixture");
        let before = spreadsheet.save().expect("serialize rejection fixture");
        assert!(spreadsheet
            .set_data_validation("Data", DataValidation::list((1, 0, 1, 0), "\"C,D\""))
            .is_err());
        assert!(spreadsheet
            .set_data_validation(
                "Data",
                DataValidation::new((4, 0, 4, 0), DvKind::Whole, DvOp::Between, ""),
            )
            .is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(
            spreadsheet.save().expect("save rejected validation"),
            before
        );

        set_test_fail_commit_after(0);
        let result = spreadsheet.set_data_validation(
            "Data",
            DataValidation::new((0, 0, 2, 0), DvKind::Whole, DvOp::Equal, "5"),
        );
        reset_test_fail_commit();
        assert!(result.is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(
            spreadsheet.save().expect("save rolled-back validation"),
            before
        );
    }

    #[test]
    fn existing_table_bottom_resize_round_trips_and_preserves_unknown_xml() {
        use crate::Table;

        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "Name");
        sheet.write(0, 1, "Value");
        sheet.write(1, 0, "one");
        sheet.write(1, 1, 1.0);
        sheet.add_table(Table {
            range: (0, 0, 1, 1),
            name: "Sales".into(),
            columns: vec!["Name".into(), "Value".into()],
            style: None,
        });
        let input = workbook.to_xlsx();
        let original_sheet = zip_member(&input, "xl/worksheets/sheet1.xml");
        let original_styles = zip_member(&input, "xl/styles.xml");
        let mut seed = Spreadsheet::open(&input).expect("open table seed");
        let tree = seed
            .package
            .as_mut()
            .expect("editable package")
            .part_tree_mut("xl/tables/table1.xml")
            .expect("promote table");
        let plan = inspect_table_part(tree).expect("inspect table");
        tree.set_attr(plan.root, b"customTable", b"preserve")
            .expect("set custom table attr");
        tree.set_attr(
            plan.auto_filter.expect("autoFilter"),
            b"customFilter",
            b"preserve",
        )
        .expect("set custom filter attr");
        let index = tree.children_of(plan.root).len();
        tree.insert_fragment_at(
            plan.root,
            index,
            br#"<extLst><ext uri="custom"><custom keep="yes"/></ext></extLst>"#,
        )
        .expect("insert table extension");
        let seeded = seed.save().expect("save table seed");

        let mut spreadsheet = Spreadsheet::open(&seeded).expect("reopen table seed");
        spreadsheet
            .set_table_range("Data", "sales", (0, 0, 5, 1))
            .expect("resize table bottom row");
        assert_eq!(spreadsheet.edited_parts(), &["xl/tables/table1.xml"]);
        let resized = spreadsheet.save().expect("save resized table");
        assert_eq!(
            zip_member(&resized, "xl/worksheets/sheet1.xml"),
            original_sheet
        );
        assert_eq!(zip_member(&resized, "xl/styles.xml"), original_styles);
        let xml =
            String::from_utf8(zip_member(&resized, "xl/tables/table1.xml")).expect("table UTF-8");
        assert!(xml.contains(r#"ref="A1:B6""#));
        assert!(xml.contains(r#"customTable="preserve""#));
        assert!(xml.contains(r#"customFilter="preserve""#));
        assert!(xml.contains(r#"<custom keep="yes"/>"#));
        let reopened = Workbook::open(&resized).expect("reopen resized table");
        assert_eq!(
            reopened.sheet_by_name("Data").expect("Data").tables()[0].range,
            (0, 0, 5, 1)
        );

        let mut spreadsheet = Spreadsheet::open(&resized).expect("reopen table rejection");
        let before = spreadsheet.save().expect("serialize resized table");
        assert!(spreadsheet
            .set_table_range("Data", "Sales", (1, 0, 5, 1))
            .is_err());
        assert!(spreadsheet
            .set_table_range("Data", "Sales", (0, 0, 5, 2))
            .is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(
            spreadsheet.save().expect("save rejected table edits"),
            before
        );
    }

    #[test]
    fn table_resize_inserts_missing_autofilter_and_rolls_back_late_failure() {
        use crate::Table;

        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "Header");
        sheet.add_table(Table {
            range: (0, 0, 1, 0),
            name: "Items".into(),
            columns: vec!["Header".into()],
            style: None,
        });
        let mut seed = Spreadsheet::open(&workbook.to_xlsx()).expect("open table fixture");
        let tree = seed
            .package
            .as_mut()
            .expect("editable package")
            .part_tree_mut("xl/tables/table1.xml")
            .expect("promote table");
        let plan = inspect_table_part(tree).expect("inspect table");
        tree.remove_child(plan.root, plan.auto_filter.expect("autoFilter"))
            .expect("remove autoFilter");
        let missing_filter = seed.save().expect("save missing autoFilter fixture");

        let mut spreadsheet = Spreadsheet::open(&missing_filter).expect("reopen rollback fixture");
        let before = spreadsheet.save().expect("serialize rollback fixture");
        set_test_fail_commit_after(0);
        let result = spreadsheet.set_table_range("Data", "Items", (0, 0, 3, 0));
        reset_test_fail_commit();
        assert!(result.is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(spreadsheet.save().expect("save rolled-back table"), before);

        spreadsheet
            .set_table_range("Data", "Items", (0, 0, 3, 0))
            .expect("resize and restore autoFilter");
        let saved = spreadsheet.save().expect("save restored autoFilter");
        let xml =
            String::from_utf8(zip_member(&saved, "xl/tables/table1.xml")).expect("table UTF-8");
        assert!(xml.contains(r#"<autoFilter ref="A1:A4"/>"#));
    }

    #[test]
    fn rename_sheet_updates_formula_name_print_and_chart_references() {
        use crate::{Chart, ChartKind, PageSetup, Series};

        let mut workbook = Workbook::new();
        {
            let data = workbook.add_sheet("Old Data");
            data.write(0, 0, 10.0);
            data.write(1, 0, 20.0);
            data.set_page_setup(PageSetup::new().with_print_area((0, 0, 1, 0)));
        }
        {
            let other = workbook.add_sheet("Other");
            other.write_formula(0, 0, r#"'Old Data'!A1&"Old Data!A1""#, "10Old Data!A1");
            other.add_chart(Chart {
                kind: ChartKind::Line,
                title: None,
                series: vec![Series {
                    name: Some("Values".into()),
                    categories: None,
                    values: "'Old Data'!$A$1:$A$2".into(),
                    bubble_sizes: None,
                }],
                legend: false,
                data_labels: false,
                x_axis_title: None,
                y_axis_title: None,
                from: (2, 0),
                to: (12, 8),
            });
        }
        workbook.define_name("InputRange", "'Old Data'!$A$1:$A$2");
        let input = workbook.to_xlsx();
        let original_source_sheet = zip_member(&input, "xl/worksheets/sheet1.xml");
        let original_workbook_rels = zip_member(&input, "xl/_rels/workbook.xml.rels");
        let original_content_types = zip_member(&input, "[Content_Types].xml");
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

        spreadsheet
            .rename_sheet("Old Data", "Renamed Data")
            .expect("rename sheet atomically");

        assert_eq!(
            spreadsheet.edited_parts(),
            &[
                "xl/charts/chart1.xml",
                "xl/workbook.xml",
                "xl/worksheets/sheet2.xml"
            ]
        );
        let saved = spreadsheet.save().expect("save renamed package");
        assert_eq!(
            zip_member(&saved, "xl/worksheets/sheet1.xml"),
            original_source_sheet,
            "the renamed sheet's cell part is untouched when it has no references to itself"
        );
        assert_eq!(
            zip_member(&saved, "xl/_rels/workbook.xml.rels"),
            original_workbook_rels,
            "renaming does not churn relationship ids or targets"
        );
        assert_eq!(
            zip_member(&saved, "[Content_Types].xml"),
            original_content_types,
            "renaming does not churn package content types"
        );

        let reopened = Workbook::open(&saved).expect("reopen renamed package");
        assert_eq!(reopened.sheet_names(), vec!["Renamed Data", "Other"]);
        assert_eq!(
            reopened.defined_names(),
            &[(
                "InputRange".to_string(),
                "'Renamed Data'!$A$1:$A$2".to_string()
            )]
        );
        assert_eq!(
            reopened
                .sheet_by_name("Renamed Data")
                .and_then(|sheet| sheet.page_setup())
                .and_then(|setup| setup.print_area),
            Some((0, 0, 1, 0))
        );
        assert_eq!(
            reopened
                .sheet_by_name("Other")
                .and_then(|sheet| sheet.cell(0, 0)),
            Some(&Cell::Formula {
                formula: r#"'Renamed Data'!A1&"Old Data!A1""#.into(),
                cached: Box::new(Cell::Text("10Old Data!A1".into())),
            })
        );
        assert_eq!(
            reopened
                .sheet_by_name("Other")
                .and_then(|sheet| sheet.charts().first())
                .and_then(|chart| chart.series.first())
                .map(|series| series.values.as_str()),
            Some("'Renamed Data'!$A$1:$A$2")
        );
    }

    #[test]
    fn rename_sheet_rolls_back_formula_and_name_updates_on_late_failure() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Old").write(0, 0, 1.0);
        workbook
            .add_sheet("Other")
            .write_formula(0, 0, "Old!A1+1", 2.0);
        workbook.define_name("Input", "Old!$A$1");
        let input = workbook.to_xlsx();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
        let before = spreadsheet.save().expect("serialize original package");

        // The workbook defined-name rewrite succeeds first; fail the later
        // worksheet formula rewrite and prove the outer rename transaction
        // discards both that first edit and all touched-part bookkeeping.
        set_test_fail_commit_after(1);
        let result = spreadsheet.rename_sheet("Old", "Renamed");
        reset_test_fail_commit();

        assert!(result.is_err(), "the injected worksheet rewrite must fail");
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(
            spreadsheet.save().expect("save rolled-back package"),
            before
        );
        let reopened = Workbook::open(&before).expect("reopen original package");
        assert_eq!(reopened.sheet_names(), vec!["Old", "Other"]);
        assert_eq!(
            reopened
                .sheet_by_name("Other")
                .and_then(|sheet| sheet.cell(0, 0)),
            Some(&Cell::Formula {
                formula: "Old!A1+1".into(),
                cached: Box::new(Cell::Number(2.0)),
            })
        );
        assert_eq!(
            reopened.defined_names(),
            &[("Input".to_string(), "Old!$A$1".to_string())]
        );
    }

    #[test]
    fn rename_sheet_rejects_case_insensitive_duplicates_without_touching_parts() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, 1.0);
        workbook.add_sheet("Other").write(0, 0, 2.0);
        let input = workbook.to_xlsx();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
        let before = spreadsheet.save().expect("save original package");

        assert!(spreadsheet.rename_sheet("Other", "data").is_err());
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(spreadsheet.save().expect("save unchanged package"), before);
    }

    #[test]
    fn document_properties_roll_back_if_the_second_part_edit_fails() {
        let mut workbook = Workbook::new();
        workbook.set_properties(
            DocProperties::new()
                .with_title("Original title")
                .with_company("Original company"),
        );
        workbook.add_sheet("Data").write(0, 0, "value");
        let input = workbook.to_xlsx();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");
        let before = spreadsheet.save().expect("serialize original package");

        // Updating core.xml's existing title consumes the first commit. Force
        // the following app.xml Company update to fail after core.xml has
        // already changed in the candidate, exercising clone-and-swap rollback.
        set_test_fail_commit_after(1);
        let result = spreadsheet.set_document_properties(
            DocProperties::new()
                .with_title("Candidate title")
                .with_company("Candidate company"),
        );
        reset_test_fail_commit();

        assert!(result.is_err(), "the injected app.xml edit must fail");
        assert!(spreadsheet.edited_parts().is_empty());
        assert_eq!(
            spreadsheet.save().expect("serialize rolled-back package"),
            before,
            "neither properties part may commit after the second part fails"
        );
        let reopened = Workbook::open(&before).expect("reopen original package");
        assert_eq!(reopened.properties.title.as_deref(), Some("Original title"));
        assert_eq!(
            reopened.properties.company.as_deref(),
            Some("Original company")
        );
    }

    #[test]
    fn document_properties_commit_core_and_app_together() {
        let mut workbook = Workbook::new();
        workbook.set_properties(
            DocProperties::new()
                .with_title("Original title")
                .with_company("Original company"),
        );
        workbook.add_sheet("Data").write(0, 0, "value");
        let input = workbook.to_xlsx();
        let mut spreadsheet = Spreadsheet::open(&input).expect("open editable xlsx");

        spreadsheet
            .set_document_properties(
                DocProperties::new()
                    .with_title("Committed title")
                    .with_company("Committed company"),
            )
            .expect("commit properties");

        assert_eq!(
            spreadsheet.edited_parts(),
            &["docProps/app.xml", "docProps/core.xml"]
        );
        let saved = spreadsheet.save().expect("save committed package");
        let reopened = Workbook::open(&saved).expect("reopen committed package");
        assert_eq!(
            reopened.properties.title.as_deref(),
            Some("Committed title")
        );
        assert_eq!(
            reopened.properties.company.as_deref(),
            Some("Committed company")
        );
    }
}
