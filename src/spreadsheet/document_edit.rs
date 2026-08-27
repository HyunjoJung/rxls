//! Document properties and workbook-global defined-name mutations.

use crate::write::xml::{esc_attr, esc_text};
use crate::xmltree::{NodeId, XmlTree};
use crate::{DocProperties, Error, Result};

use super::{
    newly_touched, peek_part_tree, remember_edited_part, validate_xml_value, workbook_path,
    Spreadsheet,
};

impl Spreadsheet {
    /// Set workbook document properties in the retained OOXML package.
    ///
    /// Core properties are edited in place in `docProps/core.xml` (only the
    /// `Some` fields are written -- the rest are left as-is if present, or
    /// removed if `None` and previously present); the extended company
    /// property is updated the same way in `docProps/app.xml` when that part
    /// exists, and only actually touched if the company value changes. The
    /// parsed [`crate::Workbook`] view is intentionally not mutated.
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

    /// Set or replace a workbook-global defined name in `xl/workbook.xml`
    /// atomically.
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
        let name = name.to_string();
        let refers_to = refers_to.to_string();
        self.mutate_atomic(move |candidate| candidate.set_defined_name_in_place(&name, &refers_to))
    }

    fn set_defined_name_in_place(&mut self, name: &str, refers_to: &str) -> Result<()> {
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
}

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
