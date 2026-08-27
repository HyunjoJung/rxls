//! `.xlsx` (SpreadsheetML / OOXML) reading.
//!
//! A `.xlsx` is a ZIP of XML parts: a workbook part (usually
//! `xl/workbook.xml`, but discoverable through `_rels/.rels`), workbook
//! relationships (relationship-id → worksheet path), shared strings, styles
//! (number formats, for date detection), and worksheet parts.
//!
//! The number-format classification and serial-date arithmetic are shared with
//! the `.xls` path ([`crate::format`]), so dates/percentages render identically.

mod chart;
mod comments;
mod drawing;
mod refs;
mod relationships;
mod style;
mod tables;
mod theme;
mod workbook;
mod worksheet;

use std::collections::{BTreeSet, HashMap};
use std::io::Read;

use quick_xml::events::{BytesRef, Event};
use quick_xml::{Reader, XmlVersion};

#[cfg(test)]
use chart::parse_chart_rgb;
pub(crate) use chart::ChartImportBudget;
#[cfg(feature = "xlsb")]
pub(crate) use chart::{
    parse_chart_with_theme, MAX_XLSX_CHART_XML_BYTES, XLSX_CHART_XML_SCAN_PASSES,
};
use comments::{comments_target, parse_comments};
use drawing::{add_drawing_loss, read_sheet_drawings};
#[cfg(test)]
use refs::{parse_range, parse_ref, shift_formula};

#[cfg(feature = "xlsb")]
pub(crate) use relationships::internal_relationship_target_by_id;
#[cfg(feature = "xlsb")]
pub(crate) use relationships::OoxmlRelationship;
pub(crate) use relationships::{
    parse_ooxml_relationships, parse_ooxml_relationships_preserving_extensions,
    relationship_type_matches, resolve_internal_relationship_part,
    unique_internal_relationship_target, RelationshipTarget,
};
use relationships::{
    OOXML_RELATIONSHIPS_NAMESPACE_STRICT, OOXML_RELATIONSHIPS_NAMESPACE_TRANSITIONAL,
};
use style::parse_styles;
#[cfg(test)]
use style::{
    built_in_table_header_style, built_in_table_style, parse_font_table, parse_table_styles,
    retain_xlsx_style_record, verified_xlsx_normal_font_size, DifferentialStyle, Styles,
    MAX_XLSX_FORMAT_CODE_BYTES, MAX_XLSX_INDEXED_COLORS, MAX_XLSX_STYLE_RECORDS,
};
use tables::{parse_table, resolve_table_styles, table_targets, ResolvedTables};
#[cfg(feature = "xlsb")]
pub(crate) use theme::chart_theme;
#[cfg(any(feature = "xlsb", test))]
pub(crate) use theme::CALC_IMPORTED_CHART_LATIN_FONT_FAMILY;
#[cfg(test)]
use theme::MAX_IMPORTED_CHART_LATIN_FONT_FAMILY_BYTES;
pub(crate) use theme::{bounded_imported_chart_latin_font_family, parse_theme, ThemeColors};
use theme::{color_attr, theme_color_slot};
#[cfg(test)]
use workbook::parse_repeat_rows;
#[cfg(test)]
use workbook::SheetDefinedName;
use workbook::{apply_sheet_defined_names, parse_workbook, ParsedWorkbook, SheetRef, Visibility};
use worksheet::{parse_sheet, ParsedSheet};
#[cfg(test)]
use worksheet::{retained_cell_cost, RETAINED_CELL_RECORD_BYTES};

use crate::error::{Error, Result};
#[cfg(test)]
use crate::model::{CellStyleOverlay, ImportedAxisMeasure, TableStyleRegion};
use crate::model::{OoxmlImplicitColumnWidth, OoxmlImplicitRowHeight};
#[cfg(test)]
use crate::{Alignment, BorderStyle, HAlign};
#[cfg(test)]
use crate::{
    Cell, CellStyle, DvKind, DvOp, PrintLossKind, PrintPageOrder, StyleLoss, StyleLossKind,
};
use crate::{Color, DocProperties, FormatScript, Sheet, SheetType, StyleFidelity, Workbook};

/// Detect the ZIP/OOXML magic (`PK\x03\x04`).
pub(crate) fn is_xlsx(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

pub(crate) fn open(bytes: &[u8]) -> Result<Workbook> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|_| Error::Zip("not a valid spreadsheet ZIP container"))?;
    crate::ziputil::validate_compression(&mut zip)?;

    let discovered_workbook_path = office_document_path(&mut zip)?;
    let workbook_path = discovered_workbook_path
        .clone()
        .unwrap_or_else(|| "xl/workbook.xml".to_string());
    let workbook_xml = part(&mut zip, &workbook_path).ok_or(Error::MissingWorkbook)?;
    let workbook_rels_xml = part(&mut zip, &sheet_rels_path(&workbook_path)).unwrap_or_default();
    let workbook_relationships = parse_ooxml_relationships(&workbook_rels_xml);
    let shared_xml = match workbook_related_part(
        &mut zip,
        &workbook_path,
        &workbook_rels_xml,
        "sharedStrings",
    ) {
        RelatedPartRead::Present(xml) => xml,
        RelatedPartRead::MissingRelationship => part(
            &mut zip,
            &normalize_part_target(&workbook_path, "sharedStrings.xml"),
        )
        .unwrap_or_default(),
        RelatedPartRead::Invalid => {
            return Err(Error::Zip("invalid shared-strings relationship"));
        }
    };
    let theme = match workbook_related_part(&mut zip, &workbook_path, &workbook_rels_xml, "theme") {
        RelatedPartRead::Present(xml) => parse_theme(&xml),
        RelatedPartRead::MissingRelationship => part(
            &mut zip,
            &normalize_part_target(&workbook_path, "theme/theme1.xml"),
        )
        .map(|xml| parse_theme(&xml))
        .unwrap_or_default(),
        RelatedPartRead::Invalid => ThemeColors {
            source_valid: false,
            ..ThemeColors::default()
        },
    };
    let styles = match workbook_related_part(&mut zip, &workbook_path, &workbook_rels_xml, "styles")
    {
        RelatedPartRead::Present(xml) => parse_styles(&xml, &theme),
        RelatedPartRead::MissingRelationship => part(
            &mut zip,
            &normalize_part_target(&workbook_path, "styles.xml"),
        )
        .map(|xml| parse_styles(&xml, &theme))
        .unwrap_or_default(),
        RelatedPartRead::Invalid => return Err(Error::Zip("invalid styles relationship")),
    };
    let shared = parse_shared_strings(&shared_xml, &theme, &styles.indexed_colors);
    let parsed = parse_workbook(&workbook_xml);
    let ParsedWorkbook {
        sheets: sheet_refs,
        date1904,
        structure_protected,
        active_sheet,
        defined_names,
        local_defined_names,
        sheet_defined_names,
    } = parsed;
    let properties = parse_doc_properties(
        part(&mut zip, "docProps/core.xml").as_deref(),
        part(&mut zip, "docProps/app.xml").as_deref(),
    );

    // Per-workbook text budget (shared across sheets) — see MAX_TEXT_BYTES.
    let mut budget = crate::MAX_TEXT_BYTES;
    let mut chart_budget = ChartImportBudget::default();
    let mut sheets = Vec::with_capacity(sheet_refs.len().min(1 << 16));
    let mut tab_selected_sheet = None;
    for (
        sheet_idx,
        SheetRef {
            name,
            rid,
            visibility,
        },
    ) in sheet_refs.into_iter().enumerate()
    {
        let relationship = workbook_relationships.as_deref().and_then(|relationships| {
            relationships
                .iter()
                .find(|relationship| relationship.id == rid)
        });
        let path = relationship
            .filter(|relationship| !relationship.external)
            .and_then(|relationship| {
                resolve_internal_relationship_part(&workbook_path, &relationship.target)
            })
            .unwrap_or_default();
        let sheet_type = match relationship.filter(|relationship| {
            !relationship.external
                && resolve_internal_relationship_part(&workbook_path, &relationship.target)
                    .is_some()
        }) {
            None => SheetType::Vba,
            Some(relationship) => match relationship.rel_type.as_deref() {
                // Missing Type remains a compatibility concession, but only
                // for a real, internal, resolvable relationship.
                None => SheetType::WorkSheet,
                Some(rel_type) if relationship_type_matches(rel_type, "worksheet") => {
                    SheetType::WorkSheet
                }
                Some(rel_type) if relationship_type_matches(rel_type, "chartsheet") => {
                    SheetType::ChartSheet
                }
                Some(rel_type) if relationship_type_matches(rel_type, "dialogsheet") => {
                    SheetType::DialogSheet
                }
                Some(rel_type)
                    if relationship_type_matches(rel_type, "macrosheet")
                        || matches!(
                            rel_type,
                            "http://schemas.microsoft.com/office/2006/relationships/xlMacrosheet"
                                | "http://schemas.microsoft.com/office/2006/relationships/xlIntlMacrosheet"
                        ) =>
                {
                    SheetType::MacroSheet
                }
                Some(_) => SheetType::Vba,
            },
        };
        let is_worksheet = sheet_type == SheetType::WorkSheet;
        let parsed_sheet = if is_worksheet {
            part_raw(&mut zip, &path)
                .map(|s| parse_sheet(&s, &shared, &styles, &theme, date1904, &mut budget))
                .unwrap_or_default()
        } else {
            ParsedSheet::default()
        };
        let ParsedSheet {
            cells,
            direct_cell_formats,
            rich,
            merges,
            hyperlink_refs,
            freeze,
            mut autofilter,
            data_validations,
            cond_formats,
            cond_format_metadata,
            mut page_setup,
            mut print_metadata,
            sparklines,
            tab_color,
            print_gridlines,
            print_headings,
            row_outline,
            col_outline,
            col_widths,
            row_heights,
            automatic_row_height_candidates,
            imported_column_axis_measures,
            imported_row_axis_measures,
            col_formats,
            row_formats,
            hidden_cols,
            hidden_rows,
            default_rows_hidden,
            explicit_visible_rows,
            default_row_height,
            automatic_default_row_height_candidate,
            default_col_width,
            imported_default_row_axis_measure,
            imported_default_column_axis_measure,
            base_col_width,
            defaulted_base_col_width,
            collapsed_rows,
            outline_summary_below,
            outline_summary_right,
            protect,
            protect_options,
            hide_gridlines,
            zoom,
            show_headers,
            right_to_left,
            tab_selected,
        } = parsed_sheet;
        let ooxml_implicit_col_width = if !is_worksheet || default_col_width.is_some() {
            OoxmlImplicitColumnWidth::None
        } else if let Some(chars) = base_col_width {
            OoxmlImplicitColumnWidth::BaseCharacters(chars)
        } else {
            OoxmlImplicitColumnWidth::ApplicationDefault
        };
        let ooxml_implicit_row_height = if is_worksheet && default_row_height.is_none() {
            OoxmlImplicitRowHeight::XlsxApplicationDefault
        } else {
            OoxmlImplicitRowHeight::None
        };
        let default_hidden_row_exceptions =
            (is_worksheet && default_rows_hidden).then_some(explicit_visible_rows);
        if tab_selected && tab_selected_sheet.is_none() {
            tab_selected_sheet = Some(sheet_idx);
        }
        // Resolve each `<hyperlink ref r:id>` through the worksheet's own rels
        // (`xl/worksheets/_rels/sheetN.xml.rels`), where the relationship `Target`
        // is the external URL.
        // The worksheet rels (`xl/worksheets/_rels/sheetN.xml.rels`) carry both the
        // hyperlink URLs (resolved by `r:id`) and the `comments{N}.xml` target
        // (resolved by relationship Type). Read the part once and reuse it.
        let sheet_rels_xml = is_worksheet
            .then(|| part(&mut zip, &sheet_rels_path(&path)))
            .flatten();
        let sheet_relationships = sheet_rels_xml
            .as_deref()
            .and_then(parse_ooxml_relationships);
        let read_hyperlinks = if hyperlink_refs.is_empty() {
            Vec::new()
        } else {
            hyperlink_refs
                .into_iter()
                .filter_map(|(row, col, rid)| {
                    sheet_relationships
                        .as_deref()
                        .and_then(|relationships| {
                            relationships.iter().find(|relationship| {
                                relationship.id == rid
                                    && relationship.rel_type.as_deref().is_some_and(|rel_type| {
                                        relationship_type_matches(rel_type, "hyperlink")
                                    })
                            })
                        })
                        .map(|relationship| (row, col, relationship.target.clone()))
                })
                .collect()
        };
        // Resolve a `comments{N}.xml` part (relationship Type `.../comments`) and
        // parse it into the authoring `comments` storage (round-trip friendly).
        let comments = sheet_rels_xml
            .as_deref()
            .and_then(comments_target)
            .map(|target| normalize_part_target(&path, &target))
            .and_then(|p| part(&mut zip, &p))
            .map(|s| parse_comments(&s))
            .unwrap_or_default();
        // Resolve every `table{N}.xml` part (relationship Type `.../table`) and
        // parse each into the authoring `tables` storage (round-trip friendly).
        let parsed_tables = sheet_rels_xml
            .as_deref()
            .map(table_targets)
            .unwrap_or_default()
            .into_iter()
            .map(|target| normalize_part_target(&path, &target))
            .filter_map(|p| part(&mut zip, &p))
            .filter_map(|s| parse_table(&s))
            .collect();
        let ResolvedTables {
            tables,
            table_header_formats,
            table_region_formats,
            style_losses: table_style_losses,
        } = resolve_table_styles(parsed_tables, &styles, &theme);
        let (images, charts, drawing_metadata, mut drawing_losses) = read_sheet_drawings(
            &mut zip,
            &path,
            sheet_rels_xml.as_deref(),
            &theme,
            &mut chart_budget,
        );
        for loss in table_style_losses {
            add_drawing_loss(&mut drawing_losses, loss.kind, loss.occurrences);
        }
        apply_sheet_defined_names(
            &mut page_setup,
            &mut print_metadata,
            &mut autofilter,
            sheet_defined_names
                .iter()
                .filter(|name| name.local_sheet_id == sheet_idx),
        );
        sheets.push(Sheet {
            name,
            is_worksheet,
            style_fidelity: StyleFidelity::Partial,
            sheet_type: Some(sheet_type),
            cells,
            rich,
            read_merges: merges,
            read_hyperlinks,
            comments,
            tables,
            table_header_formats,
            table_region_formats,
            direct_cell_formats,
            images,
            charts,
            drawing_metadata,
            style_losses: drawing_losses,
            freeze,
            autofilter,
            page_setup,
            print_metadata,
            data_validations,
            cond_formats,
            cond_format_metadata,
            sparklines,
            tab_color,
            print_gridlines,
            print_headings,
            row_outline,
            col_outline,
            col_widths,
            row_heights,
            automatic_row_height_candidates,
            imported_column_axis_measures,
            imported_default_column_axis_measure,
            imported_row_axis_measures,
            imported_default_row_axis_measure,
            col_formats,
            row_formats,
            default_format: styles.cell_styles.first().cloned(),
            hidden_cols,
            hidden_rows,
            default_hidden_row_exceptions,
            default_row_height,
            automatic_default_row_height_candidate,
            default_col_width,
            ooxml_implicit_col_width,
            ooxml_defaulted_base_col_width: defaulted_base_col_width,
            ooxml_implicit_row_height,
            xlsx_normal_font_size_pt: is_worksheet
                .then_some(styles.xlsx_normal_font_size_pt)
                .flatten(),
            collapsed_rows,
            outline_summary_below: outline_summary_below.unwrap_or(true),
            outline_summary_right: outline_summary_right.unwrap_or(true),
            protect,
            protect_options,
            hide_gridlines,
            zoom,
            show_headers,
            right_to_left,
            hidden: visibility == Visibility::Hidden,
            very_hidden: visibility == Visibility::VeryHidden,
            ..Default::default()
        });
    }
    Ok(Workbook {
        sheets,
        date1904,
        protect_structure: structure_protected,
        active_sheet: active_sheet.or(tab_selected_sheet).unwrap_or_default(),
        text_truncated: budget == 0,
        container_parse_mode: crate::ContainerParseMode::Primary,
        properties,
        defined_names,
        local_defined_names,
        ..Default::default()
    })
}

/// Read a ZIP entry to a UTF-8 string, if present. Capped to guard against a
/// zip bomb (a tiny entry that decompresses to gigabytes).
fn part(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Option<String> {
    let text = part_raw(zip, name)?;
    crate::xml_reference_work_within_budget(&text).then_some(text)
}

fn part_raw(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Option<String> {
    const MAX_PART: u64 = 256 << 20; // 256 MiB per entry
    let idx = part_index(zip, name)?;
    let f = zip.by_index(idx).ok()?;
    let mut s = String::new();
    f.take(MAX_PART).read_to_string(&mut s).ok()?;
    Some(s)
}

fn part_index(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Option<usize> {
    if let Some(idx) = zip.index_for_name(name) {
        return Some(idx);
    }
    let wanted = canonical_part_name(name);
    if let Some(idx) = unique_part_index(zip, |candidate| canonical_part_name(candidate) == wanted)
    {
        return Some(idx);
    }

    // Real-world producers sometimes vary ASCII case despite OPC part names
    // being case-sensitive. Accept that extension only when it is unambiguous.
    unique_part_index(zip, |candidate| {
        canonical_part_name(candidate).eq_ignore_ascii_case(&wanted)
    })
}

fn unique_part_index(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    mut matches: impl FnMut(&str) -> bool,
) -> Option<usize> {
    let mut found = None;
    for idx in 0..zip.len() {
        let Ok(file) = zip.by_index(idx) else {
            continue;
        };
        if matches(file.name()) {
            if found.is_some() {
                return None;
            }
            found = Some(idx);
        }
    }
    found
}

fn canonical_part_name(name: &str) -> String {
    name.replace('\\', "/").trim_start_matches('/').to_string()
}

fn office_document_path(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
) -> Result<Option<String>> {
    let Some(root_rels) = part(zip, "_rels/.rels") else {
        return Ok(None);
    };
    match unique_internal_relationship_target(&root_rels, "officeDocument") {
        RelationshipTarget::Missing => Ok(None),
        RelationshipTarget::Internal(target) => resolve_internal_relationship_part("", &target)
            .map(Some)
            .ok_or(Error::Zip("invalid office-document relationship target")),
        RelationshipTarget::Invalid => Err(Error::Zip("invalid office-document relationship")),
    }
}

enum RelatedPartRead {
    MissingRelationship,
    Present(String),
    Invalid,
}

fn workbook_related_part(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    workbook_path: &str,
    workbook_rels_xml: &str,
    rel_kind: &str,
) -> RelatedPartRead {
    match unique_internal_relationship_target(workbook_rels_xml, rel_kind) {
        RelationshipTarget::Missing => RelatedPartRead::MissingRelationship,
        RelationshipTarget::Internal(target) => {
            let Some(path) = resolve_internal_relationship_part(workbook_path, &target) else {
                return RelatedPartRead::Invalid;
            };
            part(zip, &path).map_or(RelatedPartRead::Invalid, RelatedPartRead::Present)
        }
        RelationshipTarget::Invalid => RelatedPartRead::Invalid,
    }
}

/// The rels path for a source part: `xl/worksheets/sheet1.xml` →
/// `xl/worksheets/_rels/sheet1.xml.rels`. Splits at the final `/` and inserts
/// the `_rels/` segment before the file name.
fn sheet_rels_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    match path.rfind('/') {
        Some(i) => format!("{}/_rels/{}.rels", &path[..i], &path[i + 1..]),
        None => format!("_rels/{path}.rels"),
    }
}

const MAX_XLSX_COLUMN_INDEX: u16 = 16_383;
const MAX_XLSX_ROW_INDEX: u32 = 1_048_575;
fn local(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

const OOXML_CHART_NAMESPACE_TRANSITIONAL: &str =
    "http://schemas.openxmlformats.org/drawingml/2006/chart";
const OOXML_CHART_NAMESPACE_STRICT: &str = "http://purl.oclc.org/ooxml/drawingml/chart";
const OOXML_DRAWING_NAMESPACE_TRANSITIONAL: &str =
    "http://schemas.openxmlformats.org/drawingml/2006/main";
const OOXML_DRAWING_NAMESPACE_STRICT: &str = "http://purl.oclc.org/ooxml/drawingml/main";
const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

fn chart_element_namespace_is_supported(namespace: &str) -> bool {
    matches!(
        namespace,
        OOXML_CHART_NAMESPACE_TRANSITIONAL
            | OOXML_CHART_NAMESPACE_STRICT
            | OOXML_DRAWING_NAMESPACE_TRANSITIONAL
            | OOXML_DRAWING_NAMESPACE_STRICT
    )
}

fn drawing_element_namespace_is_supported(namespace: &str) -> bool {
    matches!(
        namespace,
        OOXML_DRAWING_NAMESPACE_TRANSITIONAL | OOXML_DRAWING_NAMESPACE_STRICT
    )
}

fn qualified_prefix(name: &[u8]) -> Option<&[u8]> {
    name.iter()
        .position(|byte| *byte == b':')
        .map(|index| &name[..index])
}

/// Validate namespace scoping before any local-name chart pass runs. Extension
/// namespaces and Markup Compatibility branches fail closed because selecting
/// `mc:Choice` versus `mc:Fallback` requires feature negotiation; traversing
/// both would combine mutually exclusive chart semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkupNamespacePolicy {
    Chart,
    DrawingOnly,
}

fn markup_is_supported(xml: &str, policy: MarkupNamespacePolicy) -> bool {
    fn decoded_attribute_value(
        element: &quick_xml::events::BytesStart<'_>,
        attribute: &quick_xml::events::attributes::Attribute<'_>,
    ) -> Option<String> {
        attribute
            .decoded_and_normalized_value_with(
                XmlVersion::Implicit1_0,
                element.decoder(),
                1,
                quick_xml::escape::resolve_xml_entity,
            )
            .ok()
            .map(|value| value.into_owned())
    }

    fn inspect_element(
        element: &quick_xml::events::BytesStart<'_>,
        namespaces: &mut HashMap<Vec<u8>, String>,
        policy: MarkupNamespacePolicy,
    ) -> Option<Vec<(Vec<u8>, Option<String>)>> {
        let mut changes = Vec::new();
        let mut declared = BTreeSet::new();
        let attributes = element
            .attributes()
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()?;
        for attribute in &attributes {
            let name = attribute.key.as_ref();
            let prefix = if name == b"xmlns" {
                Some(Vec::new())
            } else {
                name.strip_prefix(b"xmlns:").map(<[u8]>::to_vec)
            };
            let Some(prefix) = prefix else {
                continue;
            };
            if !declared.insert(prefix.clone()) {
                return None;
            }
            let value = decoded_attribute_value(element, attribute)?;
            let previous = namespaces.insert(prefix.clone(), value);
            changes.push((prefix, previous));
        }

        let element_name = element.name();
        let element_name = element_name.as_ref();
        let element_prefix = qualified_prefix(element_name).unwrap_or_default();
        let element_namespace = namespaces.get(element_prefix).map(String::as_str);
        match policy {
            MarkupNamespacePolicy::Chart => match element_namespace {
                Some(namespace) if chart_element_namespace_is_supported(namespace) => {}
                None if element_prefix.is_empty() => {}
                _ => return None,
            },
            MarkupNamespacePolicy::DrawingOnly => match element_namespace {
                Some(namespace) if drawing_element_namespace_is_supported(namespace) => {}
                _ => return None,
            },
        }
        if matches!(
            local(element_name),
            b"AlternateContent" | b"Choice" | b"Fallback"
        ) {
            return None;
        }

        for attribute in &attributes {
            let name = attribute.key.as_ref();
            if name == b"xmlns" || name.starts_with(b"xmlns:") {
                continue;
            }
            let Some(prefix) = qualified_prefix(name) else {
                continue;
            };
            let namespace = namespaces.get(prefix)?;
            let attribute_local_name = local(name);
            let supported = (matches!(
                namespace.as_str(),
                OOXML_RELATIONSHIPS_NAMESPACE_TRANSITIONAL | OOXML_RELATIONSHIPS_NAMESPACE_STRICT
            ) && attribute_local_name == b"id")
                || (namespace == XML_NAMESPACE && attribute_local_name == b"space");
            if !supported {
                return None;
            }
        }
        Some(changes)
    }

    fn restore_namespaces(
        namespaces: &mut HashMap<Vec<u8>, String>,
        changes: Vec<(Vec<u8>, Option<String>)>,
    ) {
        for (prefix, previous) in changes.into_iter().rev() {
            if let Some(previous) = previous {
                namespaces.insert(prefix, previous);
            } else {
                namespaces.remove(&prefix);
            }
        }
    }

    let mut reader = Reader::from_str(xml);
    let mut namespaces = HashMap::new();
    let mut scopes = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let Some(changes) = inspect_element(&element, &mut namespaces, policy) else {
                    return false;
                };
                scopes.push(changes);
            }
            Ok(Event::Empty(element)) => {
                let Some(changes) = inspect_element(&element, &mut namespaces, policy) else {
                    return false;
                };
                restore_namespaces(&mut namespaces, changes);
            }
            Ok(Event::End(_)) => {
                let Some(changes) = scopes.pop() else {
                    return false;
                };
                restore_namespaces(&mut namespaces, changes);
            }
            Ok(Event::Eof) => return scopes.is_empty(),
            Err(_) => return false,
            _ => {}
        }
    }
}

fn chart_markup_is_supported(xml: &str) -> bool {
    markup_is_supported(xml, MarkupNamespacePolicy::Chart)
}

fn theme_markup_is_supported(xml: &str) -> bool {
    markup_is_supported(xml, MarkupNamespacePolicy::DrawingOnly)
}

fn attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if local(a.key.as_ref()) == key {
            a.decoded_and_normalized_value_with(
                XmlVersion::Implicit1_0,
                e.decoder(),
                1,
                quick_xml::escape::resolve_xml_entity,
            )
            .ok()
            .map(|v| v.into_owned())
        } else {
            None
        }
    })
}

fn unique_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
) -> std::result::Result<Option<String>, ()> {
    let mut value = None;
    for attribute in e.attributes() {
        let attribute = attribute.map_err(|_| ())?;
        if local(attribute.key.as_ref()) != key {
            continue;
        }
        if value.is_some() {
            return Err(());
        }
        value = Some(
            attribute
                .decoded_and_normalized_value_with(
                    XmlVersion::Implicit1_0,
                    e.decoder(),
                    1,
                    quick_xml::escape::resolve_xml_entity,
                )
                .map_err(|_| ())?
                .into_owned(),
        );
    }
    Ok(value)
}

fn unique_parsed_attr<T: std::str::FromStr>(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
) -> std::result::Result<Option<T>, ()> {
    unique_attr(e, key)?
        .map(|value| value.parse::<T>().map_err(|_| ()))
        .transpose()
}

fn text_of(e: &quick_xml::events::BytesText<'_>) -> String {
    e.decode().map(|c| c.into_owned()).unwrap_or_default()
}

fn with_general_ref_text(reference: &BytesRef<'_>, mut append: impl FnMut(&str)) {
    match reference.resolve_char_ref() {
        Ok(Some(ch)) if is_xml_10_char(ch) => {
            let mut encoded = [0u8; 4];
            append(ch.encode_utf8(&mut encoded));
        }
        Ok(None) => {
            if let Ok(name) = reference.decode() {
                if let Some(value) = quick_xml::escape::resolve_xml_entity(&name) {
                    append(value);
                    return;
                }
            }
            append_raw_general_ref(reference, append);
        }
        Ok(Some(_)) | Err(_) => append_raw_general_ref(reference, append),
    }
}

fn append_raw_general_ref(reference: &BytesRef<'_>, mut append: impl FnMut(&str)) {
    if let Ok(raw) = std::str::from_utf8(reference.as_ref()) {
        append("&");
        append(raw);
        append(";");
    }
}

fn append_general_ref(out: &mut String, reference: &BytesRef<'_>) {
    with_general_ref_text(reference, |value| out.push_str(value));
}

fn is_xml_10_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}'
    ) || ('\u{10000}'..='\u{10FFFF}').contains(&ch)
}

fn assign_doc_property(props: &mut DocProperties, tag: &[u8], value: String) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    let value = value.to_string();
    match tag {
        b"title" => props.title = Some(value),
        b"subject" => props.subject = Some(value),
        b"creator" => props.creator = Some(value),
        b"keywords" => props.keywords = Some(value),
        b"description" => props.description = Some(value),
        b"lastModifiedBy" => props.last_modified_by = Some(value),
        b"created" => props.created = Some(value),
        b"modified" if props.created.is_none() => props.created = Some(value),
        b"Company" => props.company = Some(value),
        _ => {}
    }
}

pub(crate) fn parse_doc_properties(core_xml: Option<&str>, app_xml: Option<&str>) -> DocProperties {
    let mut props = DocProperties::default();
    for xml in [core_xml, app_xml].into_iter().flatten() {
        let mut r = Reader::from_str(xml);
        let mut current: Option<Vec<u8>> = None;
        let mut text = String::new();
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) => {
                    current = Some(local(e.name().as_ref()).to_vec());
                    text.clear();
                }
                Ok(Event::Text(t)) if current.is_some() => text.push_str(&text_of(&t)),
                Ok(Event::GeneralRef(reference)) if current.is_some() => {
                    append_general_ref(&mut text, &reference);
                }
                Ok(Event::End(e)) => {
                    if let Some(tag) = current.take() {
                        if tag.as_slice() == local(e.name().as_ref()) {
                            assign_doc_property(&mut props, &tag, std::mem::take(&mut text));
                        }
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
    }
    props
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SharedString {
    text: String,
    runs: Vec<crate::TextRun>,
}

/// `<sst><si>…<t>text</t>…</si>` — concatenate `<t>` runs within each `<si>`,
/// but skip `<rPh>` (East Asian phonetic / ruby guide) text, which is not part of
/// the displayed string.
fn parse_shared_strings(xml: &str, theme: &ThemeColors, indexed: &[Color]) -> Vec<SharedString> {
    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur = SharedString::default();
    let mut run: Option<crate::TextRun> = None;
    let mut in_si = false;
    let mut in_t = false;
    let mut in_rph = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"si" => {
                    in_si = true;
                    cur = SharedString::default();
                }
                b"r" if in_si && !in_rph => run = Some(crate::TextRun::default()),
                b"rPh" => in_rph = true,
                b"t" => in_t = true,
                b"rFont" if run.is_some() => {
                    run.as_mut().expect("run").font.name = attr(&e, b"val");
                }
                b"sz" if run.is_some() => {
                    run.as_mut().expect("run").font.size_pt = attr(&e, b"val")
                        .and_then(|value| value.parse::<f32>().ok())
                        .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                }
                b"color" if run.is_some() => {
                    run.as_mut().expect("run").font.color = color_attr(&e, theme, indexed);
                }
                b"b" if run.is_some() => run.as_mut().expect("run").font.bold = true,
                b"i" if run.is_some() => run.as_mut().expect("run").font.italic = true,
                b"u" if run.is_some() => run.as_mut().expect("run").font.underline = true,
                b"strike" if run.is_some() => {
                    run.as_mut().expect("run").font.strikethrough = true;
                }
                b"vertAlign" if run.is_some() => {
                    run.as_mut().expect("run").font.script = match attr(&e, b"val").as_deref() {
                        Some("superscript") => FormatScript::Superscript,
                        Some("subscript") => FormatScript::Subscript,
                        _ => FormatScript::None,
                    };
                }
                _ => {}
            },
            // A self-closing `<si/>` is an empty string — it must still occupy an
            // index slot, or every later shared-string reference shifts.
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"si" => {
                out.push(SharedString::default());
            }
            Ok(Event::Empty(e)) if in_si && run.is_some() => {
                let font = &mut run.as_mut().expect("run").font;
                match local(e.name().as_ref()) {
                    b"rFont" => font.name = attr(&e, b"val"),
                    b"sz" => {
                        font.size_pt = attr(&e, b"val")
                            .and_then(|value| value.parse::<f32>().ok())
                            .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                    }
                    b"color" => font.color = color_attr(&e, theme, indexed),
                    b"b" => font.bold = true,
                    b"i" => font.italic = true,
                    b"u" => font.underline = true,
                    b"strike" => font.strikethrough = true,
                    b"vertAlign" => {
                        font.script = match attr(&e, b"val").as_deref() {
                            Some("superscript") => FormatScript::Superscript,
                            Some("subscript") => FormatScript::Subscript,
                            _ => FormatScript::None,
                        };
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"si" => {
                    in_si = false;
                    out.push(std::mem::take(&mut cur));
                }
                b"r" if run.is_some() => {
                    let completed = run.take().expect("run");
                    if !completed.text.is_empty() {
                        cur.runs.push(completed);
                    }
                }
                b"rPh" => in_rph = false,
                b"t" => in_t = false,
                _ => {}
            },
            Ok(Event::Text(t)) if in_si && in_t && !in_rph => {
                let text = text_of(&t);
                cur.text.push_str(&text);
                if let Some(run) = run.as_mut() {
                    run.text.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(reference)) if in_si && in_t && !in_rph => {
                with_general_ref_text(&reference, |text| {
                    cur.text.push_str(text);
                    if let Some(run) = run.as_mut() {
                        run.text.push_str(text);
                    }
                });
            }
            Ok(Event::CData(t)) if in_si && in_t && !in_rph => {
                let bytes = t.into_inner();
                let text = String::from_utf8_lossy(bytes.as_ref());
                cur.text.push_str(&text);
                if let Some(run) = run.as_mut() {
                    run.text.push_str(&text);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Read a ZIP entry to a UTF-8 string (shared with the `.xlsb` reader).
#[cfg(feature = "xlsb")]
pub(crate) fn part_str(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
) -> Option<String> {
    part(zip, name)
}

/// `xl/_rels/workbook.xml.rels`: `<Relationship Id Target/>`. Shared with `.xlsb`.
#[cfg(test)]
pub(crate) fn parse_rels(xml: &str) -> HashMap<String, String> {
    parse_ooxml_relationships(xml)
        .unwrap_or_default()
        .into_iter()
        .map(|relationship| (relationship.id, relationship.target))
        .collect()
}

/// Resolve a rels `Target` (relative to the source part's directory) to a ZIP
/// part path. A leading `/` is workbook-root-absolute; otherwise the target is
/// relative to the directory of `base` (the worksheet path), resolving any
/// leading `../` segments. Excess parent segments clamp at the package root
/// under RFC 3986 section 5.2.4. E.g. base `xl/worksheets/sheet1.xml` + target
/// `../comments1.xml` → `xl/comments1.xml`.
fn normalize_part_target(base: &str, target: &str) -> String {
    resolve_internal_relationship_part(base, target).unwrap_or_default()
}

fn attr_true(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}

fn attr_false(value: &str) -> bool {
    value == "0" || value.eq_ignore_ascii_case("false")
}

fn parse_bool_attr(value: &str) -> Option<bool> {
    if attr_true(value) {
        Some(true)
    } else if attr_false(value) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
