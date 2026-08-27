//! Worksheet comments, hyperlinks, data validations, and table mutations.

use std::collections::BTreeSet;

use crate::package::Package;
use crate::write::xml::{
    a1, esc_attr, esc_text, CT_COMMENTS, CT_VML, NS_R, REL_COMMENTS, REL_HYPERLINK, REL_VML_DRAWING,
};
use crate::xmltree::{NodeId, XmlTree};
use crate::{Comment, DataValidation, DvKind, DvOp, Error, Result};

use super::selection::{
    parse_a1_range, range_ref, ranges_overlap, validate_col, validate_layout_range, validate_row,
};
use super::sheet_layout::insert_worksheet_fragment;
use super::{
    canonical_part_key, canonical_part_name, direct_elements_by_local_name, local, newly_touched,
    peek_part_tree, remember_edited_part, validate_nonempty_xml_value, validate_xml_value,
    workbook_path, worksheet_path, Spreadsheet,
};

impl Spreadsheet {
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
        let vml_relation = unique_related_part(package, &worksheet_path, "vmlDrawing")?;
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
                package.add_relationship(&worksheet_path, REL_COMMENTS, &target, false)?;
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
                    let id = package.add_relationship(
                        &worksheet_path,
                        REL_VML_DRAWING,
                        &target,
                        false,
                    )?;
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
        let vml_relation = unique_related_part(package, &worksheet_path, "vmlDrawing")?
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
                Some(package.add_relationship(&worksheet_path, REL_HYPERLINK, target, true)?)
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
}

#[derive(Clone)]
struct RelatedPart {
    id: String,
    path: String,
}

#[derive(Clone, Copy)]
pub(super) enum HyperlinkEdit<'a> {
    External(&'a str),
    Internal(&'a str),
}

#[derive(Default)]
struct HyperlinkRecord {
    exists: bool,
    rid: Option<String>,
    rid_uses: usize,
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
                && crate::xlsx::relationship_type_matches(&relationship.rel_type, relationship_kind)
        })
        .collect();
    if matches.len() > 1 {
        return Err(Error::Zip(
            "multiple relationships of the requested type are unsupported",
        ));
    }
    matches
        .first()
        .map(|relationship| {
            Ok(RelatedPart {
                id: relationship.id.clone(),
                path: Package::try_resolve_rel_target(source, &relationship.target).ok_or(
                    Error::Zip("relationship target is not a valid internal part URI"),
                )?,
            })
        })
        .transpose()
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
    if !relationship.external
        || !crate::xlsx::relationship_type_matches(&relationship.rel_type, "hyperlink")
    {
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

pub(super) fn sml_set_hyperlink(
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

pub(super) fn data_validation_wrappers(tree: &XmlTree, root: NodeId) -> Vec<NodeId> {
    direct_elements_by_local_name(tree, root, b"dataValidations")
}

pub(super) fn data_validation_nodes(tree: &XmlTree, wrapper: NodeId) -> Vec<NodeId> {
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
pub(super) struct TablePartPlan {
    pub(super) name: String,
    pub(super) range: (u32, u16, u32, u16),
    pub(super) root: NodeId,
    pub(super) auto_filter: Option<NodeId>,
    pub(super) filter_tail_rows: u32,
    pub(super) column_count: usize,
    pub(super) has_sort_state: bool,
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
            || !crate::xlsx::relationship_type_matches(&matches[0].rel_type, "table")
        {
            return Err(Error::Zip("table relationship is missing or ambiguous"));
        }
        let path = Package::try_resolve_rel_target(worksheet_path, &matches[0].target)
            .ok_or(Error::Zip("table relationship target URI is invalid"))?;
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

pub(super) fn inspect_table_part(tree: &XmlTree) -> Result<TablePartPlan> {
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
