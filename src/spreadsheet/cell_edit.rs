//! Cell value, formula, append, and range-clear mutations.

use crate::write::xml::{a1, esc_text, num_str};
use crate::xmltree::{NodeId, XmlTree};
use crate::{Cell, Error, Result};

use super::{
    invalidate_calc_chain, newly_touched, peek_part_tree, remember_edited_part,
    validate_edit_cell_text, validate_xml_value, worksheet_path, Spreadsheet,
};

const MAX_EDIT_RANGE_CELLS: u64 = 10_000;

impl Spreadsheet {
    /// Set a worksheet cell in the retained OOXML package.
    ///
    /// The parsed [`crate::Workbook`] view is intentionally not mutated; reopen the
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
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.set_cell_value_in_place(&sheet_name, row, col, &value)
        })
    }

    fn set_cell_value_in_place(
        &mut self,
        sheet_name: &str,
        row: u32,
        col: u16,
        value: &Cell,
    ) -> Result<()> {
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        sml_edit_cell(tree, row, col, value)?;
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
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| candidate.append_row_in_place(&sheet_name, &values))
    }

    fn append_row_in_place(&mut self, sheet_name: &str, values: &[Cell]) -> Result<u32> {
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
        for (col, value) in values.iter().enumerate() {
            sml_edit_cell(tree, row, col as u16, value)?;
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

        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.clear_range_in_place(&sheet_name, row0, col0, row1, col1)
        })
    }

    fn clear_range_in_place(
        &mut self,
        sheet_name: &str,
        row0: u32,
        col0: u16,
        row1: u32,
        col1: u16,
    ) -> Result<()> {
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
pub(super) fn sml_sheet_data(tree: &mut XmlTree) -> Result<NodeId> {
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
pub(super) fn sml_row_ref(tree: &XmlTree, child: NodeId) -> Option<u32> {
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
pub(super) fn sml_row_node(tree: &mut XmlTree, sheet_data: NodeId, row: u32) -> Result<NodeId> {
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
pub(super) fn sml_set_cell_value(tree: &mut XmlTree, cell: NodeId, value: &Cell) -> Result<()> {
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
