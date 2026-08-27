//! Worksheet lifecycle, visibility, selection, and reference-rewrite mutations.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::package::Package;
use crate::write::xml::{esc_attr, CT_WORKSHEET, NS_MAIN, NS_R, REL_WORKSHEET};
use crate::xmltree::{NodeId, XmlTree};
use crate::{Color, Error, Result, SheetVisible};

use super::{
    canonical_part_key, canonical_part_name, direct_elements_by_local_name, invalidate_calc_chain,
    local, newly_touched, peek_part_tree, remember_edited_part, workbook_path,
    workbook_sheet_index, worksheet_path, Spreadsheet,
};

impl Spreadsheet {
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
            package.add_relationship(&workbook_path, REL_WORKSHEET, &relationship_target, false)?;
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
            if crate::xlsx::relationship_type_matches(&matches[0].rel_type, "worksheet") {
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
        if relationship.external
            || !crate::xlsx::relationship_type_matches(&relationship.rel_type, "worksheet")
        {
            return Err(Error::MissingWorkbook);
        }
        let worksheet_path = Package::try_resolve_rel_target(&workbook_path, &relationship.target)
            .ok_or(Error::MissingWorkbook)?;
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
    /// Set a worksheet visibility state in `xl/workbook.xml` atomically.
    pub fn set_sheet_visibility(&mut self, sheet_name: &str, visible: SheetVisible) -> Result<()> {
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| {
            candidate.set_sheet_visibility_in_place(&sheet_name, visible)
        })
    }

    fn set_sheet_visibility_in_place(
        &mut self,
        sheet_name: &str,
        visible: SheetVisible,
    ) -> Result<()> {
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

    /// Set the active worksheet by name in `xl/workbook.xml` atomically.
    pub fn set_active_sheet(&mut self, sheet_name: &str) -> Result<()> {
        let sheet_name = sheet_name.to_string();
        self.mutate_atomic(move |candidate| candidate.set_active_sheet_in_place(&sheet_name))
    }

    fn set_active_sheet_in_place(&mut self, sheet_name: &str) -> Result<()> {
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

    /// Set worksheet tab color in the target worksheet XML part atomically.
    pub fn set_sheet_tab_color(&mut self, sheet_name: &str, color: impl Into<Color>) -> Result<()> {
        let sheet_name = sheet_name.to_string();
        let color = color.into();
        self.mutate_atomic(move |candidate| {
            candidate.set_sheet_tab_color_in_place(&sheet_name, color)
        })
    }

    fn set_sheet_tab_color_in_place(&mut self, sheet_name: &str, color: Color) -> Result<()> {
        self.ensure_editable()?;
        let package = self.package.as_mut().ok_or(Error::Zip(
            "spreadsheet is read-only for package-preserving edit",
        ))?;
        let path = worksheet_path(package, sheet_name)?;
        let before = package.touched_parts();
        let tree = package.part_tree_mut(&path)?;
        sml_set_tab_color(tree, color)?;
        for touched in newly_touched(&before, package) {
            remember_edited_part(&mut self.edited_parts, touched);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AppSheetTitleRepair {
    worksheet_count_node: NodeId,
    titles_vector: NodeId,
    title_node: NodeId,
    new_worksheet_count: usize,
    new_titles_size: usize,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SheetReferenceRewrite {
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
            let Some(target) = Package::try_resolve_rel_target(&source, &rel.target) else {
                continue;
            };
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
    [
        "worksheet",
        "chartsheet",
        "dialogsheet",
        "macrosheet",
        "drawing",
        "chart",
        "table",
        "pivotTable",
        "pivotCacheDefinition",
    ]
    .into_iter()
    .any(|kind| crate::xlsx::relationship_type_matches(rel_type, kind))
        || matches!(
            rel_type,
            "http://schemas.microsoft.com/office/2006/relationships/xlMacrosheet"
                | "http://schemas.microsoft.com/office/2006/relationships/xlIntlMacrosheet"
        )
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

fn standard_relationship_kind(rel_type: &str) -> Option<&'static str> {
    let microsoft_kind = match rel_type {
        "http://schemas.microsoft.com/office/2006/relationships/ctrlProp" => Some("ctrlprop"),
        "http://schemas.microsoft.com/office/2007/relationships/slicer" => Some("slicer"),
        "http://schemas.microsoft.com/office/2007/relationships/slicerCache" => Some("slicercache"),
        "http://schemas.microsoft.com/office/2011/relationships/chartStyle" => Some("chartstyle"),
        "http://schemas.microsoft.com/office/2011/relationships/chartColorStyle" => {
            Some("chartcolorstyle")
        }
        "http://schemas.microsoft.com/office/2011/relationships/timeline" => Some("timeline"),
        "http://schemas.microsoft.com/office/2011/relationships/timelineCache" => {
            Some("timelinecache")
        }
        "http://schemas.microsoft.com/office/2017/10/relationships/threadedComment" => {
            Some("threadedcomment")
        }
        _ => None,
    };
    if microsoft_kind.is_some() {
        return microsoft_kind;
    }
    [
        ("drawing", "drawing"),
        ("comments", "comments"),
        ("vmlDrawing", "vmldrawing"),
        ("table", "table"),
        ("printerSettings", "printersettings"),
        ("chart", "chart"),
        ("image", "image"),
        ("diagramData", "diagramdata"),
        ("diagramLayout", "diagramlayout"),
        ("diagramColors", "diagramcolors"),
        ("diagramQuickStyle", "diagramquickstyle"),
        ("chartStyle", "chartstyle"),
        ("chartColorStyle", "chartcolorstyle"),
        ("pivotTable", "pivottable"),
        ("pivotCacheDefinition", "pivotcachedefinition"),
        ("queryTable", "querytable"),
        ("oleObject", "oleobject"),
        ("control", "control"),
        ("ctrlProp", "ctrlprop"),
        ("threadedComment", "threadedcomment"),
        ("threadedComments", "threadedcomments"),
        ("slicer", "slicer"),
        ("slicerCache", "slicercache"),
        ("timeline", "timeline"),
        ("timelineCache", "timelinecache"),
        ("connections", "connections"),
        ("externalLink", "externallink"),
        ("hyperlink", "hyperlink"),
    ]
    .into_iter()
    .find_map(|(uri_kind, classification)| {
        crate::xlsx::relationship_type_matches(rel_type, uri_kind).then_some(classification)
    })
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
            let Some(kind) = standard_relationship_kind(&relationship.rel_type) else {
                continue;
            };
            let target = Package::try_resolve_rel_target(relationship_source, &relationship.target)
                .ok_or(Error::Zip(
                    "sheet dependency relationship target URI is invalid",
                ))?;
            if !package.has_part(&target) {
                return Err(Error::Zip(
                    "sheet dependency relationship target is missing",
                ));
            }
            if unsafe_sheet_dependency_kind(kind) {
                return Err(Error::Zip(
                    "worksheet has a structural dependency that cannot be repaired safely",
                ));
            }
            let owned = if source_key == worksheet_key {
                sheet_owned_relationship_kind(kind)
            } else {
                nested_owned_relationship_kind(kind)
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
                    let Some(relationship_target) =
                        Package::try_resolve_rel_target(source, &relationship.target)
                    else {
                        return false;
                    };
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

pub(super) fn collect_sheet_reference_rewrites(
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

pub(super) fn apply_sheet_reference_rewrites(
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

pub(super) fn rewrite_sheet_qualifiers(formula: &str, old_name: &str, new_name: &str) -> String {
    rewrite_sheet_qualifiers_impl(formula, old_name, Some(new_name))
}

pub(super) fn rewrite_deleted_sheet_qualifiers(formula: &str, deleted_name: &str) -> String {
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
