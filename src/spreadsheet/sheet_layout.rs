//! Worksheet merge, dimensions, panes, and print-area mutations.

use crate::write::xml::{a1, esc_attr, esc_text, num_str};
use crate::xmltree::{NodeId, XmlTree};
use crate::{Error, Result};

use super::cell_edit::{sml_row_node, sml_row_ref, sml_sheet_data};
use super::selection::{
    parse_a1_range, range_ref, ranges_overlap, validate_col, validate_layout_range, validate_row,
};
use super::{
    local, newly_touched, peek_part_tree, remember_edited_part, workbook_path,
    workbook_sheet_index, worksheet_path, Spreadsheet, MAX_XLSX_COL,
};

impl Spreadsheet {
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

fn validate_layout_measure(value: f32, maximum: f32, message: &'static str) -> Result<()> {
    if value.is_finite() && (0.0..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(Error::Zip(message))
    }
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

pub(super) fn insert_worksheet_fragment(
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
