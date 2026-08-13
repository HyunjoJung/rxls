//! `.xlsx` (SpreadsheetML / OOXML) reading.
//!
//! A `.xlsx` is a ZIP of XML parts: a workbook part (usually
//! `xl/workbook.xml`, but discoverable through `_rels/.rels`), workbook
//! relationships (relationship-id → worksheet path), shared strings, styles
//! (number formats, for date detection), and worksheet parts.
//!
//! The number-format classification and serial-date arithmetic are shared with
//! the `.xls` path ([`crate::format`]), so dates/percentages render identically.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;

use quick_xml::events::{BytesRef, Event};
use quick_xml::{Reader, XmlVersion};

use crate::error::{Error, Result};
use crate::model::{
    parse_decimal_ratio_u64, CellStyleOverlay, ImportedAxisMeasure, OoxmlImplicitColumnWidth,
    OoxmlImplicitRowHeight, TableStyleApplication, TableStyleDefinition, TableStyleRegion,
};
use crate::{
    format, Alignment, Border, BorderStyle, Cell, CellEntry, CellProtection, CellStyle, CfRule,
    Chart, ChartBarDirection, ChartCachedPoint, ChartFrameFill, ChartFrameStyleLossKind, ChartKind,
    ChartMarkerSymbol, ChartSeriesCache, ChartSeriesStyle, ChartSeriesStyleLossKind,
    ChartTextStyle, ChartTextStyles, ChartUnsupportedReason, Color, Comment, CondFormat,
    ConditionalFormatMetadata, DataValidation, DocProperties, DrawingAnchorBehavior, DrawingCrop,
    DrawingMetadata, DrawingObjectKind, DvKind, DvOp, Fill, Font, FormatPattern, FormatScript,
    HAlign, HeaderFooterKind, Image, ImageFmt, PageSetup, PrintLossKind, PrintMetadata,
    PrintPageOrder, ProtectionOptions, Series, Sheet, SheetType, Sparkline, SparklineKind,
    StyleFidelity, StyleLoss, StyleLossKind, Table, VAlign, Workbook,
};

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
        let parsed_tables: Vec<ParsedTable> = sheet_rels_xml
            .as_deref()
            .map(table_targets)
            .unwrap_or_default()
            .into_iter()
            .map(|target| normalize_part_target(&path, &target))
            .filter_map(|p| part(&mut zip, &p))
            .filter_map(|s| parse_table(&s))
            .collect();
        let tables = parsed_tables
            .iter()
            .map(|parsed| parsed.table.clone())
            .collect::<Vec<_>>();
        let mut table_header_formats = BTreeMap::new();
        let mut table_region_formats = BTreeMap::new();
        let mut table_style_losses = styles.losses.clone();
        for parsed in parsed_tables {
            for loss in parsed.losses {
                add_drawing_loss(&mut table_style_losses, loss.kind, loss.occurrences);
            }
            let Some(style_name) = parsed.table.style.as_deref() else {
                continue;
            };
            let Some(table_style) = styles.table_style(style_name, &theme) else {
                add_drawing_loss(&mut table_style_losses, StyleLossKind::MissingReference, 1);
                continue;
            };
            for loss in table_style.losses {
                add_drawing_loss(&mut table_style_losses, loss.kind, loss.occurrences);
            }
            if let Some(header) = table_style
                .definition
                .get(TableStyleRegion::HeaderRow)
                .map(|element| element.style.clone())
            {
                table_header_formats.insert(parsed.table.name.clone(), header);
            }
            let mut application = parsed.application;
            application.definition = table_style.definition;
            table_region_formats.insert(parsed.table.name, application);
        }
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
    for idx in 0..zip.len() {
        let Ok(file) = zip.by_index(idx) else {
            continue;
        };
        if canonical_part_name(file.name()) == wanted {
            return Some(idx);
        }
    }
    None
}

fn canonical_part_name(name: &str) -> String {
    name.replace('\\', "/").trim_start_matches('/').to_string()
}

/// Resolve an OPC internal relationship target to a package part name.
///
/// Relationship targets are URI references, not raw ZIP paths. Package-part
/// lookup therefore uses only the URI path component (a query or fragment
/// identifies content within the resolved part), rejects absolute/network URI
/// references, and resolves dot segments against the source part's directory.
/// Backslashes remain accepted as a compatibility extension because package
/// lookup elsewhere in rxls already canonicalizes them.
pub(crate) fn resolve_internal_relationship_part(base: &str, target: &str) -> Option<String> {
    let base = base.replace('\\', "/");
    let target = target.replace('\\', "/");
    let path_end = target.find(['?', '#']).unwrap_or(target.len());
    let path = &target[..path_end];

    // RFC 3986 relative references cannot contain a scheme, and a network-path
    // reference (`//authority/path`) is likewise not an OPC package-part name.
    if path.starts_with("//") || has_uri_scheme(path) {
        return None;
    }

    // A fragment-only or query-only reference denotes the source part itself.
    if path.is_empty() {
        return (!base.is_empty()).then(|| canonical_part_name(&base));
    }

    let mut parts: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        base.rsplit_once('/')
            .map(|(dir, _)| {
                dir.split('/')
                    .filter(|segment| !segment.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(segment),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn has_uri_scheme(path: &str) -> bool {
    let Some(colon) = path.find(':') else {
        return false;
    };
    let candidate = &path[..colon];
    !candidate.is_empty()
        && candidate.as_bytes()[0].is_ascii_alphabetic()
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelationshipTarget {
    Missing,
    Internal(String),
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OoxmlRelationship {
    pub(crate) id: String,
    pub(crate) rel_type: Option<String>,
    pub(crate) target: String,
    pub(crate) external: bool,
}

pub(crate) fn relationship_type_matches(value: &str, rel_kind: &str) -> bool {
    [
        OOXML_RELATIONSHIPS_NAMESPACE_TRANSITIONAL,
        OOXML_RELATIONSHIPS_NAMESPACE_STRICT,
    ]
    .into_iter()
    .any(|namespace| {
        value
            .strip_prefix(namespace)
            .and_then(|suffix| suffix.strip_prefix('/'))
            == Some(rel_kind)
    })
}

struct RelationshipRootContext {
    qualified_name: Vec<u8>,
    namespace: Option<String>,
    namespaces: HashMap<Vec<u8>, String>,
}

fn relationship_root_context(
    element: &quick_xml::events::BytesStart<'_>,
    allow_extension_attributes: bool,
) -> Option<RelationshipRootContext> {
    if local(element.name().as_ref()) != b"Relationships" {
        return None;
    }
    let mut namespaces = HashMap::<Vec<u8>, String>::new();
    for attribute in element.attributes() {
        let attribute = attribute.ok()?;
        let qualified_name = attribute.key.as_ref();
        let prefix = if qualified_name == b"xmlns" {
            Vec::new()
        } else if let Some(prefix) = qualified_name.strip_prefix(b"xmlns:") {
            prefix.to_vec()
        } else if allow_extension_attributes {
            continue;
        } else {
            return None;
        };
        let value = attribute
            .decoded_and_normalized_value_with(
                XmlVersion::Implicit1_0,
                element.decoder(),
                1,
                quick_xml::escape::resolve_xml_entity,
            )
            .ok()?
            .into_owned();
        if namespaces.insert(prefix, value).is_some() {
            return None;
        }
    }
    let root_name = element.name().as_ref().to_vec();
    let prefix = qualified_prefix(&root_name).unwrap_or_default();
    let namespace = namespaces.get(prefix).cloned();
    if (!prefix.is_empty() && namespace.is_none())
        || namespace.as_deref().is_some_and(|namespace| {
            !matches!(
                namespace,
                OOXML_PACKAGE_RELATIONSHIPS_NAMESPACE_TRANSITIONAL
                    | OOXML_PACKAGE_RELATIONSHIPS_NAMESPACE_STRICT
            )
        })
    {
        return None;
    }
    Some(RelationshipRootContext {
        qualified_name: root_name,
        namespace,
        namespaces,
    })
}

/// Parse a package relationship part without applying last-entry-wins
/// semantics. Duplicate IDs, malformed structure, unknown target modes, and
/// foreign relationship namespaces invalidate the complete part. `Type`
/// remains optional here for compatibility with producer fixtures; selectors
/// that depend on a type require an exact Transitional or Strict URI below.
/// Unmodeled producer attributes are ignored, but they cannot substitute for
/// or override the required unqualified `Id` and `Target` attributes.
pub(crate) fn parse_ooxml_relationships(xml: &str) -> Option<Vec<OoxmlRelationship>> {
    parse_ooxml_relationships_with_policy(xml, true)
}

pub(crate) fn parse_ooxml_relationships_preserving_extensions(
    xml: &str,
) -> Option<Vec<OoxmlRelationship>> {
    parse_ooxml_relationships_with_policy(xml, true)
}

fn parse_ooxml_relationships_with_policy(
    xml: &str,
    allow_extension_attributes: bool,
) -> Option<Vec<OoxmlRelationship>> {
    const MAX_RELATIONSHIPS: usize = 65_536;
    const MAX_RELATIONSHIP_FIELD_BYTES: usize = 4_096;

    if xml.trim().is_empty() || !crate::xml_reference_work_within_budget(xml) {
        return None;
    }

    let mut reader = Reader::from_str(xml);
    let mut ids = BTreeSet::new();
    let mut relationships = Vec::new();
    let mut root: Option<RelationshipRootContext> = None;
    let mut root_open = false;
    let mut root_closed = false;
    let mut open_relationship: Option<(Vec<u8>, OoxmlRelationship)> = None;
    loop {
        match reader.read_event() {
            Ok(Event::End(element)) if open_relationship.is_some() => {
                let (qualified_name, relationship) = open_relationship.take()?;
                if element.name().as_ref() != qualified_name.as_slice() {
                    return None;
                }
                if relationships.len() >= MAX_RELATIONSHIPS || !ids.insert(relationship.id.clone())
                {
                    return None;
                }
                relationships.push(relationship);
            }
            Ok(Event::Text(text))
                if open_relationship.is_some()
                    && text.as_ref().iter().all(u8::is_ascii_whitespace) => {}
            Ok(Event::PI(_) | Event::Comment(_)) if open_relationship.is_some() => {}
            Ok(_) if open_relationship.is_some() => return None,
            Ok(Event::Start(element)) if root.is_none() && !root_closed => {
                root = Some(relationship_root_context(
                    &element,
                    allow_extension_attributes,
                )?);
                root_open = true;
            }
            Ok(Event::Empty(element)) if root.is_none() && !root_closed => {
                root = Some(relationship_root_context(
                    &element,
                    allow_extension_attributes,
                )?);
                root_closed = true;
            }
            Ok(Event::Empty(element)) if root_open => {
                let root = root.as_ref()?;
                let relationship = parse_ooxml_relationship_element(
                    &element,
                    root,
                    allow_extension_attributes,
                    MAX_RELATIONSHIP_FIELD_BYTES,
                )?;
                if relationships.len() >= MAX_RELATIONSHIPS || !ids.insert(relationship.id.clone())
                {
                    return None;
                }
                relationships.push(relationship);
            }
            Ok(Event::Start(element)) if root_open => {
                let root = root.as_ref()?;
                let relationship = parse_ooxml_relationship_element(
                    &element,
                    root,
                    allow_extension_attributes,
                    MAX_RELATIONSHIP_FIELD_BYTES,
                )?;
                open_relationship = Some((element.name().as_ref().to_vec(), relationship));
            }
            Ok(Event::End(element)) if root_open => {
                let root = root.as_ref()?;
                if element.name().as_ref() != root.qualified_name.as_slice() {
                    return None;
                }
                root_open = false;
                root_closed = true;
            }
            Ok(Event::Text(text)) if text.as_ref().iter().all(u8::is_ascii_whitespace) => {}
            Ok(Event::Decl(_) | Event::PI(_) | Event::Comment(_)) => {}
            Ok(Event::Eof) => break,
            Err(_) | Ok(_) => return None,
        }
    }
    if root.is_none() || root_open || !root_closed || open_relationship.is_some() {
        None
    } else {
        Some(relationships)
    }
}

fn parse_ooxml_relationship_element(
    element: &quick_xml::events::BytesStart<'_>,
    root: &RelationshipRootContext,
    allow_extension_attributes: bool,
    max_field_bytes: usize,
) -> Option<OoxmlRelationship> {
    if local(element.name().as_ref()) != b"Relationship" {
        return None;
    }
    let element_name = element.name();
    let prefix = qualified_prefix(element_name.as_ref()).unwrap_or_default();
    let namespace = root.namespaces.get(prefix).map(String::as_str);
    if (!prefix.is_empty() && namespace.is_none()) || namespace != root.namespace.as_deref() {
        return None;
    }

    let mut id = None;
    let mut rel_type = None;
    let mut target = None;
    let mut target_mode = None;
    for attribute in element.attributes() {
        let attribute = attribute.ok()?;
        let slot = match attribute.key.as_ref() {
            b"Id" => Some(&mut id),
            b"Type" => Some(&mut rel_type),
            b"Target" => Some(&mut target),
            b"TargetMode" => Some(&mut target_mode),
            _ if allow_extension_attributes => None,
            _ => return None,
        };
        let Some(slot) = slot else {
            continue;
        };
        if slot.is_some() {
            return None;
        }
        let value = attribute
            .decoded_and_normalized_value_with(
                XmlVersion::Implicit1_0,
                element.decoder(),
                1,
                quick_xml::escape::resolve_xml_entity,
            )
            .ok()?
            .into_owned();
        if value.len() > max_field_bytes {
            return None;
        }
        *slot = Some(value);
    }

    let id = id.filter(|id| !id.is_empty())?;
    let target = target.filter(|target| !target.is_empty())?;
    let external = match target_mode.as_deref() {
        None | Some("Internal") => false,
        Some("External") => true,
        Some(_) => return None,
    };
    Some(OoxmlRelationship {
        id,
        rel_type,
        target,
        external,
    })
}

/// Select exactly one internal package relationship of `rel_kind` in source
/// order. Ambiguous, malformed, duplicate-ID, or external relationships are
/// rejected instead of inheriting `HashMap` iteration order or falling back to
/// a conventional part path.
pub(crate) fn unique_internal_relationship_target(xml: &str, rel_kind: &str) -> RelationshipTarget {
    if xml.trim().is_empty() {
        return RelationshipTarget::Missing;
    }
    let Some(relationships) = parse_ooxml_relationships(xml) else {
        return RelationshipTarget::Invalid;
    };
    let mut selected = None;
    for relationship in relationships {
        if relationship
            .rel_type
            .as_deref()
            .is_some_and(|value| relationship_type_matches(value, rel_kind))
            && (relationship.external || selected.replace(relationship.target).is_some())
        {
            return RelationshipTarget::Invalid;
        }
    }
    selected.map_or(RelationshipTarget::Missing, RelationshipTarget::Internal)
}

pub(crate) fn internal_relationship_target_by_id(
    relationships: &[OoxmlRelationship],
    id: &str,
    rel_kind: &str,
) -> RelationshipTarget {
    let Some(relationship) = relationships
        .iter()
        .find(|relationship| relationship.id == id)
    else {
        return RelationshipTarget::Missing;
    };
    if relationship.external
        || !relationship
            .rel_type
            .as_deref()
            .is_some_and(|value| relationship_type_matches(value, rel_kind))
    {
        RelationshipTarget::Invalid
    } else {
        RelationshipTarget::Internal(relationship.target.clone())
    }
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

const MAX_XLSX_STYLE_RECORDS: usize = 65_536;
const MAX_XLSX_CUSTOM_NUMBER_FORMATS: usize = 65_536;
const MAX_XLSX_FORMAT_CODE_BYTES: usize = 4_096;
const MAX_XLSX_INDEXED_COLORS: usize = 256;
const MAX_XLSX_COLUMN_INDEX: u16 = 16_383;
const MAX_XLSX_ROW_INDEX: u32 = 1_048_575;
// [MS-OI29500] Part 1 §18.4.11: Office accepts SpreadsheetML font sizes from
// 1 through 409.55 points. The verified renderer sidecar is deliberately
// narrower because the public Font model uses whole points: only exact
// integral source values through 409 are eligible.
const MAX_VERIFIED_XLSX_FONT_SIZE_POINTS: u16 = 409;

/// Per-style number format, derived from `styles.xml`.
#[derive(Default)]
struct Styles {
    /// `numFmtId` per `cellXfs` style index.
    xf_numfmt: Vec<u16>,
    /// Custom `formatCode` strings keyed by `numFmtId`.
    custom: HashMap<u16, String>,
    /// Custom OOXML indexed color table from `<colors><indexedColors>`.
    indexed_colors: Vec<Color>,
    /// Full differential styles and typed parse losses per `dxfs` index.
    differential_styles: Vec<DifferentialStyle>,
    /// Common public style subset per `cellXfs` index.
    cell_styles: Vec<CellStyle>,
    /// Exact integral Normal-style font size retained only when the first cell
    /// XF and named/built-in Normal style resolve to the same source font.
    xlsx_normal_font_size_pt: Option<u16>,
    /// Exact integral source font size per retained `cellXfs` record.
    ///
    /// Entries are `None` when the XF/font provenance is inherited,
    /// fractional, malformed, duplicated, out of range, or otherwise
    /// ambiguous.
    xlsx_cell_xf_font_sizes_pt: Vec<Option<u16>>,
    /// Sparse direct-format overlays per `cellXfs` style index.
    cell_style_overlays: Vec<CellStyleOverlay>,
    /// Imported custom table region styles keyed by `<tableStyle name>`.
    table_styles: HashMap<String, ParsedTableStyle>,
    /// Workbook-global style-table truncation and parse losses.
    losses: Vec<StyleLoss>,
}

impl Styles {
    fn format_id(&self, style_idx: usize) -> u16 {
        self.xf_numfmt.get(style_idx).copied().unwrap_or(0)
    }

    fn kind(&self, style_idx: usize) -> format::Kind {
        let numfmt_id = self.format_id(style_idx);
        format::classify(numfmt_id, self.custom.get(&numfmt_id).map(String::as_str))
    }

    fn custom_format(&self, style_idx: usize) -> Option<&str> {
        let numfmt_id = self.xf_numfmt.get(style_idx).copied()?;
        self.custom.get(&numfmt_id).map(String::as_str)
    }

    fn render_text(&self, style_idx: usize, value: &str) -> String {
        self.custom_format(style_idx).map_or_else(
            || value.to_string(),
            |code| format::render_text_format(value, code),
        )
    }

    fn differential_style(&self, dxf_id: usize) -> Option<&DifferentialStyle> {
        self.differential_styles.get(dxf_id)
    }

    fn cell_style(&self, style_idx: usize) -> Option<&CellStyle> {
        self.cell_styles.get(style_idx)
    }

    fn xlsx_cell_font_size_pt(&self, style_idx: usize) -> Option<u16> {
        self.xlsx_cell_xf_font_sizes_pt
            .get(style_idx)
            .copied()
            .flatten()
    }

    fn cell_style_overlay(&self, style_idx: usize) -> Option<&CellStyleOverlay> {
        self.cell_style_overlays.get(style_idx)
    }

    fn table_style(&self, name: &str, theme: &ThemeColors) -> Option<ParsedTableStyle> {
        self.table_styles
            .get(name)
            .cloned()
            .or_else(|| built_in_table_style(name, theme))
    }
}

#[derive(Debug, Clone, Default)]
struct DifferentialStyle {
    style: CellStyle,
    losses: Vec<StyleLoss>,
}

#[derive(Debug, Clone, Default)]
struct ParsedTableStyle {
    definition: TableStyleDefinition,
    losses: Vec<StyleLoss>,
}

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
const OOXML_RELATIONSHIPS_NAMESPACE_TRANSITIONAL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const OOXML_RELATIONSHIPS_NAMESPACE_STRICT: &str =
    "http://purl.oclc.org/ooxml/officeDocument/relationships";
const OOXML_PACKAGE_RELATIONSHIPS_NAMESPACE_TRANSITIONAL: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships";
const OOXML_PACKAGE_RELATIONSHIPS_NAMESPACE_STRICT: &str =
    "http://purl.oclc.org/ooxml/package/relationships";
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

fn parse_color(value: &str) -> Option<Color> {
    let rgb = value.trim().strip_prefix('#').unwrap_or(value.trim());
    let rgb = match rgb.len() {
        8 => &rgb[2..],
        6 => rgb,
        _ => return None,
    };
    if !rgb.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let red = u8::from_str_radix(&rgb[0..2], 16).ok()?;
    let green = u8::from_str_radix(&rgb[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&rgb[4..6], 16).ok()?;
    Some(Color::rgb(red, green, blue))
}

const OOXML_DEFAULT_INDEXED_COLORS: [Color; 64] = [
    Color::rgb(0x00, 0x00, 0x00),
    Color::rgb(0xFF, 0xFF, 0xFF),
    Color::rgb(0xFF, 0x00, 0x00),
    Color::rgb(0x00, 0xFF, 0x00),
    Color::rgb(0x00, 0x00, 0xFF),
    Color::rgb(0xFF, 0xFF, 0x00),
    Color::rgb(0xFF, 0x00, 0xFF),
    Color::rgb(0x00, 0xFF, 0xFF),
    Color::rgb(0x00, 0x00, 0x00),
    Color::rgb(0xFF, 0xFF, 0xFF),
    Color::rgb(0xFF, 0x00, 0x00),
    Color::rgb(0x00, 0xFF, 0x00),
    Color::rgb(0x00, 0x00, 0xFF),
    Color::rgb(0xFF, 0xFF, 0x00),
    Color::rgb(0xFF, 0x00, 0xFF),
    Color::rgb(0x00, 0xFF, 0xFF),
    Color::rgb(0x80, 0x00, 0x00),
    Color::rgb(0x00, 0x80, 0x00),
    Color::rgb(0x00, 0x00, 0x80),
    Color::rgb(0x80, 0x80, 0x00),
    Color::rgb(0x80, 0x00, 0x80),
    Color::rgb(0x00, 0x80, 0x80),
    Color::rgb(0xC0, 0xC0, 0xC0),
    Color::rgb(0x80, 0x80, 0x80),
    Color::rgb(0x99, 0x99, 0xFF),
    Color::rgb(0x99, 0x33, 0x66),
    Color::rgb(0xFF, 0xFF, 0xCC),
    Color::rgb(0xCC, 0xFF, 0xFF),
    Color::rgb(0x66, 0x00, 0x66),
    Color::rgb(0xFF, 0x80, 0x80),
    Color::rgb(0x00, 0x66, 0xCC),
    Color::rgb(0xCC, 0xCC, 0xFF),
    Color::rgb(0x00, 0x00, 0x80),
    Color::rgb(0xFF, 0x00, 0xFF),
    Color::rgb(0xFF, 0xFF, 0x00),
    Color::rgb(0x00, 0xFF, 0xFF),
    Color::rgb(0x80, 0x00, 0x80),
    Color::rgb(0x80, 0x00, 0x00),
    Color::rgb(0x00, 0x80, 0x80),
    Color::rgb(0x00, 0x00, 0xFF),
    Color::rgb(0x00, 0xCC, 0xFF),
    Color::rgb(0xCC, 0xFF, 0xFF),
    Color::rgb(0xCC, 0xFF, 0xCC),
    Color::rgb(0xFF, 0xFF, 0x99),
    Color::rgb(0x99, 0xCC, 0xFF),
    Color::rgb(0xFF, 0x99, 0xCC),
    Color::rgb(0xCC, 0x99, 0xFF),
    Color::rgb(0xFF, 0xCC, 0x99),
    Color::rgb(0x33, 0x66, 0xFF),
    Color::rgb(0x33, 0xCC, 0xCC),
    Color::rgb(0x99, 0xCC, 0x00),
    Color::rgb(0xFF, 0xCC, 0x00),
    Color::rgb(0xFF, 0x99, 0x00),
    Color::rgb(0xFF, 0x66, 0x00),
    Color::rgb(0x66, 0x66, 0x99),
    Color::rgb(0x96, 0x96, 0x96),
    Color::rgb(0x00, 0x33, 0x66),
    Color::rgb(0x33, 0x99, 0x66),
    Color::rgb(0x00, 0x33, 0x00),
    Color::rgb(0x33, 0x33, 0x00),
    Color::rgb(0x99, 0x33, 0x00),
    Color::rgb(0x99, 0x33, 0x66),
    Color::rgb(0x33, 0x33, 0x99),
    Color::rgb(0x33, 0x33, 0x33),
];

#[derive(Clone)]
pub(crate) struct ThemeColors {
    // Canonical DrawingML order: lt1, dk1, lt2, dk2, accent1..6,
    // hyperlink, followed-hyperlink.
    colors: [Option<Color>; 12],
    major_latin_font_family: Option<String>,
    minor_latin_font_family: Option<String>,
    /// False only when a present theme part contained ambiguous or malformed
    /// data. A missing theme uses the deterministic application fallback and
    /// therefore keeps this true.
    source_valid: bool,
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self {
            colors: [None; 12],
            major_latin_font_family: None,
            minor_latin_font_family: None,
            source_valid: true,
        }
    }
}

const MAX_IMPORTED_CHART_LATIN_FONT_FAMILY_BYTES: usize = 255;
pub(crate) const CALC_IMPORTED_CHART_LATIN_FONT_FAMILY: &str = "Liberation Sans";

pub(crate) fn bounded_imported_chart_latin_font_family(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= MAX_IMPORTED_CHART_LATIN_FONT_FAMILY_BYTES)
        .then(|| value.to_string())
}

const OFFICE_CHART_ACCENTS: [Color; 6] = [
    Color::rgb(68, 114, 196),
    Color::rgb(237, 125, 49),
    Color::rgb(165, 165, 165),
    Color::rgb(255, 192, 0),
    Color::rgb(91, 155, 213),
    Color::rgb(112, 173, 71),
];

impl ThemeColors {
    fn color(&self, idx: usize, tint: Option<f64>) -> Option<Color> {
        let color = self.colors.get(idx).copied().flatten()?;
        Some(apply_optional_tint(color, tint))
    }

    fn chart_palette(&self) -> Vec<Color> {
        (0..OFFICE_CHART_ACCENTS.len())
            .map(|index| self.colors[index + 4].unwrap_or(OFFICE_CHART_ACCENTS[index]))
            .collect()
    }

    fn chart_default_latin_font_family(&self) -> &str {
        self.minor_latin_font_family
            .as_deref()
            .unwrap_or(CALC_IMPORTED_CHART_LATIN_FONT_FAMILY)
    }

    fn chart_major_latin_font_family(&self) -> &str {
        self.major_latin_font_family
            .as_deref()
            .unwrap_or(CALC_IMPORTED_CHART_LATIN_FONT_FAMILY)
    }

    pub(crate) fn source_valid(&self) -> bool {
        self.source_valid
    }

    #[cfg(feature = "xlsb")]
    pub(crate) fn xlsb_ordered_colors(&self) -> [Option<Color>; 12] {
        [
            self.colors[1],
            self.colors[0],
            self.colors[3],
            self.colors[2],
            self.colors[4],
            self.colors[5],
            self.colors[6],
            self.colors[7],
            self.colors[8],
            self.colors[9],
            self.colors[10],
            self.colors[11],
        ]
    }

    #[cfg(feature = "xlsb")]
    pub(crate) fn source_major_latin_font_family(&self) -> Option<&str> {
        self.major_latin_font_family.as_deref()
    }

    #[cfg(feature = "xlsb")]
    pub(crate) fn source_minor_latin_font_family(&self) -> Option<&str> {
        self.minor_latin_font_family.as_deref()
    }
}

#[cfg(feature = "xlsb")]
pub(crate) fn chart_theme(
    colors: [Option<Color>; 12],
    major_latin_font_family: Option<&str>,
    minor_latin_font_family: Option<&str>,
    source_valid: bool,
) -> ThemeColors {
    ThemeColors {
        colors,
        major_latin_font_family: major_latin_font_family
            .and_then(bounded_imported_chart_latin_font_family),
        minor_latin_font_family: minor_latin_font_family
            .and_then(bounded_imported_chart_latin_font_family),
        source_valid,
    }
}

fn theme_color_slot(name: &[u8]) -> Option<usize> {
    match name {
        b"lt1" => Some(0),
        b"dk1" => Some(1),
        b"lt2" => Some(2),
        b"dk2" => Some(3),
        b"accent1" => Some(4),
        b"accent2" => Some(5),
        b"accent3" => Some(6),
        b"accent4" => Some(7),
        b"accent5" => Some(8),
        b"accent6" => Some(9),
        b"hlink" => Some(10),
        b"folHlink" => Some(11),
        _ => None,
    }
}

pub(crate) fn parse_theme(xml: &str) -> ThemeColors {
    fn retain_theme_color(
        theme: &mut ThemeColors,
        active_slot: &mut Option<(usize, bool)>,
        name: &[u8],
        element: &quick_xml::events::BytesStart<'_>,
    ) {
        let Some((slot, painted)) = active_slot.as_mut() else {
            return;
        };
        if *painted {
            theme.source_valid = false;
            return;
        }
        *painted = true;
        let color = match name {
            b"srgbClr" => {
                if !chart_text_attributes_are_subset(element, &[b"val"]) {
                    None
                } else {
                    unique_attr(element, b"val")
                        .ok()
                        .flatten()
                        .as_deref()
                        .and_then(parse_chart_rgb)
                }
            }
            b"sysClr" => {
                if !chart_text_attributes_are_subset(element, &[b"val", b"lastClr"])
                    || !matches!(unique_attr(element, b"val"), Ok(Some(value)) if !value.is_empty())
                {
                    None
                } else {
                    unique_attr(element, b"lastClr")
                        .ok()
                        .flatten()
                        .as_deref()
                        .and_then(parse_chart_rgb)
                }
            }
            _ => None,
        };
        if let Some(color) = color {
            theme.colors[*slot] = Some(color);
        } else {
            theme.source_valid = false;
        }
    }

    fn retain_theme_latin(
        theme: &mut ThemeColors,
        in_major_font: bool,
        in_minor_font: bool,
        element: &quick_xml::events::BytesStart<'_>,
    ) {
        if !in_major_font && !in_minor_font {
            return;
        }
        let family = if chart_text_attributes_are_subset(element, &[b"typeface"]) {
            unique_attr(element, b"typeface")
                .ok()
                .flatten()
                .as_deref()
                .and_then(bounded_imported_chart_latin_font_family)
        } else {
            None
        };
        let target = if in_major_font {
            &mut theme.major_latin_font_family
        } else {
            &mut theme.minor_latin_font_family
        };
        if family.is_none() || target.is_some() {
            theme.source_valid = false;
        } else {
            *target = family;
        }
    }

    let mut r = Reader::from_str(xml);
    let mut theme = ThemeColors {
        source_valid: theme_markup_is_supported(xml),
        ..ThemeColors::default()
    };
    let mut active_slot: Option<(usize, bool)> = None;
    let mut seen_slots = [false; 12];
    let mut element_stack = Vec::<Vec<u8>>::new();
    let mut theme_seen = false;
    let mut theme_open = false;
    let mut theme_elements_seen = false;
    let mut theme_elements_open = false;
    let mut in_color_scheme = false;
    let mut color_scheme_seen = false;
    let mut in_font_scheme = false;
    let mut font_scheme_seen = false;
    let mut in_major_font = false;
    let mut in_minor_font = false;
    let mut major_font_seen = false;
    let mut minor_font_seen = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = local(qname.as_ref());
                let depth = element_stack.len();
                if name == b"theme" {
                    if theme_seen || theme_open || depth != 0 {
                        theme.source_valid = false;
                    }
                    theme_seen = true;
                    theme_open = true;
                } else if name == b"themeElements" {
                    if theme_elements_seen || theme_elements_open || !theme_open || depth != 1 {
                        theme.source_valid = false;
                    }
                    theme_elements_seen = true;
                    theme_elements_open = true;
                } else if name == b"clrScheme" {
                    if color_scheme_seen || in_color_scheme || !theme_elements_open || depth != 2 {
                        theme.source_valid = false;
                    }
                    color_scheme_seen = true;
                    in_color_scheme = true;
                } else if name == b"fontScheme" {
                    if font_scheme_seen || in_font_scheme || !theme_elements_open || depth != 2 {
                        theme.source_valid = false;
                    }
                    font_scheme_seen = true;
                    in_font_scheme = true;
                } else if name == b"majorFont" && in_font_scheme {
                    if major_font_seen || in_major_font || in_minor_font || depth != 3 {
                        theme.source_valid = false;
                    }
                    major_font_seen = true;
                    in_major_font = true;
                } else if name == b"minorFont" && in_font_scheme {
                    if minor_font_seen || in_major_font || in_minor_font || depth != 3 {
                        theme.source_valid = false;
                    }
                    minor_font_seen = true;
                    in_minor_font = true;
                } else if let Some(next_slot) =
                    in_color_scheme.then(|| theme_color_slot(name)).flatten()
                {
                    if active_slot.is_some() || seen_slots[next_slot] || depth != 3 {
                        theme.source_valid = false;
                    }
                    seen_slots[next_slot] = true;
                    active_slot = Some((next_slot, false));
                } else if matches!(name, b"srgbClr" | b"sysClr") {
                    if active_slot.is_some() && depth != 4 {
                        theme.source_valid = false;
                    }
                    retain_theme_color(&mut theme, &mut active_slot, name, &e);
                } else if name == b"latin" {
                    if (in_major_font || in_minor_font) && depth != 4 {
                        theme.source_valid = false;
                    }
                    retain_theme_latin(&mut theme, in_major_font, in_minor_font, &e);
                } else if active_slot.is_some() {
                    // Theme slot color choices and transforms outside the exact
                    // sRGB/system-color subset cannot be resolved portably.
                    theme.source_valid = false;
                }
                if element_stack.len() >= 64 {
                    theme.source_valid = false;
                    break;
                }
                element_stack.push(name.to_vec());
            }
            Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local(qname.as_ref());
                let depth = element_stack.len();
                if matches!(
                    name,
                    b"theme"
                        | b"themeElements"
                        | b"clrScheme"
                        | b"fontScheme"
                        | b"majorFont"
                        | b"minorFont"
                ) {
                    theme.source_valid = false;
                } else if let Some(next_slot) =
                    in_color_scheme.then(|| theme_color_slot(name)).flatten()
                {
                    if seen_slots[next_slot] || depth != 3 {
                        theme.source_valid = false;
                    }
                    seen_slots[next_slot] = true;
                    // An empty color slot has no deterministic color and must
                    // not leak into the following sibling.
                    theme.source_valid = false;
                } else if matches!(name, b"srgbClr" | b"sysClr") {
                    if active_slot.is_some() && depth != 4 {
                        theme.source_valid = false;
                    }
                    retain_theme_color(&mut theme, &mut active_slot, name, &e);
                } else if name == b"latin" {
                    if (in_major_font || in_minor_font) && depth != 4 {
                        theme.source_valid = false;
                    }
                    retain_theme_latin(&mut theme, in_major_font, in_minor_font, &e);
                } else if active_slot.is_some() {
                    theme.source_valid = false;
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = local(qname.as_ref());
                let Some(open_name) = element_stack.pop() else {
                    theme.source_valid = false;
                    break;
                };
                if open_name.as_slice() != name {
                    theme.source_valid = false;
                }
                let depth = element_stack.len();
                if name == b"clrScheme" {
                    if active_slot.is_some() || depth != 2 {
                        theme.source_valid = false;
                        active_slot = None;
                    }
                    in_color_scheme = false;
                } else if name == b"fontScheme" {
                    if depth != 2 {
                        theme.source_valid = false;
                    }
                    in_font_scheme = false;
                    in_major_font = false;
                    in_minor_font = false;
                } else if name == b"majorFont" {
                    if depth != 3 {
                        theme.source_valid = false;
                    }
                    in_major_font = false;
                } else if name == b"minorFont" {
                    if depth != 3 {
                        theme.source_valid = false;
                    }
                    in_minor_font = false;
                } else if name == b"themeElements" {
                    if depth != 1 {
                        theme.source_valid = false;
                    }
                    theme_elements_open = false;
                } else if name == b"theme" {
                    if depth != 0 {
                        theme.source_valid = false;
                    }
                    theme_open = false;
                }
                if let Some(slot) = theme_color_slot(name) {
                    if depth == 3
                        && active_slot.is_some_and(|(active, painted)| active == slot && painted)
                    {
                        active_slot = None;
                    } else if in_color_scheme {
                        theme.source_valid = false;
                        active_slot = None;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                theme.source_valid = false;
                break;
            }
            _ => {}
        }
    }
    if active_slot.is_some()
        || !element_stack.is_empty()
        || theme_open
        || theme_elements_open
        || in_color_scheme
        || in_font_scheme
        || in_major_font
        || in_minor_font
    {
        theme.source_valid = false;
    }
    if !theme_seen
        || !theme_elements_seen
        || !color_scheme_seen
        || seen_slots.iter().any(|seen| !seen)
        || !font_scheme_seen
        || !major_font_seen
        || !minor_font_seen
        || theme.major_latin_font_family.is_none()
        || theme.minor_latin_font_family.is_none()
    {
        theme.source_valid = false;
    }
    theme
}

fn apply_tint(color: Color, tint: f64) -> Color {
    fn channel(value: u8, tint: f64) -> u8 {
        let value = f64::from(value);
        let tinted = if tint < 0.0 {
            value * (1.0 + tint)
        } else {
            value * (1.0 - tint) + 255.0 * tint
        };
        tinted.round().clamp(0.0, 255.0) as u8
    }

    let [red, green, blue] = color.as_rgb();
    Color::rgb(
        channel(red, tint),
        channel(green, tint),
        channel(blue, tint),
    )
}

fn apply_optional_tint(color: Color, tint: Option<f64>) -> Color {
    match tint {
        Some(tint) if tint.is_finite() => apply_tint(color, tint),
        _ => color,
    }
}

fn indexed_color(idx: usize, indexed_colors: &[Color], tint: Option<f64>) -> Option<Color> {
    let color = indexed_colors
        .get(idx)
        .copied()
        .or_else(|| OOXML_DEFAULT_INDEXED_COLORS.get(idx).copied())?;
    Some(apply_optional_tint(color, tint))
}

fn color_attr(
    e: &quick_xml::events::BytesStart<'_>,
    theme: &ThemeColors,
    indexed_colors: &[Color],
) -> Option<Color> {
    attr(e, b"rgb")
        .as_deref()
        .and_then(parse_color)
        .or_else(|| {
            let idx = attr(e, b"theme").and_then(|s| s.parse::<usize>().ok())?;
            let tint = attr(e, b"tint").and_then(|s| s.parse::<f64>().ok());
            theme.color(idx, tint)
        })
        .or_else(|| {
            let idx = attr(e, b"indexed").and_then(|s| s.parse::<usize>().ok())?;
            let tint = attr(e, b"tint").and_then(|s| s.parse::<f64>().ok());
            indexed_color(idx, indexed_colors, tint)
        })
}

fn parse_indexed_colors(xml: &str, losses: &mut Vec<StyleLoss>) -> Vec<Color> {
    let mut r = Reader::from_str(xml);
    let mut colors = Vec::new();
    let mut in_indexed_colors = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"indexedColors" => in_indexed_colors = true,
                b"rgbColor" if in_indexed_colors => {
                    if let Some(color) = attr(&e, b"rgb").as_deref().and_then(parse_color) {
                        if colors.len() < MAX_XLSX_INDEXED_COLORS {
                            colors.push(color);
                        } else {
                            add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(e)) if in_indexed_colors && local(e.name().as_ref()) == b"rgbColor" => {
                if let Some(color) = attr(&e, b"rgb").as_deref().and_then(parse_color) {
                    if colors.len() < MAX_XLSX_INDEXED_COLORS {
                        colors.push(color);
                    } else {
                        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                    }
                }
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"indexedColors" => {
                in_indexed_colors = false;
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    colors
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

/// `<styleSheet>`: `<numFmts><numFmt numFmtId formatCode/>` + the `<cellXfs>`
/// `<xf numFmtId/>` list (cell `s` indexes cellXfs).
fn retain_custom_number_format(styles: &mut Styles, e: &quick_xml::events::BytesStart<'_>) {
    let (Some(id), Some(code)) = (attr(e, b"numFmtId"), attr(e, b"formatCode")) else {
        return;
    };
    let Ok(id) = id.parse::<u16>() else {
        return;
    };
    if code.len() > MAX_XLSX_FORMAT_CODE_BYTES {
        add_differential_loss(&mut styles.losses, StyleLossKind::LimitExceeded, 1);
        return;
    }
    if !styles.custom.contains_key(&id) && styles.custom.len() >= MAX_XLSX_CUSTOM_NUMBER_FORMATS {
        add_differential_loss(&mut styles.losses, StyleLossKind::LimitExceeded, 1);
        return;
    }
    styles.custom.insert(id, code);
}

fn retain_cell_xf_number_format(styles: &mut Styles, e: &quick_xml::events::BytesStart<'_>) {
    if styles.xf_numfmt.len() >= MAX_XLSX_STYLE_RECORDS {
        add_differential_loss(&mut styles.losses, StyleLossKind::LimitExceeded, 1);
        return;
    }
    let id = attr(e, b"numFmtId")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    styles.xf_numfmt.push(id);
}

fn parse_styles(xml: &str, theme: &ThemeColors) -> Styles {
    let mut r = Reader::from_str(xml);
    let mut styles = Styles::default();
    if !theme.source_valid() {
        add_differential_loss(&mut styles.losses, StyleLossKind::UnsupportedProperty, 1);
    }
    styles.indexed_colors = parse_indexed_colors(xml, &mut styles.losses);
    let mut in_cell_xfs = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"numFmt" => retain_custom_number_format(&mut styles, &e),
                b"cellXfs" => in_cell_xfs = true,
                b"xf" if in_cell_xfs => retain_cell_xf_number_format(&mut styles, &e),
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"numFmt" => retain_custom_number_format(&mut styles, &e),
                b"xf" if in_cell_xfs => retain_cell_xf_number_format(&mut styles, &e),
                _ => {}
            },
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"cellXfs" => in_cell_xfs = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    let (fonts, exact_font_sizes, font_table_complete) =
        parse_font_table(xml, theme, &styles.indexed_colors, &mut styles.losses);
    styles.xlsx_normal_font_size_pt =
        verified_xlsx_normal_font_size(xml, &fonts, &exact_font_sizes, font_table_complete);
    styles.xlsx_cell_xf_font_sizes_pt =
        verified_xlsx_cell_xf_font_sizes(xml, &exact_font_sizes, font_table_complete);
    let (cell_styles, cell_style_overlays) = parse_cell_styles(
        xml,
        theme,
        &styles.indexed_colors,
        &styles.custom,
        &fonts,
        &mut styles.losses,
    );
    styles.cell_styles = cell_styles;
    styles.cell_style_overlays = cell_style_overlays;
    let differential_styles =
        parse_differential_styles(xml, theme, &styles.indexed_colors, &styles.custom);
    styles.table_styles = parse_table_styles(xml, &differential_styles);
    styles.differential_styles = differential_styles;
    styles
}

fn add_differential_loss(losses: &mut Vec<StyleLoss>, kind: StyleLossKind, occurrences: u32) {
    if occurrences == 0 {
        return;
    }
    if let Some(loss) = losses.iter_mut().find(|loss| loss.kind == kind) {
        loss.occurrences = loss.occurrences.saturating_add(occurrences);
    } else {
        losses.push(StyleLoss { kind, occurrences });
    }
}

fn retain_xlsx_style_record<T>(records: &mut Vec<T>, value: T, losses: &mut Vec<StyleLoss>) {
    if records.len() < MAX_XLSX_STYLE_RECORDS {
        records.push(value);
    } else {
        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
    }
}

fn retain_cell_xf_style(
    styles: &mut Vec<CellStyle>,
    overlays: &mut Vec<CellStyleOverlay>,
    style: CellStyle,
    overlay: CellStyleOverlay,
    losses: &mut Vec<StyleLoss>,
) {
    if styles.len() < MAX_XLSX_STYLE_RECORDS {
        styles.push(style);
        overlays.push(overlay);
    } else {
        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
    }
}

fn differential_alignment_is_lossy(e: &quick_xml::events::BytesStart<'_>) -> bool {
    let horizontal = attr(e, b"horizontal");
    let vertical = attr(e, b"vertical");
    let explicit_false = |name| {
        attr(e, name)
            .as_deref()
            .is_some_and(|value| !attr_true(value))
    };
    horizontal
        .as_deref()
        .is_some_and(|value| !matches!(value, "general" | "left" | "center" | "right"))
        || vertical
            .as_deref()
            .is_some_and(|value| !matches!(value, "top" | "center" | "bottom"))
        || attr(e, b"textRotation")
            .and_then(|value| value.parse::<i16>().ok())
            .is_some_and(|value| value > 180)
        || explicit_false(b"wrapText")
        || explicit_false(b"shrinkToFit")
        || attr(e, b"indent").as_deref() == Some("0")
        || [
            b"relativeIndent".as_slice(),
            b"justifyLastLine".as_slice(),
            b"readingOrder".as_slice(),
            b"mergeCell".as_slice(),
        ]
        .into_iter()
        .any(|name| attr(e, name).is_some())
}

fn parse_differential_styles(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    custom: &HashMap<u16, String>,
) -> Vec<DifferentialStyle> {
    const MAX_DXFS: usize = 4_096;
    let mut reader = Reader::from_str(xml);
    let mut in_dxfs = false;
    let mut current: Option<CellStyle> = None;
    let mut font: Option<Font> = None;
    let mut fill: Option<Fill> = None;
    let mut border: Option<Border> = None;
    let mut border_edge = None;
    let mut losses = Vec::<StyleLoss>::new();
    let mut styles = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qualified_name = e.name();
                let name = local(qualified_name.as_ref());
                if name == b"dxfs" {
                    in_dxfs = true;
                    continue;
                }
                if !in_dxfs {
                    continue;
                }
                match name {
                    b"dxf" => {
                        if current.is_some() && styles.len() < MAX_DXFS {
                            styles.push(DifferentialStyle {
                                style: current.take().unwrap_or_default(),
                                losses: std::mem::take(&mut losses),
                            });
                        }
                        current = Some(CellStyle::default());
                        losses.clear();
                        font = None;
                        fill = None;
                        border = None;
                        border_edge = None;
                        if e.is_empty() {
                            let style = current.take().unwrap_or_default();
                            if styles.len() < MAX_DXFS {
                                styles.push(DifferentialStyle {
                                    style,
                                    losses: std::mem::take(&mut losses),
                                });
                            }
                        }
                    }
                    b"font" if current.is_some() => {
                        font = (!e.is_empty()).then(Font::default);
                    }
                    b"fill" if current.is_some() => {
                        fill = (!e.is_empty()).then(Fill::default);
                    }
                    b"border" if current.is_some() => {
                        border = (!e.is_empty()).then(Border::default);
                    }
                    b"name" if font.is_some() => {
                        font.as_mut().expect("dxf font").name = attr(&e, b"val");
                    }
                    b"sz" if font.is_some() => {
                        font.as_mut().expect("dxf font").size_pt = attr(&e, b"val")
                            .and_then(|value| value.parse::<f32>().ok())
                            .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                    }
                    b"color" if font.is_some() => {
                        let color = color_attr(&e, theme, indexed);
                        if color.is_none() {
                            add_differential_loss(&mut losses, StyleLossKind::UnresolvedColor, 1);
                        }
                        font.as_mut().expect("dxf font").color = color;
                    }
                    b"b" if font.is_some() => {
                        let enabled = attr(&e, b"val").as_deref().is_none_or(attr_true);
                        if !enabled {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        font.as_mut().expect("dxf font").bold = enabled;
                    }
                    b"i" if font.is_some() => {
                        let enabled = attr(&e, b"val").as_deref().is_none_or(attr_true);
                        if !enabled {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        font.as_mut().expect("dxf font").italic = enabled;
                    }
                    b"u" if font.is_some() => {
                        let enabled = attr(&e, b"val").as_deref().is_none_or(attr_true);
                        if !enabled {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        font.as_mut().expect("dxf font").underline = enabled;
                    }
                    b"strike" if font.is_some() => {
                        let enabled = attr(&e, b"val").as_deref().is_none_or(attr_true);
                        if !enabled {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        font.as_mut().expect("dxf font").strikethrough = enabled;
                    }
                    b"vertAlign" if font.is_some() => {
                        font.as_mut().expect("dxf font").script = match attr(&e, b"val").as_deref()
                        {
                            Some("superscript") => FormatScript::Superscript,
                            Some("subscript") => FormatScript::Subscript,
                            _ => FormatScript::None,
                        };
                    }
                    b"patternFill" if fill.is_some() => {
                        let source_pattern = attr(&e, b"patternType");
                        let pattern = format_pattern(source_pattern.as_deref());
                        if pattern == FormatPattern::None
                            && source_pattern
                                .as_deref()
                                .is_some_and(|value| value != "none")
                        {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        fill.as_mut().expect("dxf fill").pattern = pattern;
                    }
                    b"fgColor" if fill.is_some() => {
                        let color = color_attr(&e, theme, indexed);
                        if color.is_none() {
                            add_differential_loss(&mut losses, StyleLossKind::UnresolvedColor, 1);
                        }
                        fill.as_mut().expect("dxf fill").foreground = color;
                    }
                    b"bgColor" if fill.is_some() => {
                        let color = color_attr(&e, theme, indexed);
                        if color.is_none() {
                            add_differential_loss(&mut losses, StyleLossKind::UnresolvedColor, 1);
                        }
                        fill.as_mut().expect("dxf fill").background = color;
                    }
                    b"left" | b"right" | b"top" | b"bottom" if border.is_some() => {
                        let edge = match name {
                            b"left" => BorderEdge::Left,
                            b"right" => BorderEdge::Right,
                            b"top" => BorderEdge::Top,
                            _ => BorderEdge::Bottom,
                        };
                        let source_style = attr(&e, b"style");
                        let parsed_style = border_style(source_style.as_deref());
                        if parsed_style == BorderStyle::None
                            && source_style.as_deref().is_some_and(|value| value != "none")
                        {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        set_border_edge(border.as_mut().expect("dxf border"), edge, parsed_style);
                        border_edge = (!e.is_empty()).then_some(edge);
                    }
                    b"color" if border.is_some() && border_edge.is_some() => {
                        if let Some(color) = color_attr(&e, theme, indexed) {
                            set_border_color(
                                border.as_mut().expect("dxf border"),
                                border_edge.expect("dxf border edge"),
                                color,
                            );
                        } else {
                            add_differential_loss(&mut losses, StyleLossKind::UnresolvedColor, 1);
                        }
                    }
                    b"gradientFill" if fill.is_some() => {
                        add_differential_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1)
                    }
                    b"diagonal" | b"vertical" | b"horizontal" | b"start" | b"end"
                        if border.is_some() =>
                    {
                        add_differential_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    }
                    b"numFmt" if current.is_some() => {
                        current.as_mut().expect("dxf").num_fmt =
                            attr(&e, b"formatCode").or_else(|| {
                                attr(&e, b"numFmtId")
                                    .and_then(|value| value.parse::<u16>().ok())
                                    .and_then(|id| {
                                        custom
                                            .get(&id)
                                            .cloned()
                                            .or_else(|| built_in_num_fmt(id).map(str::to_string))
                                    })
                            });
                    }
                    b"alignment" if current.is_some() => {
                        current.as_mut().expect("dxf").align = Some(parse_alignment(&e));
                        if differential_alignment_is_lossy(&e) {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                    }
                    b"protection" if current.is_some() => {
                        current.as_mut().expect("dxf").protection = Some(CellProtection {
                            locked: attr(&e, b"locked").as_deref().and_then(parse_bool_attr),
                            hidden: attr(&e, b"hidden").as_deref().is_some_and(attr_true),
                        });
                    }
                    _ if font.is_some() || fill.is_some() || border.is_some() => {
                        add_differential_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    }
                    b"extLst" if current.is_some() => {
                        add_differential_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1)
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"font" if current.is_some() => {
                    let value = font.take().unwrap_or_default();
                    if value != Font::default() {
                        current.as_mut().expect("dxf").font = Some(value);
                    }
                }
                b"fill" if current.is_some() => {
                    let value = fill.take().unwrap_or_default();
                    if value != Fill::default() {
                        if value.pattern == FormatPattern::Solid {
                            current.as_mut().expect("dxf").fill =
                                value.foreground.or(value.background);
                        }
                        current.as_mut().expect("dxf").pattern_fill = Some(value);
                    }
                }
                b"left" | b"right" | b"top" | b"bottom" => border_edge = None,
                b"border" if current.is_some() => {
                    let value = border.take().unwrap_or_default();
                    if value != Border::default() {
                        current.as_mut().expect("dxf").border = Some(value);
                    }
                }
                b"dxf" if current.is_some() => {
                    if styles.len() < MAX_DXFS {
                        styles.push(DifferentialStyle {
                            style: current.take().unwrap_or_default(),
                            losses: std::mem::take(&mut losses),
                        });
                    } else {
                        current = None;
                        losses.clear();
                    }
                    font = None;
                    fill = None;
                    border = None;
                    border_edge = None;
                }
                b"dxfs" => in_dxfs = false,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    styles
}

fn table_style_region(value: &str) -> Option<TableStyleRegion> {
    match value {
        "wholeTable" => Some(TableStyleRegion::WholeTable),
        "firstColumnStripe" => Some(TableStyleRegion::FirstColumnStripe),
        "secondColumnStripe" => Some(TableStyleRegion::SecondColumnStripe),
        "firstRowStripe" => Some(TableStyleRegion::FirstRowStripe),
        "secondRowStripe" => Some(TableStyleRegion::SecondRowStripe),
        "firstColumn" => Some(TableStyleRegion::FirstColumn),
        "lastColumn" => Some(TableStyleRegion::LastColumn),
        "headerRow" => Some(TableStyleRegion::HeaderRow),
        "totalRow" => Some(TableStyleRegion::TotalRow),
        "firstHeaderCell" => Some(TableStyleRegion::FirstHeaderCell),
        "lastHeaderCell" => Some(TableStyleRegion::LastHeaderCell),
        "firstTotalCell" => Some(TableStyleRegion::FirstTotalCell),
        "lastTotalCell" => Some(TableStyleRegion::LastTotalCell),
        _ => None,
    }
}

fn table_style_region_is_stripe(region: TableStyleRegion) -> bool {
    matches!(
        region,
        TableStyleRegion::FirstColumnStripe
            | TableStyleRegion::SecondColumnStripe
            | TableStyleRegion::FirstRowStripe
            | TableStyleRegion::SecondRowStripe
    )
}

fn parse_table_styles(xml: &str, dxfs: &[DifferentialStyle]) -> HashMap<String, ParsedTableStyle> {
    const MAX_TABLE_STYLES: usize = 4_096;
    const MAX_ELEMENTS_PER_TABLE_STYLE: usize = 64;
    const MAX_TABLE_STRIPE_SIZE: u32 = 1_048_576;
    let mut reader = Reader::from_str(xml);
    let mut in_table_styles = false;
    let mut current_name: Option<String> = None;
    let mut current_elements = 0usize;
    let mut styles = HashMap::<String, ParsedTableStyle>::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"tableStyles" => in_table_styles = true,
                b"tableStyle" if in_table_styles => {
                    current_elements = 0;
                    current_name = attr(&e, b"name").filter(|name| !name.is_empty());
                    if let Some(name) = current_name.clone() {
                        if !styles.contains_key(&name) && styles.len() >= MAX_TABLE_STYLES {
                            current_name = None;
                        } else {
                            let duplicate = styles.contains_key(&name);
                            let parsed = styles.entry(name).or_default();
                            if duplicate {
                                add_differential_loss(
                                    &mut parsed.losses,
                                    StyleLossKind::UnsupportedProperty,
                                    1,
                                );
                            }
                        }
                    }
                    if e.is_empty() {
                        current_name = None;
                    }
                }
                b"tableStyleElement" if current_name.is_some() => {
                    current_elements = current_elements.saturating_add(1);
                    let parsed = styles
                        .get_mut(current_name.as_ref().expect("table style name"))
                        .expect("current table style");
                    if current_elements > MAX_ELEMENTS_PER_TABLE_STYLE {
                        add_differential_loss(&mut parsed.losses, StyleLossKind::LimitExceeded, 1);
                        continue;
                    }
                    let Some(region) = attr(&e, b"type").as_deref().and_then(table_style_region)
                    else {
                        add_differential_loss(
                            &mut parsed.losses,
                            StyleLossKind::UnsupportedProperty,
                            1,
                        );
                        continue;
                    };
                    let stripe_size = if table_style_region_is_stripe(region) {
                        match attr(&e, b"size") {
                            None => 1,
                            Some(value) => match value.parse::<u32>() {
                                Ok(size @ 1..=MAX_TABLE_STRIPE_SIZE) => size,
                                Ok(_) => {
                                    add_differential_loss(
                                        &mut parsed.losses,
                                        StyleLossKind::LimitExceeded,
                                        1,
                                    );
                                    1
                                }
                                Err(_) => {
                                    add_differential_loss(
                                        &mut parsed.losses,
                                        StyleLossKind::UnsupportedProperty,
                                        1,
                                    );
                                    1
                                }
                            },
                        }
                    } else {
                        if attr(&e, b"size").is_some() {
                            add_differential_loss(
                                &mut parsed.losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        1
                    };
                    let Some(dxf) = attr(&e, b"dxfId")
                        .and_then(|value| value.parse::<usize>().ok())
                        .and_then(|index| dxfs.get(index))
                    else {
                        add_differential_loss(
                            &mut parsed.losses,
                            StyleLossKind::MissingReference,
                            1,
                        );
                        continue;
                    };
                    for loss in &dxf.losses {
                        add_differential_loss(&mut parsed.losses, loss.kind, loss.occurrences);
                    }
                    if parsed
                        .definition
                        .insert(region, dxf.style.clone(), stripe_size)
                        .is_some()
                    {
                        add_differential_loss(
                            &mut parsed.losses,
                            StyleLossKind::UnsupportedProperty,
                            1,
                        );
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"tableStyle" => {
                    current_name = None;
                    current_elements = 0;
                }
                b"tableStyles" => in_table_styles = false,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    styles
}

fn built_in_table_style(name: &str, theme: &ThemeColors) -> Option<ParsedTableStyle> {
    const OFFICE_ACCENTS: [Color; 6] = [
        Color::rgb(0x44, 0x72, 0xC4),
        Color::rgb(0xED, 0x7D, 0x31),
        Color::rgb(0xA5, 0xA5, 0xA5),
        Color::rgb(0xFF, 0xC0, 0x00),
        Color::rgb(0x5B, 0x9B, 0xD5),
        Color::rgb(0x70, 0xAD, 0x47),
    ];
    let (family, number) = ["TableStyleLight", "TableStyleMedium", "TableStyleDark"]
        .into_iter()
        .find_map(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|suffix| suffix.parse::<usize>().ok())
                .map(|number| (prefix, number))
        })?;
    let valid = match family {
        "TableStyleLight" => (1..=21).contains(&number),
        "TableStyleMedium" => (1..=28).contains(&number),
        "TableStyleDark" => (1..=11).contains(&number),
        _ => false,
    };
    if !valid {
        return None;
    }
    let accent_index = match family {
        "TableStyleLight" => number.saturating_sub(2) % OFFICE_ACCENTS.len(),
        "TableStyleMedium" => number.saturating_sub(2) % OFFICE_ACCENTS.len(),
        "TableStyleDark" => number.saturating_sub(2) % OFFICE_ACCENTS.len(),
        _ => 0,
    };
    let accent = theme.colors[4 + accent_index].unwrap_or(OFFICE_ACCENTS[accent_index]);
    let white = Color::rgb(0xFF, 0xFF, 0xFF);
    let mut header = CellStyle {
        font: Some(Font::default().bold()),
        ..CellStyle::default()
    };
    match family {
        "TableStyleLight" => {
            header.font.as_mut().expect("table font").color = Some(accent);
            header.border = Some(
                Border::default()
                    .with_bottom(BorderStyle::Medium)
                    .with_color(accent),
            );
        }
        "TableStyleMedium" | "TableStyleDark" => {
            header.font.as_mut().expect("table font").color = Some(white);
            header.fill = Some(accent);
            header.pattern_fill = Some(Fill::solid(accent));
        }
        _ => unreachable!("validated table style family"),
    }
    let mut definition = TableStyleDefinition::default();
    definition.insert(TableStyleRegion::HeaderRow, header, 1);
    definition.insert(
        TableStyleRegion::TotalRow,
        CellStyle {
            font: Some(Font::default().bold()),
            border: Some(
                Border::default()
                    .with_top(BorderStyle::Medium)
                    .with_color(accent),
            ),
            ..CellStyle::default()
        },
        1,
    );
    let emphasis = CellStyle {
        font: Some(Font::default().bold()),
        ..CellStyle::default()
    };
    definition.insert(TableStyleRegion::FirstColumn, emphasis.clone(), 1);
    definition.insert(TableStyleRegion::LastColumn, emphasis, 1);

    let stripe = match family {
        "TableStyleLight" => apply_tint(accent, 0.90),
        "TableStyleMedium" => apply_tint(accent, 0.80),
        "TableStyleDark" => apply_tint(accent, -0.15),
        _ => unreachable!("validated table style family"),
    };
    let stripe_style = CellStyle {
        fill: Some(stripe),
        pattern_fill: Some(Fill::solid(stripe)),
        ..CellStyle::default()
    };
    definition.insert(TableStyleRegion::FirstRowStripe, stripe_style.clone(), 1);
    definition.insert(TableStyleRegion::FirstColumnStripe, stripe_style, 1);
    if family == "TableStyleDark" {
        let body = apply_tint(accent, -0.30);
        definition.insert(
            TableStyleRegion::WholeTable,
            CellStyle {
                font: Some(Font::default().with_color(white)),
                fill: Some(body),
                pattern_fill: Some(Fill::solid(body)),
                ..CellStyle::default()
            },
            1,
        );
    }
    Some(ParsedTableStyle {
        definition,
        // The built-in family recipes preserve the visible cascade regions,
        // but they do not yet encode every per-style Office border/fill
        // variation. Surface that approximation instead of presenting it as
        // exact source fidelity.
        losses: vec![StyleLoss {
            kind: StyleLossKind::UnsupportedProperty,
            occurrences: 1,
        }],
    })
}

#[cfg(test)]
fn built_in_table_header_style(name: &str, theme: &ThemeColors) -> Option<CellStyle> {
    built_in_table_style(name, theme).and_then(|style| {
        style
            .definition
            .get(TableStyleRegion::HeaderRow)
            .map(|element| element.style.clone())
    })
}

fn parse_font_table(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    losses: &mut Vec<StyleLoss>,
) -> (Vec<Font>, Vec<Option<u16>>, bool) {
    #[derive(Clone, Copy)]
    enum ExactSize {
        Absent,
        Valid(u16),
        Invalid,
    }

    impl ExactSize {
        fn observe(self, value: Option<u16>) -> Self {
            match self {
                Self::Absent => value.map_or(Self::Invalid, Self::Valid),
                Self::Valid(_) | Self::Invalid => Self::Invalid,
            }
        }

        fn invalidate(self) -> Self {
            Self::Invalid
        }

        fn value(self) -> Option<u16> {
            match self {
                Self::Valid(value) => Some(value),
                Self::Absent | Self::Invalid => None,
            }
        }
    }

    let mut reader = Reader::from_str(xml);
    let mut depth = 0_usize;
    let mut saw_root = false;
    let mut root_open = false;
    let mut in_fonts = false;
    let mut fonts_depth = None;
    let mut saw_fonts = false;
    let mut font_records_seen = 0_usize;
    let mut provenance_complete = true;
    let mut current: Option<Font> = None;
    let mut current_font_depth = None;
    let mut current_exact_size = ExactSize::Absent;
    let mut current_saw_name = false;
    let mut current_saw_bold = false;
    let mut current_saw_italic = false;
    let mut current_saw_vert_align = false;
    let mut fonts = Vec::new();
    let mut exact_sizes = Vec::new();
    let retain_font = |fonts: &mut Vec<Font>,
                       exact_sizes: &mut Vec<Option<u16>>,
                       font: Font,
                       exact_size: Option<u16>,
                       losses: &mut Vec<StyleLoss>| {
        let previous_len = fonts.len();
        retain_xlsx_style_record(fonts, font, losses);
        if fonts.len() > previous_len {
            exact_sizes.push(exact_size);
        }
    };
    loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let (e, is_empty) = match event {
                    Event::Start(e) => (e, false),
                    Event::Empty(e) => (e, true),
                    _ => unreachable!(),
                };
                let qualified_name = e.name();
                let name = local(qualified_name.as_ref());
                let element_depth = depth;
                if element_depth == 0 {
                    if saw_root || name != b"styleSheet" {
                        provenance_complete = false;
                    }
                    saw_root = true;
                    root_open = !is_empty;
                }
                match name {
                    b"fonts" => {
                        let is_direct_table = element_depth == 1 && root_open;
                        if !is_direct_table || saw_fonts || in_fonts || current.is_some() {
                            provenance_complete = false;
                        }
                        saw_fonts = true;
                        in_fonts = is_direct_table && !is_empty;
                        fonts_depth = in_fonts.then_some(element_depth);
                    }
                    b"font" if in_fonts => {
                        let is_direct_record =
                            fonts_depth.is_some_and(|table| element_depth == table + 1);
                        if !is_direct_record {
                            provenance_complete = false;
                        } else {
                            if current.is_some() {
                                provenance_complete = false;
                                retain_font(
                                    &mut fonts,
                                    &mut exact_sizes,
                                    current.take().unwrap_or_default(),
                                    current_exact_size.value().filter(|_| current_saw_name),
                                    losses,
                                );
                            }
                            font_records_seen = font_records_seen.saturating_add(1);
                            if font_records_seen > MAX_XLSX_STYLE_RECORDS {
                                provenance_complete = false;
                                add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                                current = None;
                                current_font_depth = None;
                            } else {
                                current = Some(Font::default());
                                current_font_depth = Some(element_depth);
                                current_exact_size = ExactSize::Absent;
                                current_saw_name = false;
                                current_saw_bold = false;
                                current_saw_italic = false;
                                current_saw_vert_align = false;
                                if is_empty {
                                    retain_font(
                                        &mut fonts,
                                        &mut exact_sizes,
                                        current.take().unwrap_or_default(),
                                        current_exact_size.value().filter(|_| current_saw_name),
                                        losses,
                                    );
                                    current_font_depth = None;
                                }
                            }
                        }
                    }
                    b"name" if current.is_some() => {
                        if current_font_depth.is_none_or(|font| element_depth != font + 1) {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        if current_saw_name {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current_saw_name = true;
                        let source = match unique_attr(&e, b"val") {
                            Ok(Some(value)) if !value.is_empty() => Some(value),
                            Ok(None) => {
                                current_exact_size = current_exact_size.invalidate();
                                None
                            }
                            Ok(Some(value)) => {
                                current_exact_size = current_exact_size.invalidate();
                                Some(value)
                            }
                            Err(()) => {
                                current_exact_size = current_exact_size.invalidate();
                                attr(&e, b"val")
                            }
                        };
                        current.as_mut().expect("font").name = source;
                    }
                    b"sz" if current.is_some() => {
                        if current_font_depth.is_none_or(|font| element_depth != font + 1) {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        let source = match unique_attr(&e, b"val") {
                            Ok(value) => value,
                            Err(()) => {
                                current_exact_size = current_exact_size.invalidate();
                                attr(&e, b"val")
                            }
                        };
                        if !matches!(current_exact_size, ExactSize::Invalid) {
                            current_exact_size = current_exact_size
                                .observe(source.as_deref().and_then(exact_integral_xlsx_font_size));
                        }
                        current.as_mut().expect("font").size_pt = source
                            .and_then(|value| value.parse::<f32>().ok())
                            .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                    }
                    b"color" if current.is_some() => {
                        current.as_mut().expect("font").color = color_attr(&e, theme, indexed);
                    }
                    b"b" if current.is_some() => {
                        if current_font_depth.is_none_or(|font| element_depth != font + 1)
                            || current_saw_bold
                        {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current_saw_bold = true;
                        let enabled = match unique_attr(&e, b"val") {
                            Ok(None) => true,
                            Ok(Some(value)) => matches!(value.as_str(), "1" | "true" | "on"),
                            Err(()) => false,
                        };
                        if !enabled {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current.as_mut().expect("font").bold = true;
                    }
                    b"i" if current.is_some() => {
                        if current_font_depth.is_none_or(|font| element_depth != font + 1)
                            || current_saw_italic
                        {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current_saw_italic = true;
                        let enabled = match unique_attr(&e, b"val") {
                            Ok(None) => true,
                            Ok(Some(value)) => matches!(value.as_str(), "1" | "true" | "on"),
                            Err(()) => false,
                        };
                        if !enabled {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current.as_mut().expect("font").italic = true;
                    }
                    b"u" if current.is_some() => current.as_mut().expect("font").underline = true,
                    b"strike" if current.is_some() => {
                        current.as_mut().expect("font").strikethrough = true;
                    }
                    b"vertAlign" if current.is_some() => {
                        if current_font_depth.is_none_or(|font| element_depth != font + 1)
                            || current_saw_vert_align
                        {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current_saw_vert_align = true;
                        let source = match unique_attr(&e, b"val") {
                            Ok(value) => value,
                            Err(()) => {
                                current_exact_size = current_exact_size.invalidate();
                                attr(&e, b"val")
                            }
                        };
                        current.as_mut().expect("font").script = match source.as_deref() {
                            Some("superscript") => FormatScript::Superscript,
                            Some("subscript") => FormatScript::Subscript,
                            Some("baseline") => FormatScript::None,
                            _ => {
                                current_exact_size = current_exact_size.invalidate();
                                FormatScript::None
                            }
                        };
                    }
                    _ => {}
                }
                if !is_empty {
                    depth = depth.saturating_add(1);
                }
            }
            Ok(Event::End(e)) => {
                if depth == 0 {
                    provenance_complete = false;
                    continue;
                }
                depth -= 1;
                let qualified_name = e.name();
                let name = local(qualified_name.as_ref());
                let element_depth = depth;
                if element_depth == 0 {
                    if name != b"styleSheet" || !root_open {
                        provenance_complete = false;
                    }
                    root_open = false;
                }
                match name {
                    b"font" if current.is_some() && current_font_depth == Some(element_depth) => {
                        retain_font(
                            &mut fonts,
                            &mut exact_sizes,
                            current.take().unwrap_or_default(),
                            current_exact_size.value().filter(|_| current_saw_name),
                            losses,
                        );
                        current_font_depth = None;
                    }
                    b"font" if in_fonts => {
                        provenance_complete = false;
                    }
                    b"fonts" => {
                        if !in_fonts || fonts_depth != Some(element_depth) || current.is_some() {
                            provenance_complete = false;
                        }
                        in_fonts = false;
                        fonts_depth = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => {
                if depth != 0
                    || !saw_root
                    || root_open
                    || in_fonts
                    || fonts_depth.is_some()
                    || current.is_some()
                    || current_font_depth.is_some()
                {
                    provenance_complete = false;
                }
                break;
            }
            Err(_) => {
                provenance_complete = false;
                exact_sizes.fill(None);
                break;
            }
            _ => {}
        }
    }
    (fonts, exact_sizes, provenance_complete)
}

fn exact_integral_xlsx_font_size(value: &str) -> Option<u16> {
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    let (mantissa, exponent) = match value.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => {
            if exponent.contains(['e', 'E']) {
                return None;
            }
            (mantissa, exponent.parse::<i32>().ok()?)
        }
        None => (value, 0),
    };
    let mantissa = mantissa.strip_prefix('+').unwrap_or(mantissa);
    if mantissa.is_empty() || mantissa.starts_with('-') {
        return None;
    }

    let mut digits = 0_u128;
    let mut saw_digit = false;
    let mut saw_decimal = false;
    let mut fractional_digits = 0_i32;
    for byte in mantissa.bytes() {
        match byte {
            b'0'..=b'9' => {
                saw_digit = true;
                digits = digits
                    .checked_mul(10)?
                    .checked_add(u128::from(byte - b'0'))?;
                if saw_decimal {
                    fractional_digits = fractional_digits.checked_add(1)?;
                }
            }
            b'.' if !saw_decimal => saw_decimal = true,
            _ => return None,
        }
    }
    if !saw_digit {
        return None;
    }

    let decimal_scale = fractional_digits.checked_sub(exponent)?;
    let integral = if decimal_scale >= 0 {
        let divisor = 10_u128.checked_pow(decimal_scale as u32)?;
        if digits % divisor != 0 {
            return None;
        }
        digits / divisor
    } else {
        digits.checked_mul(10_u128.checked_pow(decimal_scale.unsigned_abs())?)?
    };
    let points = u16::try_from(integral).ok()?;
    (1..=MAX_VERIFIED_XLSX_FONT_SIZE_POINTS)
        .contains(&points)
        .then_some(points)
}

fn verified_xlsx_normal_font_size(
    xml: &str,
    fonts: &[Font],
    exact_sizes: &[Option<u16>],
    font_table_complete: bool,
) -> Option<u16> {
    if !font_table_complete {
        return None;
    }

    let mut reader = Reader::from_str(xml);
    let mut depth = 0_usize;
    let mut in_cell_style_xfs = false;
    let mut in_cell_xfs = false;
    let mut in_cell_styles = false;
    let mut cell_style_xfs_depth = None;
    let mut cell_xfs_depth = None;
    let mut cell_styles_depth = None;
    let mut saw_cell_style_xfs = false;
    let mut saw_cell_xfs = false;
    let mut saw_cell_styles = false;
    let mut cell_style_xf_font_ids = Vec::<Option<usize>>::new();
    let mut cell_xf_count = 0_usize;
    let mut cell_style_count = 0_usize;
    let mut first_cell_xf_font_id = None;
    let mut first_cell_xf_style_id = None;
    let mut saw_first_cell_xf = false;
    let mut normal_xf_id = None;
    let mut saw_normal_style = false;
    let mut normal_is_ambiguous = false;

    loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let (e, is_empty) = match event {
                    Event::Start(e) => (e, false),
                    Event::Empty(e) => (e, true),
                    _ => unreachable!(),
                };
                let element_depth = depth;
                match local(e.name().as_ref()) {
                    b"cellStyleXfs" => {
                        if element_depth != 1
                            || saw_cell_style_xfs
                            || in_cell_style_xfs
                            || in_cell_xfs
                            || in_cell_styles
                        {
                            return None;
                        }
                        saw_cell_style_xfs = true;
                        in_cell_style_xfs = !is_empty;
                        cell_style_xfs_depth = in_cell_style_xfs.then_some(element_depth);
                    }
                    b"cellXfs" => {
                        if element_depth != 1
                            || saw_cell_xfs
                            || in_cell_style_xfs
                            || in_cell_xfs
                            || in_cell_styles
                        {
                            return None;
                        }
                        saw_cell_xfs = true;
                        in_cell_xfs = !is_empty;
                        cell_xfs_depth = in_cell_xfs.then_some(element_depth);
                    }
                    b"cellStyles" => {
                        if element_depth != 1
                            || saw_cell_styles
                            || in_cell_style_xfs
                            || in_cell_xfs
                            || in_cell_styles
                        {
                            return None;
                        }
                        saw_cell_styles = true;
                        in_cell_styles = !is_empty;
                        cell_styles_depth = in_cell_styles.then_some(element_depth);
                    }
                    b"xf" if in_cell_style_xfs => {
                        if cell_style_xfs_depth.is_none_or(|table| element_depth != table + 1) {
                            return None;
                        }
                        if cell_style_xf_font_ids.len() >= MAX_XLSX_STYLE_RECORDS {
                            return None;
                        }
                        cell_style_xf_font_ids
                            .push(unique_parsed_attr::<usize>(&e, b"fontId").ok()?);
                    }
                    b"xf" if in_cell_xfs => {
                        if cell_xfs_depth.is_none_or(|table| element_depth != table + 1) {
                            return None;
                        }
                        if cell_xf_count >= MAX_XLSX_STYLE_RECORDS {
                            return None;
                        }
                        cell_xf_count += 1;
                        if !saw_first_cell_xf {
                            saw_first_cell_xf = true;
                            first_cell_xf_font_id =
                                unique_parsed_attr::<usize>(&e, b"fontId").ok()?;
                            first_cell_xf_style_id =
                                unique_parsed_attr::<usize>(&e, b"xfId").ok()?;
                        }
                    }
                    b"cellStyle" if in_cell_styles => {
                        if cell_styles_depth.is_none_or(|table| element_depth != table + 1) {
                            return None;
                        }
                        if cell_style_count >= MAX_XLSX_STYLE_RECORDS {
                            return None;
                        }
                        cell_style_count += 1;
                        let builtin_id = unique_parsed_attr::<u32>(&e, b"builtinId").ok()?;
                        let name = unique_attr(&e, b"name").ok()?;
                        let named_normal = name
                            .as_deref()
                            .is_some_and(|name| name.eq_ignore_ascii_case("Normal"));
                        if named_normal && builtin_id.is_some_and(|id| id != 0) {
                            normal_is_ambiguous = true;
                        }
                        let is_normal =
                            builtin_id == Some(0) || (builtin_id.is_none() && named_normal);
                        if is_normal {
                            let candidate = unique_parsed_attr::<usize>(&e, b"xfId").ok()?;
                            if candidate.is_none() || saw_normal_style {
                                normal_is_ambiguous = true;
                            } else {
                                normal_xf_id = candidate;
                            }
                            saw_normal_style = true;
                        }
                    }
                    _ => {}
                }
                if !is_empty {
                    depth = depth.checked_add(1)?;
                }
            }
            Ok(Event::End(e)) => {
                depth = depth.checked_sub(1)?;
                match local(e.name().as_ref()) {
                    b"cellStyleXfs" => {
                        if !in_cell_style_xfs || cell_style_xfs_depth != Some(depth) {
                            return None;
                        }
                        in_cell_style_xfs = false;
                        cell_style_xfs_depth = None;
                    }
                    b"cellXfs" => {
                        if !in_cell_xfs || cell_xfs_depth != Some(depth) {
                            return None;
                        }
                        in_cell_xfs = false;
                        cell_xfs_depth = None;
                    }
                    b"cellStyles" => {
                        if !in_cell_styles || cell_styles_depth != Some(depth) {
                            return None;
                        }
                        in_cell_styles = false;
                        cell_styles_depth = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => {
                if depth != 0
                    || in_cell_style_xfs
                    || in_cell_xfs
                    || in_cell_styles
                    || cell_style_xfs_depth.is_some()
                    || cell_xfs_depth.is_some()
                    || cell_styles_depth.is_some()
                {
                    return None;
                }
                break;
            }
            Err(_) => return None,
            _ => {}
        }
    }

    if normal_is_ambiguous {
        return None;
    }
    let normal_style_id = normal_xf_id?;
    if first_cell_xf_style_id != Some(normal_style_id) {
        return None;
    }
    let first_font_id = first_cell_xf_font_id?;
    let normal_font_id = cell_style_xf_font_ids.get(normal_style_id)?.as_ref()?;
    let first_font = fonts.get(first_font_id)?;
    let normal_font = fonts.get(*normal_font_id)?;
    let first_points = exact_sizes.get(first_font_id).copied().flatten()?;
    let normal_points = exact_sizes.get(*normal_font_id).copied().flatten()?;
    (first_points == normal_points && first_font == normal_font).then_some(first_points)
}

#[derive(Clone, Copy)]
enum XlsxCellXfFontUse {
    Explicit(bool),
    Implicit { style_xf_id: Option<usize> },
}

#[derive(Clone, Copy)]
struct XlsxCellXfFontCandidate {
    font_id: usize,
    font_use: XlsxCellXfFontUse,
}

fn xlsx_cell_xf_font_candidate(
    e: &quick_xml::events::BytesStart<'_>,
) -> Option<XlsxCellXfFontCandidate> {
    let font_id = unique_parsed_attr::<usize>(e, b"fontId").ok().flatten()?;
    let font_use = match unique_attr(e, b"applyFont") {
        Ok(None) => XlsxCellXfFontUse::Implicit {
            style_xf_id: unique_parsed_attr::<usize>(e, b"xfId").ok()?,
        },
        Ok(Some(value)) => XlsxCellXfFontUse::Explicit(parse_bool_attr(&value)?),
        Err(()) => return None,
    };
    Some(XlsxCellXfFontCandidate { font_id, font_use })
}

fn exact_xlsx_cell_xf_font_size(
    candidate: XlsxCellXfFontCandidate,
    exact_sizes: &[Option<u16>],
    cell_style_xf_count: usize,
) -> Option<u16> {
    // LibreOffice's Xf::importXf/createPattern contract applies an omitted
    // `applyFont` directly when `xfId` is absent. With a parent style XF, the
    // cell font is still effective when it differs and otherwise names the
    // same font as the parent. A missing or invalid parent cannot establish
    // that provenance, so fail closed before using Calc's declared-height path.
    let applies = match candidate.font_use {
        XlsxCellXfFontUse::Explicit(applies) => applies,
        XlsxCellXfFontUse::Implicit { style_xf_id: None } => true,
        XlsxCellXfFontUse::Implicit {
            style_xf_id: Some(style_xf_id),
        } => style_xf_id < cell_style_xf_count,
    };
    if !applies {
        return None;
    }
    exact_sizes.get(candidate.font_id).copied().flatten()
}

fn verified_xlsx_cell_xf_font_sizes(
    xml: &str,
    exact_sizes: &[Option<u16>],
    font_table_complete: bool,
) -> Vec<Option<u16>> {
    if !font_table_complete {
        return Vec::new();
    }

    let mut reader = Reader::from_str(xml);
    let mut depth = 0_usize;
    let mut saw_cell_style_xfs = false;
    let mut saw_cell_xfs = false;
    let mut in_cell_style_xfs = false;
    let mut in_cell_xfs = false;
    let mut cell_style_xfs_depth = None;
    let mut cell_xfs_depth = None;
    let mut current_style_xf_depth = None;
    let mut current_xf_depth = None;
    let mut cell_style_xf_count = 0_usize;
    let mut candidates = Vec::new();

    loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let (e, is_empty) = match event {
                    Event::Start(e) => (e, false),
                    Event::Empty(e) => (e, true),
                    _ => unreachable!(),
                };
                let element_depth = depth;
                match local(e.name().as_ref()) {
                    b"cellStyleXfs" => {
                        if element_depth != 1
                            || saw_cell_style_xfs
                            || in_cell_style_xfs
                            || in_cell_xfs
                        {
                            return Vec::new();
                        }
                        saw_cell_style_xfs = true;
                        in_cell_style_xfs = !is_empty;
                        cell_style_xfs_depth = in_cell_style_xfs.then_some(element_depth);
                    }
                    b"cellXfs" => {
                        if element_depth != 1 || saw_cell_xfs || in_cell_style_xfs || in_cell_xfs {
                            return Vec::new();
                        }
                        saw_cell_xfs = true;
                        in_cell_xfs = !is_empty;
                        cell_xfs_depth = in_cell_xfs.then_some(element_depth);
                    }
                    b"xf" if in_cell_style_xfs => {
                        if cell_style_xfs_depth.is_none_or(|table| element_depth != table + 1)
                            || current_style_xf_depth.is_some()
                            || cell_style_xf_count >= MAX_XLSX_STYLE_RECORDS
                        {
                            return Vec::new();
                        }
                        cell_style_xf_count += 1;
                        if !is_empty {
                            current_style_xf_depth = Some(element_depth);
                        }
                    }
                    b"xf" if in_cell_xfs => {
                        if cell_xfs_depth.is_none_or(|table| element_depth != table + 1)
                            || current_xf_depth.is_some()
                            || candidates.len() >= MAX_XLSX_STYLE_RECORDS
                        {
                            return Vec::new();
                        }
                        candidates.push(xlsx_cell_xf_font_candidate(&e));
                        if !is_empty {
                            current_xf_depth = Some(element_depth);
                        }
                    }
                    _ => {}
                }
                if !is_empty {
                    let Some(next_depth) = depth.checked_add(1) else {
                        return Vec::new();
                    };
                    depth = next_depth;
                }
            }
            Ok(Event::End(e)) => {
                let Some(next_depth) = depth.checked_sub(1) else {
                    return Vec::new();
                };
                depth = next_depth;
                match local(e.name().as_ref()) {
                    b"xf" if in_cell_style_xfs => {
                        if current_style_xf_depth != Some(depth) {
                            return Vec::new();
                        }
                        current_style_xf_depth = None;
                    }
                    b"xf" if in_cell_xfs => {
                        if current_xf_depth != Some(depth) {
                            return Vec::new();
                        }
                        current_xf_depth = None;
                    }
                    b"cellStyleXfs" => {
                        if !in_cell_style_xfs
                            || cell_style_xfs_depth != Some(depth)
                            || current_style_xf_depth.is_some()
                        {
                            return Vec::new();
                        }
                        in_cell_style_xfs = false;
                        cell_style_xfs_depth = None;
                    }
                    b"cellXfs" => {
                        if !in_cell_xfs
                            || cell_xfs_depth != Some(depth)
                            || current_xf_depth.is_some()
                        {
                            return Vec::new();
                        }
                        in_cell_xfs = false;
                        cell_xfs_depth = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => {
                if depth != 0
                    || in_cell_style_xfs
                    || in_cell_xfs
                    || cell_style_xfs_depth.is_some()
                    || cell_xfs_depth.is_some()
                    || current_style_xf_depth.is_some()
                    || current_xf_depth.is_some()
                {
                    return Vec::new();
                }
                break;
            }
            Err(_) => return Vec::new(),
            _ => {}
        }
    }
    candidates
        .into_iter()
        .map(|candidate| {
            candidate.and_then(|candidate| {
                exact_xlsx_cell_xf_font_size(candidate, exact_sizes, cell_style_xf_count)
            })
        })
        .collect()
}

fn format_pattern(value: Option<&str>) -> FormatPattern {
    match value.unwrap_or("none") {
        "solid" => FormatPattern::Solid,
        "mediumGray" => FormatPattern::MediumGray,
        "darkGray" => FormatPattern::DarkGray,
        "lightGray" => FormatPattern::LightGray,
        "darkHorizontal" => FormatPattern::DarkHorizontal,
        "darkVertical" => FormatPattern::DarkVertical,
        "darkDown" => FormatPattern::DarkDown,
        "darkUp" => FormatPattern::DarkUp,
        "darkGrid" => FormatPattern::DarkGrid,
        "darkTrellis" => FormatPattern::DarkTrellis,
        "lightHorizontal" => FormatPattern::LightHorizontal,
        "lightVertical" => FormatPattern::LightVertical,
        "lightDown" => FormatPattern::LightDown,
        "lightUp" => FormatPattern::LightUp,
        "lightGrid" => FormatPattern::LightGrid,
        "lightTrellis" => FormatPattern::LightTrellis,
        "gray125" => FormatPattern::Gray125,
        "gray0625" => FormatPattern::Gray0625,
        _ => FormatPattern::None,
    }
}

fn parse_fill_table(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    losses: &mut Vec<StyleLoss>,
) -> Vec<Fill> {
    let mut reader = Reader::from_str(xml);
    let mut in_fills = false;
    let mut current: Option<Fill> = None;
    let mut fills = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"fills" => in_fills = true,
                b"fill" if in_fills => {
                    if let Some(previous) = current.take() {
                        retain_xlsx_style_record(&mut fills, previous, losses);
                    }
                    if fills.len() >= MAX_XLSX_STYLE_RECORDS {
                        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                        current = None;
                        continue;
                    }
                    current = Some(Fill::default());
                    if e.is_empty() {
                        retain_xlsx_style_record(
                            &mut fills,
                            current.take().unwrap_or_default(),
                            losses,
                        );
                    }
                }
                b"patternFill" if current.is_some() => {
                    current.as_mut().expect("fill").pattern =
                        format_pattern(attr(&e, b"patternType").as_deref());
                }
                b"fgColor" if current.is_some() => {
                    current.as_mut().expect("fill").foreground = color_attr(&e, theme, indexed);
                }
                b"bgColor" if current.is_some() => {
                    current.as_mut().expect("fill").background = color_attr(&e, theme, indexed);
                }
                _ => {}
            },
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"fill" && current.is_some() => {
                retain_xlsx_style_record(&mut fills, current.take().unwrap_or_default(), losses);
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"fills" => in_fills = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    fills
}

#[derive(Clone, Copy)]
enum BorderEdge {
    Left,
    Right,
    Top,
    Bottom,
}

fn border_style(value: Option<&str>) -> BorderStyle {
    match value.unwrap_or("none") {
        "thin" | "hair" | "dotted" | "dashed" | "dashDot" | "dashDotDot" => BorderStyle::Thin,
        "medium" | "mediumDashed" | "mediumDashDot" | "mediumDashDotDot" => BorderStyle::Medium,
        "thick" | "slantDashDot" => BorderStyle::Thick,
        "double" => BorderStyle::Double,
        _ => BorderStyle::None,
    }
}

fn set_border_edge(border: &mut Border, edge: BorderEdge, style: BorderStyle) {
    match edge {
        BorderEdge::Left => border.left = style,
        BorderEdge::Right => border.right = style,
        BorderEdge::Top => border.top = style,
        BorderEdge::Bottom => border.bottom = style,
    }
}

fn set_border_color(border: &mut Border, edge: BorderEdge, color: Color) {
    match edge {
        BorderEdge::Left => border.left_color = Some(color),
        BorderEdge::Right => border.right_color = Some(color),
        BorderEdge::Top => border.top_color = Some(color),
        BorderEdge::Bottom => border.bottom_color = Some(color),
    }
}

fn parse_border_table(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    losses: &mut Vec<StyleLoss>,
) -> Vec<Border> {
    let mut reader = Reader::from_str(xml);
    let mut in_borders = false;
    let mut current: Option<Border> = None;
    let mut edge = None;
    let mut borders = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"borders" => in_borders = true,
                b"border" if in_borders => {
                    if let Some(previous) = current.take() {
                        retain_xlsx_style_record(&mut borders, previous, losses);
                    }
                    if borders.len() >= MAX_XLSX_STYLE_RECORDS {
                        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                        current = None;
                        continue;
                    }
                    current = Some(Border::default());
                    if e.is_empty() {
                        retain_xlsx_style_record(
                            &mut borders,
                            current.take().unwrap_or_default(),
                            losses,
                        );
                    }
                }
                b"left" | b"right" | b"top" | b"bottom" if current.is_some() => {
                    let selected = match local(e.name().as_ref()) {
                        b"left" => BorderEdge::Left,
                        b"right" => BorderEdge::Right,
                        b"top" => BorderEdge::Top,
                        _ => BorderEdge::Bottom,
                    };
                    set_border_edge(
                        current.as_mut().expect("border"),
                        selected,
                        border_style(attr(&e, b"style").as_deref()),
                    );
                    edge = (!e.is_empty()).then_some(selected);
                }
                b"color" if current.is_some() && edge.is_some() => {
                    if let Some(color) = color_attr(&e, theme, indexed) {
                        set_border_color(
                            current.as_mut().expect("border"),
                            edge.expect("edge"),
                            color,
                        );
                    }
                }
                _ => {}
            },
            Ok(Event::End(e))
                if matches!(
                    local(e.name().as_ref()),
                    b"left" | b"right" | b"top" | b"bottom"
                ) =>
            {
                edge = None
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"border" && current.is_some() => {
                retain_xlsx_style_record(&mut borders, current.take().unwrap_or_default(), losses);
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"borders" => in_borders = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    borders
}

fn built_in_num_fmt(id: u16) -> Option<&'static str> {
    format::built_in_format_code(id)
}

fn parse_alignment(e: &quick_xml::events::BytesStart<'_>) -> Alignment {
    let horizontal = match attr(e, b"horizontal").as_deref() {
        Some("left") => Some(HAlign::Left),
        Some("center" | "centerContinuous" | "distributed") => Some(HAlign::Center),
        Some("right") => Some(HAlign::Right),
        _ => None,
    };
    let vertical = match attr(e, b"vertical").as_deref() {
        Some("top") => Some(VAlign::Top),
        Some("center" | "distributed" | "justify") => Some(VAlign::Middle),
        Some("bottom") => Some(VAlign::Bottom),
        _ => None,
    };
    let raw_rotation = attr(e, b"textRotation")
        .and_then(|value| value.parse::<i16>().ok())
        .unwrap_or(0);
    let rotation = if (91..=180).contains(&raw_rotation) {
        90 - raw_rotation
    } else if raw_rotation <= 90 {
        raw_rotation
    } else {
        0
    };
    Alignment {
        horizontal,
        vertical,
        wrap: attr(e, b"wrapText").as_deref().is_some_and(attr_true),
        rotation,
        indent: attr(e, b"indent")
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or(0),
        shrink_to_fit: attr(e, b"shrinkToFit").as_deref().is_some_and(attr_true),
    }
}

fn cell_style_from_xf(
    e: &quick_xml::events::BytesStart<'_>,
    fonts: &[Font],
    fills: &[Fill],
    borders: &[Border],
    custom: &HashMap<u16, String>,
) -> CellStyle {
    let num_fmt_id = attr(e, b"numFmtId")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let font_id = attr(e, b"fontId").and_then(|value| value.parse::<usize>().ok());
    let fill_id = attr(e, b"fillId").and_then(|value| value.parse::<usize>().ok());
    let border_id = attr(e, b"borderId").and_then(|value| value.parse::<usize>().ok());
    CellStyle {
        font: font_id.and_then(|id| fonts.get(id).cloned()),
        fill: None,
        pattern_fill: fill_id.and_then(|id| fills.get(id).copied()),
        border: border_id.and_then(|id| borders.get(id).cloned()),
        num_fmt: custom
            .get(&num_fmt_id)
            .cloned()
            .or_else(|| built_in_num_fmt(num_fmt_id).map(str::to_string)),
        align: None,
        protection: None,
    }
}

fn cell_style_overlay_from_xf(
    e: &quick_xml::events::BytesStart<'_>,
    fonts: &[Font],
    fills: &[Fill],
    borders: &[Border],
    custom: &HashMap<u16, String>,
) -> CellStyleOverlay {
    let num_fmt_id = attr(e, b"numFmtId")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let font_id = attr(e, b"fontId").and_then(|value| value.parse::<usize>().ok());
    let fill_id = attr(e, b"fillId").and_then(|value| value.parse::<usize>().ok());
    let border_id = attr(e, b"borderId").and_then(|value| value.parse::<usize>().ok());
    let applies = |name: &[u8], fallback: bool| {
        attr(e, name)
            .as_deref()
            .and_then(parse_bool_attr)
            .unwrap_or(fallback)
    };
    let replace_font = applies(b"applyFont", font_id.is_some_and(|id| id != 0));
    let replace_fill = applies(b"applyFill", fill_id.is_some_and(|id| id != 0));
    let replace_border = applies(b"applyBorder", border_id.is_some_and(|id| id != 0));
    let replace_num_fmt = applies(b"applyNumberFormat", num_fmt_id != 0);
    CellStyleOverlay {
        style: CellStyle {
            font: replace_font
                .then(|| font_id.and_then(|id| fonts.get(id).cloned()))
                .flatten(),
            fill: None,
            pattern_fill: replace_fill
                .then(|| fill_id.and_then(|id| fills.get(id).copied()))
                .flatten(),
            border: replace_border
                .then(|| border_id.and_then(|id| borders.get(id).cloned()))
                .flatten(),
            num_fmt: replace_num_fmt
                .then(|| {
                    custom
                        .get(&num_fmt_id)
                        .cloned()
                        .or_else(|| built_in_num_fmt(num_fmt_id).map(str::to_string))
                })
                .flatten(),
            align: None,
            protection: None,
        },
        replace_font,
        replace_fill,
        replace_border,
        replace_num_fmt,
        replace_alignment: applies(b"applyAlignment", false),
        replace_protection: applies(b"applyProtection", false),
    }
}

fn parse_cell_styles(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    custom: &HashMap<u16, String>,
    fonts: &[Font],
    losses: &mut Vec<StyleLoss>,
) -> (Vec<CellStyle>, Vec<CellStyleOverlay>) {
    let fills = parse_fill_table(xml, theme, indexed, losses);
    let borders = parse_border_table(xml, theme, indexed, losses);
    let mut reader = Reader::from_str(xml);
    let mut in_cell_xfs = false;
    let mut current: Option<(CellStyle, CellStyleOverlay, Option<bool>, Option<bool>)> = None;
    let mut styles = Vec::new();
    let mut overlays = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"cellXfs" => in_cell_xfs = true,
                b"xf" if in_cell_xfs => {
                    if styles.len() >= MAX_XLSX_STYLE_RECORDS {
                        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                        current = None;
                        continue;
                    }
                    current = Some((
                        cell_style_from_xf(&e, fonts, &fills, &borders, custom),
                        cell_style_overlay_from_xf(&e, fonts, &fills, &borders, custom),
                        attr(&e, b"applyAlignment")
                            .as_deref()
                            .and_then(parse_bool_attr),
                        attr(&e, b"applyProtection")
                            .as_deref()
                            .and_then(parse_bool_attr),
                    ));
                }
                b"alignment" if current.is_some() => {
                    let alignment = parse_alignment(&e);
                    let (resolved, overlay, apply_alignment, _) = current.as_mut().expect("xf");
                    resolved.align = Some(alignment.clone());
                    if *apply_alignment != Some(false) {
                        overlay.style.align = Some(alignment);
                        overlay.replace_alignment = true;
                    }
                }
                b"protection" if current.is_some() => {
                    let protection = CellProtection {
                        locked: attr(&e, b"locked").as_deref().and_then(parse_bool_attr),
                        hidden: attr(&e, b"hidden").as_deref().is_some_and(attr_true),
                    };
                    let (resolved, overlay, _, apply_protection) = current.as_mut().expect("xf");
                    resolved.protection = Some(protection.clone());
                    if *apply_protection != Some(false) {
                        overlay.style.protection = Some(protection);
                        overlay.replace_protection = true;
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"xf" if in_cell_xfs => {
                    retain_cell_xf_style(
                        &mut styles,
                        &mut overlays,
                        cell_style_from_xf(&e, fonts, &fills, &borders, custom),
                        cell_style_overlay_from_xf(&e, fonts, &fills, &borders, custom),
                        losses,
                    );
                }
                b"alignment" if current.is_some() => {
                    let alignment = parse_alignment(&e);
                    let (resolved, overlay, apply_alignment, _) = current.as_mut().expect("xf");
                    resolved.align = Some(alignment.clone());
                    if *apply_alignment != Some(false) {
                        overlay.style.align = Some(alignment);
                        overlay.replace_alignment = true;
                    }
                }
                b"protection" if current.is_some() => {
                    let protection = CellProtection {
                        locked: attr(&e, b"locked").as_deref().and_then(parse_bool_attr),
                        hidden: attr(&e, b"hidden").as_deref().is_some_and(attr_true),
                    };
                    let (resolved, overlay, _, apply_protection) = current.as_mut().expect("xf");
                    resolved.protection = Some(protection.clone());
                    if *apply_protection != Some(false) {
                        overlay.style.protection = Some(protection);
                        overlay.replace_protection = true;
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"xf" && current.is_some() => {
                let (style, overlay, _, _) = current.take().expect("xf");
                retain_cell_xf_style(&mut styles, &mut overlays, style, overlay, losses);
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"cellXfs" => in_cell_xfs = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    (styles, overlays)
}

/// A worksheet's visibility, from the `<sheet state>` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Visibility {
    /// `state` absent or `"visible"`.
    #[default]
    Visible,
    /// `state="hidden"` — unhideable via the Excel UI.
    Hidden,
    /// `state="veryHidden"` — unhideable except via VBA.
    VeryHidden,
}

/// One `<sheet>` entry: its name, relationship id, and visibility.
struct SheetRef {
    name: String,
    rid: String,
    visibility: Visibility,
}

/// A sheet-local built-in defined name, such as print area, print titles, or
/// autofilter metadata.
struct SheetDefinedName {
    local_sheet_id: usize,
    name: String,
    refers_to: String,
}

/// The workbook-globals parse: ordered sheet refs, the 1904 flag, and the
/// workbook-global defined names.
struct ParsedWorkbook {
    sheets: Vec<SheetRef>,
    date1904: bool,
    structure_protected: bool,
    active_sheet: Option<usize>,
    defined_names: Vec<(String, String)>,
    local_defined_names: Vec<crate::LocalDefinedName>,
    sheet_defined_names: Vec<SheetDefinedName>,
}

enum DefinedNameCapture {
    GlobalUser(String),
    LocalUser { local_sheet_id: usize, name: String },
    LocalBuiltin { local_sheet_id: usize, name: String },
}

/// `<workbook>`: `<workbookPr date1904/>` + ordered `<sheet name state r:id/>` +
/// `<definedNames><definedName name>refers_to</definedName></definedNames>`. The
/// workbook-global user names are surfaced through [`Workbook::defined_names`].
/// Selected sheet-local built-ins are kept separately for sheet metadata; other
/// built-ins remain internal.
fn parse_workbook(xml: &str) -> ParsedWorkbook {
    let mut r = Reader::from_str(xml);
    let mut sheets = Vec::new();
    let mut date1904 = false;
    let mut structure_protected = false;
    let mut active_sheet = None;
    let mut defined_names = Vec::new();
    let mut raw_local_defined_names = Vec::new();
    let mut sheet_defined_names = Vec::new();
    // Open `<definedName>` capture: (name, accumulated refers-to text).
    let mut cur_name: Option<DefinedNameCapture> = None;
    let mut cur_refers = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"workbookPr" => {
                    if let Some(v) = attr(&e, b"date1904") {
                        date1904 = v == "1" || v.eq_ignore_ascii_case("true");
                    }
                }
                b"workbookProtection"
                    if attr(&e, b"lockStructure").as_deref().is_some_and(attr_true) =>
                {
                    structure_protected = true;
                }
                b"workbookView" if active_sheet.is_none() => {
                    active_sheet = attr(&e, b"activeTab").and_then(|s| s.parse::<usize>().ok());
                }
                b"sheet" => {
                    let name = attr(&e, b"name").unwrap_or_default();
                    let rid = attr(&e, b"id").unwrap_or_default(); // r:id → local "id"
                    let visibility = match attr(&e, b"state").as_deref() {
                        Some(s) if s.eq_ignore_ascii_case("hidden") => Visibility::Hidden,
                        Some(s) if s.eq_ignore_ascii_case("veryHidden") => Visibility::VeryHidden,
                        _ => Visibility::Visible,
                    };
                    sheets.push(SheetRef {
                        name,
                        rid,
                        visibility,
                    });
                }
                b"definedName" => {
                    // Capture workbook-global user names only: skip built-in
                    // `_xlnm.*` names from the workbook-global list. Selected
                    // sheet-local built-ins are kept for sheet metadata.
                    let local_sheet_id =
                        attr(&e, b"localSheetId").and_then(|s| s.parse::<usize>().ok());
                    cur_name = match (attr(&e, b"name"), local_sheet_id) {
                        (Some(n), None) if !n.starts_with("_xlnm.") => {
                            Some(DefinedNameCapture::GlobalUser(n))
                        }
                        (Some(n), Some(local_sheet_id))
                            if matches!(
                                n.as_str(),
                                "_xlnm.Print_Area" | "_xlnm.Print_Titles" | "_xlnm._FilterDatabase"
                            ) =>
                        {
                            Some(DefinedNameCapture::LocalBuiltin {
                                local_sheet_id,
                                name: n,
                            })
                        }
                        (Some(n), Some(local_sheet_id)) if !n.starts_with("_xlnm.") => {
                            Some(DefinedNameCapture::LocalUser {
                                local_sheet_id,
                                name: n,
                            })
                        }
                        _ => None,
                    };
                    cur_refers.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if cur_name.is_some() => cur_refers.push_str(&text_of(&t)),
            Ok(Event::GeneralRef(reference)) if cur_name.is_some() => {
                append_general_ref(&mut cur_refers, &reference);
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"definedName" => {
                if let Some(name) = cur_name.take() {
                    match name {
                        DefinedNameCapture::GlobalUser(name) => {
                            defined_names.push((name, std::mem::take(&mut cur_refers)));
                        }
                        DefinedNameCapture::LocalUser {
                            local_sheet_id,
                            name,
                        } => raw_local_defined_names.push((
                            local_sheet_id,
                            name,
                            std::mem::take(&mut cur_refers),
                        )),
                        DefinedNameCapture::LocalBuiltin {
                            local_sheet_id,
                            name,
                        } => {
                            sheet_defined_names.push(SheetDefinedName {
                                local_sheet_id,
                                name,
                                refers_to: std::mem::take(&mut cur_refers),
                            });
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    let local_defined_names = raw_local_defined_names
        .into_iter()
        .filter_map(|(sheet_index, name, refers_to)| {
            sheets
                .get(sheet_index)
                .map(|sheet| crate::LocalDefinedName {
                    sheet: sheet.name.clone(),
                    name,
                    refers_to,
                })
        })
        .collect();
    ParsedWorkbook {
        sheets,
        date1904,
        structure_protected,
        active_sheet,
        defined_names,
        local_defined_names,
        sheet_defined_names,
    }
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

/// Find the `commentsN.xml` target in a worksheet's rels by its relationship
/// `Type` (`.../officeDocument/2006/relationships/comments`). Returns the raw
/// `Target` (typically `../comments1.xml`), to be resolved against the worksheet
/// path by [`normalize_part_target`].
fn comments_target(xml: &str) -> Option<String> {
    match unique_internal_relationship_target(xml, "comments") {
        RelationshipTarget::Internal(target) => Some(target),
        RelationshipTarget::Missing | RelationshipTarget::Invalid => None,
    }
}

/// Collect every `table{N}.xml` target in a worksheet's rels by its relationship
/// `Type` (`.../officeDocument/2006/relationships/table`). Returns the raw
/// `Target`s (typically `../tables/table1.xml`), each to be resolved against the
/// worksheet path by [`normalize_part_target`].
fn table_targets(xml: &str) -> Vec<String> {
    let Some(relationships) = parse_ooxml_relationships(xml) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for relationship in relationships {
        if relationship
            .rel_type
            .as_deref()
            .is_some_and(|value| relationship_type_matches(value, "table"))
        {
            if relationship.external {
                return Vec::new();
            }
            out.push(relationship.target);
        }
    }
    out
}

/// `xl/tables/table{N}.xml`: `<table name displayName ref="A1:C3">` with a
/// `<tableColumns><tableColumn name/>…>` list. Parses into a [`Table`] (range,
/// name, header column names, style). Returns `None` if the `ref` is unparseable.
const MAX_XLSX_DRAWINGS: usize = 16_384;
const MAX_XLSX_DRAWING_TEXT: usize = 4_096;
const MAX_XLSX_DRAWING_NUMBER_TEXT: usize = 128;
const MAX_XLSX_CHARTS_PER_WORKBOOK: usize = 16_384;

pub(crate) struct ChartImportBudget {
    pub(crate) charts_remaining: usize,
    pub(crate) cache_points_remaining: usize,
    pub(crate) series_remaining: usize,
    pub(crate) xml_work_remaining: usize,
    pub(crate) xml_work_limit: usize,
}

impl Default for ChartImportBudget {
    fn default() -> Self {
        Self {
            charts_remaining: MAX_XLSX_CHARTS_PER_WORKBOOK,
            cache_points_remaining: MAX_XLSX_CHART_CACHE_POINTS_PER_WORKBOOK,
            series_remaining: MAX_XLSX_CHART_SERIES_PER_WORKBOOK,
            xml_work_remaining: MAX_XLSX_CHART_XML_WORK_BYTES_PER_WORKBOOK,
            xml_work_limit: MAX_XLSX_CHART_XML_WORK_BYTES_PER_WORKBOOK,
        }
    }
}

impl ChartImportBudget {
    pub(crate) fn reserve_chart(&mut self) -> bool {
        if self.charts_remaining == 0 {
            false
        } else {
            self.charts_remaining -= 1;
            true
        }
    }

    pub(crate) fn reserve_xml_work(&mut self, work: usize) -> bool {
        if work > self.xml_work_remaining {
            false
        } else {
            self.xml_work_remaining -= work;
            true
        }
    }

    pub(crate) fn reconcile_xml_work(&mut self, declared: usize, actual: usize) -> bool {
        if actual > declared {
            self.reserve_xml_work(actual - declared)
        } else {
            self.xml_work_remaining = self
                .xml_work_remaining
                .saturating_add(declared - actual)
                .min(self.xml_work_limit);
            true
        }
    }
}

struct DrawingRef {
    kind: DrawingRefKind,
    rid: Option<String>,
    from: (u32, u16),
    to: Option<(u32, u16)>,
    metadata: DrawingMetadata,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DrawingRefKind {
    Image,
    Chart,
    Shape,
}

#[derive(Clone, Copy)]
enum AnchorSection {
    From,
    To,
}

#[derive(Clone, Copy)]
enum AnchorField {
    Row,
    Col,
    RowOffset,
    ColOffset,
}

fn add_drawing_loss(losses: &mut Vec<StyleLoss>, kind: StyleLossKind, occurrences: u32) {
    if occurrences == 0 {
        return;
    }
    if let Some(loss) = losses.iter_mut().find(|loss| loss.kind == kind) {
        loss.occurrences = loss.occurrences.saturating_add(occurrences);
    } else {
        losses.push(StyleLoss { kind, occurrences });
    }
}

fn truncate_drawing_text(value: &mut String, max: usize) -> bool {
    if value.len() <= max {
        return false;
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    true
}

fn bounded_drawing_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    losses: &mut Vec<StyleLoss>,
) -> Option<String> {
    attr(e, key).map(|mut value| {
        if truncate_drawing_text(&mut value, MAX_XLSX_DRAWING_TEXT) {
            add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
        }
        value
    })
}

fn append_bounded_drawing_ref(
    out: &mut String,
    reference: &BytesRef<'_>,
    max: usize,
    losses: &mut Vec<StyleLoss>,
) {
    if out.len() >= max {
        add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
        return;
    }
    match reference.resolve_char_ref() {
        Ok(Some(ch)) => out.push(ch),
        Ok(None) => {
            if let Ok(name) = reference.decode() {
                if let Some(value) = quick_xml::escape::resolve_xml_entity(&name) {
                    out.push_str(value);
                }
            }
        }
        Err(_) => {}
    }
    if truncate_drawing_text(out, max) {
        add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
    }
}

fn drawing_anchor_behavior(
    element: &[u8],
    e: &quick_xml::events::BytesStart<'_>,
) -> DrawingAnchorBehavior {
    match element {
        b"absoluteAnchor" => DrawingAnchorBehavior::Absolute,
        b"oneCellAnchor" => DrawingAnchorBehavior::MoveOnly,
        b"twoCellAnchor" => match attr(e, b"editAs").as_deref() {
            Some("absolute") => DrawingAnchorBehavior::Absolute,
            Some("oneCell") => DrawingAnchorBehavior::MoveOnly,
            _ => DrawingAnchorBehavior::MoveAndSize,
        },
        _ => DrawingAnchorBehavior::MoveAndSize,
    }
}

fn drawing_crop(e: &quick_xml::events::BytesStart<'_>) -> DrawingCrop {
    let edge = |name| {
        attr(e, name)
            .and_then(|value| value.parse::<u32>().ok())
            .map(|value| value.saturating_mul(10).min(1_000_000))
            .unwrap_or(0)
    };
    DrawingCrop {
        left_ppm: edge(b"l"),
        top_ppm: edge(b"t"),
        right_ppm: edge(b"r"),
        bottom_ppm: edge(b"b"),
    }
}

fn parse_drawing_refs_bounded(xml: &str, losses: &mut Vec<StyleLoss>) -> Vec<DrawingRef> {
    const XLSX_MAX_ROW: i64 = 1_048_575;
    const XLSX_MAX_COL: i64 = 16_383;

    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut current: Option<DrawingRef> = None;
    let mut anchor_depth = 0usize;
    let mut anchor_requires_from = false;
    let mut anchor_requires_to = false;
    let mut section: Option<AnchorSection> = None;
    let mut field: Option<AnchorField> = None;
    let mut field_text = String::new();
    let mut from_row_seen = false;
    let mut from_col_seen = false;
    let mut to_row_seen = false;
    let mut to_col_seen = false;
    let mut from_offset = (0i64, 0i64);
    let mut to_offset = (0i64, 0i64);
    let mut from_row_offset_seen = false;
    let mut from_col_offset_seen = false;
    let mut to_row_offset_seen = false;
    let mut to_col_offset_seen = false;
    let mut desc_depth = 0usize;
    let mut desc_text = String::new();

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let local_name = local(name.as_ref());
                if matches!(
                    local_name,
                    b"twoCellAnchor" | b"oneCellAnchor" | b"absoluteAnchor"
                ) {
                    if current.is_some() {
                        anchor_depth = anchor_depth.saturating_add(1);
                        add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        continue;
                    }
                    if out.len() >= MAX_XLSX_DRAWINGS {
                        add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                        break;
                    }
                    anchor_depth = 1;
                    anchor_requires_from = local_name != b"absoluteAnchor";
                    anchor_requires_to = local_name == b"twoCellAnchor";
                    current = Some(DrawingRef {
                        kind: DrawingRefKind::Shape,
                        rid: None,
                        from: (0, 0),
                        to: None,
                        metadata: DrawingMetadata {
                            behavior: drawing_anchor_behavior(local_name, &e),
                            z_order: Some(out.len().min(i32::MAX as usize) as i32),
                            ..Default::default()
                        },
                    });
                    section = None;
                    field = None;
                    field_text.clear();
                    from_row_seen = false;
                    from_col_seen = false;
                    to_row_seen = false;
                    to_col_seen = false;
                    from_offset = (0, 0);
                    to_offset = (0, 0);
                    from_row_offset_seen = false;
                    from_col_offset_seen = false;
                    to_row_offset_seen = false;
                    to_col_offset_seen = false;
                    desc_depth = 0;
                    desc_text.clear();
                    continue;
                }
                if current.is_none() || anchor_depth > 1 {
                    continue;
                }
                match local_name {
                    b"from" => section = Some(AnchorSection::From),
                    b"to" => section = Some(AnchorSection::To),
                    b"row" => {
                        field = Some(AnchorField::Row);
                        field_text.clear();
                    }
                    b"col" => {
                        field = Some(AnchorField::Col);
                        field_text.clear();
                    }
                    b"rowOff" => {
                        field = Some(AnchorField::RowOffset);
                        field_text.clear();
                    }
                    b"colOff" => {
                        field = Some(AnchorField::ColOffset);
                        field_text.clear();
                    }
                    b"pic" => current.as_mut().expect("drawing").kind = DrawingRefKind::Image,
                    b"blip" => {
                        let item = current.as_mut().expect("drawing");
                        if item.kind == DrawingRefKind::Image && item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"embed", losses);
                        }
                    }
                    b"chart" => {
                        let item = current.as_mut().expect("drawing");
                        item.kind = DrawingRefKind::Chart;
                        if item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"id", losses);
                        }
                    }
                    b"cNvPr" => {
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.name.is_none() {
                            item.metadata.name = bounded_drawing_attr(&e, b"name", losses);
                        }
                        if item.metadata.alt_text.is_none() {
                            item.metadata.alt_text = bounded_drawing_attr(&e, b"descr", losses)
                                .or_else(|| bounded_drawing_attr(&e, b"title", losses));
                        }
                    }
                    b"xfrm" => {
                        current.as_mut().expect("drawing").metadata.rotation_mdeg =
                            attr(&e, b"rot")
                                .and_then(|value| value.parse::<i32>().ok())
                                .map(|value| value / 60);
                    }
                    b"ext"
                        if !anchor_requires_to
                            || current.as_ref().is_some_and(|item| {
                                item.metadata.behavior != DrawingAnchorBehavior::MoveAndSize
                            }) =>
                    {
                        let width = attr(&e, b"cx").and_then(|value| value.parse::<u64>().ok());
                        let height = attr(&e, b"cy").and_then(|value| value.parse::<u64>().ok());
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.absolute_size_emu.is_none() {
                            item.metadata.absolute_size_emu = width.zip(height);
                        }
                        if width.is_some() ^ height.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"pos" => {
                        let x = attr(&e, b"x").and_then(|value| value.parse::<i64>().ok());
                        let y = attr(&e, b"y").and_then(|value| value.parse::<i64>().ok());
                        current.as_mut().expect("drawing").metadata.from_offset_emu = x.zip(y);
                        if x.is_some() ^ y.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"srcRect" => {
                        current.as_mut().expect("drawing").metadata.crop = Some(drawing_crop(&e));
                    }
                    b"desc" => {
                        desc_depth = 1;
                        desc_text.clear();
                    }
                    _ if desc_depth > 0 => desc_depth += 1,
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if current.is_some() && anchor_depth == 1 => {
                match local(e.name().as_ref()) {
                    b"pic" => current.as_mut().expect("drawing").kind = DrawingRefKind::Image,
                    b"blip" => {
                        let item = current.as_mut().expect("drawing");
                        if item.kind == DrawingRefKind::Image && item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"embed", losses);
                        }
                    }
                    b"chart" => {
                        let item = current.as_mut().expect("drawing");
                        item.kind = DrawingRefKind::Chart;
                        if item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"id", losses);
                        }
                    }
                    b"cNvPr" => {
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.name.is_none() {
                            item.metadata.name = bounded_drawing_attr(&e, b"name", losses);
                        }
                        if item.metadata.alt_text.is_none() {
                            item.metadata.alt_text = bounded_drawing_attr(&e, b"descr", losses)
                                .or_else(|| bounded_drawing_attr(&e, b"title", losses));
                        }
                    }
                    b"xfrm" => {
                        current.as_mut().expect("drawing").metadata.rotation_mdeg =
                            attr(&e, b"rot")
                                .and_then(|value| value.parse::<i32>().ok())
                                .map(|value| value / 60);
                    }
                    b"ext"
                        if !anchor_requires_to
                            || current.as_ref().is_some_and(|item| {
                                item.metadata.behavior != DrawingAnchorBehavior::MoveAndSize
                            }) =>
                    {
                        let width = attr(&e, b"cx").and_then(|value| value.parse::<u64>().ok());
                        let height = attr(&e, b"cy").and_then(|value| value.parse::<u64>().ok());
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.absolute_size_emu.is_none() {
                            item.metadata.absolute_size_emu = width.zip(height);
                        }
                        if width.is_some() ^ height.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"pos" => {
                        let x = attr(&e, b"x").and_then(|value| value.parse::<i64>().ok());
                        let y = attr(&e, b"y").and_then(|value| value.parse::<i64>().ok());
                        current.as_mut().expect("drawing").metadata.from_offset_emu = x.zip(y);
                        if x.is_some() ^ y.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"srcRect" => {
                        current.as_mut().expect("drawing").metadata.crop = Some(drawing_crop(&e));
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) if field.is_some() => {
                field_text.push_str(&text_of(&t));
                if truncate_drawing_text(&mut field_text, MAX_XLSX_DRAWING_NUMBER_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::Text(t)) if desc_depth > 0 => {
                desc_text.push_str(&text_of(&t));
                if truncate_drawing_text(&mut desc_text, MAX_XLSX_DRAWING_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::GeneralRef(reference)) if field.is_some() => {
                append_bounded_drawing_ref(
                    &mut field_text,
                    &reference,
                    MAX_XLSX_DRAWING_NUMBER_TEXT,
                    losses,
                );
            }
            Ok(Event::GeneralRef(reference)) if desc_depth > 0 => {
                append_bounded_drawing_ref(
                    &mut desc_text,
                    &reference,
                    MAX_XLSX_DRAWING_TEXT,
                    losses,
                );
            }
            Ok(Event::CData(t)) if field.is_some() => {
                field_text.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                if truncate_drawing_text(&mut field_text, MAX_XLSX_DRAWING_NUMBER_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::CData(t)) if desc_depth > 0 => {
                desc_text.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                if truncate_drawing_text(&mut desc_text, MAX_XLSX_DRAWING_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let local_name = local(name.as_ref());
                if matches!(
                    local_name,
                    b"twoCellAnchor" | b"oneCellAnchor" | b"absoluteAnchor"
                ) && current.is_some()
                {
                    if anchor_depth > 1 {
                        anchor_depth -= 1;
                        continue;
                    }
                    if let Some(mut item) = current.take() {
                        if from_row_offset_seen || from_col_offset_seen {
                            item.metadata.from_offset_emu = Some(from_offset);
                            if from_row_offset_seen ^ from_col_offset_seen {
                                add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                            }
                        }
                        if to_row_offset_seen || to_col_offset_seen {
                            item.metadata.to_offset_emu = Some(to_offset);
                            if to_row_offset_seen ^ to_col_offset_seen {
                                add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                            }
                        }
                        if anchor_requires_from && !(from_row_seen && from_col_seen) {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                        if anchor_requires_to && !(to_row_seen && to_col_seen) {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                        if from_row_seen && from_col_seen {
                            item.metadata.from_cell = Some(item.from);
                        }
                        if to_row_seen && to_col_seen {
                            item.metadata.to_cell = item.to;
                        }
                        out.push(item);
                    }
                    anchor_depth = 0;
                    section = None;
                    field = None;
                    desc_depth = 0;
                    continue;
                }
                if current.is_none() || anchor_depth > 1 {
                    continue;
                }
                match local_name {
                    b"row" | b"col" | b"rowOff" | b"colOff" => {
                        if let (Some(section), Some(field), Ok(value)) =
                            (section, field, field_text.trim().parse::<i64>())
                        {
                            let item = current.as_mut().expect("drawing");
                            match (section, field) {
                                (AnchorSection::From, AnchorField::Row) => {
                                    item.from.0 = value.clamp(0, XLSX_MAX_ROW) as u32;
                                    from_row_seen = true;
                                }
                                (AnchorSection::From, AnchorField::Col) => {
                                    item.from.1 = value.clamp(0, XLSX_MAX_COL) as u16;
                                    from_col_seen = true;
                                }
                                (AnchorSection::To, AnchorField::Row) => {
                                    item.to.get_or_insert((0, 0)).0 =
                                        value.clamp(0, XLSX_MAX_ROW) as u32;
                                    to_row_seen = true;
                                }
                                (AnchorSection::To, AnchorField::Col) => {
                                    item.to.get_or_insert((0, 0)).1 =
                                        value.clamp(0, XLSX_MAX_COL) as u16;
                                    to_col_seen = true;
                                }
                                (AnchorSection::From, AnchorField::RowOffset) => {
                                    from_offset.1 = value;
                                    from_row_offset_seen = true;
                                }
                                (AnchorSection::From, AnchorField::ColOffset) => {
                                    from_offset.0 = value;
                                    from_col_offset_seen = true;
                                }
                                (AnchorSection::To, AnchorField::RowOffset) => {
                                    to_offset.1 = value;
                                    to_row_offset_seen = true;
                                }
                                (AnchorSection::To, AnchorField::ColOffset) => {
                                    to_offset.0 = value;
                                    to_col_offset_seen = true;
                                }
                            }
                        } else if field.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                        field = None;
                        field_text.clear();
                    }
                    b"from" | b"to" => section = None,
                    b"desc" if desc_depth > 0 => {
                        if current
                            .as_ref()
                            .expect("drawing")
                            .metadata
                            .alt_text
                            .is_none()
                            && !desc_text.trim().is_empty()
                        {
                            current.as_mut().expect("drawing").metadata.alt_text =
                                Some(desc_text.trim().to_string());
                        }
                        desc_depth = 0;
                    }
                    _ if desc_depth > 0 => desc_depth -= 1,
                    _ => {}
                }
            }
            Ok(Event::Eof) => {
                if current.is_some() {
                    add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                }
                break;
            }
            Err(_) => {
                add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                break;
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
fn parse_drawing_refs(xml: &str) -> Vec<DrawingRef> {
    parse_drawing_refs_bounded(xml, &mut Vec::new())
}

fn image_format(path: &str) -> Option<ImageFmt> {
    match path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some(ImageFmt::Png),
        Some("jpg" | "jpeg") => Some(ImageFmt::Jpeg),
        _ => None,
    }
}

enum DrawingPartRead {
    Missing,
    LimitExceeded,
    Data(Vec<u8>),
}

fn drawing_part_declared_size(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    path: &str,
) -> Option<u64> {
    let index = part_index(zip, path)?;
    zip.by_index(index).ok().map(|file| file.size())
}

fn drawing_part_bytes(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    path: &str,
    max: u64,
) -> DrawingPartRead {
    let Some(index) = part_index(zip, path) else {
        return DrawingPartRead::Missing;
    };
    let Ok(file) = zip.by_index(index) else {
        return DrawingPartRead::Missing;
    };
    if file.size() > max {
        return DrawingPartRead::LimitExceeded;
    }
    let mut data = Vec::new();
    if file
        .take(max.saturating_add(1))
        .read_to_end(&mut data)
        .is_err()
    {
        return DrawingPartRead::Missing;
    }
    if data.len() as u64 > max {
        DrawingPartRead::LimitExceeded
    } else {
        DrawingPartRead::Data(data)
    }
}

fn retain_unrepresented_drawing(mut sidecar: DrawingMetadata, metadata: &mut Vec<DrawingMetadata>) {
    sidecar.kind = DrawingObjectKind::Shape;
    sidecar.object_index = 0;
    metadata.push(sidecar);
}

type DrawingReadResult = (Vec<Image>, Vec<Chart>, Vec<DrawingMetadata>, Vec<StyleLoss>);

fn read_sheet_drawings(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    sheet_path: &str,
    sheet_rels_xml: Option<&str>,
    theme: &ThemeColors,
    chart_budget: &mut ChartImportBudget,
) -> DrawingReadResult {
    const MAX_IMAGE_PART: u64 = 64 << 20;
    const MAX_IMAGE_TOTAL: usize = 256 << 20;
    let drawing_target = match sheet_rels_xml
        .map(|xml| unique_internal_relationship_target(xml, "drawing"))
        .unwrap_or(RelationshipTarget::Missing)
    {
        RelationshipTarget::Internal(target) => target,
        RelationshipTarget::Missing => {
            return (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        }
        RelationshipTarget::Invalid => {
            return (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vec![StyleLoss {
                    kind: StyleLossKind::DrawingMetadataPartial,
                    occurrences: 1,
                }],
            );
        }
    };
    let drawing_path = normalize_part_target(sheet_path, &drawing_target);
    let Some(drawing_xml) = part(zip, &drawing_path) else {
        return (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![StyleLoss {
                kind: StyleLossKind::DrawingMetadataPartial,
                occurrences: 1,
            }],
        );
    };
    let mut losses = Vec::new();
    let refs = parse_drawing_refs_bounded(&drawing_xml, &mut losses);
    let drawing_rels =
        part(zip, &sheet_rels_path(&drawing_path)).and_then(|xml| parse_ooxml_relationships(&xml));
    let mut images = Vec::new();
    let mut charts = Vec::new();
    let mut metadata = Vec::new();
    let mut image_bytes = 0usize;

    for drawing in refs {
        match drawing.kind {
            DrawingRefKind::Image => {
                let target =
                    match drawing
                        .rid
                        .as_deref()
                        .map_or(RelationshipTarget::Missing, |rid| {
                            drawing_rels.as_deref().map_or(
                                RelationshipTarget::Invalid,
                                |relationships| {
                                    internal_relationship_target_by_id(relationships, rid, "image")
                                },
                            )
                        }) {
                        RelationshipTarget::Internal(target) => target,
                        RelationshipTarget::Missing | RelationshipTarget::Invalid => {
                            retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                            add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                            continue;
                        }
                    };
                let media_path = normalize_part_target(&drawing_path, &target);
                let Some(format) = image_format(&media_path) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    continue;
                };
                let data = match drawing_part_bytes(zip, &media_path, MAX_IMAGE_PART) {
                    DrawingPartRead::Data(data) => data,
                    DrawingPartRead::Missing => {
                        retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                        add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                        continue;
                    }
                    DrawingPartRead::LimitExceeded => {
                        retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                        add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                        continue;
                    }
                };
                if image_bytes.saturating_add(data.len()) > MAX_IMAGE_TOTAL {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                image_bytes += data.len();
                let index = images.len();
                images.push(Image {
                    data,
                    format,
                    from: drawing.from,
                    to: drawing.to,
                });
                let mut sidecar = drawing.metadata;
                sidecar.kind = DrawingObjectKind::Image;
                sidecar.object_index = index;
                metadata.push(sidecar);
            }
            DrawingRefKind::Chart => {
                let target =
                    match drawing
                        .rid
                        .as_deref()
                        .map_or(RelationshipTarget::Missing, |rid| {
                            drawing_rels.as_deref().map_or(
                                RelationshipTarget::Invalid,
                                |relationships| {
                                    internal_relationship_target_by_id(relationships, rid, "chart")
                                },
                            )
                        }) {
                        RelationshipTarget::Internal(target) => target,
                        RelationshipTarget::Missing | RelationshipTarget::Invalid => {
                            retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                            add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                            continue;
                        }
                    };
                if !chart_budget.reserve_chart() {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let chart_path = normalize_part_target(&drawing_path, &target);
                let Some(declared_size) = drawing_part_declared_size(zip, &chart_path) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                    continue;
                };
                let Some(declared_work) = usize::try_from(declared_size)
                    .ok()
                    .and_then(|size| size.checked_mul(XLSX_CHART_XML_SCAN_PASSES))
                else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                };
                if declared_size > MAX_XLSX_CHART_XML_BYTES
                    || !chart_budget.reserve_xml_work(declared_work)
                {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let chart_bytes =
                    match drawing_part_bytes(zip, &chart_path, MAX_XLSX_CHART_XML_BYTES) {
                        DrawingPartRead::Data(data) => data,
                        DrawingPartRead::Missing => {
                            retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                            add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                            continue;
                        }
                        DrawingPartRead::LimitExceeded => {
                            retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                            add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                            continue;
                        }
                    };
                let Some(chart_work) = chart_bytes.len().checked_mul(XLSX_CHART_XML_SCAN_PASSES)
                else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                };
                if !chart_budget.reconcile_xml_work(declared_work, chart_work) {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let Ok(chart_xml) = String::from_utf8(chart_bytes) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    continue;
                };
                if !crate::xml_reference_work_within_budget(&chart_xml) {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let Some(parsed) = parse_chart_with_theme(
                    &chart_xml,
                    drawing.from,
                    drawing.to.unwrap_or(drawing.from),
                    &mut chart_budget.cache_points_remaining,
                    &mut chart_budget.series_remaining,
                    theme,
                ) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    continue;
                };
                let has_unsupported_chart_content = !parsed.unsupported_reasons.is_empty()
                    || !parsed.frame_style_losses.is_empty()
                    || parsed
                        .series_styles
                        .iter()
                        .any(|style| !style.losses.is_empty());
                let index = charts.len();
                charts.push(parsed.chart);
                let mut sidecar = drawing.metadata;
                sidecar.kind = DrawingObjectKind::Chart;
                sidecar.object_index = index;
                sidecar.chart_palette = theme.chart_palette();
                sidecar.chart_default_latin_font_family =
                    Some(theme.chart_default_latin_font_family().to_string());
                sidecar.chart_text_styles = parsed.text_styles;
                sidecar.chart_series_caches = parsed.series_caches;
                sidecar.chart_series_styles = parsed.series_styles;
                sidecar.chart_frame_fill = parsed.frame_fill;
                sidecar.chart_frame_style_losses = parsed.frame_style_losses;
                sidecar.chart_category_major_gridlines = Some(parsed.category_major_gridlines);
                sidecar.chart_value_major_gridlines = Some(parsed.value_major_gridlines);
                sidecar.chart_category_axis_visible = parsed.category_axis_visible;
                sidecar.chart_category_axis_shifted = parsed.category_axis_shifted;
                sidecar.chart_value_axis_visible = parsed.value_axis_visible;
                sidecar.chart_unsupported_reasons = parsed.unsupported_reasons;
                sidecar.chart_bar_direction = parsed.bar_direction;
                metadata.push(sidecar);
                if parsed.limit_exceeded {
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                }
                if has_unsupported_chart_content {
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                }
            }
            DrawingRefKind::Shape => {
                retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
            }
        }
    }

    (images, charts, metadata, losses)
}

#[derive(Default)]
struct ParsedChartSeries {
    name: Option<String>,
    categories: Option<String>,
    values: Option<String>,
    bubble_sizes: Option<String>,
    invalid_text_fields: u8,
    source_position: usize,
    source_index_seen: bool,
    source_order_seen: bool,
    cache: ChartSeriesCache,
    style: ChartSeriesStyle,
}

const MAX_XLSX_CHART_SERIES_PER_WORKBOOK: usize = 4_096;
const MAX_XLSX_CHART_CACHE_POINTS_PER_WORKBOOK: usize = 1_000_000;
const MAX_XLSX_CHART_CACHE_VALUE_BYTES: usize = 4_096;
const MAX_XLSX_CHART_TEXT_FIELD_BYTES: usize = 32_768;
const MAX_XLSX_CHART_AXIS_ITEMS: usize = 32;
pub(crate) const MAX_XLSX_CHART_XML_BYTES: u64 = 8 << 20;
pub(crate) const XLSX_CHART_XML_SCAN_PASSES: usize = 6;
pub(crate) const MAX_XLSX_CHART_XML_WORK_BYTES_PER_WORKBOOK: usize = 128 << 20;
// ECMA-376 Part 1 §20.1.10.35 (`ST_LineWidth`), in English Metric Units.
const MAX_OOXML_CHART_LINE_WIDTH_EMU: u32 = 20_116_800;

pub(crate) struct ParsedChart {
    pub(crate) chart: Chart,
    pub(crate) series_caches: Vec<ChartSeriesCache>,
    pub(crate) series_styles: Vec<ChartSeriesStyle>,
    pub(crate) text_styles: ChartTextStyles,
    pub(crate) frame_fill: ChartFrameFill,
    pub(crate) frame_style_losses: Vec<ChartFrameStyleLossKind>,
    pub(crate) category_major_gridlines: bool,
    pub(crate) value_major_gridlines: bool,
    pub(crate) category_axis_visible: Option<bool>,
    pub(crate) category_axis_shifted: Option<bool>,
    pub(crate) value_axis_visible: Option<bool>,
    pub(crate) limit_exceeded: bool,
    pub(crate) unsupported_reasons: Vec<ChartUnsupportedReason>,
    pub(crate) bar_direction: ChartBarDirection,
}

#[derive(Clone, Copy)]
enum ChartSeriesField {
    Name,
    Categories,
    Values,
    BubbleSizes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartAxisContext {
    Category,
    Value,
}

#[derive(Clone, Copy)]
enum ChartTitleTarget {
    Main,
    CategoryAxis,
    ValueAxis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartTextSemanticRole {
    ChartDefault,
    ChartTitle,
    CategoryAxisTitle,
    ValueAxisTitle,
    Legend,
    CategoryAxisLabels,
    ValueAxisLabels,
    DataLabels,
}

impl ChartTextSemanticRole {
    const COUNT: usize = 8;

    fn index(self) -> usize {
        match self {
            Self::ChartDefault => 0,
            Self::ChartTitle => 1,
            Self::CategoryAxisTitle => 2,
            Self::ValueAxisTitle => 3,
            Self::Legend => 4,
            Self::CategoryAxisLabels => 5,
            Self::ValueAxisLabels => 6,
            Self::DataLabels => 7,
        }
    }

    fn default_style(self, theme: &ThemeColors, color_map: &ChartTextColorMap) -> ChartTextStyle {
        let (size_hundredths_of_point, bold) = match self {
            Self::ChartDefault => (1_000, false),
            Self::ChartTitle => (1_800, true),
            Self::CategoryAxisTitle | Self::ValueAxisTitle => (1_000, true),
            Self::Legend | Self::CategoryAxisLabels | Self::ValueAxisLabels | Self::DataLabels => {
                (1_000, false)
            }
        };
        ChartTextStyle {
            latin_font_family: theme.chart_default_latin_font_family().to_string(),
            size_hundredths_of_point,
            color: chart_scheme_color(theme, color_map, "tx1")
                .unwrap_or_else(|| Color::rgb(0, 0, 0)),
            bold,
            italic: false,
            underline: false,
            strikethrough: false,
            kerning_minimum_hundredths_of_point: None,
            rotation_degrees: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PartialChartTextStyle {
    latin_font_family: Option<String>,
    size_hundredths_of_point: Option<u32>,
    color: Option<Color>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strikethrough: Option<bool>,
    kerning_minimum_hundredths_of_point: Option<u32>,
}

impl PartialChartTextStyle {
    fn merge_from(&mut self, overlay: &Self) {
        if overlay.latin_font_family.is_some() {
            self.latin_font_family
                .clone_from(&overlay.latin_font_family);
        }
        if overlay.size_hundredths_of_point.is_some() {
            self.size_hundredths_of_point = overlay.size_hundredths_of_point;
        }
        if overlay.color.is_some() {
            self.color = overlay.color;
        }
        if overlay.bold.is_some() {
            self.bold = overlay.bold;
        }
        if overlay.italic.is_some() {
            self.italic = overlay.italic;
        }
        if overlay.underline.is_some() {
            self.underline = overlay.underline;
        }
        if overlay.strikethrough.is_some() {
            self.strikethrough = overlay.strikethrough;
        }
        if overlay.kerning_minimum_hundredths_of_point.is_some() {
            self.kerning_minimum_hundredths_of_point = overlay.kerning_minimum_hundredths_of_point;
        }
    }

    fn apply_to(&self, style: &mut ChartTextStyle) {
        if let Some(family) = self.latin_font_family.as_ref() {
            style.latin_font_family.clone_from(family);
        }
        if let Some(size) = self.size_hundredths_of_point {
            style.size_hundredths_of_point = size;
        }
        if let Some(color) = self.color {
            style.color = color;
        }
        if let Some(bold) = self.bold {
            style.bold = bold;
        }
        if let Some(italic) = self.italic {
            style.italic = italic;
        }
        if let Some(underline) = self.underline {
            style.underline = underline;
        }
        if let Some(strikethrough) = self.strikethrough {
            style.strikethrough = strikethrough;
        }
        if let Some(kerning) = self.kerning_minimum_hundredths_of_point {
            style.kerning_minimum_hundredths_of_point = Some(kerning);
        }
    }
}

#[derive(Debug, Clone, Default)]
enum ChartTextStyleObservation {
    #[default]
    Unseen,
    Uniform(ChartTextStyle),
    Mixed,
    Unsupported,
}

impl ChartTextStyleObservation {
    fn observe(&mut self, style: ChartTextStyle) {
        match self {
            Self::Unseen => *self = Self::Uniform(style),
            Self::Uniform(previous) if *previous == style => {}
            Self::Uniform(_) => *self = Self::Mixed,
            Self::Mixed | Self::Unsupported => {}
        }
    }

    fn mark_unsupported(&mut self) {
        *self = Self::Unsupported;
    }

    fn mark_mixed(&mut self) {
        if !matches!(self, Self::Unsupported) {
            *self = Self::Mixed;
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ChartTextStyleObservations {
    chart_default: ChartTextStyleObservation,
    chart_title: ChartTextStyleObservation,
    category_axis_title: ChartTextStyleObservation,
    value_axis_title: ChartTextStyleObservation,
    legend: ChartTextStyleObservation,
    category_axis_labels: ChartTextStyleObservation,
    value_axis_labels: ChartTextStyleObservation,
    data_labels: ChartTextStyleObservation,
}

impl ChartTextStyleObservations {
    fn get_mut(&mut self, role: ChartTextSemanticRole) -> &mut ChartTextStyleObservation {
        match role {
            ChartTextSemanticRole::ChartDefault => &mut self.chart_default,
            ChartTextSemanticRole::ChartTitle => &mut self.chart_title,
            ChartTextSemanticRole::CategoryAxisTitle => &mut self.category_axis_title,
            ChartTextSemanticRole::ValueAxisTitle => &mut self.value_axis_title,
            ChartTextSemanticRole::Legend => &mut self.legend,
            ChartTextSemanticRole::CategoryAxisLabels => &mut self.category_axis_labels,
            ChartTextSemanticRole::ValueAxisLabels => &mut self.value_axis_labels,
            ChartTextSemanticRole::DataLabels => &mut self.data_labels,
        }
    }

    fn finish(self, unsupported_reasons: &mut Vec<ChartUnsupportedReason>) -> ChartTextStyles {
        fn finish_one(
            observation: ChartTextStyleObservation,
            unsupported_reasons: &mut Vec<ChartUnsupportedReason>,
        ) -> Option<ChartTextStyle> {
            match observation {
                ChartTextStyleObservation::Unseen => None,
                ChartTextStyleObservation::Uniform(style) => Some(style),
                ChartTextStyleObservation::Mixed => {
                    add_chart_unsupported(
                        unsupported_reasons,
                        ChartUnsupportedReason::MixedTextStyle,
                    );
                    None
                }
                ChartTextStyleObservation::Unsupported => {
                    add_chart_unsupported(
                        unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedTextStyle,
                    );
                    None
                }
            }
        }

        let _ = finish_one(self.chart_default, unsupported_reasons);
        ChartTextStyles {
            chart_title: finish_one(self.chart_title, unsupported_reasons),
            category_axis_title: finish_one(self.category_axis_title, unsupported_reasons),
            value_axis_title: finish_one(self.value_axis_title, unsupported_reasons),
            legend: finish_one(self.legend, unsupported_reasons),
            category_axis_labels: finish_one(self.category_axis_labels, unsupported_reasons),
            value_axis_labels: finish_one(self.value_axis_labels, unsupported_reasons),
            data_labels: finish_one(self.data_labels, unsupported_reasons),
        }
    }
}

#[derive(Debug)]
struct ChartTextContext {
    kind: ChartKind,
    axis: Option<ChartAxisContext>,
    axis_roles: Vec<ChartAxisContext>,
    axis_occurrence: usize,
    series_depth: usize,
    title_role: Option<ChartTextSemanticRole>,
    title_depth: usize,
    legend_depth: usize,
    data_labels_depth: usize,
    display_units_label_depth: usize,
}

impl ChartTextContext {
    fn new(kind: ChartKind, axis_roles: &[ChartAxisContext]) -> Self {
        Self {
            kind,
            axis: None,
            axis_roles: axis_roles.to_vec(),
            axis_occurrence: 0,
            series_depth: 0,
            title_role: None,
            title_depth: 0,
            legend_depth: 0,
            data_labels_depth: 0,
            display_units_label_depth: 0,
        }
    }

    fn start(&mut self, name: &[u8]) {
        match name {
            b"ser" => self.series_depth = self.series_depth.saturating_add(1),
            b"catAx" | b"dateAx" | b"valAx" => {
                self.axis = self
                    .axis_roles
                    .get(self.axis_occurrence)
                    .copied()
                    .or(match name {
                        b"catAx" | b"dateAx" => Some(ChartAxisContext::Category),
                        b"valAx" if matches!(self.kind, ChartKind::Scatter | ChartKind::Bubble) => {
                            Some(ChartAxisContext::Value)
                        }
                        b"valAx" => Some(ChartAxisContext::Value),
                        _ => None,
                    });
                self.axis_occurrence = self.axis_occurrence.saturating_add(1);
            }
            b"title" if self.series_depth == 0 => {
                if self.title_depth == 0 {
                    self.title_role = Some(match self.axis {
                        Some(ChartAxisContext::Category) => {
                            ChartTextSemanticRole::CategoryAxisTitle
                        }
                        Some(ChartAxisContext::Value) => ChartTextSemanticRole::ValueAxisTitle,
                        None => ChartTextSemanticRole::ChartTitle,
                    });
                }
                self.title_depth = self.title_depth.saturating_add(1);
            }
            b"legend" => self.legend_depth = self.legend_depth.saturating_add(1),
            b"dLbls" | b"dLbl" => {
                self.data_labels_depth = self.data_labels_depth.saturating_add(1);
            }
            b"dispUnitsLbl" => {
                self.display_units_label_depth = self.display_units_label_depth.saturating_add(1);
            }
            _ => {}
        }
    }

    fn end(&mut self, name: &[u8]) {
        match name {
            b"ser" => self.series_depth = self.series_depth.saturating_sub(1),
            b"catAx" | b"dateAx" | b"valAx" => self.axis = None,
            b"title" if self.title_depth > 0 => {
                self.title_depth -= 1;
                if self.title_depth == 0 {
                    self.title_role = None;
                }
            }
            b"legend" => self.legend_depth = self.legend_depth.saturating_sub(1),
            b"dLbls" | b"dLbl" => {
                self.data_labels_depth = self.data_labels_depth.saturating_sub(1);
            }
            b"dispUnitsLbl" => {
                self.display_units_label_depth = self.display_units_label_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn text_body_role(&self, rich: bool) -> Option<ChartTextSemanticRole> {
        if self.display_units_label_depth > 0 {
            return None;
        }
        if self.title_depth > 0 {
            return self.title_role;
        }
        if self.legend_depth > 0 {
            Some(ChartTextSemanticRole::Legend)
        } else if self.data_labels_depth > 0 {
            Some(ChartTextSemanticRole::DataLabels)
        } else if rich {
            None
        } else {
            match self.axis {
                Some(ChartAxisContext::Category) => Some(ChartTextSemanticRole::CategoryAxisLabels),
                Some(ChartAxisContext::Value) => Some(ChartTextSemanticRole::ValueAxisLabels),
                None if self.series_depth == 0 => Some(ChartTextSemanticRole::ChartDefault),
                None => None,
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ChartTextColorCapture {
    color: Color,
}

#[derive(Debug, Clone, Default)]
struct ChartTextPropertyCapture {
    style: PartialChartTextStyle,
    unsupported: bool,
    in_solid_fill: bool,
    solid_fill_seen: bool,
    color_transform_count: usize,
    color: Option<ChartTextColorCapture>,
}

// A single transform is evaluated exactly enough for the retained RGB model.
// Multiple sequential transforms would require preserving higher-precision
// DrawingML color state between operations, so they fail closed.
const MAX_CHART_TEXT_COLOR_TRANSFORMS: usize = 1;

const MIN_OOXML_CHART_TEXT_SIZE: u32 = 100;
const MAX_OOXML_CHART_TEXT_SIZE: u32 = 400_000;

fn chart_text_bounded_size(value: &str) -> Option<u32> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| (MIN_OOXML_CHART_TEXT_SIZE..=MAX_OOXML_CHART_TEXT_SIZE).contains(value))
}

fn chart_text_bounded_kerning(value: &str) -> Option<u32> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value <= MAX_OOXML_CHART_TEXT_SIZE)
}

fn parse_chart_bool_attr(value: &str) -> Option<bool> {
    match value {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn parse_chart_boolean_element(
    element: &quick_xml::events::BytesStart<'_>,
) -> std::result::Result<bool, ()> {
    match unique_attr(element, b"val")? {
        Some(value) => parse_chart_bool_attr(&value).ok_or(()),
        None => Ok(true),
    }
}

fn resolve_chart_latin_typeface(value: &str, theme: &ThemeColors) -> Option<String> {
    match value.trim() {
        "+mn-lt" => Some(theme.chart_default_latin_font_family().to_string()),
        "+mj-lt" => Some(theme.chart_major_latin_font_family().to_string()),
        value if value.starts_with('+') => None,
        value => bounded_imported_chart_latin_font_family(value),
    }
}

#[derive(Debug, Clone, Default)]
struct ChartTextColorMap(HashMap<String, String>);

impl ChartTextColorMap {
    fn resolve<'a>(&'a self, value: &'a str) -> &'a str {
        self.0.get(value).map(String::as_str).unwrap_or(value)
    }
}

fn parse_chart_text_color_map(xml: &str) -> (ChartTextColorMap, bool) {
    const SOURCE_SLOTS: [&str; 12] = [
        "bg1", "tx1", "bg2", "tx2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];
    const TARGET_SLOTS: [&str; 12] = [
        "lt1", "dk1", "lt2", "dk2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];

    fn read_override_mapping(
        element: &quick_xml::events::BytesStart<'_>,
        map: &mut ChartTextColorMap,
    ) -> bool {
        let mut unsupported = false;
        for slot in SOURCE_SLOTS {
            match unique_attr(element, slot.as_bytes()) {
                Ok(Some(value)) if TARGET_SLOTS.contains(&value.as_str()) => {
                    map.0.insert(slot.to_string(), value);
                }
                Ok(Some(_)) | Ok(None) | Err(()) => unsupported = true,
            }
        }
        for attribute in element.attributes() {
            match attribute {
                Ok(attribute) => {
                    let qualified_name = attribute.key.as_ref();
                    let name = local(qualified_name);
                    if !SOURCE_SLOTS.iter().any(|slot| slot.as_bytes() == name)
                        && qualified_name != b"xmlns"
                        && !qualified_name.starts_with(b"xmlns:")
                    {
                        unsupported = true;
                    }
                }
                Err(_) => unsupported = true,
            }
        }
        unsupported
    }

    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut chart_space_depth = None;
    let mut override_depth = None;
    let mut seen_override = false;
    let mut override_child_seen = false;
    let mut map = ChartTextColorMap::default();
    let mut unsupported = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if name == b"chartSpace" && chart_space_depth.is_none() {
                    chart_space_depth = Some(depth);
                } else if chart_space_depth.is_some_and(|root| depth == root + 1)
                    && name == b"clrMapOvr"
                {
                    if seen_override {
                        unsupported = true;
                    }
                    seen_override = true;
                    override_depth = Some(depth);
                    override_child_seen = false;
                } else if override_depth.is_some_and(|parent| depth == parent + 1) {
                    if override_child_seen {
                        unsupported = true;
                    }
                    override_child_seen = true;
                    match name {
                        b"masterClrMapping" => {}
                        b"overrideClrMapping" => {
                            unsupported |= read_override_mapping(&element, &mut map);
                        }
                        _ => unsupported = true,
                    }
                } else if override_depth.is_some_and(|parent| depth > parent + 1) {
                    unsupported = true;
                }
                depth = depth.saturating_add(1);
            }
            Ok(Event::Empty(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if chart_space_depth.is_some_and(|root| depth == root + 1) && name == b"clrMapOvr" {
                    seen_override = true;
                    unsupported = true;
                } else if override_depth.is_some_and(|parent| depth == parent + 1) {
                    if override_child_seen {
                        unsupported = true;
                    }
                    override_child_seen = true;
                    match name {
                        b"masterClrMapping" => {}
                        b"overrideClrMapping" => {
                            unsupported |= read_override_mapping(&element, &mut map);
                        }
                        _ => unsupported = true,
                    }
                } else if override_depth.is_some_and(|parent| depth > parent + 1) {
                    unsupported = true;
                }
            }
            Ok(Event::End(element)) => {
                depth = depth.saturating_sub(1);
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if name == b"clrMapOvr" && override_depth == Some(depth) {
                    unsupported |= !override_child_seen;
                    override_depth = None;
                    override_child_seen = false;
                }
                if name == b"chartSpace" && chart_space_depth == Some(depth) {
                    chart_space_depth = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                unsupported = true;
                break;
            }
            _ => {}
        }
    }
    unsupported |= override_depth.is_some();
    (map, unsupported)
}

fn chart_scheme_color(
    theme: &ThemeColors,
    color_map: &ChartTextColorMap,
    value: &str,
) -> Option<Color> {
    let value = color_map.resolve(value);
    let fallback = match value.as_bytes() {
        b"bg1" | b"lt1" => Some(Color::rgb(255, 255, 255)),
        b"tx1" | b"dk1" => Some(Color::rgb(0, 0, 0)),
        b"bg2" | b"lt2" => Some(Color::rgb(238, 236, 225)),
        b"tx2" | b"dk2" => Some(Color::rgb(31, 73, 125)),
        _ => None,
    };
    let slot_name = match value.as_bytes() {
        b"bg1" => b"lt1".as_slice(),
        b"tx1" => b"dk1".as_slice(),
        b"bg2" => b"lt2".as_slice(),
        b"tx2" => b"dk2".as_slice(),
        value => value,
    };
    theme_color_slot(slot_name)
        .and_then(|slot| theme.color(slot, None))
        .or_else(|| {
            let index = match value.as_bytes() {
                b"accent1" => 0,
                b"accent2" => 1,
                b"accent3" => 2,
                b"accent4" => 3,
                b"accent5" => 4,
                b"accent6" => 5,
                _ => return fallback,
            };
            theme.chart_palette().get(index).copied()
        })
        .or(fallback)
}

fn rgb_to_hsl(color: Color) -> (f64, f64, f64) {
    let [red, green, blue] = color.as_rgb();
    let red = f64::from(red) / 255.0;
    let green = f64::from(green) / 255.0;
    let blue = f64::from(blue) / 255.0;
    let maximum = red.max(green).max(blue);
    let minimum = red.min(green).min(blue);
    let lightness = (maximum + minimum) / 2.0;
    let difference = maximum - minimum;
    if difference == 0.0 {
        return (0.0, 0.0, lightness);
    }
    let saturation = difference / (1.0 - (2.0 * lightness - 1.0).abs());
    let hue = if maximum == red {
        ((green - blue) / difference).rem_euclid(6.0)
    } else if maximum == green {
        (blue - red) / difference + 2.0
    } else {
        (red - green) / difference + 4.0
    } / 6.0;
    (hue, saturation, lightness)
}

fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> Color {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let hue_sector = hue.rem_euclid(1.0) * 6.0;
    let intermediate = chroma * (1.0 - (hue_sector.rem_euclid(2.0) - 1.0).abs());
    let (red, green, blue) = if hue_sector < 1.0 {
        (chroma, intermediate, 0.0)
    } else if hue_sector < 2.0 {
        (intermediate, chroma, 0.0)
    } else if hue_sector < 3.0 {
        (0.0, chroma, intermediate)
    } else if hue_sector < 4.0 {
        (0.0, intermediate, chroma)
    } else if hue_sector < 5.0 {
        (intermediate, 0.0, chroma)
    } else {
        (chroma, 0.0, intermediate)
    };
    let match_value = lightness - chroma / 2.0;
    let channel = |value: f64| ((value + match_value) * 255.0).round().clamp(0.0, 255.0) as u8;
    Color::rgb(channel(red), channel(green), channel(blue))
}

fn apply_chart_luminance(color: Color, modulation: u32, offset: u32) -> Color {
    let (hue, saturation, lightness) = rgb_to_hsl(color);
    let lightness = (lightness * f64::from(modulation) / 100_000.0 + f64::from(offset) / 100_000.0)
        .clamp(0.0, 1.0);
    hsl_to_rgb(hue, saturation, lightness)
}

fn parse_chart_color_transform(value: Option<String>) -> Option<u32> {
    value
        .as_deref()?
        .parse::<u32>()
        .ok()
        .filter(|value| *value <= 100_000)
}

fn parse_chart_rgb(value: &str) -> Option<Color> {
    if value.len() != 6 || !value.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    Some(Color::rgb(
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ))
}

fn chart_text_unique_attr(
    element: &quick_xml::events::BytesStart<'_>,
    name: &[u8],
    unsupported: &mut bool,
) -> Option<String> {
    match unique_attr(element, name) {
        Ok(value) => value,
        Err(()) => {
            *unsupported = true;
            None
        }
    }
}

fn chart_text_attributes_are_subset(
    element: &quick_xml::events::BytesStart<'_>,
    allowed: &[&[u8]],
) -> bool {
    element.attributes().all(|attribute| {
        let Ok(attribute) = attribute else {
            return false;
        };
        let qualified_name = attribute.key.as_ref();
        qualified_name == b"xmlns"
            || qualified_name.starts_with(b"xmlns:")
            || allowed.contains(&qualified_name)
    })
}

fn chart_text_partial_from_attributes(
    element: &quick_xml::events::BytesStart<'_>,
) -> ChartTextPropertyCapture {
    let mut capture = ChartTextPropertyCapture::default();
    if !chart_text_attributes_are_subset(
        element,
        &[
            b"sz",
            b"b",
            b"i",
            b"u",
            b"strike",
            b"kern",
            b"baseline",
            b"spc",
            b"cap",
            b"normalizeH",
            b"kumimoji",
        ],
    ) {
        capture.unsupported = true;
    }
    if let Some(value) = chart_text_unique_attr(element, b"sz", &mut capture.unsupported) {
        match chart_text_bounded_size(&value) {
            Some(value) => capture.style.size_hundredths_of_point = Some(value),
            None => capture.unsupported = true,
        }
    }
    if let Some(value) = chart_text_unique_attr(element, b"b", &mut capture.unsupported) {
        match parse_chart_bool_attr(&value) {
            Some(value) => capture.style.bold = Some(value),
            None => capture.unsupported = true,
        }
    }
    if let Some(value) = chart_text_unique_attr(element, b"i", &mut capture.unsupported) {
        match parse_chart_bool_attr(&value) {
            Some(value) => capture.style.italic = Some(value),
            None => capture.unsupported = true,
        }
    }
    if let Some(value) = chart_text_unique_attr(element, b"u", &mut capture.unsupported) {
        match value.as_str() {
            "none" => capture.style.underline = Some(false),
            "sng" => capture.style.underline = Some(true),
            _ => capture.unsupported = true,
        }
    }
    if let Some(value) = chart_text_unique_attr(element, b"strike", &mut capture.unsupported) {
        match value.as_str() {
            "noStrike" => capture.style.strikethrough = Some(false),
            "sngStrike" => capture.style.strikethrough = Some(true),
            _ => capture.unsupported = true,
        }
    }
    if let Some(value) = chart_text_unique_attr(element, b"kern", &mut capture.unsupported) {
        match chart_text_bounded_kerning(&value) {
            Some(value) => capture.style.kerning_minimum_hundredths_of_point = Some(value),
            None => capture.unsupported = true,
        }
    }
    for name in [b"baseline".as_slice(), b"spc".as_slice()] {
        if chart_text_unique_attr(element, name, &mut capture.unsupported)
            .as_deref()
            .is_some_and(|value| value.parse::<i32>().ok() != Some(0))
        {
            capture.unsupported = true;
        }
    }
    if chart_text_unique_attr(element, b"cap", &mut capture.unsupported)
        .as_deref()
        .is_some_and(|value| value != "none")
    {
        capture.unsupported = true;
    }
    for name in [b"normalizeH".as_slice(), b"kumimoji".as_slice()] {
        if let Some(value) = chart_text_unique_attr(element, name, &mut capture.unsupported) {
            if parse_chart_bool_attr(&value) != Some(false) {
                capture.unsupported = true;
            }
        }
    }
    capture
}

fn update_chart_text_property_capture(
    capture: &mut ChartTextPropertyCapture,
    element: &quick_xml::events::BytesStart<'_>,
    theme: &ThemeColors,
    color_map: &ChartTextColorMap,
    empty: bool,
) {
    let qualified_name = element.name();
    let name = local(qualified_name.as_ref());
    match name {
        b"latin" => {
            if !chart_text_attributes_are_subset(element, &[b"typeface"]) {
                capture.unsupported = true;
            }
            let family = unique_attr(element, b"typeface")
                .ok()
                .flatten()
                .as_deref()
                .and_then(|value| resolve_chart_latin_typeface(value, theme));
            if family.is_none() || capture.style.latin_font_family.is_some() {
                capture.unsupported = true;
            } else {
                capture.style.latin_font_family = family;
            }
        }
        b"ea" | b"cs" => {
            if !chart_text_attributes_are_subset(element, &[b"typeface"]) {
                capture.unsupported = true;
            }
            match unique_attr(element, b"typeface") {
                Ok(None) => {}
                Ok(Some(value)) if value.trim().is_empty() => {}
                _ => capture.unsupported = true,
            }
        }
        b"solidFill" => {
            if !chart_text_attributes_are_subset(element, &[]) {
                capture.unsupported = true;
            }
            if capture.solid_fill_seen || capture.in_solid_fill {
                capture.unsupported = true;
            }
            capture.solid_fill_seen = true;
            capture.in_solid_fill = true;
            if empty {
                capture.unsupported = true;
                capture.in_solid_fill = false;
            }
        }
        b"srgbClr" | b"schemeClr" | b"sysClr" if capture.in_solid_fill => {
            let allowed: &[&[u8]] = if name == b"sysClr" {
                &[b"val", b"lastClr"]
            } else {
                &[b"val"]
            };
            if !chart_text_attributes_are_subset(element, allowed) {
                capture.unsupported = true;
            }
            let duplicate_color = capture.color.is_some() || capture.style.color.is_some();
            if duplicate_color {
                capture.unsupported = true;
            }
            let color = match name {
                b"srgbClr" => unique_attr(element, b"val")
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(parse_chart_rgb),
                b"schemeClr" => unique_attr(element, b"val")
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(|value| chart_scheme_color(theme, color_map, value)),
                b"sysClr" => unique_attr(element, b"lastClr")
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(parse_chart_rgb),
                _ => unreachable!("guarded chart text color"),
            };
            match color {
                Some(color) if !duplicate_color => {
                    capture.color = Some(ChartTextColorCapture { color });
                }
                Some(_) => {}
                None => capture.unsupported = true,
            }
            if empty {
                if let Some(color) = capture.color.take() {
                    capture.style.color = Some(color.color);
                }
            }
        }
        b"lumMod" | b"lumOff" | b"tint" | b"shade" if capture.color.is_some() => {
            if !chart_text_attributes_are_subset(element, &[b"val"]) {
                capture.unsupported = true;
            }
            capture.color_transform_count = capture.color_transform_count.saturating_add(1);
            if capture.color_transform_count > MAX_CHART_TEXT_COLOR_TRANSFORMS {
                capture.unsupported = true;
                return;
            }
            let Some(value) = unique_attr(element, b"val")
                .ok()
                .flatten()
                .and_then(|value| parse_chart_color_transform(Some(value)))
            else {
                capture.unsupported = true;
                return;
            };
            let color = capture.color.as_mut().expect("chart color checked above");
            color.color = match name {
                b"lumMod" => apply_chart_luminance(color.color, value, 0),
                b"lumOff" => apply_chart_luminance(color.color, 100_000, value),
                b"tint" => apply_chart_luminance(color.color, value, 100_000 - value),
                b"shade" => apply_chart_luminance(color.color, value, 0),
                _ => unreachable!("guarded luminance transform"),
            };
        }
        b"lumMod" | b"lumOff" | b"tint" | b"shade" => capture.unsupported = true,
        b"noFill" | b"gradFill" | b"pattFill" | b"blipFill" | b"grpFill" | b"highlight"
        | b"uLn" | b"uLnTx" | b"uFill" | b"uFillTx" | b"rtl" | b"sym" | b"ln" | b"effectLst"
        | b"effectDag" | b"scene3d" | b"sp3d" | b"glow" | b"outerShdw" | b"innerShdw"
        | b"reflection" | b"softEdge" | b"hlinkClick" | b"hlinkMouseOver" => {
            capture.unsupported = true;
        }
        b"alpha" | b"alphaMod" | b"alphaOff" | b"blue" | b"blueMod" | b"blueOff" | b"green"
        | b"greenMod" | b"greenOff" | b"hue" | b"hueMod" | b"hueOff" | b"lum" | b"red"
        | b"redMod" | b"redOff" | b"sat" | b"satMod" | b"satOff" | b"comp" | b"gamma" | b"gray"
        | b"inv" | b"invGamma" => capture.unsupported = true,
        _ => capture.unsupported = true,
    }
}

fn finish_chart_text_property_element(capture: &mut ChartTextPropertyCapture, name: &[u8]) {
    match name {
        b"srgbClr" | b"schemeClr" | b"sysClr" if capture.in_solid_fill => {
            if let Some(color) = capture.color.take() {
                capture.style.color = Some(color.color);
            } else {
                capture.unsupported = true;
            }
        }
        b"solidFill" => {
            if capture.style.color.is_none() {
                capture.unsupported = true;
            }
            capture.in_solid_fill = false;
            capture.color = None;
        }
        _ => {}
    }
}

const MAX_CHART_TEXT_STYLE_FACTS_PER_ROLE: usize = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ChartTextRotationState {
    #[default]
    Inherit,
    Automatic,
    Degrees(i16),
}

#[derive(Debug, Clone, Default)]
enum PartialChartTextStyleObservation {
    #[default]
    Unseen,
    Uniform {
        style: PartialChartTextStyle,
        rotation: ChartTextRotationState,
    },
    Mixed,
    Unsupported,
}

impl PartialChartTextStyleObservation {
    fn observe(&mut self, style: PartialChartTextStyle, rotation: ChartTextRotationState) {
        match self {
            Self::Unseen => *self = Self::Uniform { style, rotation },
            Self::Uniform {
                style: previous_style,
                rotation: previous_rotation,
            } if *previous_style == style && *previous_rotation == rotation => {}
            Self::Uniform { .. } => *self = Self::Mixed,
            Self::Mixed | Self::Unsupported => {}
        }
    }

    fn mark_unsupported(&mut self) {
        *self = Self::Unsupported;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaintedChartTextStyleFact {
    style: PartialChartTextStyle,
    rotation: ChartTextRotationState,
    unsupported: bool,
}

#[derive(Debug, Clone, Copy)]
enum ChartTextPropertyTarget {
    List(usize),
    Paragraph,
    Run,
}

#[derive(Debug)]
struct UnifiedChartTextBody {
    rich: bool,
    role: ChartTextSemanticRole,
    rotation: ChartTextRotationState,
    body_unsupported: bool,
    body_properties_seen: bool,
    body_properties_open: bool,
    autofit_seen: bool,
    list_style_seen: bool,
    list_style_open: bool,
    paragraph_open: bool,
    paragraph_properties_open: bool,
    paragraph_content_started: bool,
    run_open: bool,
    list_styles: [PartialChartTextStyle; 9],
    list_unsupported: [bool; 9],
    list_property_seen: [bool; 9],
    list_level_context: Option<usize>,
    paragraph_level: usize,
    paragraph_style: PartialChartTextStyle,
    paragraph_unsupported: bool,
    paragraph_property_seen: bool,
    run_style: PartialChartTextStyle,
    run_unsupported: bool,
    run_property_seen: bool,
    in_text: bool,
    current_run_painted: bool,
    current_paragraph_painted: bool,
    paragraph_seen: bool,
    painted_paragraphs: usize,
    default_candidate: Option<PartialChartTextStyle>,
    default_candidate_mixed: bool,
    default_candidate_unsupported: bool,
}

impl UnifiedChartTextBody {
    fn new(rich: bool, role: ChartTextSemanticRole) -> Self {
        Self {
            rich,
            role,
            rotation: ChartTextRotationState::Inherit,
            body_unsupported: false,
            body_properties_seen: false,
            body_properties_open: false,
            autofit_seen: false,
            list_style_seen: false,
            list_style_open: false,
            paragraph_open: false,
            paragraph_properties_open: false,
            paragraph_content_started: false,
            run_open: false,
            list_styles: std::array::from_fn(|_| PartialChartTextStyle::default()),
            list_unsupported: [false; 9],
            list_property_seen: [false; 9],
            list_level_context: None,
            paragraph_level: 0,
            paragraph_style: PartialChartTextStyle::default(),
            paragraph_unsupported: false,
            paragraph_property_seen: false,
            run_style: PartialChartTextStyle::default(),
            run_unsupported: false,
            run_property_seen: false,
            in_text: false,
            current_run_painted: false,
            current_paragraph_painted: false,
            paragraph_seen: false,
            painted_paragraphs: 0,
            default_candidate: None,
            default_candidate_mixed: false,
            default_candidate_unsupported: false,
        }
    }

    fn reset_paragraph(&mut self) {
        self.paragraph_level = 0;
        self.paragraph_style = PartialChartTextStyle::default();
        self.paragraph_unsupported = false;
        self.paragraph_property_seen = false;
        self.paragraph_content_started = false;
        self.run_style = PartialChartTextStyle::default();
        self.run_unsupported = false;
        self.run_property_seen = false;
        self.current_run_painted = false;
        self.current_paragraph_painted = false;
    }

    fn reset_run(&mut self) {
        self.run_style = PartialChartTextStyle::default();
        self.run_unsupported = false;
        self.run_property_seen = false;
        self.current_run_painted = false;
    }

    fn effective_partial(&self, include_run: bool) -> PartialChartTextStyle {
        let mut style = self.list_styles[self.paragraph_level].clone();
        style.merge_from(&self.paragraph_style);
        if include_run {
            style.merge_from(&self.run_style);
        }
        style
    }

    fn observe_default_candidate(&mut self) {
        if self.rich {
            return;
        }
        self.default_candidate_unsupported |=
            self.list_unsupported[self.paragraph_level] || self.paragraph_unsupported;
        let candidate = self.effective_partial(false);
        match self.default_candidate.as_ref() {
            Some(previous) if previous != &candidate => self.default_candidate_mixed = true,
            None => self.default_candidate = Some(candidate),
            _ => {}
        }
    }

    fn property_target(&self, run: bool) -> ChartTextPropertyTarget {
        if run {
            ChartTextPropertyTarget::Run
        } else if let Some(level) = self.list_level_context {
            ChartTextPropertyTarget::List(level)
        } else {
            ChartTextPropertyTarget::Paragraph
        }
    }

    fn apply_property(
        &mut self,
        target: ChartTextPropertyTarget,
        capture: ChartTextPropertyCapture,
    ) {
        match target {
            ChartTextPropertyTarget::List(level) => {
                if self.list_property_seen[level] {
                    self.list_unsupported[level] = true;
                }
                self.list_property_seen[level] = true;
                self.list_unsupported[level] |= capture.unsupported;
                self.list_styles[level].merge_from(&capture.style);
            }
            ChartTextPropertyTarget::Paragraph => {
                if self.paragraph_property_seen {
                    self.paragraph_unsupported = true;
                }
                self.paragraph_property_seen = true;
                self.paragraph_unsupported |= capture.unsupported;
                self.paragraph_style.merge_from(&capture.style);
            }
            ChartTextPropertyTarget::Run => {
                if self.run_property_seen {
                    self.run_unsupported = true;
                }
                self.run_property_seen = true;
                self.run_unsupported |= capture.unsupported;
                self.run_style.merge_from(&capture.style);
            }
        }
    }

    fn current_fact(&mut self) -> PaintedChartTextStyleFact {
        self.current_run_painted = true;
        if !self.current_paragraph_painted {
            self.current_paragraph_painted = true;
            self.painted_paragraphs = self.painted_paragraphs.saturating_add(1);
        }
        PaintedChartTextStyleFact {
            style: self.effective_partial(true),
            rotation: self.rotation,
            unsupported: self.body_unsupported
                || self.list_unsupported[self.paragraph_level]
                || self.paragraph_unsupported
                || self.run_unsupported
                || self.painted_paragraphs > 1,
        }
    }
}

fn chart_text_list_level(name: &[u8]) -> Option<usize> {
    match name {
        b"lvl1pPr" => Some(0),
        b"lvl2pPr" => Some(1),
        b"lvl3pPr" => Some(2),
        b"lvl4pPr" => Some(3),
        b"lvl5pPr" => Some(4),
        b"lvl6pPr" => Some(5),
        b"lvl7pPr" => Some(6),
        b"lvl8pPr" => Some(7),
        b"lvl9pPr" => Some(8),
        _ => None,
    }
}

fn normalize_imported_chart_rotation(degrees: i32) -> Option<i16> {
    let normalized = degrees.rem_euclid(360);
    i16::try_from(if normalized > 180 {
        normalized - 360
    } else {
        normalized
    })
    .ok()
}

fn parse_chart_body_rotation_state(
    element: &quick_xml::events::BytesStart<'_>,
    role: ChartTextSemanticRole,
) -> std::result::Result<ChartTextRotationState, ()> {
    if !chart_text_attributes_are_subset(element, &[b"vert", b"rot"]) {
        return Err(());
    }
    if unique_attr(element, b"vert")?
        .as_deref()
        .is_some_and(|value| value != "horz")
    {
        return Err(());
    }
    let Some(value) = unique_attr(element, b"rot")? else {
        return Ok(ChartTextRotationState::Inherit);
    };
    if value == "-60000000" {
        return if matches!(
            role,
            ChartTextSemanticRole::CategoryAxisLabels | ChartTextSemanticRole::ValueAxisLabels
        ) {
            Ok(ChartTextRotationState::Automatic)
        } else {
            Err(())
        };
    }
    let value = value.parse::<i32>().map_err(|_| ())?;
    if value % 60_000 != 0 {
        return Err(());
    }
    normalize_imported_chart_rotation(value / 60_000)
        .map(ChartTextRotationState::Degrees)
        .ok_or(())
}

fn resolve_chart_text_rotation(
    inherited: Option<i16>,
    overlay: ChartTextRotationState,
) -> Option<i16> {
    match overlay {
        ChartTextRotationState::Inherit => inherited,
        ChartTextRotationState::Automatic => None,
        ChartTextRotationState::Degrees(degrees) => Some(degrees),
    }
}

fn chart_text_norm_autofit_is_neutral(element: &quick_xml::events::BytesStart<'_>) -> bool {
    if !chart_text_attributes_are_subset(element, &[b"fontScale", b"lnSpcReduction"]) {
        return false;
    }
    let font_scale = match unique_attr(element, b"fontScale") {
        Ok(None) => 100_000,
        Ok(Some(value)) => match value.parse::<u32>() {
            Ok(value) => value,
            Err(_) => return false,
        },
        Err(()) => return false,
    };
    let line_spacing_reduction = match unique_attr(element, b"lnSpcReduction") {
        Ok(None) => 0,
        Ok(Some(value)) => match value.parse::<u32>() {
            Ok(value) => value,
            Err(_) => return false,
        },
        Err(()) => return false,
    };
    font_scale == 100_000 && line_spacing_reduction == 0
}

fn apply_chart_text_paragraph_properties(
    body: &mut UnifiedChartTextBody,
    element: &quick_xml::events::BytesStart<'_>,
) {
    if !chart_text_attributes_are_subset(element, &[b"lvl"]) {
        body.paragraph_unsupported = true;
    }
    match unique_parsed_attr::<usize>(element, b"lvl") {
        Ok(Some(level)) if level <= 8 => body.paragraph_level = level,
        Ok(None) => {}
        _ => body.paragraph_unsupported = true,
    }
}

fn resolve_unified_chart_text_style(
    role: ChartTextSemanticRole,
    theme: &ThemeColors,
    color_map: &ChartTextColorMap,
    chart_default: Option<(&PartialChartTextStyle, ChartTextRotationState)>,
    role_default: Option<(&PartialChartTextStyle, ChartTextRotationState)>,
    fact: Option<&PaintedChartTextStyleFact>,
) -> ChartTextStyle {
    let mut style = role.default_style(theme, color_map);
    let mut rotation = None;
    if let Some((partial, state)) = chart_default {
        partial.apply_to(&mut style);
        rotation = resolve_chart_text_rotation(rotation, state);
    }
    if let Some((partial, state)) = role_default {
        partial.apply_to(&mut style);
        rotation = resolve_chart_text_rotation(rotation, state);
    }
    if let Some(fact) = fact {
        fact.style.apply_to(&mut style);
        rotation = resolve_chart_text_rotation(rotation, fact.rotation);
    }
    style.rotation_degrees = rotation;
    style
}

fn push_chart_text_style_fact(
    role: ChartTextSemanticRole,
    fact: PaintedChartTextStyleFact,
    facts: &mut [Vec<PaintedChartTextStyleFact>; ChartTextSemanticRole::COUNT],
    role_unsupported: &mut [bool; ChartTextSemanticRole::COUNT],
    limit_exceeded: &mut bool,
) {
    let target = &mut facts[role.index()];
    if target.last() == Some(&fact) {
        return;
    }
    if target.len() == MAX_CHART_TEXT_STYLE_FACTS_PER_ROLE {
        role_unsupported[role.index()] = true;
        *limit_exceeded = true;
    } else if target.len() < MAX_CHART_TEXT_STYLE_FACTS_PER_ROLE {
        target.push(fact);
    }
}

fn parse_chart_text_styles_unified(
    xml: &str,
    kind: ChartKind,
    axis_roles: &[ChartAxisContext],
    theme: &ThemeColors,
    unsupported_reasons: &mut Vec<ChartUnsupportedReason>,
    limit_exceeded: &mut bool,
) -> ChartTextStyles {
    let (color_map, invalid_color_map) = parse_chart_text_color_map(xml);
    if invalid_color_map {
        add_chart_unsupported(
            unsupported_reasons,
            ChartUnsupportedReason::UnsupportedTextStyle,
        );
        return ChartTextStyles::default();
    }
    let mut reader = Reader::from_str(xml);
    let mut context = ChartTextContext::new(kind, axis_roles);
    let mut defaults: [PartialChartTextStyleObservation; ChartTextSemanticRole::COUNT] =
        std::array::from_fn(|_| PartialChartTextStyleObservation::default());
    let mut facts: [Vec<PaintedChartTextStyleFact>; ChartTextSemanticRole::COUNT] =
        std::array::from_fn(|_| Vec::new());
    let mut role_unsupported = [false; ChartTextSemanticRole::COUNT];
    let mut body: Option<UnifiedChartTextBody> = None;
    let mut property: Option<(ChartTextPropertyTarget, ChartTextPropertyCapture, usize)> = None;
    let mut ignored_end_paragraph_depth = 0usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if ignored_end_paragraph_depth > 0 {
                    ignored_end_paragraph_depth = ignored_end_paragraph_depth.saturating_add(1);
                    continue;
                }
                if let Some((_, capture, depth)) = property.as_mut() {
                    update_chart_text_property_capture(capture, &element, theme, &color_map, false);
                    *depth = depth.saturating_add(1);
                    continue;
                }
                context.start(name);
                if name == b"txPr" || name == b"rich" {
                    let rich = name == b"rich";
                    if let Some(active) = body.as_mut() {
                        active.body_unsupported = true;
                    } else {
                        body = context
                            .text_body_role(rich)
                            .map(|role| UnifiedChartTextBody::new(rich, role));
                    }
                } else if let Some(body) = body.as_mut() {
                    if name == b"endParaRPr" {
                        if !body.paragraph_open || body.run_open {
                            body.body_unsupported = true;
                        }
                        ignored_end_paragraph_depth = 1;
                    } else if name == b"bodyPr" {
                        if body.body_properties_seen || body.list_style_open || body.paragraph_open
                        {
                            body.body_unsupported = true;
                        }
                        body.body_properties_seen = true;
                        body.body_properties_open = true;
                        match parse_chart_body_rotation_state(&element, body.role) {
                            Ok(rotation) => body.rotation = rotation,
                            Err(()) => body.body_unsupported = true,
                        }
                    } else if name == b"lstStyle" {
                        if body.list_style_seen || body.body_properties_open || body.paragraph_open
                        {
                            body.body_unsupported = true;
                        }
                        if !chart_text_attributes_are_subset(&element, &[]) {
                            body.body_unsupported = true;
                        }
                        body.list_style_seen = true;
                        body.list_style_open = true;
                    } else if let Some(level) = chart_text_list_level(name) {
                        if !body.list_style_open
                            || body.list_level_context.is_some()
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.list_level_context = Some(level);
                    } else if name == b"p" {
                        if body.body_properties_open
                            || body.list_style_open
                            || body.paragraph_open
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.paragraph_open = true;
                        body.paragraph_seen = true;
                        body.reset_paragraph();
                    } else if name == b"pPr" {
                        if !body.paragraph_open
                            || body.paragraph_properties_open
                            || body.paragraph_content_started
                            || body.run_open
                        {
                            body.paragraph_unsupported = true;
                        }
                        body.paragraph_properties_open = true;
                        apply_chart_text_paragraph_properties(body, &element);
                    } else if name == b"r" || name == b"fld" {
                        if !body.paragraph_open
                            || body.paragraph_properties_open
                            || body.run_open
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.paragraph_content_started = true;
                        body.run_open = true;
                        body.reset_run();
                    } else if name == b"defRPr" || name == b"rPr" {
                        let valid_parent = if name == b"rPr" {
                            body.run_open && !body.current_run_painted && !body.in_text
                        } else {
                            body.paragraph_properties_open || body.list_level_context.is_some()
                        };
                        if valid_parent {
                            let target = body.property_target(name == b"rPr");
                            property =
                                Some((target, chart_text_partial_from_attributes(&element), 1));
                        } else {
                            body.body_unsupported = true;
                        }
                    } else if name == b"t" {
                        if !body.run_open || body.in_text {
                            body.body_unsupported = true;
                        }
                        body.in_text = true;
                    } else if name == b"br" {
                        if !body.paragraph_open || body.paragraph_properties_open || body.run_open {
                            body.body_unsupported = true;
                        }
                        body.paragraph_content_started = true;
                        role_unsupported[body.role.index()] = true;
                    } else if matches!(name, b"noAutofit" | b"normAutofit" | b"spAutoFit") {
                        if !body.body_properties_open || body.autofit_seen {
                            body.body_unsupported = true;
                        }
                        body.autofit_seen = true;
                        match name {
                            b"noAutofit" => {
                                body.body_unsupported |=
                                    !chart_text_attributes_are_subset(&element, &[]);
                            }
                            b"normAutofit" => {
                                body.body_unsupported |=
                                    !chart_text_norm_autofit_is_neutral(&element);
                            }
                            b"spAutoFit" => body.body_unsupported = true,
                            _ => unreachable!("guarded chart text autofit"),
                        }
                    } else {
                        body.body_unsupported = true;
                    }
                }
            }
            Ok(Event::Empty(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if ignored_end_paragraph_depth > 0 {
                    continue;
                }
                if let Some((_, capture, _)) = property.as_mut() {
                    update_chart_text_property_capture(capture, &element, theme, &color_map, true);
                    continue;
                }
                if name == b"txPr" || name == b"rich" {
                    if let Some(role) = context.text_body_role(name == b"rich") {
                        role_unsupported[role.index()] = true;
                    }
                } else if let Some(body) = body.as_mut() {
                    if name == b"endParaRPr" {
                        if !body.paragraph_open || body.run_open {
                            body.body_unsupported = true;
                        }
                    } else if name == b"bodyPr" {
                        if body.body_properties_seen || body.list_style_open || body.paragraph_open
                        {
                            body.body_unsupported = true;
                        }
                        body.body_properties_seen = true;
                        match parse_chart_body_rotation_state(&element, body.role) {
                            Ok(rotation) => body.rotation = rotation,
                            Err(()) => body.body_unsupported = true,
                        }
                    } else if name == b"lstStyle" {
                        if body.list_style_seen
                            || body.body_properties_open
                            || body.paragraph_open
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.list_style_seen = true;
                    } else if chart_text_list_level(name).is_some() {
                        if !body.list_style_open
                            || body.list_level_context.is_some()
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                    } else if name == b"p" {
                        if body.body_properties_open
                            || body.list_style_open
                            || body.paragraph_open
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.paragraph_seen = true;
                        body.reset_paragraph();
                        body.observe_default_candidate();
                        body.reset_paragraph();
                    } else if name == b"pPr" {
                        if !body.paragraph_open
                            || body.paragraph_properties_open
                            || body.paragraph_content_started
                            || body.run_open
                        {
                            body.paragraph_unsupported = true;
                        }
                        apply_chart_text_paragraph_properties(body, &element);
                    } else if name == b"r" || name == b"fld" {
                        if !body.paragraph_open
                            || body.paragraph_properties_open
                            || body.run_open
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.paragraph_content_started = true;
                    } else if name == b"defRPr" || name == b"rPr" {
                        let valid_parent = if name == b"rPr" {
                            body.run_open && !body.current_run_painted && !body.in_text
                        } else {
                            body.paragraph_properties_open || body.list_level_context.is_some()
                        };
                        if valid_parent {
                            let target = body.property_target(name == b"rPr");
                            body.apply_property(
                                target,
                                chart_text_partial_from_attributes(&element),
                            );
                        } else {
                            body.body_unsupported = true;
                        }
                    } else if name == b"t" {
                        if !body.run_open || !chart_text_attributes_are_subset(&element, &[]) {
                            body.body_unsupported = true;
                        }
                    } else if name == b"br" {
                        if !body.paragraph_open || body.paragraph_properties_open || body.run_open {
                            body.body_unsupported = true;
                        }
                        body.paragraph_content_started = true;
                        role_unsupported[body.role.index()] = true;
                    } else if matches!(name, b"noAutofit" | b"normAutofit" | b"spAutoFit") {
                        if !body.body_properties_open || body.autofit_seen {
                            body.body_unsupported = true;
                        }
                        body.autofit_seen = true;
                        match name {
                            b"noAutofit" => {
                                body.body_unsupported |=
                                    !chart_text_attributes_are_subset(&element, &[]);
                            }
                            b"normAutofit" => {
                                body.body_unsupported |=
                                    !chart_text_norm_autofit_is_neutral(&element);
                            }
                            b"spAutoFit" => body.body_unsupported = true,
                            _ => unreachable!("guarded chart text autofit"),
                        }
                    } else {
                        body.body_unsupported = true;
                    }
                }
            }
            Ok(Event::Text(text)) => {
                if let Some(body) = body.as_mut().filter(|body| body.in_text) {
                    if !text_of(&text).is_empty() {
                        let role = body.role;
                        let fact = body.current_fact();
                        push_chart_text_style_fact(
                            role,
                            fact,
                            &mut facts,
                            &mut role_unsupported,
                            limit_exceeded,
                        );
                    }
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(body) = body.as_mut().filter(|body| body.in_text) {
                    with_general_ref_text(&reference, |text| {
                        if !text.is_empty() {
                            let role = body.role;
                            let fact = body.current_fact();
                            push_chart_text_style_fact(
                                role,
                                fact,
                                &mut facts,
                                &mut role_unsupported,
                                limit_exceeded,
                            );
                        }
                    });
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(body) = body.as_mut().filter(|body| body.in_text) {
                    if !text.as_ref().is_empty() {
                        let role = body.role;
                        let fact = body.current_fact();
                        push_chart_text_style_fact(
                            role,
                            fact,
                            &mut facts,
                            &mut role_unsupported,
                            limit_exceeded,
                        );
                    }
                }
            }
            Ok(Event::End(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if ignored_end_paragraph_depth > 0 {
                    ignored_end_paragraph_depth = ignored_end_paragraph_depth.saturating_sub(1);
                    continue;
                }
                if let Some((_, capture, depth)) = property.as_mut() {
                    *depth = depth.saturating_sub(1);
                    if *depth > 0 {
                        finish_chart_text_property_element(capture, name);
                        continue;
                    }
                    let (target, capture, _) = property.take().expect("text property is active");
                    if let Some(body) = body.as_mut() {
                        body.apply_property(target, capture);
                    }
                    continue;
                }
                match name {
                    b"t" => {
                        if let Some(body) = body.as_mut() {
                            if !body.in_text {
                                body.body_unsupported = true;
                            }
                            body.in_text = false;
                        }
                    }
                    b"r" | b"fld" => {
                        if let Some(body) = body.as_mut() {
                            if !body.run_open {
                                body.body_unsupported = true;
                            }
                            if body.run_unsupported && body.current_run_painted {
                                role_unsupported[body.role.index()] = true;
                            }
                            body.run_open = false;
                            body.reset_run();
                        }
                    }
                    b"pPr" => {
                        if let Some(body) = body.as_mut() {
                            if !body.paragraph_properties_open {
                                body.paragraph_unsupported = true;
                            }
                            body.paragraph_properties_open = false;
                        }
                    }
                    b"p" => {
                        if let Some(body) = body.as_mut() {
                            if !body.paragraph_open
                                || body.paragraph_properties_open
                                || body.run_open
                            {
                                body.body_unsupported = true;
                            }
                            if body.current_paragraph_painted
                                && (body.paragraph_unsupported
                                    || body.list_unsupported[body.paragraph_level])
                            {
                                role_unsupported[body.role.index()] = true;
                            }
                            body.observe_default_candidate();
                            body.paragraph_open = false;
                            body.reset_paragraph();
                        }
                    }
                    b"bodyPr" => {
                        if let Some(body) = body.as_mut() {
                            if !body.body_properties_open {
                                body.body_unsupported = true;
                            }
                            body.body_properties_open = false;
                        }
                    }
                    b"lstStyle" => {
                        if let Some(body) = body.as_mut() {
                            if !body.list_style_open || body.list_level_context.is_some() {
                                body.body_unsupported = true;
                            }
                            body.list_style_open = false;
                        }
                    }
                    b"lvl1pPr" | b"lvl2pPr" | b"lvl3pPr" | b"lvl4pPr" | b"lvl5pPr" | b"lvl6pPr"
                    | b"lvl7pPr" | b"lvl8pPr" | b"lvl9pPr" => {
                        if let Some(body) = body.as_mut() {
                            body.list_level_context = None;
                        }
                    }
                    b"txPr" | b"rich" => {
                        if let Some(mut completed) = body.take() {
                            completed.body_unsupported |= completed.body_properties_open
                                || completed.list_style_open
                                || completed.list_level_context.is_some()
                                || completed.paragraph_open
                                || completed.paragraph_properties_open
                                || completed.run_open
                                || completed.in_text
                                || !completed.body_properties_seen
                                || !completed.paragraph_seen;
                            if completed.painted_paragraphs > 1 {
                                role_unsupported[completed.role.index()] = true;
                            }
                            if !completed.rich {
                                if completed.default_candidate.is_none() {
                                    completed.observe_default_candidate();
                                }
                                let observation = &mut defaults[completed.role.index()];
                                if completed.body_unsupported
                                    || completed.default_candidate_mixed
                                    || completed.default_candidate_unsupported
                                {
                                    observation.mark_unsupported();
                                } else {
                                    observation.observe(
                                        completed.default_candidate.unwrap_or_default(),
                                        completed.rotation,
                                    );
                                }
                            } else if completed.body_unsupported && completed.painted_paragraphs > 0
                            {
                                role_unsupported[completed.role.index()] = true;
                            }
                        }
                    }
                    _ => {}
                }
                context.end(name);
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                add_chart_unsupported(
                    unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedTextStyle,
                );
                return ChartTextStyles::default();
            }
            _ => {}
        }
    }
    if body.is_some() || property.is_some() || ignored_end_paragraph_depth > 0 {
        add_chart_unsupported(
            unsupported_reasons,
            ChartUnsupportedReason::UnsupportedTextStyle,
        );
        return ChartTextStyles::default();
    }

    let chart_default = match &defaults[ChartTextSemanticRole::ChartDefault.index()] {
        PartialChartTextStyleObservation::Uniform { style, rotation } => Some((style, *rotation)),
        PartialChartTextStyleObservation::Unseen => None,
        PartialChartTextStyleObservation::Mixed => {
            add_chart_unsupported(unsupported_reasons, ChartUnsupportedReason::MixedTextStyle);
            return ChartTextStyles::default();
        }
        PartialChartTextStyleObservation::Unsupported => {
            add_chart_unsupported(
                unsupported_reasons,
                ChartUnsupportedReason::UnsupportedTextStyle,
            );
            return ChartTextStyles::default();
        }
    };

    let mut resolved = ChartTextStyleObservations::default();
    for role in [
        ChartTextSemanticRole::ChartTitle,
        ChartTextSemanticRole::CategoryAxisTitle,
        ChartTextSemanticRole::ValueAxisTitle,
        ChartTextSemanticRole::Legend,
        ChartTextSemanticRole::CategoryAxisLabels,
        ChartTextSemanticRole::ValueAxisLabels,
        ChartTextSemanticRole::DataLabels,
    ] {
        let role_default = match &defaults[role.index()] {
            PartialChartTextStyleObservation::Uniform { style, rotation } => {
                Some((style, *rotation))
            }
            PartialChartTextStyleObservation::Unseen => None,
            PartialChartTextStyleObservation::Mixed => {
                resolved.get_mut(role).mark_mixed();
                continue;
            }
            PartialChartTextStyleObservation::Unsupported => {
                resolved.get_mut(role).mark_unsupported();
                continue;
            }
        };
        if role_unsupported[role.index()] {
            resolved.get_mut(role).mark_unsupported();
            continue;
        }
        if facts[role.index()].is_empty() {
            if chart_default.is_some() || role_default.is_some() {
                resolved
                    .get_mut(role)
                    .observe(resolve_unified_chart_text_style(
                        role,
                        theme,
                        &color_map,
                        chart_default,
                        role_default,
                        None,
                    ));
            }
            continue;
        }
        for fact in &facts[role.index()] {
            if fact.unsupported {
                resolved.get_mut(role).mark_unsupported();
                break;
            }
            resolved
                .get_mut(role)
                .observe(resolve_unified_chart_text_style(
                    role,
                    theme,
                    &color_map,
                    chart_default,
                    role_default,
                    Some(fact),
                ));
        }
    }
    resolved.finish(unsupported_reasons)
}

#[allow(clippy::too_many_arguments)]
fn append_chart_text(
    current_series: &mut Option<ParsedChartSeries>,
    capture_series_field: Option<ChartSeriesField>,
    capture_cache_value: bool,
    cache_value: &mut String,
    title_target: Option<ChartTitleTarget>,
    in_title_text: bool,
    title_text: &mut String,
    title_text_valid: &mut bool,
    text: &str,
    limit_exceeded: &mut bool,
    cache_value_valid: &mut bool,
) {
    if capture_cache_value {
        let remaining = MAX_XLSX_CHART_CACHE_VALUE_BYTES.saturating_sub(cache_value.len());
        if text.len() <= remaining {
            cache_value.push_str(text);
        } else {
            *limit_exceeded = true;
            *cache_value_valid = false;
        }
    } else if let Some(field) = capture_series_field {
        if let Some(series) = current_series.as_mut() {
            let invalid_bit = match field {
                ChartSeriesField::Name => 1 << 0,
                ChartSeriesField::Categories => 1 << 1,
                ChartSeriesField::Values => 1 << 2,
                ChartSeriesField::BubbleSizes => 1 << 3,
            };
            if series.invalid_text_fields & invalid_bit != 0 {
                return;
            }
            let slot = match field {
                ChartSeriesField::Name => &mut series.name,
                ChartSeriesField::Categories => &mut series.categories,
                ChartSeriesField::Values => &mut series.values,
                ChartSeriesField::BubbleSizes => &mut series.bubble_sizes,
            };
            let current_len = slot.as_ref().map_or(0, String::len);
            if text.len() <= MAX_XLSX_CHART_TEXT_FIELD_BYTES.saturating_sub(current_len) {
                slot.get_or_insert_with(String::new).push_str(text);
            } else {
                *slot = None;
                series.invalid_text_fields |= invalid_bit;
                *limit_exceeded = true;
            }
        }
    } else if title_target.is_some() && in_title_text {
        if *title_text_valid
            && text.len() <= MAX_XLSX_CHART_TEXT_FIELD_BYTES.saturating_sub(title_text.len())
        {
            title_text.push_str(text);
        } else {
            title_text.clear();
            *title_text_valid = false;
            *limit_exceeded = true;
        }
    }
}

fn chart_cache_points_mut(
    cache: &mut ChartSeriesCache,
    field: ChartSeriesField,
) -> &mut Vec<ChartCachedPoint> {
    match field {
        ChartSeriesField::Name => &mut cache.name,
        ChartSeriesField::Categories => &mut cache.categories,
        ChartSeriesField::Values => &mut cache.values,
        ChartSeriesField::BubbleSizes => &mut cache.bubble_sizes,
    }
}

fn chart_kind_element(name: &[u8]) -> Option<ChartKind> {
    match name {
        b"barChart" => Some(ChartKind::Bar),
        b"lineChart" => Some(ChartKind::Line),
        b"pieChart" => Some(ChartKind::Pie),
        b"scatterChart" => Some(ChartKind::Scatter),
        b"areaChart" => Some(ChartKind::Area),
        b"doughnutChart" => Some(ChartKind::Doughnut),
        b"radarChart" => Some(ChartKind::Radar),
        b"bubbleChart" => Some(ChartKind::Bubble),
        _ => None,
    }
}

fn chart_3d_kind_element(name: &[u8]) -> Option<ChartKind> {
    match name {
        b"bar3DChart" => Some(ChartKind::Bar),
        b"line3DChart" => Some(ChartKind::Line),
        b"pie3DChart" => Some(ChartKind::Pie),
        b"area3DChart" => Some(ChartKind::Area),
        _ => None,
    }
}

fn add_chart_unsupported(
    reasons: &mut Vec<ChartUnsupportedReason>,
    reason: ChartUnsupportedReason,
) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn add_chart_series_style_loss(style: &mut ChartSeriesStyle, loss: ChartSeriesStyleLossKind) {
    if !style.losses.contains(&loss) {
        style.losses.push(loss);
    }
}

fn add_chart_frame_style_loss(
    losses: &mut Vec<ChartFrameStyleLossKind>,
    loss: ChartFrameStyleLossKind,
) {
    if !losses.contains(&loss) {
        losses.push(loss);
    }
}

fn retain_chart_marker_symbol(style: &mut ChartSeriesStyle, value: Option<&str>) {
    style.marker = match value {
        Some("none") => ChartMarkerSymbol::None,
        Some("circle") => ChartMarkerSymbol::Circle,
        Some("square") => ChartMarkerSymbol::Square,
        Some("diamond") => ChartMarkerSymbol::Diamond,
        Some("triangle") => ChartMarkerSymbol::Triangle,
        Some("auto") | None => ChartMarkerSymbol::Automatic,
        Some(_) => {
            add_chart_series_style_loss(style, ChartSeriesStyleLossKind::UnsupportedMarkerSymbol);
            ChartMarkerSymbol::Automatic
        }
    };
}

fn retain_chart_marker_size(style: &mut ChartSeriesStyle, value: Option<&str>) {
    match value.and_then(|value| value.parse::<u8>().ok()) {
        Some(size @ 2..=72) => style.marker_size = Some(size),
        _ => add_chart_series_style_loss(style, ChartSeriesStyleLossKind::InvalidMarkerSize),
    }
}

fn retain_chart_series_line_width(style: &mut ChartSeriesStyle, value: Option<&str>) {
    let Some(value) = value else {
        // LibreOffice's DrawingML chart import initializes an authored `a:ln`
        // to one point when the optional width is absent.
        style.line_width_emu = Some(12_700);
        return;
    };
    match value.parse::<u32>() {
        Ok(width) if width <= MAX_OOXML_CHART_LINE_WIDTH_EMU => {
            style.line_width_emu = Some(width);
        }
        _ => add_chart_series_style_loss(style, ChartSeriesStyleLossKind::InvalidLineWidth),
    }
}

fn chart_series_line_color(
    name: &[u8],
    element: &quick_xml::events::BytesStart<'_>,
    theme: &ThemeColors,
    color_map: &ChartTextColorMap,
) -> Option<Color> {
    match name {
        b"srgbClr" => chart_text_attributes_are_subset(element, &[b"val"])
            .then(|| unique_attr(element, b"val").ok().flatten())
            .flatten()
            .as_deref()
            .and_then(parse_chart_rgb),
        b"sysClr" => {
            if !chart_text_attributes_are_subset(element, &[b"val", b"lastClr"])
                || !matches!(unique_attr(element, b"val"), Ok(Some(value)) if !value.is_empty())
            {
                return None;
            }
            unique_attr(element, b"lastClr")
                .ok()
                .flatten()
                .as_deref()
                .and_then(parse_chart_rgb)
        }
        b"schemeClr" => chart_text_attributes_are_subset(element, &[b"val"])
            .then(|| unique_attr(element, b"val").ok().flatten())
            .flatten()
            .as_deref()
            .and_then(|value| chart_scheme_color(theme, color_map, value)),
        _ => None,
    }
}

fn observe_chart_kind(
    kind: &mut Option<ChartKind>,
    next: ChartKind,
    reasons: &mut Vec<ChartUnsupportedReason>,
) {
    match *kind {
        Some(previous) if previous != next => {
            add_chart_unsupported(reasons, ChartUnsupportedReason::Combo);
        }
        None => *kind = Some(next),
        _ => {}
    }
}

fn is_external_chart_reference(reference: &str) -> bool {
    let Some(open) = reference.find('[') else {
        return false;
    };
    let Some(close) = reference[open + 1..]
        .find(']')
        .map(|index| index + open + 1)
    else {
        return false;
    };
    reference[close + 1..].contains('!')
}

fn chart_plot_option_supported(
    kind: Option<ChartKind>,
    name: &[u8],
    element: &quick_xml::events::BytesStart<'_>,
) -> Option<bool> {
    if !matches!(
        name,
        b"grouping"
            | b"overlap"
            | b"gapWidth"
            | b"smooth"
            | b"varyColors"
            | b"firstSliceAng"
            | b"holeSize"
            | b"explosion"
            | b"showNegBubbles"
            | b"bubble3D"
            | b"showDLblsOverMax"
            | b"dispBlanksAs"
            | b"plotVisOnly"
            | b"scatterStyle"
            | b"radarStyle"
            | b"gapDepth"
            | b"bubbleScale"
            | b"sizeRepresents"
            | b"secondPieSize"
            | b"splitType"
            | b"splitPos"
            | b"custSplit"
            | b"ofPieType"
            | b"serLines"
            | b"dropLines"
            | b"hiLowLines"
            | b"upDownBars"
            | b"shape"
    ) {
        return None;
    }
    if !chart_text_attributes_are_subset(element, &[b"val"]) {
        return Some(false);
    }
    let value = || unique_attr(element, b"val");
    let numeric = || {
        value()
            .ok()
            .flatten()
            .and_then(|value| value.parse::<i64>().ok())
    };
    let boolean = || parse_chart_boolean_element(element).ok();
    Some(match name {
        b"grouping" => match (kind, value()) {
            (Some(ChartKind::Bar), Ok(Some(value))) => value == "clustered",
            (Some(ChartKind::Line | ChartKind::Area), Ok(Some(value))) => value == "standard",
            _ => false,
        },
        b"overlap" => kind == Some(ChartKind::Bar) && numeric() == Some(0),
        b"gapWidth" => kind == Some(ChartKind::Bar) && numeric() == Some(150),
        b"smooth" => {
            matches!(kind, Some(ChartKind::Line | ChartKind::Scatter)) && boolean() == Some(false)
        }
        b"varyColors" => {
            boolean() == Some(matches!(kind, Some(ChartKind::Pie | ChartKind::Doughnut)))
        }
        b"firstSliceAng" => {
            matches!(kind, Some(ChartKind::Pie | ChartKind::Doughnut)) && numeric() == Some(0)
        }
        b"holeSize" => kind == Some(ChartKind::Doughnut) && numeric() == Some(50),
        b"explosion" => {
            matches!(kind, Some(ChartKind::Pie | ChartKind::Doughnut)) && numeric() == Some(0)
        }
        b"showNegBubbles" | b"bubble3D" => {
            kind == Some(ChartKind::Bubble) && boolean() == Some(false)
        }
        b"showDLblsOverMax" => boolean() == Some(false),
        b"dispBlanksAs" => matches!(value(), Ok(Some(value)) if value == "gap"),
        b"plotVisOnly" => boolean() == Some(true),
        b"scatterStyle" => {
            kind == Some(ChartKind::Scatter)
                && matches!(value(), Ok(Some(value)) if value == "marker")
        }
        b"radarStyle" => {
            kind == Some(ChartKind::Radar)
                && matches!(value(), Ok(Some(value)) if value == "standard")
        }
        b"gapDepth" | b"bubbleScale" | b"sizeRepresents" | b"secondPieSize" | b"splitType"
        | b"splitPos" | b"custSplit" | b"ofPieType" | b"serLines" | b"dropLines"
        | b"hiLowLines" | b"upDownBars" | b"shape" => false,
        _ => return None,
    })
}

fn retain_chart_series_position(
    series: &mut ParsedChartSeries,
    name: &[u8],
    element: &quick_xml::events::BytesStart<'_>,
) -> bool {
    let seen = if name == b"idx" {
        &mut series.source_index_seen
    } else {
        &mut series.source_order_seen
    };
    if *seen {
        return false;
    }
    *seen = true;
    unique_attr(element, b"val")
        .ok()
        .flatten()
        .and_then(|value| value.parse::<usize>().ok())
        == Some(series.source_position)
}

#[derive(Debug, Default)]
struct ChartDataLabelCapture {
    show_value: Option<bool>,
    deleted: Option<bool>,
    show_legend_key: Option<bool>,
    show_category_name: Option<bool>,
    show_series_name: Option<bool>,
    show_percent: Option<bool>,
    show_bubble_size: Option<bool>,
    show_leader_lines: Option<bool>,
    unsupported_formatting: bool,
    unsupported: bool,
}

impl ChartDataLabelCapture {
    fn set_boolean(
        target: &mut Option<bool>,
        element: &quick_xml::events::BytesStart<'_>,
    ) -> std::result::Result<(), ()> {
        if target.is_some() {
            return Err(());
        }
        let value = match unique_attr(element, b"val")? {
            Some(value) => parse_chart_bool_attr(&value).ok_or(())?,
            None => true,
        };
        *target = Some(value);
        Ok(())
    }

    fn observe(&mut self, element: &quick_xml::events::BytesStart<'_>) {
        let qualified_name = element.name();
        let name = local(qualified_name.as_ref());
        let result = match name {
            b"showVal" => Self::set_boolean(&mut self.show_value, element),
            b"delete" => Self::set_boolean(&mut self.deleted, element),
            b"showLegendKey" => Self::set_boolean(&mut self.show_legend_key, element),
            b"showCatName" => Self::set_boolean(&mut self.show_category_name, element),
            b"showSerName" => Self::set_boolean(&mut self.show_series_name, element),
            b"showPercent" => Self::set_boolean(&mut self.show_percent, element),
            b"showBubbleSize" => Self::set_boolean(&mut self.show_bubble_size, element),
            b"showLeaderLines" => Self::set_boolean(&mut self.show_leader_lines, element),
            b"dLblPos" | b"numFmt" | b"separator" | b"tx" | b"leaderLines" | b"spPr"
            | b"layout" => {
                self.unsupported_formatting = true;
                Ok(())
            }
            b"txPr" => Ok(()),
            b"dLbl" | b"extLst" => {
                self.unsupported = true;
                Ok(())
            }
            _ => {
                self.unsupported = true;
                Ok(())
            }
        };
        if result.is_err() {
            self.unsupported = true;
        }
    }

    fn finish(self) -> std::result::Result<bool, ()> {
        let unsupported_content = [
            self.show_legend_key,
            self.show_category_name,
            self.show_series_name,
            self.show_percent,
            self.show_bubble_size,
            self.show_leader_lines,
        ]
        .into_iter()
        .flatten()
        .any(|value| value);
        let deleted = self.deleted.unwrap_or(false);
        let visible = !deleted && self.show_value.unwrap_or(false);
        if self.unsupported
            || (!deleted && unsupported_content)
            || (visible && self.unsupported_formatting)
        {
            Err(())
        } else {
            Ok(visible)
        }
    }
}

fn parse_chart_data_labels(xml: &str) -> (bool, bool) {
    fn retain_policy(
        target: Option<usize>,
        policy: std::result::Result<bool, ()>,
        global: &mut Option<bool>,
        per_series: &mut Vec<Option<bool>>,
        unsupported: &mut bool,
    ) {
        let Ok(policy) = policy else {
            *unsupported = true;
            return;
        };
        match target {
            Some(index) => {
                if per_series.len() <= index {
                    per_series.resize(index + 1, None);
                }
                if per_series[index].replace(policy).is_some() {
                    *unsupported = true;
                }
            }
            None => {
                if global.replace(policy).is_some() {
                    *unsupported = true;
                }
            }
        }
    }

    let mut reader = Reader::from_str(xml);
    let mut current_series = None;
    let mut next_series = 0usize;
    let mut capture: Option<(usize, Option<usize>, ChartDataLabelCapture)> = None;
    let mut global = None;
    let mut per_series = Vec::<Option<bool>>::new();
    let mut unsupported = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if let Some((depth, _, policy)) = capture.as_mut() {
                    if *depth == 1 {
                        policy.observe(&element);
                    }
                    *depth = depth.saturating_add(1);
                } else if name == b"dLbls" {
                    capture = Some((1, current_series, ChartDataLabelCapture::default()));
                } else if name == b"ser" {
                    if next_series < MAX_XLSX_CHART_SERIES_PER_WORKBOOK {
                        current_series = Some(next_series);
                        next_series += 1;
                    } else {
                        current_series = None;
                        unsupported = true;
                    }
                }
            }
            Ok(Event::Empty(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if let Some((depth, _, policy)) = capture.as_mut() {
                    if *depth == 1 {
                        policy.observe(&element);
                    }
                } else if name == b"dLbls" {
                    retain_policy(
                        current_series,
                        Ok(false),
                        &mut global,
                        &mut per_series,
                        &mut unsupported,
                    );
                }
            }
            Ok(Event::End(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if let Some((depth, _, _)) = capture.as_mut() {
                    *depth = depth.saturating_sub(1);
                    if *depth == 0 {
                        let (_, target, policy) = capture.take().expect("label capture is active");
                        retain_policy(
                            target,
                            policy.finish(),
                            &mut global,
                            &mut per_series,
                            &mut unsupported,
                        );
                    }
                } else if name == b"ser" {
                    current_series = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                unsupported = true;
                break;
            }
            _ => {}
        }
    }
    if capture.is_some() {
        unsupported = true;
    }

    let series_count = next_series.max(per_series.len());
    let effective = if series_count == 0 {
        vec![global.unwrap_or(false)]
    } else {
        (0..series_count)
            .map(|index| {
                per_series
                    .get(index)
                    .copied()
                    .flatten()
                    .or(global)
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>()
    };
    if effective.iter().any(|value| *value != effective[0]) {
        unsupported = true;
    }
    (
        effective.first().copied().unwrap_or(false) && !unsupported,
        unsupported,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawChartAxisKind {
    Category,
    Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawChartAxisPosition {
    Bottom,
    Left,
}

#[derive(Debug)]
struct RawChartAxis {
    id: Option<u32>,
    cross_axis_id: Option<u32>,
    kind: RawChartAxisKind,
    visible: bool,
    visibility_valid: bool,
    major_gridlines: bool,
    unsupported_presentation: bool,
    scaling_open: bool,
    major_gridlines_open: bool,
    tick_label_position_seen: bool,
    position: Option<RawChartAxisPosition>,
    number_format_seen: bool,
    crosses_seen: bool,
    auto_seen: bool,
    label_alignment_seen: bool,
    label_offset_seen: bool,
    cross_between_seen: bool,
    cross_between_shifted: Option<bool>,
}

#[derive(Debug)]
struct RawChartPlot {
    kind: ChartKind,
    axis_ids: Vec<u32>,
}

#[derive(Debug, Default)]
struct ChartAxisSemantics {
    axis_roles: Vec<ChartAxisContext>,
    category_visible: Option<bool>,
    value_visible: Option<bool>,
    category_major_gridlines: bool,
    value_major_gridlines: bool,
    category_position: Option<RawChartAxisPosition>,
    value_position: Option<RawChartAxisPosition>,
    category_axis_shifted: Option<bool>,
    invalid_visibility: bool,
    unsupported_topology: bool,
    unsupported_presentation: bool,
}

fn parse_chart_axis_semantics(xml: &str) -> ChartAxisSemantics {
    fn element_u32(element: &quick_xml::events::BytesStart<'_>) -> std::result::Result<u32, ()> {
        unique_attr(element, b"val")?
            .ok_or(())?
            .parse::<u32>()
            .map_err(|_| ())
    }

    fn element_bool(element: &quick_xml::events::BytesStart<'_>) -> std::result::Result<bool, ()> {
        match unique_attr(element, b"val")? {
            Some(value) => parse_chart_bool_attr(&value).ok_or(()),
            None => Ok(true),
        }
    }

    fn observe_axis_presentation(
        axis: &mut RawChartAxis,
        name: &[u8],
        element: &quick_xml::events::BytesStart<'_>,
        start: bool,
    ) {
        let value = || unique_attr(element, b"val");
        match name {
            b"majorGridlines" => {
                axis.major_gridlines = true;
                axis.major_gridlines_open = start;
                axis.unsupported_presentation |= !chart_text_attributes_are_subset(element, &[]);
            }
            b"scaling" => {
                axis.scaling_open = start;
                axis.unsupported_presentation |= !chart_text_attributes_are_subset(element, &[]);
            }
            b"tickLblPos" => {
                if axis.tick_label_position_seen {
                    axis.unsupported_presentation = true;
                }
                axis.tick_label_position_seen = true;
                match value() {
                    Ok(Some(value)) if value == "nextTo" => {}
                    Ok(Some(_)) | Ok(None) | Err(()) => axis.unsupported_presentation = true,
                }
            }
            b"axPos" => {
                if axis.position.is_some() || !chart_text_attributes_are_subset(element, &[b"val"])
                {
                    axis.unsupported_presentation = true;
                }
                let position = match value() {
                    Ok(Some(value)) if value == "b" => Some(RawChartAxisPosition::Bottom),
                    Ok(Some(value)) if value == "l" => Some(RawChartAxisPosition::Left),
                    Ok(Some(_)) | Ok(None) | Err(()) => None,
                };
                if position.is_none() {
                    axis.unsupported_presentation = true;
                } else if axis.position.is_none() {
                    axis.position = position;
                }
            }
            b"numFmt" => {
                if axis.number_format_seen {
                    axis.unsupported_presentation = true;
                }
                axis.number_format_seen = true;
                let format_code = unique_attr(element, b"formatCode");
                let source_linked = unique_attr(element, b"sourceLinked");
                let source_linked_is_default = matches!(source_linked, Ok(None))
                    || matches!(source_linked, Ok(Some(ref value)) if value == "1" || value == "true");
                if !chart_text_attributes_are_subset(element, &[b"formatCode", b"sourceLinked"])
                    || !matches!(format_code, Ok(Some(ref value)) if value == "General")
                    || !source_linked_is_default
                {
                    axis.unsupported_presentation = true;
                }
            }
            b"crosses" => {
                if axis.crosses_seen || !chart_text_attributes_are_subset(element, &[b"val"]) {
                    axis.unsupported_presentation = true;
                }
                axis.crosses_seen = true;
                // `autoZero` is the canonical default emitted by spreadsheet
                // producers. It carries the same retained semantics as an
                // omitted crossing policy; explicit coordinates and the
                // non-default edge policies remain unsupported.
                if !matches!(value(), Ok(Some(ref value)) if value == "autoZero") {
                    axis.unsupported_presentation = true;
                }
            }
            b"auto" => {
                if axis.auto_seen
                    || axis.kind != RawChartAxisKind::Category
                    || !chart_text_attributes_are_subset(element, &[b"val"])
                    || element_bool(element) != Ok(true)
                {
                    axis.unsupported_presentation = true;
                }
                axis.auto_seen = true;
            }
            b"lblAlgn" => {
                if axis.label_alignment_seen
                    || axis.kind != RawChartAxisKind::Category
                    || !chart_text_attributes_are_subset(element, &[b"val"])
                    || !matches!(value(), Ok(Some(ref value)) if value == "ctr")
                {
                    axis.unsupported_presentation = true;
                }
                axis.label_alignment_seen = true;
            }
            b"lblOffset" => {
                if axis.label_offset_seen
                    || axis.kind != RawChartAxisKind::Category
                    || !chart_text_attributes_are_subset(element, &[b"val"])
                    || !matches!(value(), Ok(Some(ref value)) if value == "100")
                {
                    axis.unsupported_presentation = true;
                }
                axis.label_offset_seen = true;
            }
            b"crossBetween" => {
                if axis.cross_between_seen
                    || axis.kind != RawChartAxisKind::Value
                    || !chart_text_attributes_are_subset(element, &[b"val"])
                {
                    axis.unsupported_presentation = true;
                }
                axis.cross_between_seen = true;
                match value() {
                    Ok(Some(value)) if value == "between" => {
                        axis.cross_between_shifted = Some(true);
                    }
                    Ok(Some(value)) if value == "midCat" => {
                        axis.cross_between_shifted = Some(false);
                    }
                    Ok(Some(_)) | Ok(None) | Err(()) => {
                        axis.unsupported_presentation = true;
                    }
                }
            }
            b"majorTickMark" | b"minorTickMark" => match value() {
                Ok(Some(value)) if value == "none" => {}
                Ok(Some(_)) | Ok(None) | Err(()) => axis.unsupported_presentation = true,
            },
            b"axId" | b"delete" | b"crossAx" | b"title" | b"txPr" => {}
            b"minorGridlines" | b"majorUnit" | b"minorUnit" | b"tickLblSkip" | b"tickMarkSkip"
            | b"crossesAt" | b"dispUnits" | b"spPr" | b"extLst" | b"noMultiLvlLbl" => {
                axis.unsupported_presentation = true;
            }
            _ => axis.unsupported_presentation = true,
        }
    }

    fn observe_axis_scaling_child(
        axis: &mut RawChartAxis,
        name: &[u8],
        element: &quick_xml::events::BytesStart<'_>,
    ) {
        match name {
            b"orientation" => match unique_attr(element, b"val") {
                Ok(Some(value)) if value == "minMax" => {}
                Ok(Some(_)) | Ok(None) | Err(()) => axis.unsupported_presentation = true,
            },
            b"logBase" | b"min" | b"max" | b"extLst" => {
                axis.unsupported_presentation = true;
            }
            _ => axis.unsupported_presentation = true,
        }
    }

    let mut reader = Reader::from_str(xml);
    let mut plots = Vec::<RawChartPlot>::new();
    let mut plot: Option<(usize, RawChartPlot)> = None;
    let mut axes = Vec::<RawChartAxis>::new();
    let mut axis: Option<(usize, RawChartAxis, bool)> = None;
    let mut malformed = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if let Some((depth, current, delete_seen)) = axis.as_mut() {
                    let current_depth = *depth;
                    if current_depth == 1 {
                        match name {
                            b"axId" => match element_u32(&element) {
                                Ok(id) if current.id.replace(id).is_none() => {}
                                _ => malformed = true,
                            },
                            b"delete" => {
                                if *delete_seen {
                                    current.visibility_valid = false;
                                } else {
                                    *delete_seen = true;
                                    match element_bool(&element) {
                                        Ok(deleted) => current.visible = !deleted,
                                        Err(()) => current.visibility_valid = false,
                                    }
                                }
                            }
                            b"crossAx" => match element_u32(&element) {
                                Ok(id) if current.cross_axis_id.replace(id).is_none() => {}
                                _ => malformed = true,
                            },
                            _ => observe_axis_presentation(current, name, &element, true),
                        }
                    } else if current.scaling_open && current_depth == 2 {
                        observe_axis_scaling_child(current, name, &element);
                    } else if current.major_gridlines_open && current_depth >= 2 {
                        current.unsupported_presentation = true;
                    }
                    *depth = depth.saturating_add(1);
                } else if matches!(name, b"catAx" | b"dateAx" | b"valAx") {
                    axis = Some((
                        1,
                        RawChartAxis {
                            id: None,
                            cross_axis_id: None,
                            kind: if name == b"valAx" {
                                RawChartAxisKind::Value
                            } else {
                                RawChartAxisKind::Category
                            },
                            visible: true,
                            visibility_valid: true,
                            major_gridlines: false,
                            unsupported_presentation: name == b"dateAx",
                            scaling_open: false,
                            major_gridlines_open: false,
                            tick_label_position_seen: false,
                            position: None,
                            number_format_seen: false,
                            crosses_seen: false,
                            auto_seen: false,
                            label_alignment_seen: false,
                            label_offset_seen: false,
                            cross_between_seen: false,
                            cross_between_shifted: None,
                        },
                        false,
                    ));
                } else if let Some((depth, current)) = plot.as_mut() {
                    let direct_child = *depth == 1;
                    *depth = depth.saturating_add(1);
                    if direct_child && name == b"axId" {
                        match element_u32(&element) {
                            Ok(id) if current.axis_ids.len() < MAX_XLSX_CHART_AXIS_ITEMS => {
                                current.axis_ids.push(id);
                            }
                            Err(()) => malformed = true,
                            Ok(_) => malformed = true,
                        }
                    }
                } else if let Some(kind) =
                    chart_kind_element(name).or_else(|| chart_3d_kind_element(name))
                {
                    plot = Some((
                        1,
                        RawChartPlot {
                            kind,
                            axis_ids: Vec::new(),
                        },
                    ));
                }
            }
            Ok(Event::Empty(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if let Some((depth, current, delete_seen)) = axis.as_mut() {
                    if *depth == 1 {
                        match name {
                            b"axId" => match element_u32(&element) {
                                Ok(id) if current.id.replace(id).is_none() => {}
                                _ => malformed = true,
                            },
                            b"delete" => {
                                if *delete_seen {
                                    current.visibility_valid = false;
                                } else {
                                    *delete_seen = true;
                                    match element_bool(&element) {
                                        Ok(deleted) => current.visible = !deleted,
                                        Err(()) => current.visibility_valid = false,
                                    }
                                }
                            }
                            b"crossAx" => match element_u32(&element) {
                                Ok(id) if current.cross_axis_id.replace(id).is_none() => {}
                                _ => malformed = true,
                            },
                            _ => observe_axis_presentation(current, name, &element, false),
                        }
                    } else if current.scaling_open && *depth == 2 {
                        observe_axis_scaling_child(current, name, &element);
                    } else if current.major_gridlines_open && *depth >= 2 {
                        current.unsupported_presentation = true;
                    }
                } else if matches!(name, b"catAx" | b"dateAx" | b"valAx") {
                    if axes.len() < MAX_XLSX_CHART_AXIS_ITEMS {
                        axes.push(RawChartAxis {
                            id: None,
                            cross_axis_id: None,
                            kind: if name == b"valAx" {
                                RawChartAxisKind::Value
                            } else {
                                RawChartAxisKind::Category
                            },
                            visible: true,
                            visibility_valid: true,
                            major_gridlines: false,
                            unsupported_presentation: name == b"dateAx",
                            scaling_open: false,
                            major_gridlines_open: false,
                            tick_label_position_seen: false,
                            position: None,
                            number_format_seen: false,
                            crosses_seen: false,
                            auto_seen: false,
                            label_alignment_seen: false,
                            label_offset_seen: false,
                            cross_between_seen: false,
                            cross_between_shifted: None,
                        });
                    }
                    malformed = true;
                } else if let Some((depth, current)) = plot.as_mut() {
                    if *depth == 1 && name == b"axId" {
                        match element_u32(&element) {
                            Ok(id) if current.axis_ids.len() < MAX_XLSX_CHART_AXIS_ITEMS => {
                                current.axis_ids.push(id);
                            }
                            Err(()) => malformed = true,
                            Ok(_) => malformed = true,
                        }
                    }
                } else if let Some(kind) =
                    chart_kind_element(name).or_else(|| chart_3d_kind_element(name))
                {
                    if plots.len() < MAX_XLSX_CHART_AXIS_ITEMS {
                        plots.push(RawChartPlot {
                            kind,
                            axis_ids: Vec::new(),
                        });
                    } else {
                        malformed = true;
                    }
                }
            }
            Ok(Event::End(element)) => {
                if let Some((depth, current, _)) = axis.as_mut() {
                    let qualified_name = element.name();
                    let name = local(qualified_name.as_ref());
                    if *depth == 2 && name == b"scaling" {
                        current.scaling_open = false;
                    }
                    if *depth == 2 && name == b"majorGridlines" {
                        current.major_gridlines_open = false;
                    }
                    *depth = depth.saturating_sub(1);
                    if *depth == 0 {
                        let (_, completed, _) = axis.take().expect("axis capture is active");
                        if axes.len() < MAX_XLSX_CHART_AXIS_ITEMS {
                            axes.push(completed);
                        } else {
                            malformed = true;
                        }
                    }
                } else if let Some((depth, _)) = plot.as_mut() {
                    *depth = depth.saturating_sub(1);
                    if *depth == 0 {
                        let (_, completed) = plot.take().expect("plot capture is active");
                        if plots.len() < MAX_XLSX_CHART_AXIS_ITEMS {
                            plots.push(completed);
                        } else {
                            malformed = true;
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                malformed = true;
                break;
            }
            _ => {}
        }
    }
    if axis.is_some() || plot.is_some() {
        malformed = true;
    }

    let mut semantics = ChartAxisSemantics {
        invalid_visibility: axes.iter().any(|axis| !axis.visibility_valid),
        unsupported_topology: malformed || plots.len() > 1,
        unsupported_presentation: axes.iter().any(|axis| axis.unsupported_presentation),
        ..Default::default()
    };
    if axes.is_empty() {
        if plots.first().is_some_and(|plot| {
            !plot.axis_ids.is_empty()
                || matches!(
                    plot.kind,
                    ChartKind::Bar
                        | ChartKind::Line
                        | ChartKind::Scatter
                        | ChartKind::Area
                        | ChartKind::Radar
                        | ChartKind::Bubble
                )
        }) {
            semantics.unsupported_topology = true;
        }
        return semantics;
    }

    let mut id_to_index = HashMap::<u32, usize>::new();
    for (index, axis) in axes.iter().enumerate() {
        let Some(id) = axis.id else {
            semantics.unsupported_topology = true;
            continue;
        };
        if id_to_index.insert(id, index).is_some() {
            semantics.unsupported_topology = true;
        }
    }

    let plot_kind = plots.first().map(|plot| plot.kind);
    let axis_based_plot = matches!(
        plot_kind,
        Some(
            ChartKind::Bar
                | ChartKind::Line
                | ChartKind::Scatter
                | ChartKind::Area
                | ChartKind::Radar
                | ChartKind::Bubble
        )
    );
    if !axis_based_plot || plots.is_empty() {
        semantics.unsupported_topology = true;
    }
    if let Some(plot) = plots.first() {
        let mut unique_axis_ids = plot.axis_ids.clone();
        unique_axis_ids.sort_unstable();
        unique_axis_ids.dedup();
        if plot.axis_ids.len() != 2 || unique_axis_ids.len() != 2 {
            semantics.unsupported_topology = true;
        }
    }
    let mut roles = vec![None; axes.len()];
    if matches!(plot_kind, Some(ChartKind::Scatter | ChartKind::Bubble)) {
        let Some(plot) = plots.first() else {
            semantics.unsupported_topology = true;
            return semantics;
        };
        if plot.axis_ids.len() != 2 {
            semantics.unsupported_topology = true;
        } else {
            for (role, id) in [ChartAxisContext::Category, ChartAxisContext::Value]
                .into_iter()
                .zip(plot.axis_ids.iter())
            {
                match id_to_index.get(id).copied() {
                    Some(index)
                        if axes[index].kind == RawChartAxisKind::Value
                            && roles[index].replace(role).is_none() => {}
                    _ => semantics.unsupported_topology = true,
                }
            }
        }
    } else {
        for (index, axis) in axes.iter().enumerate() {
            roles[index] = Some(match axis.kind {
                RawChartAxisKind::Category => ChartAxisContext::Category,
                RawChartAxisKind::Value => ChartAxisContext::Value,
            });
        }
        if let Some(plot) = plots.first() {
            if plot.axis_ids.iter().any(|id| !id_to_index.contains_key(id)) {
                semantics.unsupported_topology = true;
            }
        }
    }

    let category_count = roles
        .iter()
        .filter(|role| **role == Some(ChartAxisContext::Category))
        .count();
    let value_count = roles
        .iter()
        .filter(|role| **role == Some(ChartAxisContext::Value))
        .count();
    if category_count != 1 || value_count != 1 || roles.iter().any(Option::is_none) {
        semantics.unsupported_topology = true;
    } else {
        let category_index = roles
            .iter()
            .position(|role| *role == Some(ChartAxisContext::Category))
            .expect("validated category axis role");
        let value_index = roles
            .iter()
            .position(|role| *role == Some(ChartAxisContext::Value))
            .expect("validated value axis role");
        let category_axis = &axes[category_index];
        let value_axis = &axes[value_index];
        if category_axis.cross_axis_id != value_axis.id
            || value_axis.cross_axis_id != category_axis.id
        {
            semantics.unsupported_topology = true;
        }
    }

    semantics.axis_roles = roles
        .into_iter()
        .zip(axes.iter())
        .map(|(role, axis)| {
            let role = role.unwrap_or(match axis.kind {
                RawChartAxisKind::Category => ChartAxisContext::Category,
                RawChartAxisKind::Value => ChartAxisContext::Value,
            });
            match role {
                ChartAxisContext::Category => {
                    semantics.category_visible = Some(axis.visible);
                    semantics.category_major_gridlines |= axis.major_gridlines;
                    semantics.category_position = axis.position;
                }
                ChartAxisContext::Value => {
                    semantics.value_visible = Some(axis.visible);
                    semantics.value_major_gridlines |= axis.major_gridlines;
                    semantics.value_position = axis.position;
                    if axis.cross_between_shifted.is_some() {
                        semantics.category_axis_shifted = axis.cross_between_shifted;
                    }
                }
            }
            role
        })
        .collect();
    if semantics.category_axis_shifted.is_none() {
        semantics.category_axis_shifted = match plot_kind {
            Some(ChartKind::Bar | ChartKind::Line) => Some(true),
            Some(ChartKind::Area | ChartKind::Radar) => Some(false),
            _ => None,
        };
    }
    semantics
}

#[cfg(any(test, feature = "xlsb"))]
#[cfg(test)]
pub(crate) fn parse_chart(
    xml: &str,
    from: (u32, u16),
    to: (u32, u16),
    chart_cache_points_remaining: &mut usize,
    chart_series_remaining: &mut usize,
) -> Option<ParsedChart> {
    parse_chart_with_theme(
        xml,
        from,
        to,
        chart_cache_points_remaining,
        chart_series_remaining,
        &ThemeColors::default(),
    )
}

pub(crate) fn parse_chart_with_theme(
    xml: &str,
    from: (u32, u16),
    to: (u32, u16),
    chart_cache_points_remaining: &mut usize,
    chart_series_remaining: &mut usize,
    theme: &ThemeColors,
) -> Option<ParsedChart> {
    if xml.len() > usize::try_from(MAX_XLSX_CHART_XML_BYTES).unwrap_or(usize::MAX) {
        return None;
    }
    let unsupported_markup = !chart_markup_is_supported(xml);
    let axis_semantics = parse_chart_axis_semantics(xml);
    let (chart_color_map, unsupported_chart_color_map) = parse_chart_text_color_map(xml);
    let mut r = Reader::from_str(xml);
    let mut kind: Option<ChartKind> = None;
    let mut title: Option<String> = None;
    let mut category_axis_title: Option<String> = None;
    let mut value_axis_title: Option<String> = None;
    let mut title_text = String::new();
    let mut title_text_valid = true;
    let mut title_target: Option<ChartTitleTarget> = None;
    let mut in_title_text = false;
    let mut legend = false;
    let (data_labels, unsupported_data_labels) = parse_chart_data_labels(xml);
    let mut series = Vec::new();
    let mut series_caches = Vec::new();
    let mut series_styles = Vec::new();
    let mut current_series: Option<ParsedChartSeries> = None;
    let mut source_series_position = 0usize;
    let mut series_field: Option<ChartSeriesField> = None;
    let mut capture_series_field: Option<ChartSeriesField> = None;
    let mut series_cache_depth = 0usize;
    let mut cache_field: Option<ChartSeriesField> = None;
    let mut cache_point_index: Option<u32> = None;
    let mut cache_value = String::new();
    let mut cache_value_valid = true;
    let mut capture_cache_value = false;
    let mut limit_exceeded = false;
    let mut unsupported_reasons = Vec::new();
    if unsupported_markup {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedMarkup,
        );
    }
    if !theme.source_valid() {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedChartStyle,
        );
    }
    if unsupported_data_labels {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedDataLabels,
        );
    }
    if unsupported_chart_color_map {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedChartStyle,
        );
    }
    if axis_semantics.invalid_visibility {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::InvalidAxisVisibility,
        );
    }
    if axis_semantics.unsupported_topology {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedAxisTopology,
        );
    }
    if axis_semantics.unsupported_presentation {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedAxisPresentation,
        );
    }
    let mut frame_fill = ChartFrameFill::Automatic;
    let mut frame_style_losses = Vec::new();
    let category_major_gridlines = axis_semantics.category_major_gridlines;
    let value_major_gridlines = axis_semantics.value_major_gridlines;
    let mut bar_direction = ChartBarDirection::Column;
    let mut bar_chart_depth = 0usize;
    let mut chart_depth = 0usize;
    let mut axis_context: Option<ChartAxisContext> = None;
    let mut axis_occurrence = 0usize;
    let mut in_legend = false;
    let mut marker_depth = 0usize;
    let mut marker_symbol_seen = false;
    let mut marker_size_seen = false;
    let mut data_point_depth = 0usize;
    let mut data_label_container_depth = 0usize;
    let mut trendline_depth = 0usize;
    let mut error_bars_depth = 0usize;
    let mut series_shape_depth = 0usize;
    let mut series_shape_seen = false;
    let mut series_line_depth = 0usize;
    let mut series_line_seen = false;
    let mut series_line_paint_seen = false;
    let mut series_line_color_seen = false;
    let mut series_line_solid_fill_depth = 0usize;
    let mut frame_shape_depth = 0usize;
    let mut frame_shape_seen = false;
    let mut frame_fill_choice_seen = false;
    let mut frame_solid_fill_color_seen = false;
    let mut frame_line_depth = 0usize;
    let mut frame_solid_fill_depth = 0usize;
    let mut frame_solid_fill_resolved = false;

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"chart" => chart_depth = chart_depth.saturating_add(1),
                name if chart_kind_element(name).is_some() => {
                    let observed = chart_kind_element(name).expect("guarded chart kind");
                    observe_chart_kind(&mut kind, observed, &mut unsupported_reasons);
                    if observed == ChartKind::Bar {
                        bar_chart_depth = bar_chart_depth.saturating_add(1);
                    }
                }
                name if chart_3d_kind_element(name).is_some() => {
                    observe_chart_kind(
                        &mut kind,
                        chart_3d_kind_element(name).expect("guarded 3-D chart kind"),
                        &mut unsupported_reasons,
                    );
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::ThreeDimensional,
                    );
                }
                b"stockChart" | b"surfaceChart" | b"surface3DChart" | b"ofPieChart" => {
                    let fallback = match local(e.name().as_ref()) {
                        b"stockChart" => ChartKind::Line,
                        b"ofPieChart" => ChartKind::Pie,
                        _ => ChartKind::Area,
                    };
                    observe_chart_kind(&mut kind, fallback, &mut unsupported_reasons);
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedKind,
                    );
                    if local(e.name().as_ref()) == b"surface3DChart" {
                        add_chart_unsupported(
                            &mut unsupported_reasons,
                            ChartUnsupportedReason::ThreeDimensional,
                        );
                    }
                }
                b"view3D" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ThreeDimensional,
                ),
                b"pivotSource" => {
                    add_chart_unsupported(&mut unsupported_reasons, ChartUnsupportedReason::Pivot)
                }
                b"externalData" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ExternalData,
                ),
                b"barDir" if bar_chart_depth > 0 => match unique_attr(&e, b"val") {
                    Ok(Some(value)) if value == "bar" => {
                        bar_direction = ChartBarDirection::Horizontal;
                    }
                    Ok(Some(value)) if value == "col" => {
                        bar_direction = ChartBarDirection::Column;
                    }
                    _ => add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    ),
                },
                name if chart_plot_option_supported(kind, name, &e) == Some(false) => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                    if name == b"bubble3D" {
                        add_chart_unsupported(
                            &mut unsupported_reasons,
                            ChartUnsupportedReason::ThreeDimensional,
                        );
                    }
                }
                b"style"
                    if chart_depth == 0
                        && current_series.is_none()
                        && !matches!(
                            unique_attr(&e, b"val"),
                            Ok(Some(value)) if value == "2"
                        ) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedChartStyle,
                    );
                }
                b"catAx" | b"dateAx" | b"valAx" => {
                    axis_context = axis_semantics
                        .axis_roles
                        .get(axis_occurrence)
                        .copied()
                        .or_else(|| {
                            (local(e.name().as_ref()) != b"valAx")
                                .then_some(ChartAxisContext::Category)
                                .or(Some(ChartAxisContext::Value))
                        });
                    axis_occurrence = axis_occurrence.saturating_add(1);
                }
                b"title" if current_series.is_none() => {
                    let target = match axis_context {
                        Some(ChartAxisContext::Category) if category_axis_title.is_none() => {
                            Some(ChartTitleTarget::CategoryAxis)
                        }
                        Some(ChartAxisContext::Value) if value_axis_title.is_none() => {
                            Some(ChartTitleTarget::ValueAxis)
                        }
                        None if title.is_none() => Some(ChartTitleTarget::Main),
                        _ => None,
                    };
                    if let Some(target) = target {
                        title_target = Some(target);
                        title_text.clear();
                        title_text_valid = true;
                    }
                }
                b"legend" => {
                    legend = true;
                    in_legend = true;
                }
                b"legendPos"
                    if !matches!(
                        unique_attr(&e, b"val"),
                        Ok(Some(value)) if value == "r"
                    ) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedLegend,
                    );
                }
                b"legendEntry" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedLegend,
                ),
                b"overlay" if parse_chart_boolean_element(&e) != Ok(false) => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedLegend,
                    );
                }
                b"manualLayout" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedPlotSemantics,
                ),
                b"ser" if current_series.is_some() => return None,
                b"ser" => {
                    current_series = Some(ParsedChartSeries {
                        source_position: source_series_position,
                        ..ParsedChartSeries::default()
                    });
                    source_series_position = source_series_position.saturating_add(1);
                    series_field = None;
                    capture_series_field = None;
                    series_cache_depth = 0;
                    marker_depth = 0;
                    marker_symbol_seen = false;
                    marker_size_seen = false;
                    data_point_depth = 0;
                    data_label_container_depth = 0;
                    trendline_depth = 0;
                    error_bars_depth = 0;
                    series_shape_depth = 0;
                    series_shape_seen = false;
                    series_line_depth = 0;
                    series_line_seen = false;
                    series_line_paint_seen = false;
                    series_line_color_seen = false;
                    series_line_solid_fill_depth = 0;
                }
                b"marker" if current_series.is_some() => marker_depth = 1,
                b"dPt" if current_series.is_some() => {
                    data_point_depth = 1;
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"dLbls" | b"dLbl" if current_series.is_some() => {
                    data_label_container_depth = data_label_container_depth.saturating_add(1);
                }
                b"trendline" if current_series.is_some() => {
                    trendline_depth = 1;
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"errBars" if current_series.is_some() => {
                    error_bars_depth = 1;
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"invertIfNegative" | b"pictureOptions" if current_series.is_some() => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"spPr" if in_legend => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedLegend,
                ),
                b"spPr"
                    if chart_depth == 0 && current_series.is_none() && frame_shape_depth == 0 =>
                {
                    if frame_shape_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_shape_seen = true;
                    frame_shape_depth = 1;
                }
                b"spPr"
                    if current_series.is_some()
                        && (marker_depth > 0
                            || data_point_depth > 0
                            || trendline_depth > 0
                            || error_bars_depth > 0) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"spPr"
                    if current_series.is_some()
                        && marker_depth == 0
                        && data_point_depth == 0
                        && trendline_depth == 0
                        && error_bars_depth == 0
                        && series_shape_depth == 0 =>
                {
                    if series_shape_seen {
                        if let Some(series) = current_series.as_mut() {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                    series_shape_seen = true;
                    series_shape_depth = 1;
                }
                b"spPr" if current_series.is_none() && chart_depth > 0 => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"ln" if frame_shape_depth > 0 => {
                    frame_line_depth = 1;
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"solidFill" if frame_shape_depth > 0 && frame_line_depth == 0 => {
                    if frame_fill_choice_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_fill_choice_seen = true;
                    frame_solid_fill_depth = 1;
                    frame_solid_fill_resolved = false;
                    frame_solid_fill_color_seen = false;
                }
                b"noFill" if frame_shape_depth > 0 && frame_line_depth == 0 => {
                    if frame_fill_choice_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_fill_choice_seen = true;
                    frame_fill = ChartFrameFill::NoFill;
                }
                b"srgbClr" | b"schemeClr" if frame_solid_fill_depth > 0 => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    if frame_solid_fill_color_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_solid_fill_color_seen = true;
                    if let Some(color) = chart_series_line_color(name, &e, theme, &chart_color_map)
                    {
                        frame_fill = ChartFrameFill::Solid(color);
                        frame_solid_fill_resolved = true;
                    } else {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                }
                b"sysClr" if frame_solid_fill_depth > 0 => {
                    if frame_solid_fill_color_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_solid_fill_color_seen = true;
                    if let Some(color) =
                        chart_series_line_color(b"sysClr", &e, theme, &chart_color_map)
                    {
                        frame_fill = ChartFrameFill::Solid(color);
                        frame_solid_fill_resolved = true;
                    } else {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                }
                b"gradFill" | b"pattFill" | b"blipFill" | b"grpFill"
                    if frame_shape_depth > 0 && frame_line_depth == 0 =>
                {
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"alpha" | b"alphaMod" | b"alphaOff" | b"blue" | b"blueMod" | b"blueOff"
                | b"comp" | b"gamma" | b"gray" | b"green" | b"greenMod" | b"greenOff" | b"hue"
                | b"hueMod" | b"hueOff" | b"inv" | b"invGamma" | b"lum" | b"lumMod" | b"lumOff"
                | b"red" | b"redMod" | b"redOff" | b"sat" | b"satMod" | b"satOff" | b"shade"
                | b"tint"
                    if frame_solid_fill_depth > 0 =>
                {
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"ln" if series_shape_depth > 0 => {
                    series_line_depth = 1;
                    if let Some(series) = current_series.as_mut() {
                        if series_line_seen || !chart_text_attributes_are_subset(&e, &[b"w"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_seen = true;
                        series_line_paint_seen = false;
                        series_line_color_seen = false;
                        retain_chart_series_line_width(
                            &mut series.style,
                            attr(&e, b"w").as_deref(),
                        );
                    }
                }
                b"solidFill" if series_line_depth > 0 => {
                    series_line_solid_fill_depth = 1;
                    if let Some(series) = current_series.as_mut() {
                        if series_line_paint_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_paint_seen = true;
                        series_line_color_seen = false;
                        series.style.line_visible = true;
                    }
                }
                b"noFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if series_line_paint_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_paint_seen = true;
                        series.style.line_visible = false;
                        series.style.line_color = None;
                    }
                }
                b"srgbClr" | b"schemeClr" if series_line_solid_fill_depth > 0 => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    if let Some(series) = current_series.as_mut() {
                        if series_line_color_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_color_seen = true;
                        if let Some(color) =
                            chart_series_line_color(name, &e, theme, &chart_color_map)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"sysClr" if series_line_solid_fill_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if series_line_color_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_color_seen = true;
                        if let Some(color) =
                            chart_series_line_color(b"sysClr", &e, theme, &chart_color_map)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"gradFill" | b"pattFill" | b"blipFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"prstDash"
                    if series_line_depth > 0
                        && (!chart_text_attributes_are_subset(&e, &[b"val"])
                            || !matches!(
                                unique_attr(&e, b"val"),
                                Ok(Some(value)) if value == "solid"
                            )) =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"alpha" | b"alphaMod" | b"alphaOff" | b"blue" | b"blueMod" | b"blueOff"
                | b"comp" | b"gamma" | b"gray" | b"green" | b"greenMod" | b"greenOff" | b"hue"
                | b"hueMod" | b"hueOff" | b"inv" | b"invGamma" | b"lum" | b"lumMod" | b"lumOff"
                | b"red" | b"redMod" | b"redOff" | b"sat" | b"satMod" | b"satOff" | b"shade"
                | b"tint"
                    if series_line_solid_fill_depth > 0 =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"custDash" | b"round" | b"bevel" | b"miter" | b"headEnd" | b"tailEnd"
                    if series_line_depth > 0 =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"solidFill" | b"noFill" | b"gradFill" | b"pattFill" | b"blipFill" | b"grpFill"
                    if series_shape_depth > 0 && series_line_depth == 0 =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"symbol" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if marker_symbol_seen || !chart_text_attributes_are_subset(&e, &[b"val"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedMarkerSymbol,
                            );
                        }
                        marker_symbol_seen = true;
                        let value = unique_attr(&e, b"val").ok().flatten();
                        retain_chart_marker_symbol(&mut series.style, value.as_deref());
                    }
                }
                b"size" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if marker_size_seen || !chart_text_attributes_are_subset(&e, &[b"val"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::InvalidMarkerSize,
                            );
                        }
                        marker_size_seen = true;
                        let value = unique_attr(&e, b"val").ok().flatten();
                        retain_chart_marker_size(&mut series.style, value.as_deref());
                    }
                }
                b"idx" | b"order"
                    if current_series.is_some()
                        && data_point_depth == 0
                        && data_label_container_depth == 0
                        && marker_depth == 0
                        && trendline_depth == 0
                        && error_bars_depth == 0
                        && series_cache_depth == 0
                        && current_series.as_mut().is_some_and(|series| {
                            !retain_chart_series_position(series, local(e.name().as_ref()), &e)
                        }) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"tx" if current_series.is_some() => series_field = Some(ChartSeriesField::Name),
                b"cat" | b"xVal" if current_series.is_some() => {
                    series_field = Some(ChartSeriesField::Categories);
                }
                b"val" | b"yVal" if current_series.is_some() => {
                    series_field = Some(ChartSeriesField::Values);
                }
                b"bubbleSize" if current_series.is_some() => {
                    series_field = Some(ChartSeriesField::BubbleSizes);
                }
                b"strCache" | b"numCache" | b"strLit" | b"numLit" if current_series.is_some() => {
                    if series_cache_depth == 0 {
                        cache_field = series_field;
                    }
                    series_cache_depth += 1;
                }
                b"multiLvlStrCache" if current_series.is_some() => {
                    // Multi-level categories cannot be represented faithfully by
                    // the flat public Series API. Keep the A1 reference and
                    // deliberately leave this cache unusable.
                    if series_cache_depth == 0 {
                        cache_field = None;
                    }
                    series_cache_depth += 1;
                }
                b"pt" if current_series.is_some() && series_cache_depth > 0 => {
                    cache_point_index = attr(&e, b"idx").and_then(|value| value.parse().ok());
                    cache_value.clear();
                    cache_value_valid = true;
                }
                b"f" if current_series.is_some() => {
                    capture_series_field = series_field;
                }
                b"v" if current_series.is_some()
                    && series_cache_depth > 0
                    && cache_point_index.is_some() =>
                {
                    capture_cache_value = true;
                }
                b"v" if current_series.is_some() && series_cache_depth == 0 => {
                    capture_series_field = series_field;
                }
                b"t" | b"v" if title_target.is_some() => in_title_text = true,
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                name if chart_kind_element(name).is_some() => observe_chart_kind(
                    &mut kind,
                    chart_kind_element(name).expect("guarded chart kind"),
                    &mut unsupported_reasons,
                ),
                name if chart_3d_kind_element(name).is_some() => {
                    observe_chart_kind(
                        &mut kind,
                        chart_3d_kind_element(name).expect("guarded 3-D chart kind"),
                        &mut unsupported_reasons,
                    );
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::ThreeDimensional,
                    );
                }
                b"stockChart" | b"surfaceChart" | b"surface3DChart" | b"ofPieChart" => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    let fallback = match name {
                        b"stockChart" => ChartKind::Line,
                        b"ofPieChart" => ChartKind::Pie,
                        _ => ChartKind::Area,
                    };
                    observe_chart_kind(&mut kind, fallback, &mut unsupported_reasons);
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedKind,
                    );
                    if name == b"surface3DChart" {
                        add_chart_unsupported(
                            &mut unsupported_reasons,
                            ChartUnsupportedReason::ThreeDimensional,
                        );
                    }
                }
                b"view3D" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ThreeDimensional,
                ),
                b"pivotSource" => {
                    add_chart_unsupported(&mut unsupported_reasons, ChartUnsupportedReason::Pivot)
                }
                b"externalData" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ExternalData,
                ),
                b"barDir" if bar_chart_depth > 0 => match unique_attr(&e, b"val") {
                    Ok(Some(value)) if value == "bar" => {
                        bar_direction = ChartBarDirection::Horizontal;
                    }
                    Ok(Some(value)) if value == "col" => {
                        bar_direction = ChartBarDirection::Column;
                    }
                    _ => add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    ),
                },
                name if chart_plot_option_supported(kind, name, &e) == Some(false) => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                    if name == b"bubble3D" {
                        add_chart_unsupported(
                            &mut unsupported_reasons,
                            ChartUnsupportedReason::ThreeDimensional,
                        );
                    }
                }
                b"style"
                    if chart_depth == 0
                        && current_series.is_none()
                        && !matches!(
                            unique_attr(&e, b"val"),
                            Ok(Some(value)) if value == "2"
                        ) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedChartStyle,
                    );
                }
                b"legend" => legend = true,
                b"legendPos"
                    if !matches!(
                        unique_attr(&e, b"val"),
                        Ok(Some(value)) if value == "r"
                    ) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedLegend,
                    );
                }
                b"legendEntry" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedLegend,
                ),
                b"overlay" if parse_chart_boolean_element(&e) != Ok(false) => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedLegend,
                    );
                }
                b"manualLayout" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedPlotSemantics,
                ),
                b"ser" => {
                    source_series_position = source_series_position.saturating_add(1);
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"dPt" | b"trendline" | b"errBars" | b"invertIfNegative" | b"pictureOptions"
                    if current_series.is_some() =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"spPr"
                    if current_series.is_some()
                        && (marker_depth > 0
                            || data_point_depth > 0
                            || trendline_depth > 0
                            || error_bars_depth > 0) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"spPr" if in_legend => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedLegend,
                ),
                b"idx" | b"order"
                    if current_series.is_some()
                        && data_point_depth == 0
                        && data_label_container_depth == 0
                        && marker_depth == 0
                        && trendline_depth == 0
                        && error_bars_depth == 0
                        && series_cache_depth == 0
                        && current_series.as_mut().is_some_and(|series| {
                            !retain_chart_series_position(series, local(e.name().as_ref()), &e)
                        }) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"symbol" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if marker_symbol_seen || !chart_text_attributes_are_subset(&e, &[b"val"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedMarkerSymbol,
                            );
                        }
                        marker_symbol_seen = true;
                        let value = unique_attr(&e, b"val").ok().flatten();
                        retain_chart_marker_symbol(&mut series.style, value.as_deref());
                    }
                }
                b"size" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if marker_size_seen || !chart_text_attributes_are_subset(&e, &[b"val"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::InvalidMarkerSize,
                            );
                        }
                        marker_size_seen = true;
                        let value = unique_attr(&e, b"val").ok().flatten();
                        retain_chart_marker_size(&mut series.style, value.as_deref());
                    }
                }
                b"ln" if frame_shape_depth > 0 => {
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"noFill" if frame_shape_depth > 0 && frame_line_depth == 0 => {
                    if frame_fill_choice_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_fill_choice_seen = true;
                    frame_fill = ChartFrameFill::NoFill;
                }
                b"solidFill" if frame_shape_depth > 0 && frame_line_depth == 0 => {
                    frame_fill_choice_seen = true;
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"srgbClr" | b"schemeClr" if frame_solid_fill_depth > 0 => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    if frame_solid_fill_color_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_solid_fill_color_seen = true;
                    if let Some(color) = chart_series_line_color(name, &e, theme, &chart_color_map)
                    {
                        frame_fill = ChartFrameFill::Solid(color);
                        frame_solid_fill_resolved = true;
                    } else {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                }
                b"sysClr" if frame_solid_fill_depth > 0 => {
                    if frame_solid_fill_color_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_solid_fill_color_seen = true;
                    if let Some(color) =
                        chart_series_line_color(b"sysClr", &e, theme, &chart_color_map)
                    {
                        frame_fill = ChartFrameFill::Solid(color);
                        frame_solid_fill_resolved = true;
                    } else {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                }
                b"gradFill" | b"pattFill" | b"blipFill" | b"grpFill"
                    if frame_shape_depth > 0 && frame_line_depth == 0 =>
                {
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"alpha" | b"alphaMod" | b"alphaOff" | b"blue" | b"blueMod" | b"blueOff"
                | b"comp" | b"gamma" | b"gray" | b"green" | b"greenMod" | b"greenOff" | b"hue"
                | b"hueMod" | b"hueOff" | b"inv" | b"invGamma" | b"lum" | b"lumMod" | b"lumOff"
                | b"red" | b"redMod" | b"redOff" | b"sat" | b"satMod" | b"satOff" | b"shade"
                | b"tint"
                    if frame_solid_fill_depth > 0 =>
                {
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"ln" if series_shape_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if series_line_seen || !chart_text_attributes_are_subset(&e, &[b"w"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_seen = true;
                        retain_chart_series_line_width(
                            &mut series.style,
                            attr(&e, b"w").as_deref(),
                        );
                    }
                }
                b"noFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if series_line_paint_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_paint_seen = true;
                        series.style.line_visible = false;
                        series.style.line_color = None;
                    }
                }
                b"srgbClr" | b"schemeClr" if series_line_solid_fill_depth > 0 => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    if let Some(series) = current_series.as_mut() {
                        if series_line_color_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_color_seen = true;
                        if let Some(color) =
                            chart_series_line_color(name, &e, theme, &chart_color_map)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"sysClr" if series_line_solid_fill_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if series_line_color_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_color_seen = true;
                        if let Some(color) =
                            chart_series_line_color(b"sysClr", &e, theme, &chart_color_map)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"gradFill" | b"pattFill" | b"blipFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"prstDash"
                    if series_line_depth > 0
                        && (!chart_text_attributes_are_subset(&e, &[b"val"])
                            || !matches!(
                                unique_attr(&e, b"val"),
                                Ok(Some(value)) if value == "solid"
                            )) =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"alpha" | b"alphaMod" | b"alphaOff" | b"blue" | b"blueMod" | b"blueOff"
                | b"comp" | b"gamma" | b"gray" | b"green" | b"greenMod" | b"greenOff" | b"hue"
                | b"hueMod" | b"hueOff" | b"inv" | b"invGamma" | b"lum" | b"lumMod" | b"lumOff"
                | b"red" | b"redMod" | b"redOff" | b"sat" | b"satMod" | b"satOff" | b"shade"
                | b"tint"
                    if series_line_solid_fill_depth > 0 =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"solidFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"custDash" | b"round" | b"bevel" | b"miter" | b"headEnd" | b"tailEnd"
                    if series_line_depth > 0 =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"solidFill" | b"noFill" | b"gradFill" | b"pattFill" | b"blipFill" | b"grpFill"
                    if series_shape_depth > 0 && series_line_depth == 0 =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                _ => {}
            },
            Ok(Event::Text(t)) => {
                append_chart_text(
                    &mut current_series,
                    capture_series_field,
                    capture_cache_value,
                    &mut cache_value,
                    title_target,
                    in_title_text,
                    &mut title_text,
                    &mut title_text_valid,
                    &text_of(&t),
                    &mut limit_exceeded,
                    &mut cache_value_valid,
                );
            }
            Ok(Event::GeneralRef(reference)) => {
                with_general_ref_text(&reference, |text| {
                    append_chart_text(
                        &mut current_series,
                        capture_series_field,
                        capture_cache_value,
                        &mut cache_value,
                        title_target,
                        in_title_text,
                        &mut title_text,
                        &mut title_text_valid,
                        text,
                        &mut limit_exceeded,
                        &mut cache_value_valid,
                    );
                });
            }
            Ok(Event::CData(t)) => {
                let text = String::from_utf8_lossy(t.into_inner().as_ref()).into_owned();
                append_chart_text(
                    &mut current_series,
                    capture_series_field,
                    capture_cache_value,
                    &mut cache_value,
                    title_target,
                    in_title_text,
                    &mut title_text,
                    &mut title_text_valid,
                    &text,
                    &mut limit_exceeded,
                    &mut cache_value_valid,
                );
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"chart" if chart_depth > 0 => chart_depth -= 1,
                b"legend" => in_legend = false,
                b"barChart" if bar_chart_depth > 0 => {
                    bar_chart_depth -= 1;
                }
                b"marker" if marker_depth > 0 => marker_depth = 0,
                b"dPt" if data_point_depth > 0 => data_point_depth = 0,
                b"dLbls" | b"dLbl" if data_label_container_depth > 0 => {
                    data_label_container_depth -= 1;
                }
                b"trendline" if trendline_depth > 0 => trendline_depth = 0,
                b"errBars" if error_bars_depth > 0 => error_bars_depth = 0,
                b"solidFill" if frame_solid_fill_depth > 0 => {
                    if !frame_solid_fill_resolved {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_solid_fill_depth = 0;
                    frame_solid_fill_resolved = false;
                    frame_solid_fill_color_seen = false;
                }
                b"ln" if frame_line_depth > 0 => frame_line_depth = 0,
                b"spPr" if frame_shape_depth > 0 => {
                    frame_shape_depth = 0;
                    frame_line_depth = 0;
                    frame_solid_fill_depth = 0;
                    frame_solid_fill_resolved = false;
                    frame_solid_fill_color_seen = false;
                }
                b"solidFill" if series_line_solid_fill_depth > 0 => {
                    if !series_line_color_seen {
                        if let Some(series) = current_series.as_mut() {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                    series_line_solid_fill_depth = 0;
                    series_line_color_seen = false;
                }
                b"ln" if series_line_depth > 0 => {
                    series_line_depth = 0;
                    series_line_solid_fill_depth = 0;
                    series_line_paint_seen = false;
                    series_line_color_seen = false;
                }
                b"spPr" if series_shape_depth > 0 => {
                    series_shape_depth = 0;
                    series_line_depth = 0;
                    series_line_solid_fill_depth = 0;
                    series_line_paint_seen = false;
                    series_line_color_seen = false;
                }
                b"v" if capture_cache_value => capture_cache_value = false,
                b"t" | b"v" if in_title_text => in_title_text = false,
                b"title" if title_target.is_some() => {
                    let text = title_text.trim();
                    if title_text_valid && !text.is_empty() {
                        match title_target.expect("title target checked above") {
                            ChartTitleTarget::Main => title = Some(text.to_string()),
                            ChartTitleTarget::CategoryAxis => {
                                category_axis_title = Some(text.to_string());
                            }
                            ChartTitleTarget::ValueAxis => {
                                value_axis_title = Some(text.to_string());
                            }
                        }
                    }
                    title_target = None;
                    in_title_text = false;
                    title_text.clear();
                    title_text_valid = true;
                }
                b"catAx" | b"dateAx" | b"valAx" => axis_context = None,
                b"f" | b"v" if capture_series_field.is_some() => capture_series_field = None,
                b"pt" if series_cache_depth > 0 => {
                    if cache_value_valid {
                        if let (Some(field), Some(index), Some(parsed)) =
                            (cache_field, cache_point_index, current_series.as_mut())
                        {
                            if *chart_cache_points_remaining == 0 {
                                limit_exceeded = true;
                            } else {
                                chart_cache_points_mut(&mut parsed.cache, field).push(
                                    ChartCachedPoint {
                                        index,
                                        value: std::mem::take(&mut cache_value),
                                    },
                                );
                                *chart_cache_points_remaining -= 1;
                            }
                        }
                    }
                    cache_point_index = None;
                    cache_value.clear();
                    cache_value_valid = true;
                    capture_cache_value = false;
                }
                b"strCache" | b"numCache" | b"strLit" | b"numLit" | b"multiLvlStrCache"
                    if series_cache_depth > 0 =>
                {
                    series_cache_depth -= 1;
                    if series_cache_depth == 0 {
                        cache_field = None;
                        cache_point_index = None;
                        cache_value.clear();
                        cache_value_valid = true;
                        capture_cache_value = false;
                    }
                }
                b"tx" | b"cat" | b"xVal" | b"val" | b"yVal" | b"bubbleSize"
                    if current_series.is_some() =>
                {
                    series_field = None;
                }
                b"ser" => {
                    if let Some(parsed) = current_series.take() {
                        if !parsed.source_index_seen || !parsed.source_order_seen {
                            add_chart_unsupported(
                                &mut unsupported_reasons,
                                ChartUnsupportedReason::UnsupportedPlotSemantics,
                            );
                        }
                        if [
                            parsed.name.as_deref(),
                            parsed.categories.as_deref(),
                            parsed.values.as_deref(),
                            parsed.bubble_sizes.as_deref(),
                        ]
                        .into_iter()
                        .flatten()
                        .any(is_external_chart_reference)
                        {
                            add_chart_unsupported(
                                &mut unsupported_reasons,
                                ChartUnsupportedReason::ExternalData,
                            );
                        }
                        if let Some(values) = parsed.values {
                            if *chart_series_remaining > 0 {
                                series.push(Series {
                                    name: parsed.name,
                                    categories: parsed.categories,
                                    values,
                                    bubble_sizes: parsed.bubble_sizes,
                                });
                                series_caches.push(parsed.cache);
                                series_styles.push(parsed.style);
                                *chart_series_remaining -= 1;
                            } else {
                                limit_exceeded = true;
                            }
                        }
                    }
                    series_field = None;
                    capture_series_field = None;
                    series_cache_depth = 0;
                    cache_field = None;
                    cache_point_index = None;
                    cache_value.clear();
                    cache_value_valid = true;
                    capture_cache_value = false;
                    marker_depth = 0;
                    marker_symbol_seen = false;
                    marker_size_seen = false;
                    data_point_depth = 0;
                    data_label_container_depth = 0;
                    trendline_depth = 0;
                    error_bars_depth = 0;
                    series_shape_depth = 0;
                    series_shape_seen = false;
                    series_line_depth = 0;
                    series_line_seen = false;
                    series_line_paint_seen = false;
                    series_line_color_seen = false;
                    series_line_solid_fill_depth = 0;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }

    let kind = kind?;
    let (expected_category_position, expected_value_position) =
        if kind == ChartKind::Bar && bar_direction == ChartBarDirection::Horizontal {
            (RawChartAxisPosition::Left, RawChartAxisPosition::Bottom)
        } else {
            (RawChartAxisPosition::Bottom, RawChartAxisPosition::Left)
        };
    if axis_semantics
        .category_position
        .is_some_and(|position| position != expected_category_position)
        || axis_semantics
            .value_position
            .is_some_and(|position| position != expected_value_position)
    {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedAxisPresentation,
        );
    }
    let text_styles = parse_chart_text_styles_unified(
        xml,
        kind,
        &axis_semantics.axis_roles,
        theme,
        &mut unsupported_reasons,
        &mut limit_exceeded,
    );
    let (x_axis_title, y_axis_title) =
        if kind == ChartKind::Bar && bar_direction == ChartBarDirection::Horizontal {
            (value_axis_title, category_axis_title)
        } else {
            (category_axis_title, value_axis_title)
        };

    Some(ParsedChart {
        chart: Chart {
            kind,
            title,
            series,
            legend,
            data_labels,
            x_axis_title,
            y_axis_title,
            from,
            to,
        },
        series_caches,
        series_styles,
        text_styles,
        frame_fill,
        frame_style_losses,
        category_major_gridlines,
        value_major_gridlines,
        category_axis_visible: axis_semantics.category_visible,
        category_axis_shifted: axis_semantics.category_axis_shifted,
        value_axis_visible: axis_semantics.value_visible,
        limit_exceeded,
        unsupported_reasons,
        bar_direction,
    })
}

#[derive(Debug)]
struct ParsedTable {
    table: Table,
    application: TableStyleApplication,
    losses: Vec<StyleLoss>,
}

fn table_bool_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    default: bool,
    losses: &mut Vec<StyleLoss>,
) -> bool {
    match attr(e, key) {
        Some(value) => parse_bool_attr(&value).unwrap_or_else(|| {
            add_differential_loss(losses, StyleLossKind::UnsupportedProperty, 1);
            default
        }),
        None => default,
    }
}

fn table_single_row_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    default: bool,
    losses: &mut Vec<StyleLoss>,
) -> bool {
    match attr(e, key) {
        None => default,
        Some(value) => match value.parse::<u32>() {
            Ok(0) => false,
            Ok(1) => true,
            Ok(_) => {
                add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                true
            }
            Err(_) => {
                add_differential_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                default
            }
        },
    }
}

fn parse_table(xml: &str) -> Option<ParsedTable> {
    const MAX_TABLE_COLUMNS: usize = 16_384;
    let mut r = Reader::from_str(xml);
    let mut range: Option<(u32, u16, u32, u16)> = None;
    // Prefer `displayName`, falling back to `name`.
    let mut display_name: Option<String> = None;
    let mut name: Option<String> = None;
    let mut style: Option<String> = None;
    let mut columns: Vec<String> = Vec::new();
    let mut application = TableStyleApplication::default();
    let mut losses = Vec::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"table" => {
                    range = attr(&e, b"ref").as_deref().and_then(parse_range);
                    display_name = attr(&e, b"displayName");
                    name = attr(&e, b"name");
                    application.header_row =
                        table_single_row_attr(&e, b"headerRowCount", true, &mut losses);
                    let totals_count =
                        table_single_row_attr(&e, b"totalsRowCount", false, &mut losses);
                    let totals_shown = table_bool_attr(&e, b"totalsRowShown", false, &mut losses);
                    application.totals_row = totals_count || totals_shown;
                }
                b"tableColumn" => {
                    if let Some(n) = attr(&e, b"name") {
                        if columns.len() < MAX_TABLE_COLUMNS {
                            columns.push(n);
                        } else {
                            add_differential_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                        }
                    }
                }
                b"tableStyleInfo" => {
                    style = attr(&e, b"name");
                    application.show_first_column =
                        table_bool_attr(&e, b"showFirstColumn", false, &mut losses);
                    application.show_last_column =
                        table_bool_attr(&e, b"showLastColumn", false, &mut losses);
                    application.show_row_stripes =
                        table_bool_attr(&e, b"showRowStripes", false, &mut losses);
                    application.show_column_stripes =
                        table_bool_attr(&e, b"showColumnStripes", false, &mut losses);
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    Some(ParsedTable {
        table: Table {
            range: range?,
            name: display_name.or(name).unwrap_or_default(),
            columns,
            style,
        },
        application,
        losses,
    })
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

/// `xl/comments{N}.xml`: an `<authors><author>` table followed by a
/// `<commentList>` of `<comment ref authorId>` notes, whose body is the
/// concatenated `<text>…<t>text</t>…</text>` runs. Resolves each comment's
/// `authorId` against the authors table.
fn parse_comments(xml: &str) -> Vec<Comment> {
    let mut r = Reader::from_str(xml);
    let mut authors: Vec<String> = Vec::new();
    let mut out: Vec<Comment> = Vec::new();
    let mut in_authors = false;
    let mut in_author = false;
    let mut cur_author = String::new();
    // Current `<comment>` capture.
    let mut cur_rc: Option<(u32, u16)> = None;
    let mut cur_author_id: usize = 0;
    let mut cur_text = String::new();
    let mut in_t = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"authors" => in_authors = true,
                b"author" => {
                    in_author = true;
                    cur_author.clear();
                }
                b"comment" => {
                    cur_rc = attr(&e, b"ref").as_deref().and_then(parse_ref);
                    cur_author_id = attr(&e, b"authorId")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    cur_text.clear();
                }
                // `<t>` runs only count inside a `<comment>`'s `<text>`; the
                // authors table has no `<t>`, so a plain `in_t` flag suffices.
                b"t" if cur_rc.is_some() => in_t = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_author => cur_author.push_str(&text_of(&t)),
            Ok(Event::Text(t)) if in_t => cur_text.push_str(&text_of(&t)),
            Ok(Event::GeneralRef(reference)) if in_author => {
                append_general_ref(&mut cur_author, &reference);
            }
            Ok(Event::GeneralRef(reference)) if in_t => {
                append_general_ref(&mut cur_text, &reference);
            }
            Ok(Event::CData(t)) if in_t => {
                cur_text.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"authors" => in_authors = false,
                b"author" => {
                    if in_authors {
                        authors.push(std::mem::take(&mut cur_author));
                    }
                    in_author = false;
                }
                b"t" => in_t = false,
                b"comment" => {
                    if let Some((row, col)) = cur_rc.take() {
                        let author = authors
                            .get(cur_author_id)
                            .filter(|a| !a.is_empty())
                            .cloned();
                        out.push(Comment {
                            row,
                            col,
                            text: std::mem::take(&mut cur_text),
                            author,
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// A1-style reference → 0-based `(row, col)`.
fn parse_ref(r: &str) -> Option<(u32, u16)> {
    let mut col: u32 = 0;
    let mut row: u32 = 0;
    let mut seen_col = false;
    let mut seen_row = false;
    for c in r.chars() {
        if c.is_ascii_alphabetic() {
            if seen_row {
                return None;
            }
            // Checked arithmetic: a hostile ref like `ZZZZZZZ1` must not overflow
            // (the crate's panic-free contract).
            col = col
                .checked_mul(26)?
                .checked_add(c.to_ascii_uppercase() as u32 - 'A' as u32 + 1)?;
            seen_col = true;
        } else if c.is_ascii_digit() {
            row = row.checked_mul(10)?.checked_add(c as u32 - '0' as u32)?;
            seen_row = true;
        }
    }
    // Reject anything past Excel's grid (XFD = col 16384 1-based, 1048576 rows).
    if !seen_col || !seen_row || col == 0 || row == 0 || col > 16384 || row > 1_048_576 {
        return None;
    }
    Some((row - 1, u16::try_from(col - 1).ok()?))
}

/// `A1:C3` (or a lone `A1`) → `(first_row, first_col, last_row, last_col)`.
fn parse_range(s: &str) -> Option<(u32, u16, u32, u16)> {
    let mut it = s.split(':');
    let first = parse_ref(it.next()?)?;
    let last = match it.next() {
        Some(r) => parse_ref(r)?,
        None => first,
    };
    Some((first.0, first.1, last.0, last.1))
}

/// Convert a 0-based column index to A1 letters (0→`A`, 25→`Z`, 26→`AA`).
fn col_letters(mut idx: u32) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (idx % 26) as u8);
        if idx < 26 {
            break;
        }
        idx = idx / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// Parse 1–3 A1 column letters to a 0-based index.
fn letters_col(s: &[char]) -> Option<u32> {
    if s.is_empty() || s.len() > 3 {
        return None;
    }
    let mut idx: u32 = 0;
    for &c in s {
        if !c.is_ascii_uppercase() {
            return None;
        }
        idx = idx
            .checked_mul(26)?
            .checked_add(c as u32 - 'A' as u32 + 1)?;
    }
    Some(idx - 1)
}

/// Try to read an A1 cell reference at `ch[start]` and return `(chars_consumed,
/// shifted_ref)` after shifting its relative parts by `(drow, dcol)`. `$`-absolute
/// parts are unchanged; a ref shifted off-grid becomes `#REF!`. Returns `None` if
/// there is no reference here (identifier char before, `(`/alnum after → a function
/// name like `LOG10(` or a larger token, not a reference).
fn try_shift_ref(ch: &[char], start: usize, drow: i64, dcol: i64) -> Option<(usize, String)> {
    if !token_boundary_before(ch, start) {
        return None;
    }
    let mut i = start;
    let col_abs = ch.get(i) == Some(&'$');
    if col_abs {
        i += 1;
    }
    let lstart = i;
    while i < ch.len() && ch[i].is_ascii_uppercase() && i - lstart < 3 {
        i += 1;
    }
    let letters = &ch[lstart..i];
    if letters.is_empty() {
        return None;
    }
    let row_abs = ch.get(i) == Some(&'$');
    if row_abs {
        i += 1;
    }
    let dstart = i;
    while i < ch.len() && ch[i].is_ascii_digit() {
        i += 1;
    }
    if i == dstart {
        return None;
    }
    if !token_boundary_after(ch, i) {
        return None;
    }
    let col = letters_col(letters)?;
    let row: u32 = ch[dstart..i].iter().collect::<String>().parse().ok()?;
    // The *original* token must itself be an in-grid reference; an A1-shaped name
    // outside the grid (e.g. `XFE1`, col > XFD) is not a cell ref — leave it verbatim.
    if row == 0 || col > 16383 || row > 1_048_576 {
        return None;
    }
    let new_col = if col_abs {
        col as i64
    } else {
        col as i64 + dcol
    };
    let new_row = if row_abs {
        row as i64
    } else {
        row as i64 + drow
    };
    if !(0..=16383).contains(&new_col) || !(1..=1_048_576).contains(&new_row) {
        return Some((i - start, "#REF!".to_string()));
    }
    let mut out = String::new();
    if col_abs {
        out.push('$');
    }
    out.push_str(&col_letters(new_col as u32));
    if row_abs {
        out.push('$');
    }
    out.push_str(&new_row.to_string());
    Some((i - start, out))
}

fn token_boundary_before(ch: &[char], start: usize) -> bool {
    start == 0 || {
        let p = ch[start - 1];
        !(p.is_ascii_alphanumeric() || p == '_')
    }
}

fn token_boundary_after(ch: &[char], end: usize) -> bool {
    !matches!(
        ch.get(end),
        Some(after) if *after == '(' || after.is_ascii_alphanumeric() || *after == '_'
    )
}

fn parse_whole_row_part(ch: &[char], mut i: usize) -> Option<(bool, u32, usize)> {
    let abs = ch.get(i) == Some(&'$');
    if abs {
        i += 1;
    }
    let start = i;
    while i < ch.len() && ch[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    let row: u32 = ch[start..i].iter().collect::<String>().parse().ok()?;
    if row == 0 || row > 1_048_576 {
        return None;
    }
    Some((abs, row, i))
}

fn shift_row_part(row: u32, abs: bool, drow: i64) -> Option<u32> {
    let shifted = if abs { row as i64 } else { row as i64 + drow };
    (1..=1_048_576).contains(&shifted).then_some(shifted as u32)
}

fn try_shift_whole_row_ref(ch: &[char], start: usize, drow: i64) -> Option<(usize, String)> {
    if !token_boundary_before(ch, start) {
        return None;
    }
    let (first_abs, first, mut i) = parse_whole_row_part(ch, start)?;
    if ch.get(i) != Some(&':') {
        return None;
    }
    i += 1;
    let (last_abs, last, end) = parse_whole_row_part(ch, i)?;
    if !token_boundary_after(ch, end) {
        return None;
    }
    let (Some(first), Some(last)) = (
        shift_row_part(first, first_abs, drow),
        shift_row_part(last, last_abs, drow),
    ) else {
        return Some((end - start, "#REF!".to_string()));
    };
    let mut out = String::new();
    if first_abs {
        out.push('$');
    }
    out.push_str(&first.to_string());
    out.push(':');
    if last_abs {
        out.push('$');
    }
    out.push_str(&last.to_string());
    Some((end - start, out))
}

fn parse_whole_col_part(ch: &[char], mut i: usize) -> Option<(bool, u32, usize)> {
    let abs = ch.get(i) == Some(&'$');
    if abs {
        i += 1;
    }
    let start = i;
    while i < ch.len() && ch[i].is_ascii_uppercase() && i - start < 3 {
        i += 1;
    }
    if i == start {
        return None;
    }
    let col = letters_col(&ch[start..i])?;
    if col > 16_383 {
        return None;
    }
    Some((abs, col, i))
}

fn shift_col_part(col: u32, abs: bool, dcol: i64) -> Option<u32> {
    let shifted = if abs { col as i64 } else { col as i64 + dcol };
    (0..=16_383).contains(&shifted).then_some(shifted as u32)
}

fn try_shift_whole_col_ref(ch: &[char], start: usize, dcol: i64) -> Option<(usize, String)> {
    if !token_boundary_before(ch, start) {
        return None;
    }
    let (first_abs, first, mut i) = parse_whole_col_part(ch, start)?;
    if ch.get(i) != Some(&':') {
        return None;
    }
    i += 1;
    let (last_abs, last, end) = parse_whole_col_part(ch, i)?;
    if !token_boundary_after(ch, end) {
        return None;
    }
    let (Some(first), Some(last)) = (
        shift_col_part(first, first_abs, dcol),
        shift_col_part(last, last_abs, dcol),
    ) else {
        return Some((end - start, "#REF!".to_string()));
    };
    let mut out = String::new();
    if first_abs {
        out.push('$');
    }
    out.push_str(&col_letters(first));
    out.push(':');
    if last_abs {
        out.push('$');
    }
    out.push_str(&col_letters(last));
    Some((end - start, out))
}

/// Shift the relative A1 references in a formula by `(drow, dcol)` — the core of
/// reconstructing a shared-formula follower from its master. References inside
/// `"…"` string literals and `'…'` quoted sheet names, and `$`-absolute parts, are
/// left unchanged; off-grid shifts become `#REF!`.
fn shift_formula(f: &str, drow: i64, dcol: i64) -> String {
    let ch: Vec<char> = f.chars().collect();
    let mut out = String::with_capacity(f.len());
    let mut i = 0;
    let mut in_string = false; // "…" string literal
    let mut in_quote = false; // '…' quoted sheet name
    while i < ch.len() {
        let c = ch[i];
        if c == '"' && !in_quote {
            in_string = !in_string;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\'' && !in_string {
            in_quote = !in_quote;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_string && !in_quote {
            if let Some((consumed, shifted)) = try_shift_whole_row_ref(&ch, i, drow) {
                out.push_str(&shifted);
                i += consumed;
                continue;
            }
            if let Some((consumed, shifted)) = try_shift_whole_col_ref(&ch, i, dcol) {
                out.push_str(&shifted);
                i += consumed;
                continue;
            }
            if let Some((consumed, shifted)) = try_shift_ref(&ch, i, drow, dcol) {
                out.push_str(&shifted);
                i += consumed;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `<sheetData><row><c r t s><f>formula</f><v>…</v>|<is><t>…</t></is></c>` →
/// typed cells, plus the sheet's `<mergeCells>` ranges and the unresolved
/// `<hyperlinks>` as `(row, col, r:id)` (the caller resolves each `r:id` via the
/// worksheet rels).
#[derive(Debug, Default)]
struct ParsedSheet {
    cells: Vec<CellEntry>,
    direct_cell_formats: BTreeMap<(u32, u16), CellStyleOverlay>,
    rich: BTreeMap<(u32, u16), Vec<crate::TextRun>>,
    merges: Vec<(u32, u16, u32, u16)>,
    hyperlink_refs: Vec<(u32, u16, String)>,
    freeze: Option<(u32, u16)>,
    autofilter: Option<(u32, u16, u32, u16)>,
    data_validations: Vec<DataValidation>,
    cond_formats: Vec<CondFormat>,
    cond_format_metadata: Vec<ConditionalFormatMetadata>,
    page_setup: Option<PageSetup>,
    print_metadata: PrintMetadata,
    sparklines: Vec<Sparkline>,
    tab_color: Option<Color>,
    print_gridlines: bool,
    print_headings: bool,
    row_outline: BTreeMap<u32, u8>,
    col_outline: BTreeMap<u16, u8>,
    col_widths: BTreeMap<u16, f32>,
    row_heights: BTreeMap<u32, f32>,
    automatic_row_height_candidates: BTreeSet<u32>,
    imported_column_axis_measures: BTreeMap<u16, ImportedAxisMeasure>,
    imported_row_axis_measures: BTreeMap<u32, ImportedAxisMeasure>,
    col_formats: BTreeMap<u16, CellStyle>,
    row_formats: BTreeMap<u32, CellStyle>,
    hidden_cols: BTreeSet<u16>,
    hidden_rows: BTreeSet<u32>,
    default_rows_hidden: bool,
    explicit_visible_rows: BTreeSet<u32>,
    default_row_height: Option<f32>,
    automatic_default_row_height_candidate: bool,
    default_col_width: Option<f32>,
    imported_default_row_axis_measure: Option<ImportedAxisMeasure>,
    imported_default_column_axis_measure: Option<ImportedAxisMeasure>,
    base_col_width: Option<f32>,
    defaulted_base_col_width: bool,
    collapsed_rows: BTreeSet<u32>,
    outline_summary_below: Option<bool>,
    outline_summary_right: Option<bool>,
    protect: bool,
    protect_options: Option<ProtectionOptions>,
    hide_gridlines: bool,
    zoom: Option<u16>,
    show_headers: Option<bool>,
    right_to_left: bool,
    tab_selected: bool,
}

fn scaled_ratio_u32(numerator: u64, denominator: u64, scale: u32) -> Option<u32> {
    let scaled = u128::from(numerator).checked_mul(u128::from(scale))?;
    (scaled % u128::from(denominator) == 0)
        .then(|| u32::try_from(scaled / u128::from(denominator)).ok())
        .flatten()
}

fn parse_point_axis_measure(value: &str) -> Option<ImportedAxisMeasure> {
    let (numerator, denominator) = parse_decimal_ratio_u64(value)?;
    if numerator == 0 {
        return None;
    }
    Some(
        scaled_ratio_u32(numerator, denominator, 20)
            .map(ImportedAxisMeasure::Twips)
            .unwrap_or(ImportedAxisMeasure::PointRatio(numerator, denominator)),
    )
}

fn parse_character_axis_measure(value: &str) -> Option<ImportedAxisMeasure> {
    let (numerator, denominator) = parse_decimal_ratio_u64(value)?;
    if numerator == 0 {
        return None;
    }
    Some(ImportedAxisMeasure::CharacterWidthRatio(
        numerator,
        denominator,
    ))
}

type SheetRange = (u32, u16, u32, u16);
type ParsedDataValidation = (DataValidation, Vec<SheetRange>);

#[derive(Clone, Copy, Debug)]
enum HeaderFooterField {
    Header,
    Footer,
}

#[derive(Clone, Copy, Debug)]
struct HeaderFooterCapture {
    kind: HeaderFooterKind,
    legacy: Option<HeaderFooterField>,
}

#[derive(Clone, Copy, Debug)]
enum PageBreakAxis {
    Row,
    Column,
}

#[derive(Debug)]
enum PendingCfKind {
    CellIs {
        op: DvOp,
        fill: Color,
    },
    ColorScale,
    DataBar,
    TopBottom {
        rank: u32,
        bottom: bool,
        percent: bool,
        fill: Color,
    },
    AboveAverage {
        below: bool,
        fill: Color,
    },
    DuplicateValues {
        unique: bool,
        fill: Color,
    },
    Expression {
        fill: Color,
    },
}

#[derive(Debug)]
struct PendingCfRule {
    ranges: Vec<SheetRange>,
    kind: PendingCfKind,
    formulas: Vec<String>,
    colors: Vec<Color>,
    metadata: ConditionalFormatMetadata,
}

impl PendingCfRule {
    fn build_rule(&self) -> Option<CfRule> {
        match &self.kind {
            PendingCfKind::CellIs { op, fill } => Some(CfRule::CellIs {
                op: *op,
                formula1: self.formulas.first()?.clone(),
                formula2: self.formulas.get(1).filter(|s| !s.is_empty()).cloned(),
                fill: *fill,
            }),
            PendingCfKind::ColorScale => match self.colors.as_slice() {
                [min, max] => Some(CfRule::ColorScale2 {
                    min: *min,
                    max: *max,
                }),
                [min, mid, max, ..] => Some(CfRule::ColorScale3 {
                    min: *min,
                    mid: *mid,
                    max: *max,
                }),
                _ => None,
            },
            PendingCfKind::DataBar => self
                .colors
                .first()
                .copied()
                .map(|color| CfRule::DataBar { color }),
            PendingCfKind::TopBottom {
                rank,
                bottom,
                percent,
                fill,
            } => Some(CfRule::TopBottom {
                rank: *rank,
                bottom: *bottom,
                percent: *percent,
                fill: *fill,
            }),
            PendingCfKind::AboveAverage { below, fill } => Some(CfRule::AboveAverage {
                below: *below,
                fill: *fill,
            }),
            PendingCfKind::DuplicateValues { unique, fill } => Some(CfRule::DuplicateValues {
                unique: *unique,
                fill: *fill,
            }),
            PendingCfKind::Expression { fill } => Some(CfRule::Expression {
                formula: self.formulas.first()?.clone(),
                fill: *fill,
            }),
        }
    }
}

fn parse_sheet(
    xml: &str,
    shared: &[SharedString],
    styles: &Styles,
    theme: &ThemeColors,
    date1904: bool,
    budget: &mut usize,
) -> ParsedSheet {
    if !crate::xml_reference_work_within_budget(xml) {
        *budget = 0;
        return ParsedSheet::default();
    }
    let mut r = Reader::from_str(xml);
    let mut parsed = ParsedSheet::default();
    // Current cell state.
    let mut rc: Option<(u32, u16)> = None;
    let mut ctype = String::new();
    let mut style_idx = 0usize;
    let mut value = String::new();
    let mut inline_value = String::new();
    let mut inline_text_seen = false;
    let mut inline_run: Option<crate::TextRun> = None;
    let mut inline_runs = Vec::<crate::TextRun>::new();
    let mut formula = String::new();
    // Shared-formula state: si → (master formula text, base row, base col).
    let mut shared_masters: HashMap<u32, (String, u32, u16)> = HashMap::new();
    // Array-formula state: declared rectangular range → anchor formula text.
    let mut array_formulas: Vec<(SheetRange, String)> = Vec::new();
    let mut f_si: Option<u32> = None;
    let mut f_array_ref: Option<SheetRange> = None;
    let mut in_v = false;
    let mut in_f = false;
    let mut in_is_t = false;
    let mut in_rph = false; // East Asian phonetic (ruby) guide — excluded from value
    let mut current_dv: Option<DataValidation> = None;
    let mut current_dv_extra_ranges: Vec<SheetRange> = Vec::new();
    let mut in_dv_formula1 = false;
    let mut in_dv_formula2 = false;
    let mut current_cf_ranges: Vec<SheetRange> = Vec::new();
    let mut current_cf: Option<PendingCfRule> = None;
    let mut in_cf_formula = false;
    let mut header_footer_capture: Option<HeaderFooterCapture> = None;
    let mut page_break_axis: Option<PageBreakAxis> = None;
    let mut current_sparkline_kind = SparklineKind::Line;
    let mut current_sparkline_range = String::new();
    let mut current_sparkline_location = String::new();
    let mut in_sparkline = false;
    let mut in_sparkline_formula = false;
    let mut in_sparkline_sqref = false;
    // Implicit position tracking: the `r` attribute on `<row>`/`<c>` is optional
    // in [ISO/IEC 29500]; when omitted, position is implicit (cells fill
    // left-to-right, rows top-to-bottom). Some writers (LibreOffice, EPPlus, …)
    // omit it. Without this, every `r`-less cell would be dropped.
    let mut cur_row: Option<u32> = Some(0);
    let mut cur_col: Option<u16> = Some(0);
    let mut row_started = false;
    let mut selected_sheet_view_rank = 0u8;
    let mut in_selected_sheet_view = false;
    loop {
        match r.read_event() {
            // A self-closing `<f/>` (a shared-formula follower has no formula text)
            // must NOT open formula-text capture: otherwise pretty-printing
            // whitespace between `<f/>` and `<v>` is captured as the formula and the
            // follower is mis-registered as a master. Capture only formula metadata.
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"f" => {
                let formula_kind = attr(&e, b"t");
                f_si = if formula_kind.as_deref() == Some("shared") {
                    attr(&e, b"si").and_then(|s| s.parse::<u32>().ok())
                } else {
                    None
                };
                f_array_ref = if formula_kind.as_deref() == Some("array") {
                    attr(&e, b"ref").as_deref().and_then(parse_range)
                } else {
                    None
                };
                in_f = false;
            }
            Ok(Event::Empty(e))
                if matches!(
                    local(e.name().as_ref()),
                    b"dataValidation" | b"formula1" | b"formula2"
                ) => {}
            Ok(Event::Empty(e)) if header_footer_field(local(e.name().as_ref())).is_some() => {
                if let Some((kind, field, preferred)) =
                    header_footer_field(local(e.name().as_ref()))
                {
                    begin_header_footer_capture(&mut parsed, kind, field, preferred);
                }
            }
            Ok(Event::Empty(e))
                if matches!(local(e.name().as_ref()), b"rowBreaks" | b"colBreaks") =>
            {
                parsed.print_metadata.mark_source();
                page_break_axis = None;
            }
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"conditionalFormatting" => {
                push_current_conditional_format(&mut parsed, current_cf.take());
                current_cf_ranges.clear();
            }
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"cfRule" => {
                push_current_conditional_format(&mut parsed, current_cf.take());
                let current = parse_conditional_rule(&e, &current_cf_ranges, styles);
                push_current_conditional_format(&mut parsed, current);
            }
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"row" => {
                    cur_row = match attr(&e, b"r") {
                        Some(value) => value
                            .parse::<u32>()
                            .ok()
                            .and_then(|row| row.checked_sub(1))
                            .filter(|row| *row <= MAX_XLSX_ROW_INDEX),
                        None if row_started => cur_row
                            .and_then(|row| row.checked_add(1))
                            .filter(|row| *row <= MAX_XLSX_ROW_INDEX),
                        None => Some(0),
                    };
                    row_started = true;
                    cur_col = Some(0);
                    if let Some(cur_row) = cur_row {
                        if let Some(level) = attr(&e, b"outlineLevel")
                            .and_then(|s| s.parse::<u8>().ok())
                            .filter(|level| (1..=7).contains(level))
                        {
                            parsed.row_outline.insert(cur_row, level);
                        }
                        if attr(&e, b"collapsed").as_deref().is_some_and(attr_true) {
                            parsed.collapsed_rows.insert(cur_row);
                        }
                        if let Some(source_height) = attr(&e, b"ht") {
                            let exact_measure = parse_point_axis_measure(&source_height);
                            if let Ok(height) = source_height.trim().parse::<f32>() {
                                if height.is_finite() && height > 0.0 {
                                    parsed.row_heights.insert(cur_row, height);
                                    if let Some(measure) = exact_measure {
                                        parsed.imported_row_axis_measures.insert(cur_row, measure);
                                    } else {
                                        parsed.imported_row_axis_measures.remove(&cur_row);
                                    }
                                    if attr(&e, b"customHeight").as_deref().is_some_and(attr_true) {
                                        parsed.automatic_row_height_candidates.remove(&cur_row);
                                    } else {
                                        parsed.automatic_row_height_candidates.insert(cur_row);
                                    }
                                }
                            }
                        }
                        if attr(&e, b"hidden").as_deref().is_some_and(attr_true) {
                            parsed.hidden_rows.insert(cur_row);
                            parsed.explicit_visible_rows.remove(&cur_row);
                        } else {
                            parsed.hidden_rows.remove(&cur_row);
                            parsed.explicit_visible_rows.insert(cur_row);
                        }
                        if let Some(style) = attr(&e, b"s")
                            .and_then(|value| value.parse::<usize>().ok())
                            .and_then(|index| styles.cell_style(index))
                        {
                            parsed.row_formats.insert(cur_row, style.clone());
                        }
                    }
                }
                b"sheetFormatPr" => {
                    let default_row_height = attr(&e, b"defaultRowHeight");
                    parsed.default_row_height = default_row_height
                        .as_deref()
                        .and_then(|source| source.trim().parse::<f32>().ok())
                        .filter(|height| height.is_finite() && *height > 0.0);
                    parsed.automatic_default_row_height_candidate =
                        parsed.default_row_height.is_some()
                            && !attr(&e, b"customHeight").as_deref().is_some_and(attr_true);
                    // Preserve the compatibility float independently of exact
                    // source geometry. Valid xsd:double lexical values can
                    // exceed the bounded rational provenance representation.
                    parsed.imported_default_row_axis_measure = parsed
                        .default_row_height
                        .and(default_row_height.as_deref())
                        .and_then(parse_point_axis_measure);
                    let default_col_width = attr(&e, b"defaultColWidth");
                    parsed.default_col_width = default_col_width
                        .as_deref()
                        .and_then(|s| s.trim().parse::<f32>().ok())
                        .filter(|width| width.is_finite() && *width > 0.0);
                    // ECMA-376 defaults baseColWidth to 8 when the element is
                    // present. Keep that import branch separate from the
                    // 8.5-character constructor default used when the entire
                    // element is absent, without changing the compatibility
                    // character-width projection.
                    let base_col_width = attr(&e, b"baseColWidth");
                    parsed.defaulted_base_col_width = base_col_width
                        .as_deref()
                        .is_none_or(|value| value.parse::<i32>().is_err());
                    parsed.base_col_width = match base_col_width {
                        Some(value) => value
                            .parse::<i32>()
                            .ok()
                            .filter(|width| *width > 0)
                            .map(|width| width as f32),
                        None => None,
                    };
                    parsed.imported_default_column_axis_measure =
                        if parsed.default_col_width.is_some() {
                            default_col_width
                                .as_deref()
                                .and_then(parse_character_axis_measure)
                        } else {
                            parsed.base_col_width.and_then(|characters| {
                                let characters = characters as u32;
                                characters
                                    .checked_mul(256)
                                    .map(ImportedAxisMeasure::CharacterBaseWidth256)
                            })
                        };
                    parsed.default_rows_hidden = attr(&e, b"zeroHeight")
                        .as_deref()
                        .and_then(parse_bool_attr)
                        .unwrap_or(false);
                }
                b"outlinePr" => {
                    if let Some(value) = attr(&e, b"summaryBelow")
                        .as_deref()
                        .and_then(parse_bool_attr)
                    {
                        parsed.outline_summary_below = Some(value);
                    }
                    if let Some(value) = attr(&e, b"summaryRight")
                        .as_deref()
                        .and_then(parse_bool_attr)
                    {
                        parsed.outline_summary_right = Some(value);
                    }
                }
                b"pageSetUpPr" => {
                    // ECMA-376 §18.3.1.65 makes this the mode switch; Annex A.2
                    // defaults a missing fitToPage attribute to false.
                    let fit_to_page = print_bool_attr(&e, b"fitToPage", &mut parsed.print_metadata);
                    parsed.print_metadata.set_fit_to_page(fit_to_page);
                }
                b"col" => {
                    let first = attr(&e, b"min")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(1)
                        .max(1);
                    let last = attr(&e, b"max")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(first)
                        .min(16_384);
                    if first <= last {
                        let width_source = attr(&e, b"width");
                        let width = width_source
                            .as_deref()
                            .and_then(|s| s.trim().parse::<f32>().ok());
                        let width_measure = width_source
                            .as_deref()
                            .and_then(parse_character_axis_measure);
                        let hidden = attr(&e, b"hidden").as_deref().is_some_and(attr_true);
                        let style = attr(&e, b"style")
                            .and_then(|value| value.parse::<usize>().ok())
                            .and_then(|index| styles.cell_style(index));
                        for col in first..=last {
                            if let Ok(col) = u16::try_from(col - 1) {
                                if let Some(width) = width {
                                    parsed.col_widths.insert(col, width);
                                    match width_measure {
                                        Some(measure) => {
                                            parsed
                                                .imported_column_axis_measures
                                                .insert(col, measure);
                                        }
                                        None => {
                                            parsed.imported_column_axis_measures.remove(&col);
                                        }
                                    }
                                }
                                if hidden {
                                    parsed.hidden_cols.insert(col);
                                }
                                if let Some(style) = style {
                                    parsed.col_formats.insert(col, style.clone());
                                }
                            }
                        }
                    }
                    if let Some(level) = attr(&e, b"outlineLevel")
                        .and_then(|s| s.parse::<u8>().ok())
                        .filter(|level| (1..=7).contains(level))
                    {
                        if first <= last {
                            for col in first..=last {
                                if let Ok(col) = u16::try_from(col - 1) {
                                    parsed.col_outline.insert(col, level);
                                }
                            }
                        }
                    }
                }
                b"sheetProtection" => {
                    if let Some(options) = parse_sheet_protection(&e) {
                        parsed.protect = true;
                        parsed.protect_options = options;
                    }
                }
                b"sheetView" => {
                    if attr(&e, b"tabSelected").as_deref().is_some_and(attr_true) {
                        parsed.tab_selected = true;
                    }
                    let rank = sheet_view_rank(&e);
                    in_selected_sheet_view = rank > selected_sheet_view_rank;
                    if in_selected_sheet_view {
                        selected_sheet_view_rank = rank;
                        clear_sheet_view_metadata(&mut parsed);
                        if attr(&e, b"showGridLines")
                            .as_deref()
                            .is_some_and(attr_false)
                        {
                            parsed.hide_gridlines = true;
                        }
                        if let Some(show_headers) = attr(&e, b"showRowColHeaders")
                            .as_deref()
                            .and_then(parse_bool_attr)
                        {
                            parsed.show_headers = Some(show_headers);
                        }
                        if attr(&e, b"rightToLeft").as_deref().is_some_and(attr_true) {
                            parsed.right_to_left = true;
                        }
                        if let Some(zoom) =
                            attr(&e, b"zoomScale").and_then(|s| s.parse::<u16>().ok())
                        {
                            parsed.zoom = Some(zoom);
                        }
                    }
                }
                b"pane"
                    if in_selected_sheet_view
                        && attr(&e, b"state")
                            .as_deref()
                            .is_some_and(|state| matches!(state, "frozen" | "frozenSplit")) =>
                {
                    let row = attr(&e, b"ySplit")
                        .as_deref()
                        .and_then(parse_split_u32)
                        .unwrap_or(0);
                    let col = attr(&e, b"xSplit")
                        .as_deref()
                        .and_then(parse_split_u16)
                        .unwrap_or(0);
                    if row > 0 || col > 0 {
                        parsed.freeze = Some((row, col));
                    }
                }
                b"tabColor" => {
                    parsed.tab_color = color_attr(&e, theme, &styles.indexed_colors);
                }
                b"c" => {
                    // Use the explicit `r` when present (and resync the implicit
                    // column to it); otherwise fall back to the running position.
                    // A cell reference never repairs an invalid enclosing row:
                    // only a later valid `<row r>` may resynchronize row order.
                    let pos = match (cur_row, attr(&e, b"r")) {
                        (Some(_), Some(reference)) => match parse_ref(&reference) {
                            Some((row, col)) => {
                                cur_col = col
                                    .checked_add(1)
                                    .filter(|column| *column <= MAX_XLSX_COLUMN_INDEX);
                                Some((row, col))
                            }
                            None => {
                                cur_col = None;
                                None
                            }
                        },
                        (Some(row), None) => cur_col.map(|col| {
                            cur_col = col
                                .checked_add(1)
                                .filter(|column| *column <= MAX_XLSX_COLUMN_INDEX);
                            (row, col)
                        }),
                        (None, _) => None,
                    };
                    rc = pos;
                    ctype = attr(&e, b"t").unwrap_or_default();
                    style_idx = attr(&e, b"s").and_then(|s| s.parse().ok()).unwrap_or(0);
                    value.clear();
                    inline_value.clear();
                    inline_text_seen = false;
                    inline_run = None;
                    inline_runs.clear();
                    formula.clear();
                    f_si = None;
                    f_array_ref = None;
                    // Reset text-capture flags so a stray one (e.g. a self-closing
                    // `<f/>` that never fires an End) cannot leak into this cell.
                    (in_v, in_f, in_is_t, in_rph) = (false, false, false, false);
                }
                // A `<v>` is a sibling of `<f>`, never inside it: entering `<v>`
                // clears `in_f` so a self-closing `<f/>` (shared-formula follower,
                // no End event) can't capture the value text as formula text.
                b"v" => (in_v, in_f) = (true, false),
                b"f" if in_sparkline => in_sparkline_formula = true,
                b"f" => {
                    in_f = true; // formula text (sibling of <v>)
                                 // A shared formula carries `t="shared" si="N"`; the master also
                                 // has the formula text + a `ref`, followers are empty `<f/>`.
                    let formula_kind = attr(&e, b"t");
                    f_si = if formula_kind.as_deref() == Some("shared") {
                        attr(&e, b"si").and_then(|s| s.parse::<u32>().ok())
                    } else {
                        None
                    };
                    f_array_ref = if formula_kind.as_deref() == Some("array") {
                        attr(&e, b"ref").as_deref().and_then(parse_range)
                    } else {
                        None
                    };
                }
                b"rPh" => in_rph = true, // phonetic/ruby guide in an inline string
                b"r" if ctype == "inlineStr" && rc.is_some() && !in_rph => {
                    inline_run = Some(crate::TextRun::default());
                }
                b"rFont" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.name = attr(&e, b"val");
                }
                b"sz" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.size_pt = attr(&e, b"val")
                        .and_then(|value| value.parse::<f32>().ok())
                        .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                }
                b"color" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.color =
                        color_attr(&e, theme, &styles.indexed_colors);
                }
                b"b" if inline_run.is_some() => inline_run.as_mut().expect("run").font.bold = true,
                b"i" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.italic = true;
                }
                b"u" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.underline = true;
                }
                b"strike" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.strikethrough = true;
                }
                b"vertAlign" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.script =
                        match attr(&e, b"val").as_deref() {
                            Some("superscript") => FormatScript::Superscript,
                            Some("subscript") => FormatScript::Subscript,
                            _ => FormatScript::None,
                        };
                }
                b"t" => {
                    in_is_t = true; // inline-string text (within <is>)
                    if !in_rph {
                        inline_text_seen = true;
                    }
                }
                // `<mergeCell ref="A1:C3"/>` — usually self-closing (Empty), but
                // accept Start too.
                b"mergeCell" => {
                    if let Some(rng) = attr(&e, b"ref").as_deref().and_then(parse_range) {
                        parsed.merges.push(rng);
                    }
                }
                b"autoFilter" => {
                    if let Some(rng) = attr(&e, b"ref").as_deref().and_then(parse_range) {
                        parsed.autofilter = Some(rng);
                    }
                }
                b"printOptions" => {
                    let gridlines = print_bool_attr(&e, b"gridLines", &mut parsed.print_metadata);
                    let headings = print_bool_attr(&e, b"headings", &mut parsed.print_metadata);
                    let horizontal =
                        print_bool_attr(&e, b"horizontalCentered", &mut parsed.print_metadata);
                    let vertical =
                        print_bool_attr(&e, b"verticalCentered", &mut parsed.print_metadata);
                    parsed.print_metadata.set_print_gridlines(gridlines);
                    parsed.print_metadata.set_print_headings(headings);
                    parsed.print_metadata.set_center_horizontally(horizontal);
                    parsed.print_metadata.set_center_vertically(vertical);
                    if gridlines {
                        parsed.print_gridlines = true;
                    }
                    if headings {
                        parsed.print_headings = true;
                    }
                    if horizontal {
                        page_setup_mut(&mut parsed).center_horizontally = true;
                    }
                    if vertical {
                        page_setup_mut(&mut parsed).center_vertically = true;
                    }
                }
                b"pageMargins" => {
                    let margins = (
                        attr_f64(&e, b"left"),
                        attr_f64(&e, b"right"),
                        attr_f64(&e, b"top"),
                        attr_f64(&e, b"bottom"),
                        attr_f64(&e, b"header"),
                        attr_f64(&e, b"footer"),
                    );
                    if let (
                        Some(left),
                        Some(right),
                        Some(top),
                        Some(bottom),
                        Some(header),
                        Some(footer),
                    ) = margins
                    {
                        page_setup_mut(&mut parsed).margins =
                            Some((left, right, top, bottom, header, footer));
                    }
                }
                b"pageSetup" => {
                    // A missing pageSetUpPr/fitToPage is percentage mode, even
                    // when stale fit counts remain on pageSetup.
                    if parsed.print_metadata.fit_to_page().is_none() {
                        parsed.print_metadata.set_fit_to_page(false);
                    }
                    match attr(&e, b"pageOrder").as_deref() {
                        Some("overThenDown") => parsed
                            .print_metadata
                            .set_page_order(PrintPageOrder::OverThenDown),
                        Some("downThenOver") | None => parsed
                            .print_metadata
                            .set_page_order(PrintPageOrder::DownThenOver),
                        Some(_) => parsed
                            .print_metadata
                            .add_loss(PrintLossKind::UnsupportedProperty),
                    }
                    let fit_to_width =
                        print_fit_count_attr(&e, b"fitToWidth", &mut parsed.print_metadata);
                    let fit_to_height =
                        print_fit_count_attr(&e, b"fitToHeight", &mut parsed.print_metadata);
                    let ps = page_setup_mut(&mut parsed);
                    ps.landscape = attr(&e, b"orientation")
                        .as_deref()
                        .is_some_and(|orientation| orientation.eq_ignore_ascii_case("landscape"));
                    ps.paper_size = attr_u16(&e, b"paperSize");
                    ps.scale = attr_u16(&e, b"scale");
                    ps.fit_to_width = fit_to_width;
                    ps.fit_to_height = fit_to_height;
                    ps.first_page_number = attr(&e, b"useFirstPageNumber")
                        .as_deref()
                        .is_some_and(attr_true)
                        .then(|| attr_u16(&e, b"firstPageNumber"))
                        .flatten();
                }
                b"headerFooter" => {
                    let different_odd_even = header_footer_bool_attr(
                        &e,
                        b"differentOddEven",
                        false,
                        &mut parsed.print_metadata,
                    );
                    let different_first = header_footer_bool_attr(
                        &e,
                        b"differentFirst",
                        false,
                        &mut parsed.print_metadata,
                    );
                    let scale_with_document = header_footer_bool_attr(
                        &e,
                        b"scaleWithDoc",
                        true,
                        &mut parsed.print_metadata,
                    );
                    let align_with_margins = header_footer_bool_attr(
                        &e,
                        b"alignWithMargins",
                        true,
                        &mut parsed.print_metadata,
                    );
                    parsed.print_metadata.set_header_footer_flag(
                        Some(different_odd_even),
                        Some(different_first),
                        Some(scale_with_document),
                        Some(align_with_margins),
                    );
                }
                b"oddHeader" | b"firstHeader" | b"evenHeader" | b"oddFooter" | b"firstFooter"
                | b"evenFooter" => {
                    if let Some((kind, field, preferred)) =
                        header_footer_field(local(e.name().as_ref()))
                    {
                        header_footer_capture = Some(begin_header_footer_capture(
                            &mut parsed,
                            kind,
                            field,
                            preferred,
                        ));
                    }
                }
                b"rowBreaks" => page_break_axis = Some(PageBreakAxis::Row),
                b"colBreaks" => page_break_axis = Some(PageBreakAxis::Column),
                b"brk" if page_break_axis.is_some() => {
                    parse_manual_page_break(&e, page_break_axis, &mut parsed.print_metadata);
                }
                b"sparklineGroup" => {
                    current_sparkline_kind = attr(&e, b"type")
                        .as_deref()
                        .map(parse_sparkline_kind)
                        .unwrap_or(SparklineKind::Line);
                }
                b"sparkline" => {
                    in_sparkline = true;
                    current_sparkline_range.clear();
                    current_sparkline_location.clear();
                }
                b"dataValidation" => {
                    push_current_data_validation(
                        &mut parsed,
                        current_dv.take(),
                        &mut current_dv_extra_ranges,
                    );
                    if let Some((dv, ranges)) = parse_data_validation(&e) {
                        current_dv = Some(dv);
                        current_dv_extra_ranges = ranges;
                    }
                }
                b"conditionalFormatting" => {
                    push_current_conditional_format(&mut parsed, current_cf.take());
                    current_cf_ranges = attr(&e, b"sqref")
                        .map(|sqref| sqref.split_whitespace().filter_map(parse_range).collect())
                        .unwrap_or_default();
                }
                b"cfRule" => {
                    push_current_conditional_format(&mut parsed, current_cf.take());
                    current_cf = parse_conditional_rule(&e, &current_cf_ranges, styles);
                }
                b"formula" if current_cf.is_some() => {
                    if let Some(cf) = current_cf.as_mut() {
                        cf.formulas.push(String::new());
                    }
                    in_cf_formula = true;
                }
                b"sqref" if in_sparkline => in_sparkline_sqref = true,
                b"color" if current_cf.is_some() => {
                    if let (Some(cf), Some(color)) = (
                        current_cf.as_mut(),
                        color_attr(&e, theme, &styles.indexed_colors),
                    ) {
                        cf.colors.push(color);
                    }
                }
                b"formula1" if current_dv.is_some() => in_dv_formula1 = true,
                b"formula2" if current_dv.is_some() => {
                    if let Some(dv) = current_dv.as_mut() {
                        dv.formula2.get_or_insert_with(String::new);
                    }
                    in_dv_formula2 = true;
                }
                // `<hyperlink ref="A1" r:id="rIdN"/>` — the `ref` may be a single
                // cell or a range; anchor at its top-left. The URL lives in the
                // worksheet rels (`r:id` → local "id"), resolved by the caller.
                b"hyperlink" => {
                    if let (Some((r0, c0, r1, c1)), Some(rid)) = (
                        attr(&e, b"ref").as_deref().and_then(parse_range),
                        attr(&e, b"id"),
                    ) {
                        // A `ref` may be a range (`A1:A3`) — surface every cell, not
                        // just the top-left, bounded so a whole-column ref can't
                        // amplify into millions of entries.
                        let mut n = 0usize;
                        'hl: for row in r0..=r1 {
                            for col in c0..=c1 {
                                if n >= (1 << 16) {
                                    break 'hl;
                                }
                                parsed.hyperlink_refs.push((row, col, rid.clone()));
                                n += 1;
                            }
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) if header_footer_capture.is_some() => {
                if let Some(capture) = header_footer_capture {
                    append_header_footer_text(&mut parsed, capture, &text_of(&t));
                }
            }
            Ok(Event::Text(t)) if in_sparkline_formula => {
                current_sparkline_range.push_str(&text_of(&t));
            }
            Ok(Event::Text(t)) if in_sparkline_sqref => {
                current_sparkline_location.push_str(&text_of(&t));
            }
            Ok(Event::Text(t)) if in_cf_formula => {
                if let Some(formula) = current_cf.as_mut().and_then(|cf| cf.formulas.last_mut()) {
                    formula.push_str(&text_of(&t));
                }
            }
            Ok(Event::Text(t)) if in_dv_formula1 => {
                if let Some(dv) = current_dv.as_mut() {
                    dv.formula1.push_str(&text_of(&t));
                }
            }
            Ok(Event::Text(t)) if in_dv_formula2 => {
                if let Some(dv) = current_dv.as_mut() {
                    if let Some(formula2) = dv.formula2.as_mut() {
                        formula2.push_str(&text_of(&t));
                    }
                }
            }
            Ok(Event::Text(t)) if in_f => formula.push_str(&text_of(&t)),
            Ok(Event::Text(t)) if in_v => value.push_str(&text_of(&t)),
            Ok(Event::Text(t)) if in_is_t && !in_rph => {
                let text = text_of(&t);
                inline_value.push_str(&text);
                if let Some(run) = inline_run.as_mut() {
                    run.text.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                with_general_ref_text(&reference, |text| {
                    if let Some(capture) = header_footer_capture {
                        append_header_footer_text(&mut parsed, capture, text);
                    } else if in_sparkline_formula {
                        current_sparkline_range.push_str(text);
                    } else if in_sparkline_sqref {
                        current_sparkline_location.push_str(text);
                    } else if in_cf_formula {
                        if let Some(formula) =
                            current_cf.as_mut().and_then(|cf| cf.formulas.last_mut())
                        {
                            formula.push_str(text);
                        }
                    } else if in_dv_formula1 {
                        if let Some(dv) = current_dv.as_mut() {
                            dv.formula1.push_str(text);
                        }
                    } else if in_dv_formula2 {
                        if let Some(formula2) =
                            current_dv.as_mut().and_then(|dv| dv.formula2.as_mut())
                        {
                            formula2.push_str(text);
                        }
                    } else if in_f {
                        formula.push_str(text);
                    } else if in_v {
                        value.push_str(text);
                    } else if in_is_t && !in_rph {
                        inline_value.push_str(text);
                        if let Some(run) = inline_run.as_mut() {
                            run.text.push_str(text);
                        }
                    }
                });
            }
            Ok(Event::CData(t)) if header_footer_capture.is_some() => {
                if let Some(capture) = header_footer_capture {
                    let bytes = t.into_inner();
                    let text = String::from_utf8_lossy(bytes.as_ref());
                    append_header_footer_text(&mut parsed, capture, text.as_ref());
                }
            }
            Ok(Event::CData(t)) if in_sparkline_formula => {
                current_sparkline_range.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::CData(t)) if in_sparkline_sqref => {
                current_sparkline_location
                    .push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::CData(t)) if in_cf_formula => {
                if let Some(formula) = current_cf.as_mut().and_then(|cf| cf.formulas.last_mut()) {
                    formula.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                }
            }
            Ok(Event::CData(t)) if in_dv_formula1 => {
                if let Some(dv) = current_dv.as_mut() {
                    dv.formula1
                        .push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                }
            }
            Ok(Event::CData(t)) if in_dv_formula2 => {
                if let Some(dv) = current_dv.as_mut() {
                    if let Some(formula2) = dv.formula2.as_mut() {
                        formula2.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                    }
                }
            }
            Ok(Event::CData(t)) if in_v => {
                value.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::CData(t)) if in_is_t && !in_rph => {
                let bytes = t.into_inner();
                let text = String::from_utf8_lossy(bytes.as_ref());
                inline_value.push_str(&text);
                if let Some(run) = inline_run.as_mut() {
                    run.text.push_str(&text);
                }
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"v" => in_v = false,
                b"f" if in_sparkline_formula => in_sparkline_formula = false,
                b"f" => in_f = false,
                b"rPh" => in_rph = false,
                b"t" => in_is_t = false,
                b"r" if inline_run.is_some() => {
                    let completed = inline_run.take().expect("run");
                    if !completed.text.is_empty() {
                        inline_runs.push(completed);
                    }
                }
                b"sheetView" => in_selected_sheet_view = false,
                b"oddHeader" | b"firstHeader" | b"evenHeader" | b"oddFooter" | b"firstFooter"
                | b"evenFooter" => header_footer_capture = None,
                b"rowBreaks" | b"colBreaks" => page_break_axis = None,
                b"sqref" if in_sparkline_sqref => in_sparkline_sqref = false,
                b"sparkline" => {
                    in_sparkline = false;
                    if let Some(sparkline) = parse_sparkline(
                        current_sparkline_kind,
                        &current_sparkline_range,
                        &current_sparkline_location,
                    ) {
                        parsed.sparklines.push(sparkline);
                    }
                    current_sparkline_range.clear();
                    current_sparkline_location.clear();
                }
                b"sparklineGroup" => current_sparkline_kind = SparklineKind::Line,
                b"formula" => in_cf_formula = false,
                b"cfRule" => {
                    in_cf_formula = false;
                    push_current_conditional_format(&mut parsed, current_cf.take());
                }
                b"conditionalFormatting" => {
                    push_current_conditional_format(&mut parsed, current_cf.take());
                    current_cf_ranges.clear();
                }
                b"formula1" => in_dv_formula1 = false,
                b"formula2" => in_dv_formula2 = false,
                b"dataValidation" => {
                    in_dv_formula1 = false;
                    in_dv_formula2 = false;
                    push_current_data_validation(
                        &mut parsed,
                        current_dv.take(),
                        &mut current_dv_extra_ranges,
                    );
                }
                b"c" => {
                    if let Some((row, col)) = rc.take() {
                        let cell_value = if ctype == "inlineStr" && inline_text_seen {
                            inline_value.as_str()
                        } else {
                            value.as_str()
                        };
                        // Resolve a shared formula: a master (`si` + formula text)
                        // registers itself; a follower (`si`, empty text) rebuilds the
                        // formula by shifting the master's relative refs to this cell.
                        let mut resolved = match f_si {
                            Some(si) if !formula.is_empty() => {
                                shared_masters.insert(si, (formula.clone(), row, col));
                                formula.clone()
                            }
                            Some(si) => match shared_masters.get(&si) {
                                Some((mf, br, bc)) => shift_formula(
                                    mf,
                                    i64::from(row) - i64::from(*br),
                                    i64::from(col) - i64::from(*bc),
                                ),
                                None => formula.clone(),
                            },
                            None => formula.clone(),
                        };
                        if resolved.is_empty() {
                            if let Some((_, array_formula)) =
                                array_formulas.iter().rev().find(|((r0, c0, r1, c1), _)| {
                                    row >= *r0 && row <= *r1 && col >= *c0 && col <= *c1
                                })
                            {
                                resolved = array_formula.clone();
                            }
                        }
                        if !formula.is_empty() {
                            if let Some(array_ref) = f_array_ref.take() {
                                array_formulas.push((array_ref, resolved.clone()));
                            }
                        } else {
                            f_array_ref = None;
                        }
                        if let Some(entry) = build_cell(
                            row, col, &ctype, style_idx, cell_value, &resolved, shared, styles,
                            date1904,
                        ) {
                            // Account for the complete retained cell rather than
                            // only its rendered text. A custom number format can
                            // deliberately render a real value as an empty string;
                            // that value must remain available through the typed
                            // API without giving empty-display cells a zero-cost
                            // path around the workbook allocation budget.
                            let retained_cost = retained_cell_cost(&entry);
                            if retained_cost > *budget {
                                *budget = 0;
                                break;
                            }
                            *budget -= retained_cost;
                            // Coordinate sidecars follow the retained cell stream's
                            // last-write-wins contract too. Clear an earlier
                            // duplicate before installing the final cell's rich
                            // text or direct-format overlay.
                            parsed.rich.remove(&(row, col));
                            if ctype == "s" {
                                if let Some(runs) = value
                                    .trim()
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|index| shared.get(index))
                                    .map(|shared| shared.runs.clone())
                                    .filter(|runs| !runs.is_empty())
                                {
                                    parsed.rich.insert((row, col), runs);
                                }
                            } else if ctype == "inlineStr" && !inline_runs.is_empty() {
                                parsed.rich.insert((row, col), inline_runs.clone());
                            }
                            parsed.direct_cell_formats.remove(&(row, col));
                            if style_idx != 0 {
                                if let Some(overlay) = styles.cell_style_overlay(style_idx) {
                                    parsed
                                        .direct_cell_formats
                                        .insert((row, col), overlay.clone());
                                }
                            }
                            parsed.cells.push(entry);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    push_current_conditional_format(&mut parsed, current_cf.take());
    parsed
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

fn print_bool_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    metadata: &mut PrintMetadata,
) -> bool {
    match attr(e, key).as_deref() {
        Some(value) => parse_bool_attr(value).unwrap_or_else(|| {
            metadata.add_loss(PrintLossKind::UnsupportedProperty);
            false
        }),
        None => false,
    }
}

fn header_footer_bool_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    default: bool,
    metadata: &mut PrintMetadata,
) -> bool {
    match attr(e, key).as_deref() {
        Some(value) => parse_bool_attr(value).unwrap_or_else(|| {
            metadata.add_loss(PrintLossKind::MalformedHeaderFooter);
            default
        }),
        None => default,
    }
}

fn parse_sheet_protection(
    e: &quick_xml::events::BytesStart<'_>,
) -> Option<Option<ProtectionOptions>> {
    if attr(e, b"sheet").as_deref().is_some_and(attr_false) {
        return None;
    }

    let mut options = ProtectionOptions::default();
    for (key, field) in [
        (b"sort".as_slice(), &mut options.sort),
        (b"autoFilter".as_slice(), &mut options.auto_filter),
        (b"formatCells".as_slice(), &mut options.format_cells),
        (b"formatColumns".as_slice(), &mut options.format_columns),
        (b"formatRows".as_slice(), &mut options.format_rows),
        (b"insertColumns".as_slice(), &mut options.insert_columns),
        (b"insertRows".as_slice(), &mut options.insert_rows),
        (
            b"insertHyperlinks".as_slice(),
            &mut options.insert_hyperlinks,
        ),
        (b"deleteColumns".as_slice(), &mut options.delete_columns),
        (b"deleteRows".as_slice(), &mut options.delete_rows),
        (b"pivotTables".as_slice(), &mut options.pivot_tables),
    ] {
        if attr(e, key).as_deref().is_some_and(attr_false) {
            *field = true;
        }
    }

    if options == ProtectionOptions::default() {
        Some(None)
    } else {
        Some(Some(options))
    }
}

fn sheet_view_rank(e: &quick_xml::events::BytesStart<'_>) -> u8 {
    match attr(e, b"workbookViewId").as_deref() {
        Some("0") | None => 2,
        Some(_) => 1,
    }
}

fn clear_sheet_view_metadata(parsed: &mut ParsedSheet) {
    parsed.freeze = None;
    parsed.hide_gridlines = false;
    parsed.zoom = None;
    parsed.show_headers = None;
    parsed.right_to_left = false;
}

fn page_setup_mut(parsed: &mut ParsedSheet) -> &mut PageSetup {
    parsed.page_setup.get_or_insert_with(PageSetup::default)
}

fn header_footer_field(name: &[u8]) -> Option<(HeaderFooterKind, HeaderFooterField, bool)> {
    match name {
        b"oddHeader" => Some((HeaderFooterKind::OddHeader, HeaderFooterField::Header, true)),
        b"firstHeader" => Some((
            HeaderFooterKind::FirstHeader,
            HeaderFooterField::Header,
            false,
        )),
        b"evenHeader" => Some((
            HeaderFooterKind::EvenHeader,
            HeaderFooterField::Header,
            false,
        )),
        b"oddFooter" => Some((HeaderFooterKind::OddFooter, HeaderFooterField::Footer, true)),
        b"firstFooter" => Some((
            HeaderFooterKind::FirstFooter,
            HeaderFooterField::Footer,
            false,
        )),
        b"evenFooter" => Some((
            HeaderFooterKind::EvenFooter,
            HeaderFooterField::Footer,
            false,
        )),
        _ => None,
    }
}

fn begin_header_footer_capture(
    parsed: &mut ParsedSheet,
    kind: HeaderFooterKind,
    field: HeaderFooterField,
    preferred: bool,
) -> HeaderFooterCapture {
    parsed.print_metadata.set_header_footer(kind, String::new());
    let page_setup = page_setup_mut(parsed);
    let slot = match field {
        HeaderFooterField::Header => &mut page_setup.header,
        HeaderFooterField::Footer => &mut page_setup.footer,
    };
    let legacy = if preferred || slot.is_none() {
        *slot = Some(String::new());
        Some(field)
    } else {
        None
    };
    HeaderFooterCapture { kind, legacy }
}

fn append_header_footer_text(parsed: &mut ParsedSheet, capture: HeaderFooterCapture, text: &str) {
    parsed
        .print_metadata
        .append_header_footer(capture.kind, text);
    match capture.legacy {
        Some(HeaderFooterField::Header) => {
            if let Some(header) = page_setup_mut(parsed).header.as_mut() {
                header.push_str(text);
            }
        }
        Some(HeaderFooterField::Footer) => {
            if let Some(footer) = page_setup_mut(parsed).footer.as_mut() {
                footer.push_str(text);
            }
        }
        None => {}
    }
}

fn parse_manual_page_break(
    e: &quick_xml::events::BytesStart<'_>,
    axis: Option<PageBreakAxis>,
    metadata: &mut PrintMetadata,
) {
    let manual = match attr(e, b"man").as_deref() {
        Some(value) => match parse_bool_attr(value) {
            Some(value) => value,
            None => {
                metadata.add_loss(PrintLossKind::InvalidPageBreak);
                return;
            }
        },
        None => false,
    };
    if !manual {
        return;
    }
    let Some(id) = attr(e, b"id").and_then(|value| value.parse::<u32>().ok()) else {
        metadata.add_loss(PrintLossKind::InvalidPageBreak);
        return;
    };
    match axis {
        Some(PageBreakAxis::Row) => metadata.push_manual_row_break(id),
        Some(PageBreakAxis::Column) => match u16::try_from(id) {
            Ok(col) => metadata.push_manual_col_break(col),
            Err(_) => metadata.add_loss(PrintLossKind::InvalidPageBreak),
        },
        None => metadata.add_loss(PrintLossKind::InvalidPageBreak),
    }
}

fn attr_f64(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<f64> {
    attr(e, key).and_then(|s| s.parse::<f64>().ok())
}

fn attr_u16(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<u16> {
    attr(e, key).and_then(|s| s.parse::<u16>().ok())
}

fn print_fit_count_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    metadata: &mut PrintMetadata,
) -> Option<u16> {
    let value = attr(e, key)?;
    match value.parse::<u32>() {
        Ok(value) => match u16::try_from(value) {
            Ok(value) => Some(value),
            Err(_) => {
                // OOXML permits the full unsignedInt range, while the stable
                // public PageSetup model is u16. Saturation preserves a large,
                // effectively unconstrained target instead of turning it into
                // an omitted dimension, whose active-fit default is one page.
                metadata.add_loss(PrintLossKind::LimitExceeded);
                Some(u16::MAX)
            }
        },
        Err(_) => {
            metadata.add_loss(PrintLossKind::UnsupportedProperty);
            None
        }
    }
}

fn apply_sheet_defined_names<'a, I>(
    page_setup: &mut Option<PageSetup>,
    print_metadata: &mut PrintMetadata,
    autofilter: &mut Option<SheetRange>,
    names: I,
) where
    I: IntoIterator<Item = &'a SheetDefinedName>,
{
    for name in names {
        match name.name.as_str() {
            "_xlnm.Print_Area" => {
                let mut first = None;
                for part in split_defined_name_refs(&name.refers_to) {
                    if let Some(range) = parse_defined_name_range(part) {
                        first.get_or_insert(range);
                        print_metadata.push_print_area(range);
                    } else if part.contains("#REF!") {
                        print_metadata.add_loss(PrintLossKind::MissingReference);
                    } else {
                        print_metadata.add_loss(PrintLossKind::InvalidPrintArea);
                    }
                }
                if let Some(range) = first {
                    page_setup.get_or_insert_with(PageSetup::default).print_area = Some(range);
                }
            }
            "_xlnm.Print_Titles" => {
                for part in split_defined_name_refs(&name.refers_to) {
                    let body = strip_sheet_prefix(part);
                    if let Some(rows) = parse_repeat_rows(body) {
                        page_setup
                            .get_or_insert_with(PageSetup::default)
                            .repeat_rows = Some(rows);
                    } else if let Some(cols) = parse_repeat_cols(body) {
                        page_setup
                            .get_or_insert_with(PageSetup::default)
                            .repeat_cols = Some(cols);
                    }
                }
            }
            "_xlnm._FilterDatabase" => {
                if let Some(range) = parse_defined_name_range(&name.refers_to) {
                    *autofilter = Some(range);
                }
            }
            _ => {}
        }
    }
}

fn split_defined_name_refs(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_quote = false;
    let mut chars = value.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '\'' => {
                if in_quote && chars.peek().is_some_and(|(_, next)| *next == '\'') {
                    chars.next();
                } else {
                    in_quote = !in_quote;
                }
            }
            ',' if !in_quote => {
                out.push(value[start..idx].trim());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    out.push(value[start..].trim());
    out
}

fn strip_sheet_prefix(value: &str) -> &str {
    value
        .rsplit_once('!')
        .map(|(_, reference)| reference.trim())
        .unwrap_or_else(|| value.trim())
}

fn parse_defined_name_range(value: &str) -> Option<SheetRange> {
    parse_range(strip_sheet_prefix(value))
}

fn parse_repeat_rows(value: &str) -> Option<(u32, u32)> {
    let (first, last) = value.split_once(':')?;
    let first = parse_one_based_row(first)?;
    let last = parse_one_based_row(last)?;
    Some((first.min(last), first.max(last)))
}

fn parse_one_based_row(value: &str) -> Option<u32> {
    let row = value.trim().trim_start_matches('$').parse::<u32>().ok()?;
    if !(1..=1_048_576).contains(&row) {
        return None;
    }
    Some(row - 1)
}

fn parse_repeat_cols(value: &str) -> Option<(u16, u16)> {
    let (first, last) = value.split_once(':')?;
    let first = parse_col_ref(first)?;
    let last = parse_col_ref(last)?;
    Some((first.min(last), first.max(last)))
}

fn parse_col_ref(value: &str) -> Option<u16> {
    let letters: Vec<char> = value
        .trim()
        .trim_start_matches('$')
        .chars()
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let col = letters_col(&letters)?;
    (col <= 16_383).then(|| u16::try_from(col).ok()).flatten()
}

fn parse_sparkline_kind(value: &str) -> SparklineKind {
    match value {
        "column" => SparklineKind::Column,
        "stacked" => SparklineKind::WinLoss,
        _ => SparklineKind::Line,
    }
}

fn parse_sparkline(kind: SparklineKind, range: &str, location: &str) -> Option<Sparkline> {
    let range = range.trim();
    if range.is_empty() {
        return None;
    }
    let (row, col) = location.split_whitespace().next().and_then(parse_ref)?;
    Some(Sparkline {
        location: (row, col),
        range: range.to_string(),
        kind,
    })
}

fn parse_conditional_metadata(
    e: &quick_xml::events::BytesStart<'_>,
    styles: &Styles,
) -> ConditionalFormatMetadata {
    let mut metadata = ConditionalFormatMetadata {
        priority: attr(e, b"priority")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|priority| *priority != 0),
        stop_if_true: attr(e, b"stopIfTrue").as_deref().is_some_and(attr_true),
        ..ConditionalFormatMetadata::default()
    };
    let Some(dxf_id) = attr(e, b"dxfId") else {
        return metadata;
    };
    let Some(dxf) = dxf_id
        .parse::<usize>()
        .ok()
        .and_then(|id| styles.differential_style(id))
    else {
        metadata.style_losses.push(StyleLoss {
            kind: StyleLossKind::MissingReference,
            occurrences: 1,
        });
        return metadata;
    };
    metadata.differential_style = Some(dxf.style.clone());
    metadata.style_losses = dxf.losses.clone();
    metadata
}

fn conditional_compatibility_fill(metadata: &ConditionalFormatMetadata) -> Color {
    metadata
        .differential_style
        .as_ref()
        .and_then(|style| {
            style.fill.or_else(|| {
                style.pattern_fill.and_then(|fill| {
                    (fill.pattern == FormatPattern::Solid)
                        .then(|| fill.foreground.or(fill.background))
                        .flatten()
                })
            })
        })
        .unwrap_or_default()
}

fn parse_conditional_rule(
    e: &quick_xml::events::BytesStart<'_>,
    ranges: &[SheetRange],
    styles: &Styles,
) -> Option<PendingCfRule> {
    if ranges.is_empty() {
        return None;
    }
    let ty = attr(e, b"type")?;
    let metadata = parse_conditional_metadata(e, styles);
    let compatibility_fill = conditional_compatibility_fill(&metadata);
    let kind = match ty.as_str() {
        "cellIs" => PendingCfKind::CellIs {
            op: attr(e, b"operator")
                .as_deref()
                .and_then(parse_dv_op)
                .unwrap_or(DvOp::Between),
            fill: compatibility_fill,
        },
        "colorScale" => PendingCfKind::ColorScale,
        "dataBar" => PendingCfKind::DataBar,
        "top10" => PendingCfKind::TopBottom {
            rank: attr(e, b"rank")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(10),
            bottom: attr(e, b"bottom").as_deref().is_some_and(attr_true),
            percent: attr(e, b"percent").as_deref().is_some_and(attr_true),
            fill: compatibility_fill,
        },
        "aboveAverage" => PendingCfKind::AboveAverage {
            below: attr(e, b"aboveAverage").as_deref().is_some_and(attr_false),
            fill: compatibility_fill,
        },
        "duplicateValues" => PendingCfKind::DuplicateValues {
            unique: false,
            fill: compatibility_fill,
        },
        "uniqueValues" => PendingCfKind::DuplicateValues {
            unique: true,
            fill: compatibility_fill,
        },
        "expression" => PendingCfKind::Expression {
            fill: compatibility_fill,
        },
        _ => return None,
    };
    Some(PendingCfRule {
        ranges: ranges.to_vec(),
        kind,
        formulas: Vec::new(),
        colors: Vec::new(),
        metadata,
    })
}

fn push_current_conditional_format(parsed: &mut ParsedSheet, current: Option<PendingCfRule>) {
    let Some(current) = current else {
        return;
    };
    let Some(rule) = current.build_rule() else {
        return;
    };
    for sqref in current.ranges.into_iter().take(1 << 16) {
        parsed.cond_formats.push(CondFormat {
            sqref,
            rule: rule.clone(),
        });
        parsed.cond_format_metadata.push(current.metadata.clone());
    }
}

fn parse_data_validation(e: &quick_xml::events::BytesStart<'_>) -> Option<ParsedDataValidation> {
    let ranges: Vec<_> = attr(e, b"sqref")?
        .split_whitespace()
        .filter_map(parse_range)
        .collect();
    let (&sqref, rest) = ranges.split_first()?;
    let kind = attr(e, b"type").as_deref().and_then(parse_dv_kind)?;
    let operator = attr(e, b"operator")
        .as_deref()
        .and_then(parse_dv_op)
        .unwrap_or(DvOp::Between);
    let allow_blank = attr(e, b"allowBlank")
        .as_deref()
        .map(attr_true)
        .unwrap_or(false);
    let show_input_message = attr(e, b"showInputMessage")
        .as_deref()
        .map(attr_true)
        .unwrap_or(false);
    let show_error_message = attr(e, b"showErrorMessage")
        .as_deref()
        .map(attr_true)
        .unwrap_or(false);
    let prompt = match (attr(e, b"promptTitle"), attr(e, b"prompt")) {
        (None, None) => None,
        (title, message) => Some((title.unwrap_or_default(), message.unwrap_or_default())),
    };
    let error = match (attr(e, b"errorTitle"), attr(e, b"error")) {
        (None, None) => None,
        (title, message) => Some((title.unwrap_or_default(), message.unwrap_or_default())),
    };
    Some((
        DataValidation {
            sqref,
            kind,
            operator,
            formula1: String::new(),
            formula2: None,
            allow_blank,
            show_input_message,
            show_error_message,
            prompt,
            error,
        },
        rest.to_vec(),
    ))
}

fn push_current_data_validation(
    parsed: &mut ParsedSheet,
    current: Option<DataValidation>,
    extra_ranges: &mut Vec<SheetRange>,
) {
    let Some(mut dv) = current else {
        extra_ranges.clear();
        return;
    };
    if dv.formula1.is_empty() {
        extra_ranges.clear();
        return;
    }
    if dv.formula2.as_deref() == Some("") {
        dv.formula2 = None;
    }
    parsed.data_validations.push(dv.clone());
    for sqref in extra_ranges.drain(..) {
        let mut clone = dv.clone();
        clone.sqref = sqref;
        parsed.data_validations.push(clone);
    }
}

fn parse_dv_kind(value: &str) -> Option<DvKind> {
    match value {
        "list" => Some(DvKind::List),
        "whole" => Some(DvKind::Whole),
        "decimal" => Some(DvKind::Decimal),
        "date" => Some(DvKind::Date),
        "time" => Some(DvKind::Time),
        "textLength" => Some(DvKind::TextLength),
        "custom" => Some(DvKind::Custom),
        _ => None,
    }
}

fn parse_dv_op(value: &str) -> Option<DvOp> {
    match value {
        "between" => Some(DvOp::Between),
        "notBetween" => Some(DvOp::NotBetween),
        "equal" => Some(DvOp::Equal),
        "notEqual" => Some(DvOp::NotEqual),
        "greaterThan" => Some(DvOp::GreaterThan),
        "lessThan" => Some(DvOp::LessThan),
        "greaterThanOrEqual" => Some(DvOp::GreaterThanOrEqual),
        "lessThanOrEqual" => Some(DvOp::LessThanOrEqual),
        _ => None,
    }
}

fn parse_split_u32(value: &str) -> Option<u32> {
    let n = value.parse::<f64>().ok()?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > f64::from(u32::MAX) {
        return None;
    }
    Some(n as u32)
}

fn parse_split_u16(value: &str) -> Option<u16> {
    u16::try_from(parse_split_u32(value)?).ok()
}

#[allow(clippy::too_many_arguments)]
fn build_cell(
    row: u32,
    col: u16,
    ctype: &str,
    style_idx: usize,
    value: &str,
    formula: &str,
    shared: &[SharedString],
    styles: &Styles,
    date1904: bool,
) -> Option<CellEntry> {
    // The cached value (the displayed result), if one is present and parseable.
    let cached: Option<(Cell, String)> = match ctype {
        "s" => value
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|idx| shared.get(idx))
            .map(|shared| {
                (
                    Cell::Text(shared.text.clone()),
                    styles.render_text(style_idx, &shared.text),
                )
            }),
        "str" | "inlineStr" if !value.is_empty() => Some((
            Cell::Text(value.to_string()),
            styles.render_text(style_idx, value),
        )),
        "b" if !value.trim().is_empty() => {
            let b = value.trim() == "1";
            Some((Cell::Bool(b), if b { "TRUE" } else { "FALSE" }.to_string()))
        }
        "e" if !value.is_empty() => Some((Cell::Error(value.to_string()), value.to_string())),
        // ISO-8601 date/time cell (`t="d"`, emitted by some non-Excel writers).
        "d" if !value.is_empty() => format::iso_date_to_serial(value).map(|serial| {
            let kind = styles.kind(style_idx);
            let display = if let Some(code) = styles.custom_format(style_idx) {
                format::render_format(serial, code, false)
            } else if kind.is_datetime() {
                format::render_value(serial, kind, false)
            } else {
                value.to_string()
            };
            (Cell::Date(serial), display)
        }),
        "str" | "inlineStr" | "b" | "e" | "d" => None,
        // "" or "n" → number.
        _ => value.trim().parse::<f64>().ok().map(|f| {
            let kind = styles.kind(style_idx);
            let display = styles.custom_format(style_idx).map_or_else(
                || format::render_indexed(f, styles.format_id(style_idx), date1904),
                |code| format::render_format(f, code, date1904),
            );
            let cell = if kind.is_datetime() {
                Cell::Date(f)
            } else {
                Cell::Number(f)
            };
            (cell, display)
        }),
    };

    // A `<f>` makes this a formula cell: surface the formula source via
    // `Cell::Formula` even when no cached value is present (an uncalculated
    // formula), keeping the cached value as the display text when there is one.
    let (value, text) = match (cached, formula.is_empty()) {
        (Some((cell, text)), true) => (cell, text),
        (Some((cell, text)), false) => (
            Cell::Formula {
                formula: formula.to_string(),
                cached: Box::new(cell),
            },
            text,
        ),
        (None, false) => (
            Cell::Formula {
                formula: formula.to_string(),
                cached: Box::new(Cell::Text(String::new())),
            },
            String::new(),
        ),
        (None, true) => return None,
    };
    Some(CellEntry {
        row,
        col,
        value,
        text,
        style: styles.cell_style(style_idx).cloned(),
        xlsx_font_size_pt: styles.xlsx_cell_font_size_pt(style_idx),
        hyperlink: None,
    })
}

// Architecture-neutral retained-allocation charges. The record allowance is
// deliberately 256 bytes: it exceeds the current 64-bit `CellEntry` layout and
// still permits one million short numeric cells within the 256 MiB workbook
// budget. The boxed-cell allowance likewise exceeds the current 64-bit `Cell`
// layout plus allocator bookkeeping. Compile-time assertions keep future model
// growth from silently invalidating those conservative bounds on any target.
const RETAINED_CELL_RECORD_BYTES: usize = 256;
const RETAINED_BOXED_CELL_BYTES: usize = 64;
const _: () = assert!(std::mem::size_of::<CellEntry>() <= RETAINED_CELL_RECORD_BYTES);
const _: () = assert!(std::mem::size_of::<Cell>() <= RETAINED_BOXED_CELL_BYTES);

fn retained_cell_cost(entry: &CellEntry) -> usize {
    RETAINED_CELL_RECORD_BYTES
        .saturating_add(entry.text.len())
        .saturating_add(retained_cell_value_heap_bytes(&entry.value))
        // Direct OOXML formats are retained both on the cell and in the
        // cell-XF overlay map. Charge both variable string copies; the fixed
        // record allowance covers their non-allocating structures.
        .saturating_add(retained_cell_style_heap_bytes(entry.style.as_ref()).saturating_mul(2))
}

fn retained_cell_value_heap_bytes(mut value: &Cell) -> usize {
    let mut bytes = 0usize;
    loop {
        match value {
            Cell::Text(source) | Cell::Error(source) => {
                return bytes.saturating_add(source.len());
            }
            Cell::Formula { formula, cached } => {
                bytes = bytes
                    .saturating_add(formula.len())
                    .saturating_add(RETAINED_BOXED_CELL_BYTES);
                value = cached;
            }
            Cell::Number(_) | Cell::Date(_) | Cell::Bool(_) => return bytes,
        }
    }
}

fn retained_cell_style_heap_bytes(style: Option<&CellStyle>) -> usize {
    style.map_or(0, |style| {
        style
            .font
            .as_ref()
            .and_then(|font| font.name.as_deref())
            .map(str::len)
            .unwrap_or(0)
            .saturating_add(style.num_fmt.as_deref().map(str::len).unwrap_or(0))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_theme_xml(
        major_latin: &str,
        minor_latin: &str,
        accent1: &str,
        accent2: &str,
    ) -> String {
        format!(
            r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:accent1><a:srgbClr val="{accent1}"/></a:accent1><a:accent2><a:srgbClr val="{accent2}"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme><a:majorFont><a:latin typeface="{major_latin}"/></a:majorFont><a:minorFont><a:latin typeface="{minor_latin}"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#
        )
    }

    fn overlay_is_empty(overlay: &CellStyleOverlay) -> bool {
        !overlay.replace_font
            && !overlay.replace_fill
            && !overlay.replace_border
            && !overlay.replace_num_fmt
            && !overlay.replace_alignment
            && !overlay.replace_protection
    }

    fn shared_texts(xml: &str) -> Vec<String> {
        parse_shared_strings(xml, &ThemeColors::default(), &[])
            .into_iter()
            .map(|shared| shared.text)
            .collect()
    }

    fn workbook_with_worksheet_xml(worksheet: &str) -> Workbook {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            ("xl/worksheets/sheet1.xml", worksheet),
        ];
        for (name, body) in parts {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        Workbook::open(&zip.finish().unwrap().into_inner()).unwrap()
    }

    #[test]
    fn relationship_selection_is_exact_deterministic_and_internal_only() {
        let transitional = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="drawings/drawing1.xml"/></Relationships>"#;
        let strict = r#"<Relationships xmlns="http://purl.oclc.org/ooxml/package/relationships"><Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/drawing" Target="drawings/drawing1.xml"/></Relationships>"#;
        assert_eq!(
            unique_internal_relationship_target(transitional, "drawing"),
            RelationshipTarget::Internal("drawings/drawing1.xml".to_string())
        );
        assert_eq!(
            unique_internal_relationship_target(strict, "drawing"),
            RelationshipTarget::Internal("drawings/drawing1.xml".to_string())
        );

        let attacker_type = r#"<Relationships><Relationship Id="rId1" Type="https://attacker.invalid/officeDocument/2006/relationships/drawing" Target="evil.xml"/></Relationships>"#;
        assert_eq!(
            unique_internal_relationship_target(attacker_type, "drawing"),
            RelationshipTarget::Missing
        );

        let duplicate_id = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="a.xml"/><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="b.xml"/></Relationships>"#;
        assert_eq!(
            unique_internal_relationship_target(duplicate_id, "drawing"),
            RelationshipTarget::Invalid
        );
        assert!(parse_rels(duplicate_id).is_empty());

        let external = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="https://example.invalid/drawing.xml" TargetMode="External"/></Relationships>"#;
        assert_eq!(
            unique_internal_relationship_target(external, "drawing"),
            RelationshipTarget::Invalid
        );

        let foreign_namespace = r#"<Relationships xmlns="https://attacker.invalid/package/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="evil.xml"/></Relationships>"#;
        assert_eq!(
            unique_internal_relationship_target(foreign_namespace, "drawing"),
            RelationshipTarget::Invalid
        );

        let explicitly_closed = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="drawings/drawing1.xml"></Relationship></Relationships>"#;
        assert_eq!(
            unique_internal_relationship_target(explicitly_closed, "drawing"),
            RelationshipTarget::Internal("drawings/drawing1.xml".to_string())
        );

        let child_content = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="evil.xml"><extension/></Relationship></Relationships>"#;
        assert_eq!(
            unique_internal_relationship_target(child_content, "drawing"),
            RelationshipTarget::Invalid
        );

        let text_content = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="evil.xml">content</Relationship></Relationships>"#;
        assert_eq!(
            unique_internal_relationship_target(text_content, "drawing"),
            RelationshipTarget::Invalid
        );
    }

    #[test]
    fn relationship_extensions_do_not_weaken_core_attribute_validation() {
        let extended = r#"<Relationships xmlns:ext="urn:producer:relationships" ext:producer="example"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="drawings/drawing1.xml" ext:metadata="kept-by-package-editor"/></Relationships>"#;
        assert_eq!(
            unique_internal_relationship_target(extended, "drawing"),
            RelationshipTarget::Internal("drawings/drawing1.xml".to_string())
        );

        for malformed in [
            r#"<Relationships xmlns:ext="urn:producer:relationships"><Relationship ext:Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="drawings/drawing1.xml"/></Relationships>"#,
            r#"<Relationships xmlns:ext="urn:producer:relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" ext:Target="drawings/drawing1.xml"/></Relationships>"#,
            r#"<Relationships xmlns:ext="urn:producer:relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="drawings/drawing1.xml" TargetMode="invalid" ext:TargetMode="External"/></Relationships>"#,
        ] {
            assert_eq!(
                unique_internal_relationship_target(malformed, "drawing"),
                RelationshipTarget::Invalid
            );
        }
    }

    #[test]
    fn drawing_relationship_ids_require_the_exact_internal_object_type() {
        let xml = r#"<Relationships><Relationship Id="chart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/><Relationship Id="image" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/><Relationship Id="external" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="https://example.invalid/chart.xml" TargetMode="External"/></Relationships>"#;
        let relationships = parse_ooxml_relationships(xml).expect("valid relationship part");
        assert!(matches!(
            internal_relationship_target_by_id(&relationships, "chart", "chart"),
            RelationshipTarget::Internal(target) if target == "../charts/chart1.xml"
        ));
        assert_eq!(
            internal_relationship_target_by_id(&relationships, "image", "chart"),
            RelationshipTarget::Invalid
        );
        assert_eq!(
            internal_relationship_target_by_id(&relationships, "external", "chart"),
            RelationshipTarget::Invalid
        );
    }

    #[test]
    fn chart_count_and_xml_work_budgets_are_shared_across_sheets() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        const CHART_XML: &str = r#"<chartSpace xmlns="http://schemas.openxmlformats.org/drawingml/2006/chart"><chart><plotArea><lineChart><grouping val="standard"/><varyColors val="0"/><axId val="1"/><axId val="2"/></lineChart><catAx><axId val="1"/><crossAx val="2"/></catAx><valAx><axId val="2"/><crossAx val="1"/></valAx></plotArea></chart></chartSpace>"#;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for index in 1..=2 {
            writer
                .start_file(format!("xl/drawings/drawing{index}.xml"), options)
                .unwrap();
            writer
                .write_all(
                    format!(r#"<wsDr><twoCellAnchor><from><col>0</col><row>0</row></from><to><col>4</col><row>8</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart{index}"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#).as_bytes(),
                )
                .unwrap();
            writer
                .start_file(
                    format!("xl/drawings/_rels/drawing{index}.xml.rels"),
                    options,
                )
                .unwrap();
            writer
                .write_all(
                    format!(r#"<Relationships><Relationship Id="rIdChart{index}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart{index}.xml"/></Relationships>"#).as_bytes(),
                )
                .unwrap();
            writer
                .start_file(format!("xl/charts/chart{index}.xml"), options)
                .unwrap();
            writer.write_all(CHART_XML.as_bytes()).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
        let sheet_rels = |index| {
            format!(
                r#"<Relationships><Relationship Id="rIdDraw{index}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing{index}.xml"/></Relationships>"#
            )
        };

        let mut count_budget = ChartImportBudget {
            charts_remaining: 1,
            ..ChartImportBudget::default()
        };
        let first = read_sheet_drawings(
            &mut zip,
            "xl/worksheets/sheet1.xml",
            Some(&sheet_rels(1)),
            &ThemeColors::default(),
            &mut count_budget,
        );
        let second = read_sheet_drawings(
            &mut zip,
            "xl/worksheets/sheet2.xml",
            Some(&sheet_rels(2)),
            &ThemeColors::default(),
            &mut count_budget,
        );
        assert_eq!(first.1.len(), 1);
        assert!(second.1.is_empty());
        assert!(second
            .3
            .iter()
            .any(|loss| loss.kind == StyleLossKind::LimitExceeded));

        let chart_work = CHART_XML.len() * XLSX_CHART_XML_SCAN_PASSES;
        let mut work_budget = ChartImportBudget {
            charts_remaining: 2,
            xml_work_remaining: chart_work,
            xml_work_limit: chart_work,
            ..ChartImportBudget::default()
        };
        let first = read_sheet_drawings(
            &mut zip,
            "xl/worksheets/sheet1.xml",
            Some(&sheet_rels(1)),
            &ThemeColors::default(),
            &mut work_budget,
        );
        let second = read_sheet_drawings(
            &mut zip,
            "xl/worksheets/sheet2.xml",
            Some(&sheet_rels(2)),
            &ThemeColors::default(),
            &mut work_budget,
        );
        assert_eq!(first.1.len(), 1);
        assert!(second.1.is_empty());
        assert!(second
            .3
            .iter()
            .any(|loss| loss.kind == StyleLossKind::LimitExceeded));
    }

    #[test]
    fn cell_ref_parsing() {
        assert_eq!(parse_ref("A1"), Some((0, 0)));
        assert_eq!(parse_ref("B2"), Some((1, 1)));
        assert_eq!(parse_ref("Z1"), Some((0, 25)));
        assert_eq!(parse_ref("AA1"), Some((0, 26)));
        assert_eq!(parse_ref("XFD1048576"), Some((1_048_575, 16_383))); // Excel max
        assert_eq!(parse_ref("A"), None);
        assert_eq!(parse_ref("XFE1"), None); // past the last column
        assert_eq!(parse_ref("ZZZZZZZ1"), None); // overflow → None, NOT a panic
    }

    #[test]
    fn shared_strings_concatenate_runs() {
        let xml = r#"<sst><si><t>Hello</t></si><si><r><rPr><b/><color rgb="FF112233"/></rPr><t>가</t></r><r><rPr><i/></rPr><t>나</t></r></si></sst>"#;
        assert_eq!(shared_texts(xml), vec!["Hello", "가나"]);
        let parsed = parse_shared_strings(xml, &ThemeColors::default(), &[]);
        assert_eq!(parsed[1].runs.len(), 2);
        assert!(parsed[1].runs[0].font.bold);
        assert_eq!(
            parsed[1].runs[0].font.color,
            Some(Color::rgb(0x11, 0x22, 0x33))
        );
        assert!(parsed[1].runs[1].font.italic);
    }

    #[test]
    fn general_refs_are_reassembled_across_xlsx_text_surfaces() {
        assert_eq!(
            shared_texts("<sst><si><t>A&amp;B&#33;</t></si></sst>"),
            vec!["A&B!"]
        );

        let props = parse_doc_properties(
            Some("<coreProperties><title>A&amp;B&#33;</title></coreProperties>"),
            None,
        );
        assert_eq!(props.title.as_deref(), Some("A&B!"));

        let comments = parse_comments(
            r#"<comments><authors><author>R&amp;D</author></authors><commentList><comment ref="A1" authorId="0"><text><t>Check &lt;now&gt;</t></text></comment></commentList></comments>"#,
        );
        assert_eq!(comments[0].author.as_deref(), Some("R&D"));
        assert_eq!(comments[0].text, "Check <now>");

        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>A&amp;B&#33;</t></is></c><c r="B1"><f>A1&amp;"!"</f><v>1&#48;</v></c></row></sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        assert_eq!(cells[0].value, Cell::Text("A&B!".to_string()));
        match &cells[1].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "A1&\"!\"");
                assert_eq!(**cached, Cell::Number(10.0));
            }
            other => panic!("expected formula cell, got {other:?}"),
        }
    }

    #[test]
    fn unknown_and_illegal_general_refs_are_preserved_lexically_on_read() {
        assert_eq!(
            shared_texts("<sst><si><t>A&bogus;&#x1;</t></si></sst>"),
            vec!["A&bogus;&#x1;"]
        );
    }

    #[test]
    fn attributes_accept_only_xml_predefined_entities() {
        let mut reader = Reader::from_str(r#"<x value="a&nbsp;b"/>"#);
        let Event::Empty(element) = reader.read_event().unwrap() else {
            panic!("expected empty element");
        };
        assert_eq!(attr(&element, b"value"), None);
    }

    #[test]
    fn general_refs_are_reassembled_in_drawing_coordinates_and_chart_refs() {
        let drawing = r#"<wsDr><twoCellAnchor><from><col>1&#48;</col><row>2&#48;</row></from><to><col>3&#48;</col><row>4&#48;</row></to><graphicFrame><chart r:id="rId&amp;Chart"/></graphicFrame></twoCellAnchor></wsDr>"#;
        let refs = parse_drawing_refs(drawing);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].rid.as_deref(), Some("rId&Chart"));
        assert_eq!(refs[0].from, (20, 10));
        assert_eq!(refs[0].to, Some((40, 30)));

        let mut cache_points = 16;
        let mut chart_series = 16;
        let chart = parse_chart(
            r#"<chartSpace><chart><plotArea><lineChart><ser><tx><strRef><f>Data&amp;More!$A$1</f></strRef></tx><cat><strRef><f>Data!$A$2:$A$3</f></strRef></cat><val><numRef><f>Data!$B$2:$B$3</f></numRef></val></ser></lineChart></plotArea></chart></chartSpace>"#,
            (20, 10),
            (40, 30),
            &mut cache_points,
            &mut chart_series,
        )
        .unwrap()
        .chart;
        assert_eq!(chart.series[0].name.as_deref(), Some("Data&More!$A$1"));
        assert_eq!(
            chart.series[0].categories.as_deref(),
            Some("Data!$A$2:$A$3")
        );
        assert_eq!(chart.series[0].values, "Data!$B$2:$B$3");
    }

    #[test]
    fn drawing_sidecars_retain_all_anchor_geometry_and_unsupported_shapes() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let drawing = r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"
                xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
                xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
            <xdr:twoCellAnchor editAs="oneCell">
                <xdr:from><xdr:col>1</xdr:col><xdr:colOff>123</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>456</xdr:rowOff></xdr:from>
                <xdr:to><xdr:col>4</xdr:col><xdr:colOff>789</xdr:colOff><xdr:row>5</xdr:row><xdr:rowOff>1011</xdr:rowOff></xdr:to>
                <xdr:pic>
                    <xdr:nvPicPr><xdr:cNvPr id="2" name="Logo &amp; mark" descr="Accessible logo"/></xdr:nvPicPr>
                    <xdr:blipFill><a:blip r:embed="rIdImage"/><a:srcRect l="1000" t="2000" r="3000" b="4000"/></xdr:blipFill>
                    <xdr:spPr><a:xfrm rot="60000"><a:ext cx="914400" cy="457200"/></a:xfrm></xdr:spPr>
                </xdr:pic>
            </xdr:twoCellAnchor>
            <xdr:oneCellAnchor>
                <xdr:from><xdr:col>6</xdr:col><xdr:colOff>-5</xdr:colOff><xdr:row>7</xdr:row><xdr:rowOff>6</xdr:rowOff></xdr:from>
                <xdr:ext cx="1828800" cy="914400"/>
                <xdr:graphicFrame>
                    <xdr:nvGraphicFramePr><xdr:cNvPr id="3" name="Sales chart" title="Chart fallback text"/></xdr:nvGraphicFramePr>
                    <a:graphic><a:graphicData><c:chart r:id="rIdChart"/></a:graphicData></a:graphic>
                </xdr:graphicFrame>
            </xdr:oneCellAnchor>
            <xdr:absoluteAnchor>
                <xdr:pos x="1234" y="5678"/><xdr:ext cx="777" cy="888"/>
                <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="4" name="Callout" descr="Unsupported callout"/></xdr:nvSpPr>
                    <xdr:spPr><a:xfrm rot="-120000"/></xdr:spPr>
                </xdr:sp>
            </xdr:absoluteAnchor>
        </xdr:wsDr>"#;
        let parts = [
            (
                "xl/workbook.xml",
                br#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#.as_slice(),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#.as_slice(),
            ),
            (
                "xl/worksheets/sheet1.xml",
                br#"<worksheet><sheetData/><drawing r:id="rIdDrawing"/></worksheet>"#.as_slice(),
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                br#"<Relationships><Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#.as_slice(),
            ),
            ("xl/drawings/drawing1.xml", drawing.as_bytes()),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                br#"<Relationships><Relationship Id="rIdImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#.as_slice(),
            ),
            (
                "xl/charts/chart1.xml",
                br#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea><c:lineChart><c:axId val="1"/><c:axId val="2"/></c:lineChart><c:catAx><c:axId val="1"/><c:crossAx val="2"/></c:catAx><c:valAx><c:axId val="2"/><c:crossAx val="1"/></c:valAx></c:plotArea></c:chart></c:chartSpace>"#.as_slice(),
            ),
            ("xl/media/image1.png", b"\x89PNG\r\n\x1a\n".as_slice()),
        ];
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in parts {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();

        let workbook = Workbook::open(&bytes).unwrap();
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.images().len(), 1);
        assert_eq!(sheet.images()[0].from, (2, 1));
        assert_eq!(sheet.images()[0].to, Some((5, 4)));
        assert_eq!(sheet.charts().len(), 1);
        assert_eq!(sheet.charts()[0].from, (7, 6));
        assert_eq!(sheet.charts()[0].to, (7, 6));

        let metadata = sheet.drawing_metadata();
        assert_eq!(metadata.len(), 3);
        assert_eq!(metadata[0].kind, DrawingObjectKind::Image);
        assert_eq!(metadata[0].object_index, 0);
        assert_eq!(metadata[0].from_cell, Some((2, 1)));
        assert_eq!(metadata[0].to_cell, Some((5, 4)));
        assert_eq!(metadata[0].from_offset_emu, Some((123, 456)));
        assert_eq!(metadata[0].to_offset_emu, Some((789, 1011)));
        assert_eq!(metadata[0].absolute_size_emu, Some((914400, 457200)));
        assert_eq!(
            metadata[0].crop,
            Some(DrawingCrop {
                left_ppm: 10_000,
                top_ppm: 20_000,
                right_ppm: 30_000,
                bottom_ppm: 40_000,
            })
        );
        assert_eq!(metadata[0].rotation_mdeg, Some(1000));
        assert_eq!(metadata[0].z_order, Some(0));
        assert_eq!(metadata[0].name.as_deref(), Some("Logo & mark"));
        assert_eq!(metadata[0].alt_text.as_deref(), Some("Accessible logo"));
        assert_eq!(metadata[0].behavior, DrawingAnchorBehavior::MoveOnly);

        assert_eq!(metadata[1].kind, DrawingObjectKind::Chart);
        assert_eq!(metadata[1].object_index, 0);
        assert_eq!(metadata[1].from_cell, Some((7, 6)));
        assert_eq!(metadata[1].to_cell, None);
        assert_eq!(metadata[1].from_offset_emu, Some((-5, 6)));
        assert_eq!(metadata[1].to_offset_emu, None);
        assert_eq!(metadata[1].absolute_size_emu, Some((1_828_800, 914_400)));
        assert_eq!(metadata[1].z_order, Some(1));
        assert_eq!(metadata[1].name.as_deref(), Some("Sales chart"));
        assert_eq!(metadata[1].alt_text.as_deref(), Some("Chart fallback text"));
        assert_eq!(metadata[1].behavior, DrawingAnchorBehavior::MoveOnly);

        assert_eq!(metadata[2].kind, DrawingObjectKind::Shape);
        assert_eq!(metadata[2].from_cell, None);
        assert_eq!(metadata[2].to_cell, None);
        assert_eq!(metadata[2].from_offset_emu, Some((1234, 5678)));
        assert_eq!(metadata[2].absolute_size_emu, Some((777, 888)));
        assert_eq!(metadata[2].rotation_mdeg, Some(-2000));
        assert_eq!(metadata[2].z_order, Some(2));
        assert_eq!(metadata[2].name.as_deref(), Some("Callout"));
        assert_eq!(metadata[2].alt_text.as_deref(), Some("Unsupported callout"));
        assert_eq!(metadata[2].behavior, DrawingAnchorBehavior::Absolute);
        assert_eq!(
            sheet.style_losses(),
            &[StyleLoss {
                kind: StyleLossKind::UnsupportedProperty,
                occurrences: 1,
            }]
        );
    }

    #[test]
    fn drawing_sidecar_strings_are_utf8_bounded_and_loss_aware() {
        let long_name = format!("{}한", "a".repeat(MAX_XLSX_DRAWING_TEXT));
        let xml = format!(
            r#"<wsDr><absoluteAnchor><pos x="1" y="2"/><ext cx="3" cy="4"/><sp><nvSpPr><cNvPr name="{long_name}"/></nvSpPr></sp></absoluteAnchor></wsDr>"#
        );
        let mut losses = Vec::new();
        let refs = parse_drawing_refs_bounded(&xml, &mut losses);

        assert_eq!(refs.len(), 1);
        let name = refs[0].metadata.name.as_deref().unwrap();
        assert_eq!(name.len(), MAX_XLSX_DRAWING_TEXT);
        assert!(name.is_char_boundary(name.len()));
        assert_eq!(
            losses,
            vec![StyleLoss {
                kind: StyleLossKind::LimitExceeded,
                occurrences: 1,
            }]
        );
    }

    #[test]
    fn drawing_anchor_behavior_matrix_and_zero_offsets_are_exact() {
        let cases = [
            (None, DrawingAnchorBehavior::MoveAndSize),
            (Some("twoCell"), DrawingAnchorBehavior::MoveAndSize),
            (Some("oneCell"), DrawingAnchorBehavior::MoveOnly),
            (Some("absolute"), DrawingAnchorBehavior::Absolute),
        ];
        for (edit_as, expected) in cases {
            let edit_as = edit_as
                .map(|value| format!(r#" editAs="{value}""#))
                .unwrap_or_default();
            let xml = format!(
                r#"<wsDr><twoCellAnchor{edit_as}><from><col>0</col><colOff>0</colOff><row>0</row><rowOff>0</rowOff></from><to><col>1</col><colOff>0</colOff><row>1</row><rowOff>0</rowOff></to><sp/></twoCellAnchor></wsDr>"#
            );
            let refs = parse_drawing_refs(&xml);
            assert_eq!(refs.len(), 1);
            assert_eq!(refs[0].metadata.behavior, expected);
            assert_eq!(refs[0].metadata.from_offset_emu, Some((0, 0)));
            assert_eq!(refs[0].metadata.to_offset_emu, Some((0, 0)));
        }

        let one_cell = parse_drawing_refs(
            "<wsDr><oneCellAnchor><from><col>0</col><row>0</row></from><sp/></oneCellAnchor></wsDr>",
        );
        assert_eq!(
            one_cell[0].metadata.behavior,
            DrawingAnchorBehavior::MoveOnly
        );
        let absolute = parse_drawing_refs(
            "<wsDr><absoluteAnchor><pos x=\"0\" y=\"0\"/><ext cx=\"1\" cy=\"1\"/><sp/></absoluteAnchor></wsDr>",
        );
        assert_eq!(
            absolute[0].metadata.behavior,
            DrawingAnchorBehavior::Absolute
        );
    }

    #[test]
    fn drawing_anchor_count_is_bounded_and_reports_the_limit() {
        let anchor =
            "<absoluteAnchor><pos x=\"0\" y=\"0\"/><ext cx=\"1\" cy=\"1\"/><sp/></absoluteAnchor>";
        let xml = format!("<wsDr>{}</wsDr>", anchor.repeat(MAX_XLSX_DRAWINGS + 1));
        let mut losses = Vec::new();
        let refs = parse_drawing_refs_bounded(&xml, &mut losses);

        assert_eq!(refs.len(), MAX_XLSX_DRAWINGS);
        assert_eq!(
            losses,
            vec![StyleLoss {
                kind: StyleLossKind::LimitExceeded,
                occurrences: 1,
            }]
        );
    }

    #[test]
    fn shared_strings_keep_empty_slots() {
        // A self-closing <si/> and an empty <si></si> must each occupy an index,
        // so later references don't shift.
        let xml = r#"<sst><si><t>품목</t></si><si/><si></si><si><t>가격</t></si></sst>"#;
        assert_eq!(shared_texts(xml), vec!["품목", "", "", "가격"]);
    }

    #[test]
    fn implicit_cell_positions() {
        // No `r` on <row>/<c>: position is implicit (col by order, row by order).
        // Some writers (LibreOffice, EPPlus) emit this; every cell would be lost
        // without implicit-position tracking.
        let xml = "<worksheet><sheetData>\
            <row><c t=\"inlineStr\"><is><t>A</t></is></c><c t=\"inlineStr\"><is><t>B</t></is></c></row>\
            <row><c t=\"inlineStr\"><is><t>C</t></is></c></row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        let got: Vec<_> = cells
            .iter()
            .map(|c| (c.row, c.col, c.text.as_str()))
            .collect();
        assert_eq!(got, vec![(0, 0, "A"), (0, 1, "B"), (1, 0, "C")]);
    }

    #[test]
    fn mixed_explicit_and_implicit_positions() {
        // An explicit `r` resyncs the running position; following r-less cells
        // continue from there.
        let xml = "<worksheet><sheetData>\
            <row r=\"5\"><c r=\"C5\" t=\"inlineStr\"><is><t>X</t></is></c>\
            <c t=\"inlineStr\"><is><t>Y</t></is></c></row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        let got: Vec<_> = cells
            .iter()
            .map(|c| (c.row, c.col, c.text.as_str()))
            .collect();
        assert_eq!(got, vec![(4, 2, "X"), (4, 3, "Y")]);
    }

    #[test]
    fn inline_string_with_cached_value_uses_inline_text_not_concatenation() {
        let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\" t=\"inlineStr\"><v>1.0</v><is><t>1.</t></is></c>\
            </row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;

        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].value, Cell::Text("1.".to_string()));
        assert_eq!(cells[0].text, "1.");
    }

    #[test]
    fn text_budget_caps_shared_string_amplification() {
        // The shared-string DoS: one large pooled string referenced by very many
        // cells. Retained values, display text, and cell records must stay within
        // the budget (here, a deliberately small one).
        let shared = vec![SharedString {
            text: "X".repeat(100),
            runs: Vec::new(),
        }];
        let mut xml = String::from("<worksheet><sheetData><row>");
        for _ in 0..1000 {
            xml.push_str("<c t=\"s\"><v>0</v></c>");
        }
        xml.push_str("</row></sheetData></worksheet>");
        let initial_budget = 1_024usize;
        let mut budget = initial_budget;
        let cells = parse_sheet(
            &xml,
            &shared,
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        let total: usize = cells.iter().map(retained_cell_cost).sum();
        assert!(
            total <= initial_budget,
            "accumulated {total} bytes exceeded the {initial_budget} budget"
        );
        assert!(!cells.is_empty(), "should still extract up to the cap");
    }

    #[test]
    fn text_budget_exhaustion_leaves_zero_budget_signal() {
        let shared = vec![SharedString {
            text: "X".repeat(100),
            runs: Vec::new(),
        }];
        let xml =
            "<worksheet><sheetData><row><c t=\"s\"><v>0</v></c></row></sheetData></worksheet>";
        let mut budget = 50usize;
        let cells = parse_sheet(
            xml,
            &shared,
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;

        assert!(cells.is_empty());
        assert_eq!(budget, 0);
    }

    #[test]
    fn empty_number_formats_retain_typed_values_under_a_structural_budget() {
        // These are the two real-world cases that exposed the regression:
        // `#` hides zero, while an explicitly empty format hides every number.
        // Both cells remain semantically present even though their display text
        // is empty.
        let styles = parse_styles(
            r##"<styleSheet><numFmts count="2"><numFmt numFmtId="0" formatCode=""/><numFmt numFmtId="1" formatCode="#"/></numFmts><cellXfs count="2"><xf numFmtId="1"/><xf numFmtId="0"/></cellXfs></styleSheet>"##,
            &ThemeColors::default(),
        );
        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1" s="0" t="n"><v>0</v></c><c r="B1" s="1" t="n"><v>3</v></c></row></sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &styles,
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;

        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].value, Cell::Number(0.0));
        assert_eq!(cells[1].value, Cell::Number(3.0));
        assert_eq!(cells[0].text, "");
        assert_eq!(cells[1].text, "");

        let one_cell_xml = r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="n"><v>3</v></c></row></sheetData></worksheet>"#;
        let exact_cost = retained_cell_cost(&cells[1]);
        assert_eq!(
            exact_cost, RETAINED_CELL_RECORD_BYTES,
            "the explicitly empty format adds no variable bytes"
        );

        // The exact boundary admits the hidden value. One byte less rejects it
        // and leaves the same explicit partial-extraction signal used by other
        // text-budget exhaustion paths.
        let mut budget = exact_cost;
        let parsed = parse_sheet(
            one_cell_xml,
            &[],
            &styles,
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        assert_eq!(parsed.cells.len(), 1);
        assert_eq!(parsed.cells[0].value, Cell::Number(3.0));
        assert_eq!(budget, 0);

        let mut budget = exact_cost.saturating_sub(1);
        let parsed = parse_sheet(
            one_cell_xml,
            &[],
            &styles,
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        assert!(parsed.cells.is_empty());
        assert_eq!(budget, 0);
    }

    #[test]
    fn retained_cell_budget_keeps_ordinary_high_cell_count_sheets_complete() {
        const CELLS: usize = 65_536;
        let row = "<row><c t=\"n\"><v>1</v></c></row>";
        let mut xml = String::with_capacity(
            "<worksheet><sheetData></sheetData></worksheet>".len() + row.len() * CELLS,
        );
        xml.push_str("<worksheet><sheetData>");
        for _ in 0..CELLS {
            xml.push_str(row);
        }
        xml.push_str("</sheetData></worksheet>");

        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            &xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;

        assert_eq!(cells.len(), CELLS);
        let charged = cells
            .iter()
            .map(retained_cell_cost)
            .fold(0usize, usize::saturating_add);
        assert_eq!(budget, crate::MAX_TEXT_BYTES - charged);
        assert!(budget > 0);
    }

    /// Build a minimal `.xlsx` in memory and read it end-to-end.
    #[test]
    fn custom_number_formats_are_applied_to_xlsx_display_text() {
        let styles = parse_styles(
            r#"<styleSheet><numFmts count="3"><numFmt numFmtId="164" formatCode="[$₩-412]#,##0.00"/><numFmt numFmtId="165" formatCode="yyyy&quot;년&quot; m&quot;월&quot; d&quot;일&quot;"/><numFmt numFmtId="166" formatCode="0;[Red](0);0;&quot;값: &quot;@"/></numFmts><cellXfs count="3"><xf numFmtId="164"/><xf numFmtId="165"/><xf numFmtId="166"/></cellXfs></styleSheet>"#,
            &ThemeColors::default(),
        );
        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1" s="0"><v>1234.5</v></c><c r="B1" s="1"><v>45366</v></c><c r="C1" s="2" t="inlineStr"><is><t>한글</t></is></c></row></sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &styles,
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.cells[0].text, "₩1,234.50");
        assert_eq!(parsed.cells[1].text, "2024년 3월 15일");
        assert!(matches!(parsed.cells[1].value, Cell::Date(45_366.0)));
        assert_eq!(parsed.cells[2].text, "값: 한글");
    }

    /// Build a minimal `.xlsx` in memory and read it end-to-end.
    #[test]
    fn reads_a_minimal_xlsx() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="가격" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            ("xl/sharedStrings.xml", r#"<sst><si><t>품목</t></si></sst>"#),
            (
                "xl/styles.xml",
                r#"<styleSheet><cellXfs><xf numFmtId="0"/><xf numFmtId="14"/></cellXfs></styleSheet>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>42</v></c><c r="C1" s="1"><v>45366</v></c><c r="D1" t="b"><v>1</v></c></row></sheetData></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let s = &wb.sheets[0];
        assert_eq!(s.name, "가격");
        assert_eq!(s.cell(0, 0), Some(&Cell::Text("품목".to_string())));
        assert_eq!(s.cell(0, 1), Some(&Cell::Number(42.0)));
        assert_eq!(s.cell(0, 2), Some(&Cell::Date(45366.0))); // numFmt 14 → date
        assert_eq!(s.cell(0, 3), Some(&Cell::Bool(true)));
        assert!(s.to_text().contains("2024-03-15"));
        assert_eq!(s.default_column_width(), None);
        assert_eq!(s.implicit_ooxml_column_width(), Some(None));
        assert_eq!(s.default_row_height(), None);
        assert!(s.has_implicit_ooxml_row_height());
        assert_eq!(
            s.implicit_ooxml_row_height_source(),
            Some(OoxmlImplicitRowHeight::XlsxApplicationDefault)
        );
    }

    #[test]
    fn sheet_format_retains_explicit_and_base_column_width_provenance() {
        let parse = |format: &str| {
            let xml = format!(
                r#"<worksheet>{format}<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
            );
            let mut budget = crate::MAX_TEXT_BYTES;
            parse_sheet(
                &xml,
                &[],
                &Styles::default(),
                &ThemeColors::default(),
                false,
                &mut budget,
            )
        };

        let absent = parse("");
        assert_eq!(absent.default_col_width, None);
        assert_eq!(absent.base_col_width, None);
        assert!(!absent.defaulted_base_col_width);

        let defaulted_base = parse(r#"<sheetFormatPr/>"#);
        assert_eq!(defaulted_base.default_col_width, None);
        assert_eq!(defaulted_base.base_col_width, None);
        assert!(defaulted_base.defaulted_base_col_width);

        let ignored_non_positive =
            parse(r#"<sheetFormatPr baseColWidth="0" defaultColWidth="-1"/>"#);
        assert_eq!(ignored_non_positive.default_col_width, None);
        assert_eq!(ignored_non_positive.base_col_width, None);
        assert!(!ignored_non_positive.defaulted_base_col_width);

        let explicit = parse(r#"<sheetFormatPr baseColWidth="8" defaultColWidth="8.43"/>"#);
        assert_eq!(explicit.default_col_width, Some(8.43));
        assert_eq!(explicit.base_col_width, Some(8.0));
        assert!(!explicit.defaulted_base_col_width);
        assert_eq!(
            explicit.imported_default_column_axis_measure,
            Some(ImportedAxisMeasure::CharacterWidthRatio(843, 100))
        );

        let base = parse(r#"<sheetFormatPr baseColWidth="10"/>"#);
        assert_eq!(base.default_col_width, None);
        assert_eq!(base.base_col_width, Some(10.0));
        assert!(!base.defaulted_base_col_width);
        assert_eq!(
            base.imported_default_column_axis_measure,
            Some(ImportedAxisMeasure::CharacterBaseWidth256(10 * 256))
        );
    }

    #[test]
    fn worksheet_retains_exact_twip_and_ooxml_character_axis_sources() {
        let xml = r#"<worksheet><sheetFormatPr defaultRowHeight="15" defaultColWidth="8"/><cols><col min="1" max="2" width="14"/></cols><sheetData><row r="1" ht="18"/><row r="2" hidden="1" ht="12.75"/></sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(
            parsed.imported_default_row_axis_measure,
            Some(ImportedAxisMeasure::Twips(300))
        );
        assert_eq!(
            parsed.imported_default_column_axis_measure,
            Some(ImportedAxisMeasure::CharacterWidthRatio(8, 1))
        );
        assert_eq!(
            parsed
                .imported_column_axis_measures
                .values()
                .copied()
                .collect::<Vec<_>>(),
            [
                ImportedAxisMeasure::CharacterWidthRatio(14, 1),
                ImportedAxisMeasure::CharacterWidthRatio(14, 1),
            ]
        );
        assert_eq!(
            parsed.imported_row_axis_measures.get(&0),
            Some(&ImportedAxisMeasure::Twips(360))
        );
        assert_eq!(
            parsed.imported_row_axis_measures.get(&1),
            Some(&ImportedAxisMeasure::Twips(255))
        );
        assert!(parsed.hidden_rows.contains(&1));
    }

    #[test]
    fn worksheet_retains_decimal_and_scientific_axis_sources_without_float_drift() {
        let xml = r#"<worksheet><sheetFormatPr defaultRowHeight=" 1.5E1 " defaultColWidth="8.43"/><cols><col min="1" max="1" width="8.43"/></cols><sheetData><row r="1" ht="1.2345E1"/></sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.default_row_height, Some(15.0));
        assert_eq!(
            parsed.imported_default_row_axis_measure,
            Some(ImportedAxisMeasure::Twips(300))
        );
        assert_eq!(
            parsed.imported_default_column_axis_measure,
            Some(ImportedAxisMeasure::CharacterWidthRatio(843, 100))
        );
        assert_eq!(
            parsed.imported_column_axis_measures.get(&0),
            Some(&ImportedAxisMeasure::CharacterWidthRatio(843, 100))
        );
        assert_eq!(
            parsed.imported_row_axis_measures.get(&0),
            Some(&ImportedAxisMeasure::PointRatio(2_469, 200))
        );
    }

    #[test]
    fn worksheet_keeps_valid_row_heights_when_exact_provenance_is_unrepresentable() {
        let source = "15.1234567890123456789012345";
        let xml = format!(
            r#"<worksheet><sheetFormatPr defaultRowHeight="{source}"/><sheetData><row r="1" ht="{source}"/></sheetData></worksheet>"#
        );
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            &xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        let expected = source.parse::<f32>().expect("finite height");

        assert_eq!(parsed.default_row_height, Some(expected));
        assert_eq!(parsed.row_heights.get(&0), Some(&expected));
        assert_eq!(parsed.imported_default_row_axis_measure, None);
        assert!(!parsed.imported_row_axis_measures.contains_key(&0));
        assert!(parsed.automatic_default_row_height_candidate);
        assert!(parsed.automatic_row_height_candidates.contains(&0));
    }

    #[test]
    fn worksheet_row_height_manuality_tracks_custom_height_separately() {
        let xml = r#"<worksheet><sheetData>
            <row r="1" ht="18" customHeight="1"/>
            <row r="2" ht="19" customHeight="true"/>
            <row r="3" ht="20" customHeight="0"/>
            <row r="4" ht="21" customHeight="false"/>
            <row r="5" ht="22"/>
            <row r="6" ht="23" customHeight="malformed"/>
            <row r="7" ht="NaN" customHeight="1"/>
            <row r="8" ht="-1" customHeight="1"/>
            <row r="9" ht="1e309" customHeight="1"/>
            <row r="10" ht="0" customHeight="1"/>
            <row r="1048576" ht="24" customHeight="TRUE"/>
            <row r="1048577" ht="25" customHeight="1"/>
        </sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(
            parsed.row_heights.keys().copied().collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, MAX_XLSX_ROW_INDEX]
        );
        assert_eq!(
            parsed
                .imported_row_axis_measures
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, MAX_XLSX_ROW_INDEX]
        );
        assert_eq!(
            parsed
                .automatic_row_height_candidates
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [2, 3, 4, 5]
        );
    }

    #[test]
    fn imported_xlsx_exposes_row_height_manuality_to_renderers() {
        let workbook = workbook_with_worksheet_xml(
            r#"<worksheet><sheetData>
                <row r="1" ht="18" customHeight="1"/>
                <row r="2" ht="19" customHeight="false"/>
                <row r="3" ht="20"/>
                <row r="4" customHeight="1"/>
            </sheetData></worksheet>"#,
        );
        let sheet = &workbook.sheets[0];

        assert!(sheet.row_height_is_manual(0));
        assert!(!sheet.row_height_is_manual(1));
        assert!(!sheet.row_height_is_manual(2));
        assert!(!sheet.row_height_is_manual(3));
        assert!(!sheet.row_height_is_manual(4));
    }

    #[test]
    fn imported_xlsx_exposes_default_row_height_manuality_to_renderers() {
        for (attributes, expected_manual) in [
            (r#"defaultRowHeight="15" customHeight="1""#, true),
            (r#"defaultRowHeight="15" customHeight="true""#, true),
            (r#"defaultRowHeight="15" customHeight="0""#, false),
            (r#"defaultRowHeight="15" customHeight="false""#, false),
            (r#"defaultRowHeight="15""#, false),
            (r#"defaultRowHeight="15" customHeight="malformed""#, false),
        ] {
            let workbook = workbook_with_worksheet_xml(&format!(
                r#"<worksheet><sheetFormatPr {attributes}/><sheetData/></worksheet>"#
            ));
            let sheet = &workbook.sheets[0];

            assert_eq!(sheet.default_row_height(), Some(15.0), "{attributes}");
            assert_eq!(
                sheet.default_row_height_is_manual(),
                expected_manual,
                "{attributes}"
            );
        }

        let workbook = workbook_with_worksheet_xml(r#"<worksheet><sheetData/></worksheet>"#);
        assert!(!workbook.sheets[0].default_row_height_is_manual());

        for invalid_height in ["NaN", "-1", "0", "1e309"] {
            let workbook = workbook_with_worksheet_xml(&format!(
                r#"<worksheet><sheetFormatPr defaultRowHeight="{invalid_height}" customHeight="1"/><sheetData/></worksheet>"#
            ));
            let sheet = &workbook.sheets[0];

            assert_eq!(sheet.default_row_height(), None, "{invalid_height}");
            assert_eq!(
                sheet.imported_default_row_axis_measure(),
                None,
                "{invalid_height}"
            );
            assert!(!sheet.default_row_height_is_manual(), "{invalid_height}");
        }
    }

    #[test]
    fn worksheet_invalid_column_widths_keep_legacy_values_without_exact_provenance() {
        let xml = r#"<worksheet><cols>
            <col min="1" max="1" width="0" style="1"/>
            <col min="2" max="2" width="NaN" hidden="1" style="1"/>
        </cols><sheetData/></worksheet>"#;
        let styles = parse_styles(
            r#"<styleSheet><cellXfs count="2"><xf/><xf applyAlignment="1"><alignment wrapText="1"/></xf></cellXfs></styleSheet>"#,
            &ThemeColors::default(),
        );
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &styles,
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.col_widths.get(&0), Some(&0.0));
        assert!(parsed
            .col_widths
            .get(&1)
            .is_some_and(|width| width.is_nan()));
        assert!(parsed.imported_column_axis_measures.is_empty());
        assert_eq!(
            parsed.col_formats.keys().copied().collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(parsed.hidden_cols.iter().copied().collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn worksheet_implicit_cell_columns_fail_closed_outside_the_ooxml_grid() {
        let xml = r#"<worksheet><sheetData><row r="1">
            <c r="XFD1" t="inlineStr"><is><t>last</t></is></c>
            <c t="inlineStr"><is><t>overflow</t></is></c>
            <c t="inlineStr"><is><t>still-overflow</t></is></c>
            <c r="A1" t="inlineStr"><is><t>resynced</t></is></c>
            <c t="inlineStr"><is><t>next</t></is></c>
            <c r="XFE1" t="inlineStr"><is><t>invalid-explicit</t></is></c>
            <c t="inlineStr"><is><t>poisoned</t></is></c>
            <c r="C1" t="inlineStr"><is><t>resynced-again</t></is></c>
        </row></sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(
            parsed
                .cells
                .iter()
                .map(|cell| (cell.row, cell.col, cell.text.as_str()))
                .collect::<Vec<_>>(),
            [
                (0, MAX_XLSX_COLUMN_INDEX, "last"),
                (0, 0, "resynced"),
                (0, 1, "next"),
                (0, 2, "resynced-again"),
            ]
        );
    }

    #[test]
    fn worksheet_outline_levels_are_bounded_to_the_ooxml_depth() {
        let xml = r#"<worksheet><cols>
            <col min="1" max="1" outlineLevel="0"/>
            <col min="2" max="2" outlineLevel="7"/>
            <col min="3" max="3" outlineLevel="8"/>
        </cols><sheetData>
            <row r="1" outlineLevel="0"/>
            <row r="2" outlineLevel="7"/>
            <row r="3" outlineLevel="8"/>
        </sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.col_outline.into_iter().collect::<Vec<_>>(), [(1, 7)]);
        assert_eq!(parsed.row_outline.into_iter().collect::<Vec<_>>(), [(1, 7)]);
    }

    #[test]
    fn worksheet_rows_and_implicit_cells_fail_closed_outside_the_ooxml_grid() {
        let xml = r#"<worksheet><sheetData>
            <row r="1048576" ht="18" hidden="1" outlineLevel="1" collapsed="1" s="1"><c t="inlineStr"><is><t>last</t></is></c></row>
            <row ht="19" outlineLevel="2" collapsed="1" s="1"><c r="A1" t="inlineStr"><is><t>invalid-explicit-cell</t></is></c><c t="inlineStr"><is><t>invalid-implicit-cell</t></is></c></row>
            <row ht="20"><c t="inlineStr"><is><t>still-invalid-implicit-row</t></is></c></row>
            <row r="1048577" ht="22" hidden="1" outlineLevel="4" collapsed="1" s="1"><c t="inlineStr"><is><t>invalid-explicit-row</t></is></c></row>
            <row r="1" ht="21" outlineLevel="3" collapsed="1" s="1"><c t="inlineStr"><is><t>resynced</t></is></c><c r="A1048577" t="inlineStr"><is><t>invalid-cell-ref</t></is></c></row>
        </sheetData></worksheet>"#;
        let styles = parse_styles(
            r#"<styleSheet><cellXfs count="2"><xf/><xf applyAlignment="1"><alignment wrapText="1"/></xf></cellXfs></styleSheet>"#,
            &ThemeColors::default(),
        );
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &styles,
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(
            parsed
                .cells
                .iter()
                .map(|cell| (cell.row, cell.col, cell.text.as_str()))
                .collect::<Vec<_>>(),
            [(MAX_XLSX_ROW_INDEX, 0, "last"), (0, 0, "resynced"),]
        );
        assert_eq!(
            parsed.row_heights.keys().copied().collect::<Vec<_>>(),
            [0, MAX_XLSX_ROW_INDEX]
        );
        assert_eq!(
            parsed
                .imported_row_axis_measures
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [0, MAX_XLSX_ROW_INDEX]
        );
        assert_eq!(
            parsed.row_outline.keys().copied().collect::<Vec<_>>(),
            [0, MAX_XLSX_ROW_INDEX]
        );
        assert_eq!(
            parsed.collapsed_rows.iter().copied().collect::<Vec<_>>(),
            [0, MAX_XLSX_ROW_INDEX]
        );
        assert_eq!(
            parsed.hidden_rows.iter().copied().collect::<Vec<_>>(),
            [MAX_XLSX_ROW_INDEX]
        );
        assert_eq!(
            parsed
                .explicit_visible_rows
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [0]
        );
        assert_eq!(
            parsed.row_formats.keys().copied().collect::<Vec<_>>(),
            [0, MAX_XLSX_ROW_INDEX]
        );
    }

    #[test]
    fn sheet_format_zero_height_retains_explicit_visible_row_exceptions() {
        let xml = r#"<worksheet><sheetFormatPr zeroHeight="1"/><sheetData><row r="2"/><row r="3" hidden="1"/><row r="5" hidden="0"/></sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert!(parsed.default_rows_hidden);
        assert_eq!(
            parsed
                .explicit_visible_rows
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [1, 4]
        );
        assert_eq!(parsed.hidden_rows.iter().copied().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn malformed_zip_container_reports_zip_error_not_biff() {
        let err = Workbook::open(b"PK\x03\x04 truncated").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid ZIP package: not a valid spreadsheet ZIP container"
        );
    }

    #[test]
    fn reads_xlsx_with_backslash_package_paths() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl\\workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl\\_rels\\workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets\sheet1.xml"/></Relationships>"#,
            ),
            ("xl\\sharedStrings.xml", r#"<sst><si><t>ok</t></si></sst>"#),
            (
                "xl\\worksheets\\sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let s = &wb.sheets[0];
        assert_eq!(s.name, "Data");
        assert_eq!(s.cell(0, 0), Some(&Cell::Text("ok".to_string())));
    }

    #[test]
    fn reads_xlsx_with_root_office_document_part_and_above_root_target() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "_rels/.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="../../workbook.xml"/></Relationships>"#,
            ),
            (
                "workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Root" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="sheet1.xml"/></Relationships>"#,
            ),
            (
                "sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>root</t></is></c></row></sheetData></worksheet>"#,
            ),
            ("styles.xml", r#"<styleSheet/>"#),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let s = &wb.sheets[0];
        assert_eq!(s.name, "Root");
        assert_eq!(s.cell(0, 0), Some(&Cell::Text("root".to_string())));
    }

    #[test]
    fn workbook_sheet_visibility_and_defined_names() {
        // `<sheet state>` carries visibility; `<definedNames>` carry workbook-global
        // names. A built-in `_xlnm.*` name is skipped; a user name is kept.
        let xml = r#"<workbook>
            <sheets>
                <sheet name="Vis" r:id="rId1"/>
                <sheet name="Hid" state="hidden" r:id="rId2"/>
                <sheet name="VHid" state="veryHidden" r:id="rId3"/>
            </sheets>
            <definedNames>
                <definedName name="TaxRate">Sheet1!$B$1</definedName>
                <definedName name="_xlnm.Print_Area" localSheetId="0">Sheet1!$A$1:$C$3</definedName>
                <definedName name="LocalOnly" localSheetId="1">Sheet2!$A$1</definedName>
            </definedNames>
        </workbook>"#;
        let parsed = parse_workbook(xml);
        assert_eq!(parsed.sheets.len(), 3);
        assert_eq!(parsed.sheets[0].visibility, Visibility::Visible);
        assert_eq!(parsed.sheets[1].visibility, Visibility::Hidden);
        assert_eq!(parsed.sheets[2].visibility, Visibility::VeryHidden);
        // Global and local user names remain distinct; the built-in print area
        // stays in the sheet-metadata path.
        assert_eq!(
            parsed.defined_names,
            vec![("TaxRate".to_string(), "Sheet1!$B$1".to_string())]
        );
        assert_eq!(
            parsed.local_defined_names,
            vec![crate::LocalDefinedName {
                sheet: "Hid".to_string(),
                name: "LocalOnly".to_string(),
                refers_to: "Sheet2!$A$1".to_string(),
            }]
        );
    }

    /// End-to-end `.xlsx` read: a hidden sheet + a defined name surface via the
    /// public `is_hidden()` / `defined_names()` accessors.
    #[test]
    fn hidden_sheet_and_defined_name_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/><sheet name="Secret" state="hidden" r:id="rId2"/></sheets><definedNames><definedName name="TaxRate">Data!$A$1</definedName></definedNames></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="worksheets/sheet2.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
            (
                "xl/worksheets/sheet2.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 2);
        assert!(!wb.sheets[0].is_hidden(), "Data is visible");
        assert!(wb.sheets[1].is_hidden(), "Secret is hidden");
        assert!(!wb.sheets[1].is_very_hidden());
        assert_eq!(
            wb.defined_names(),
            &[("TaxRate".to_string(), "Data!$A$1".to_string())]
        );
    }

    #[test]
    fn chart_series_refs_retain_bounded_caches_and_theme_palette_sidecar() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rIdTheme" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/></Relationships>"#,
            ),
            (
                "xl/theme/theme1.xml",
                r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:accent1><a:srgbClr val="010203"/></a:accent1><a:accent2><a:srgbClr val="A0B0C0"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme><a:majorFont><a:latin typeface="Ignored Major"/></a:majorFont><a:minorFont><a:latin typeface="Source Sans 3"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/><drawing r:id="rIdDraw"/></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
            ),
            (
                "xl/drawings/drawing1.xml",
                r#"<wsDr>
                    <twoCellAnchor>
                        <from><col>2</col><row>4</row></from>
                        <to><col>8</col><row>16</row></to>
                        <graphicFrame>
                            <graphic>
                                <graphicData>
                                    <chart r:id="rIdChart"/>
                                </graphicData>
                            </graphic>
                        </graphicFrame>
                    </twoCellAnchor>
                </wsDr>"#,
            ),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
            ),
            (
                "xl/charts/chart1.xml",
                r#"<chartSpace><chart><plotArea><lineChart><ser>
                    <tx><strRef><f>Data!$C$1</f><strCache><pt idx="0"><v>Cached Series</v></pt></strCache></strRef></tx>
                    <marker><symbol val="circle"/><size val="5"/></marker>
                    <spPr><a:ln w="38100"><a:solidFill><a:schemeClr val="accent2"/></a:solidFill></a:ln></spPr>
                    <cat><strRef><f>Data!$A$2:$A$4</f><strCache><pt idx="0"><v>Q1</v></pt><pt idx="1"><v>Q2</v></pt><pt idx="2"><v>Q3</v></pt></strCache></strRef></cat>
                    <val><numRef><f>Data!$B$2:$B$4</f><numCache><pt idx="0"><v>10</v></pt><pt idx="1"><v>20</v></pt><pt idx="2"><v>30</v></pt></numCache></numRef></val>
                </ser></lineChart></plotArea></chart></chartSpace>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        let charts = wb.sheets[0].charts();

        assert_eq!(charts.len(), 1);
        assert_eq!(charts[0].kind, ChartKind::Line);
        assert_eq!(charts[0].from, (4, 2));
        assert_eq!(charts[0].to, (16, 8));
        assert_eq!(charts[0].series.len(), 1);
        assert_eq!(charts[0].series[0].name.as_deref(), Some("Data!$C$1"));
        assert_eq!(
            charts[0].series[0].categories.as_deref(),
            Some("Data!$A$2:$A$4")
        );
        assert_eq!(charts[0].series[0].values, "Data!$B$2:$B$4");
        let sidecar = wb.sheets[0]
            .drawing_metadata()
            .iter()
            .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
            .expect("chart rendering sidecar");
        assert_eq!(sidecar.chart_palette[0], Color::rgb(1, 2, 3));
        assert_eq!(sidecar.chart_palette[1], Color::rgb(160, 176, 192));
        assert_eq!(
            sidecar.chart_default_latin_font_family.as_deref(),
            Some("Source Sans 3")
        );
        assert_eq!(sidecar.chart_series_caches.len(), 1);
        assert_eq!(sidecar.chart_series_styles.len(), 1);
        assert_eq!(
            sidecar.chart_series_styles[0].marker,
            ChartMarkerSymbol::Circle
        );
        assert_eq!(sidecar.chart_series_styles[0].marker_size, Some(5));
        assert!(sidecar.chart_series_styles[0].line_visible);
        assert_eq!(
            sidecar.chart_series_styles[0].line_color,
            Some(Color::rgb(160, 176, 192))
        );
        assert_eq!(sidecar.chart_series_styles[0].line_width_emu, Some(38_100));
        assert!(sidecar.chart_series_styles[0].losses.is_empty());
        let cache = &sidecar.chart_series_caches[0];
        assert_eq!(cache.name[0].value, "Cached Series");
        assert_eq!(
            cache
                .categories
                .iter()
                .map(|point| point.value.as_str())
                .collect::<Vec<_>>(),
            ["Q1", "Q2", "Q3"]
        );
        assert_eq!(
            cache
                .values
                .iter()
                .map(|point| point.value.as_str())
                .collect::<Vec<_>>(),
            ["10", "20", "30"]
        );
    }

    #[test]
    fn chart_sidecar_uses_calc_latin_fallback_when_package_theme_is_missing() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/><drawing r:id="rIdDraw"/></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
            ),
            (
                "xl/drawings/drawing1.xml",
                r#"<wsDr><twoCellAnchor><from><col>2</col><row>4</row></from><to><col>8</col><row>16</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#,
            ),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
            ),
            (
                "xl/charts/chart1.xml",
                r#"<chartSpace><chart><plotArea><lineChart/></plotArea></chart></chartSpace>"#,
            ),
        ];
        for (name, body) in parts {
            writer.start_file(name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }

        let bytes = writer.finish().unwrap().into_inner();
        let workbook = Workbook::open(&bytes).unwrap();
        let sidecar = workbook.sheets[0]
            .drawing_metadata()
            .iter()
            .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
            .expect("chart rendering sidecar");

        assert_eq!(
            sidecar.chart_default_latin_font_family.as_deref(),
            Some(CALC_IMPORTED_CHART_LATIN_FONT_FAMILY)
        );
    }

    #[test]
    fn minor_theme_latin_font_family_is_trimmed_and_bounded() {
        let theme = parse_theme(&complete_theme_xml(
            "Ignored Major",
            "  Theme Sans  ",
            "4472C4",
            "ED7D31",
        ));
        assert!(theme.source_valid);
        assert_eq!(theme.minor_latin_font_family.as_deref(), Some("Theme Sans"));

        let boundary = "x".repeat(MAX_IMPORTED_CHART_LATIN_FONT_FAMILY_BYTES);
        let xml = complete_theme_xml("Major", &boundary, "4472C4", "ED7D31");
        assert_eq!(
            parse_theme(&xml).minor_latin_font_family.as_deref(),
            Some(boundary.as_str())
        );

        assert_eq!(
            parse_theme(r#"<a:theme><a:themeElements><a:fontScheme/></a:themeElements></a:theme>"#)
                .chart_default_latin_font_family(),
            CALC_IMPORTED_CHART_LATIN_FONT_FAMILY
        );

        for invalid in [String::new(), " ".to_string(), "x".repeat(256)] {
            let xml = complete_theme_xml("Major", &invalid, "4472C4", "ED7D31");
            let theme = parse_theme(&xml);
            assert!(!theme.source_valid);
            assert!(theme.minor_latin_font_family.is_none());
            assert_eq!(
                theme.chart_default_latin_font_family(),
                CALC_IMPORTED_CHART_LATIN_FONT_FAMILY
            );
        }
    }

    #[test]
    fn theme_requires_complete_structural_and_namespace_exact_source() {
        let valid = complete_theme_xml("Major", "Minor", "4472C4", "ED7D31");
        assert!(parse_theme(&valid).source_valid);

        let missing_slot = valid.replace(r#"<a:accent6><a:srgbClr val="70AD47"/></a:accent6>"#, "");
        let missing_scheme = valid
            .replace("<a:fontScheme>", "<a:notFontScheme>")
            .replace("</a:fontScheme>", "</a:notFontScheme>");
        let duplicate_slot = valid.replace(
            "<a:accent1>",
            r#"<a:accent1><a:srgbClr val="010203"/></a:accent1><a:accent1>"#,
        );
        let wrong_namespace = valid.replace(
            OOXML_DRAWING_NAMESPACE_TRANSITIONAL,
            OOXML_CHART_NAMESPACE_TRANSITIONAL,
        );
        let foreign_paint = valid
            .replace(
                "<a:theme ",
                r#"<a:theme xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" "#,
            )
            .replacen(
                r#"<a:srgbClr val="E7E6E6"/>"#,
                r#"<c:srgbClr val="E7E6E6"/>"#,
                1,
            );
        let wrapped_scheme = valid
            .replacen("<a:clrScheme>", "<a:wrapper><a:clrScheme>", 1)
            .replacen("</a:clrScheme>", "</a:clrScheme></a:wrapper>", 1);
        let hash_rgb = valid.replacen("4472C4", "#4472C4", 1);
        let argb = valid.replacen("4472C4", "FF4472C4", 1);
        let malformed = valid.trim_end_matches("</a:theme>").to_string();

        for invalid in [
            missing_slot,
            missing_scheme,
            duplicate_slot,
            wrong_namespace,
            foreign_paint,
            wrapped_scheme,
            hash_rgb,
            argb,
            malformed,
        ] {
            assert!(!parse_theme(&invalid).source_valid, "{invalid}");
        }
    }

    #[test]
    fn chart_rgb_is_exact_and_drawingml_tint_endpoints_are_correct() {
        assert_eq!(
            parse_chart_rgb("112233"),
            Some(Color::rgb(0x11, 0x22, 0x33))
        );
        for invalid in ["#112233", " 112233", "112233 ", "FF112233", "11223G"] {
            assert_eq!(parse_chart_rgb(invalid), None, "{invalid}");
        }

        let source = Color::rgb(0x20, 0x40, 0x80);
        assert_eq!(apply_chart_luminance(source, 100_000, 0), source);
        let white = Color::rgb(255, 255, 255);
        assert_eq!(apply_chart_luminance(source, 0, 100_000), white);
        let midpoint = apply_chart_luminance(source, 50_000, 50_000);
        assert_ne!(midpoint, source);
        assert_ne!(midpoint, white);
    }

    #[test]
    fn foreign_chart_markup_and_alternate_content_are_never_applied_silently() {
        fn reasons(xml: &str) -> Vec<ChartUnsupportedReason> {
            let mut cache_points = 16;
            let mut chart_series = 16;
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series)
                .expect("local-name parser still identifies the chart")
                .unsupported_reasons
        }

        let foreign_kind = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:evil="urn:evil"><c:chart><c:plotArea><evil:pieChart/></c:plotArea></c:chart></c:chartSpace>"#;
        assert!(reasons(foreign_kind).contains(&ChartUnsupportedReason::UnsupportedMarkup));

        let foreign_val = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><c:chart><c:plotArea><c:pieChart><c:varyColors a:val="0"/></c:pieChart></c:plotArea></c:chart></c:chartSpace>"#;
        assert!(reasons(foreign_val).contains(&ChartUnsupportedReason::UnsupportedMarkup));

        let alternate = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><c:chart><c:plotArea><mc:AlternateContent><mc:Choice Requires="c"><c:pieChart/></mc:Choice><mc:Fallback><c:barChart/></mc:Fallback></mc:AlternateContent></c:plotArea></c:chart></c:chartSpace>"#;
        assert!(reasons(alternate).contains(&ChartUnsupportedReason::UnsupportedMarkup));
    }

    #[test]
    fn imported_chart_title_retains_uniform_painted_run_style() {
        let theme = parse_theme(&complete_theme_xml(
            "Major Face",
            "Minor Face",
            "4472C4",
            "ED7D31",
        ));
        assert!(theme.source_valid);
        let xml = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><c:chart><c:title><c:tx><c:rich>
            <a:bodyPr/><a:lstStyle/><a:p><a:pPr><a:defRPr sz="1000" b="0">
                <a:latin typeface="Calibri"/>
            </a:defRPr></a:pPr><a:r><a:rPr sz="2400" b="1" i="0" u="none" strike="noStrike">
                <a:solidFill><a:sysClr val="windowText" lastClr="000000"/></a:solidFill>
                <a:latin typeface="Eurostile"/>
            </a:rPr><a:t>Sales</a:t></a:r>
            <a:endParaRPr sz="2400" b="1" u="sng" strike="sngStrike"><a:latin typeface="Eurostile"/></a:endParaRPr>
            </a:p></c:rich></c:tx></c:title><c:plotArea><c:lineChart><c:axId val="1"/><c:axId val="2"/></c:lineChart><c:catAx><c:axId val="1"/><c:crossAx val="2"/></c:catAx><c:valAx><c:axId val="2"/><c:crossAx val="1"/></c:valAx></c:plotArea></c:chart></c:chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed = parse_chart_with_theme(
            xml,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
            &theme,
        )
        .unwrap();

        assert!(parsed.unsupported_reasons.is_empty());
        assert_eq!(parsed.chart.title.as_deref(), Some("Sales"));
        assert_eq!(
            parsed.text_styles.chart_title,
            Some(ChartTextStyle {
                latin_font_family: "Eurostile".to_string(),
                size_hundredths_of_point: 2_400,
                color: Color::rgb(0, 0, 0),
                bold: true,
                italic: false,
                underline: false,
                strikethrough: false,
                kerning_minimum_hundredths_of_point: None,
                rotation_degrees: None,
            })
        );
    }

    #[test]
    fn imported_chart_text_resolves_theme_roles_and_rejects_mixed_runs() {
        let theme = parse_theme(&complete_theme_xml(
            "Major Face",
            "Minor Face",
            "4472C4",
            "ED7D31",
        ));
        assert!(theme.source_valid);
        let uniform = r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><title><tx><rich><a:bodyPr/><a:lstStyle/><a:p>
            <a:r><a:rPr sz="1400"><a:latin typeface="+mj-lt"/></a:rPr><a:t>A</a:t></a:r>
            <a:r><a:rPr sz="1400"><a:latin typeface="+mj-lt"/></a:rPr><a:t>B</a:t></a:r>
            </a:p></rich></tx></title><plotArea><lineChart><axId val="1"/><axId val="2"/></lineChart><catAx><axId val="1"/><crossAx val="2"/></catAx><valAx><axId val="2"/><crossAx val="1"/></valAx></plotArea></chart></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed = parse_chart_with_theme(
            uniform,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
            &theme,
        )
        .unwrap();
        let style = parsed.text_styles.chart_title.unwrap();
        assert_eq!(style.latin_font_family, "Major Face");
        assert_eq!(style.size_hundredths_of_point, 1_400);
        assert!(parsed.unsupported_reasons.is_empty());

        let mixed = uniform.replacen(
            r#"sz="1400"><a:latin typeface="+mj-lt"/></a:rPr><a:t>B"#,
            r#"sz="1600"><a:latin typeface="+mn-lt"/></a:rPr><a:t>B"#,
            1,
        );
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed = parse_chart_with_theme(
            &mixed,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
            &theme,
        )
        .unwrap();
        assert!(parsed.text_styles.chart_title.is_none());
        assert_eq!(
            parsed.unsupported_reasons,
            [ChartUnsupportedReason::MixedTextStyle]
        );
    }

    #[test]
    fn imported_horizontal_bar_maps_semantic_axis_titles_after_direction() {
        let xml = r#"<chartSpace><chart><plotArea><barChart><barDir val="bar"/></barChart>
            <catAx><title><tx><rich><a:bodyPr/><a:p><a:r><a:rPr sz="900"><a:latin typeface="Category Face"/></a:rPr><a:t>Category</a:t></a:r></a:p></rich></tx></title></catAx>
            <valAx><title><tx><rich><a:bodyPr/><a:p><a:r><a:rPr sz="1200"><a:latin typeface="Value Face"/></a:rPr><a:t>Value</a:t></a:r></a:p></rich></tx></title></valAx>
            </plotArea></chart></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();

        assert_eq!(parsed.chart.x_axis_title.as_deref(), Some("Value"));
        assert_eq!(parsed.chart.y_axis_title.as_deref(), Some("Category"));
        assert_eq!(
            parsed
                .text_styles
                .category_axis_title
                .as_ref()
                .map(|style| style.latin_font_family.as_str()),
            Some("Category Face")
        );
        assert_eq!(
            parsed
                .text_styles
                .value_axis_title
                .as_ref()
                .map(|style| style.latin_font_family.as_str()),
            Some("Value Face")
        );
    }

    #[test]
    fn imported_chart_text_enforces_size_family_and_decoration_boundaries() {
        for size in ["100", "400000"] {
            assert_eq!(chart_text_bounded_size(size), Some(size.parse().unwrap()));
        }
        for size in ["99", "400001", "-1", "large"] {
            assert_eq!(chart_text_bounded_size(size), None);
        }
        assert!(bounded_imported_chart_latin_font_family(&"x".repeat(255)).is_some());
        assert!(bounded_imported_chart_latin_font_family(&"x".repeat(256)).is_none());

        for attributes in [
            r#"sz="99""#,
            r#"sz="400001""#,
            r#"u="dbl""#,
            r#"strike="dblStrike""#,
            r#"baseline="30000""#,
            r#"spc="150""#,
        ] {
            let xml = format!(
                r#"<chartSpace><chart><title><tx><rich><a:bodyPr/><a:p><a:r><a:rPr {attributes}><a:latin typeface="Face"/></a:rPr><a:t>X</a:t></a:r></a:p></rich></tx></title><plotArea><lineChart/></plotArea></chart></chartSpace>"#
            );
            let mut cache_points = 16;
            let mut chart_series = 16;
            let parsed =
                parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
            assert!(parsed
                .unsupported_reasons
                .contains(&ChartUnsupportedReason::UnsupportedTextStyle));
        }
    }

    #[test]
    fn chart_data_label_visibility_is_effective_and_extensions_fail_closed() {
        for (labels, expected_visible, expected_unsupported) in [
            ("<dLbls/>", false, false),
            ("<dLbls><showVal val=\"0\"/></dLbls>", false, false),
            ("<dLbls><showVal/></dLbls>", true, false),
            ("<dLbls><showVal/><delete/></dLbls>", false, false),
            ("<dLbls><showCatName/></dLbls>", false, true),
            (
                "<dLbls><extLst><ext><showVal/></ext></extLst></dLbls>",
                false,
                true,
            ),
            (
                "<dLbls><showVal/><dLbl><idx val=\"0\"/><delete/></dLbl></dLbls>",
                false,
                true,
            ),
            ("<dLbls><showVal val=\"TRUE\"/></dLbls>", false, true),
        ] {
            let xml = format!(
                "<chartSpace><chart><plotArea><pieChart>{labels}</pieChart></plotArea></chart></chartSpace>"
            );
            assert_eq!(
                parse_chart_data_labels(&xml),
                (expected_visible, expected_unsupported),
                "{labels}"
            );
        }
    }

    #[test]
    fn chart_axis_semantics_follow_plot_ids_visibility_and_cross_links() {
        let xml = r#"<chartSpace><chart><plotArea>
            <scatterChart><axId val="10"/><axId val="20"/></scatterChart>
            <valAx><axId val="20"/><majorGridlines/><delete val="0"/><crossAx val="10"/></valAx>
            <valAx><axId val="10"/><delete/><crossAx val="20"/></valAx>
        </plotArea></chart></chartSpace>"#;
        let semantics = parse_chart_axis_semantics(xml);
        assert!(!semantics.unsupported_topology);
        assert!(!semantics.invalid_visibility);
        assert_eq!(
            semantics.axis_roles,
            [ChartAxisContext::Value, ChartAxisContext::Category]
        );
        assert_eq!(semantics.category_visible, Some(false));
        assert_eq!(semantics.value_visible, Some(true));
        assert!(!semantics.category_major_gridlines);
        assert!(semantics.value_major_gridlines);

        let invalid_cross = xml.replace(r#"crossAx val="10""#, r#"crossAx val="99""#);
        assert!(parse_chart_axis_semantics(&invalid_cross).unsupported_topology);

        for (plot, unsupported) in [
            ("<lineChart/>", true),
            ("<scatterChart/>", true),
            ("<pieChart/>", false),
            ("<doughnutChart/>", false),
        ] {
            let xml =
                format!("<chartSpace><chart><plotArea>{plot}</plotArea></chart></chartSpace>");
            assert_eq!(
                parse_chart_axis_semantics(&xml).unsupported_topology,
                unsupported,
                "{plot}"
            );
        }
    }

    #[test]
    fn chart_axis_generated_defaults_preserve_supported_chart_semantics() {
        fn parsed_chart(xml: &str) -> ParsedChart {
            let mut cache_points = 16;
            let mut chart_series = 16;
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).expect("chart")
        }

        let default_axes = r#"<catAx><axId val="1"/><scaling><orientation val="minMax"/></scaling><delete val="0"/><axPos val="b"/><tickLblPos val="nextTo"/><crossAx val="2"/><crosses val="autoZero"/><auto val="1"/><lblAlgn val="ctr"/><lblOffset val="100"/></catAx><valAx><axId val="2"/><scaling><orientation val="minMax"/></scaling><delete val="0"/><axPos val="l"/><numFmt formatCode="General" sourceLinked="1"/><tickLblPos val="nextTo"/><crossAx val="1"/><crosses val="autoZero"/><crossBetween val="between"/></valAx>"#;
        for (plot, expected_kind) in [
            (
                r#"<lineChart><grouping val="standard"/><varyColors val="0"/><axId val="1"/><axId val="2"/></lineChart>"#,
                ChartKind::Line,
            ),
            (
                r#"<barChart><barDir val="col"/><grouping val="clustered"/><axId val="1"/><axId val="2"/></barChart>"#,
                ChartKind::Bar,
            ),
        ] {
            let xml = format!(
                "<chartSpace><chart><plotArea>{plot}{default_axes}</plotArea></chart></chartSpace>"
            );
            let semantics = parse_chart_axis_semantics(&xml);
            assert!(!semantics.unsupported_topology, "{expected_kind:?}");
            assert!(!semantics.unsupported_presentation, "{expected_kind:?}");
            assert_eq!(semantics.category_visible, Some(true));
            assert_eq!(semantics.value_visible, Some(true));
            assert_eq!(semantics.category_axis_shifted, Some(true));
            let parsed = parsed_chart(&xml);
            assert_eq!(parsed.chart.kind, expected_kind);
            assert_eq!(parsed.category_axis_shifted, Some(true));
            assert!(
                parsed.unsupported_reasons.is_empty(),
                "{expected_kind:?}: {:?}",
                parsed.unsupported_reasons
            );
        }

        let pie = parsed_chart(
            r#"<chartSpace><chart><plotArea><pieChart><varyColors val="1"/></pieChart></plotArea></chart></chartSpace>"#,
        );
        assert_eq!(pie.chart.kind, ChartKind::Pie);
        assert!(pie.unsupported_reasons.is_empty());
    }

    #[test]
    fn chart_category_shifted_position_replays_cross_between_and_calc_defaults() {
        let axes = r#"<catAx><axId val="1"/><axPos val="b"/><crossAx val="2"/></catAx><valAx><axId val="2"/><axPos val="l"/><crossAx val="1"/>"#;
        let base = |cross_between: &str| {
            format!(
                "<chartSpace><chart><plotArea><lineChart><axId val=\"1\"/><axId val=\"2\"/></lineChart>{axes}{cross_between}</valAx></plotArea></chart></chartSpace>"
            )
        };

        let between = parse_chart_axis_semantics(&base("<crossBetween val=\"between\"/>"));
        assert!(!between.unsupported_presentation);
        assert_eq!(between.category_axis_shifted, Some(true));

        let mid_cat = parse_chart_axis_semantics(&base("<crossBetween val=\"midCat\"/>"));
        assert!(!mid_cat.unsupported_presentation);
        assert_eq!(mid_cat.category_axis_shifted, Some(false));

        let omitted = parse_chart_axis_semantics(&base(""));
        assert!(!omitted.unsupported_presentation);
        assert_eq!(omitted.category_axis_shifted, Some(true));
    }

    #[test]
    fn chart_axis_positions_and_non_default_presentation_fail_closed() {
        fn unsupported_reasons(xml: &str) -> Vec<ChartUnsupportedReason> {
            let mut cache_points = 16;
            let mut chart_series = 16;
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series)
                .expect("chart")
                .unsupported_reasons
        }

        let column = r#"<chartSpace><chart><plotArea><barChart><barDir val="col"/><axId val="1"/><axId val="2"/></barChart><catAx><axId val="1"/><axPos val="b"/><crossAx val="2"/></catAx><valAx><axId val="2"/><axPos val="l"/><crossAx val="1"/></valAx></plotArea></chart></chartSpace>"#;
        assert!(!unsupported_reasons(column)
            .contains(&ChartUnsupportedReason::UnsupportedAxisPresentation));

        let horizontal = column
            .replace(r#"val="col""#, r#"val="bar""#)
            .replace(
                r#"<catAx><axId val="1"/><axPos val="b"/>"#,
                r#"<catAx><axId val="1"/><axPos val="l"/>"#,
            )
            .replace(
                r#"<valAx><axId val="2"/><axPos val="l"/>"#,
                r#"<valAx><axId val="2"/><axPos val="b"/>"#,
            );
        assert!(!unsupported_reasons(&horizontal)
            .contains(&ChartUnsupportedReason::UnsupportedAxisPresentation));

        let reversed = horizontal.replace(r#"val="bar""#, r#"val="col""#);
        assert!(unsupported_reasons(&reversed)
            .contains(&ChartUnsupportedReason::UnsupportedAxisPresentation));

        let generated_defaults = column
            .replace(
                r#"<crossAx val="2"/>"#,
                r#"<crossAx val="2"/><crosses val="autoZero"/><auto val="1"/><lblAlgn val="ctr"/><lblOffset val="100"/>"#,
            )
            .replace(
                r#"<crossAx val="1"/>"#,
                r#"<crossAx val="1"/><crosses val="autoZero"/><crossBetween val="between"/>"#,
            );
        assert!(!unsupported_reasons(&generated_defaults)
            .contains(&ChartUnsupportedReason::UnsupportedAxisPresentation));

        for (supported, unsupported) in [
            (r#"crosses val="autoZero""#, r#"crosses val="max""#),
            (r#"crosses val="autoZero""#, r#"crossesAt val="0""#),
            (r#"auto val="1""#, r#"auto val="0""#),
            (r#"lblAlgn val="ctr""#, r#"lblAlgn val="l""#),
            (r#"lblOffset val="100""#, r#"lblOffset val="101""#),
            (
                r#"crossBetween val="between""#,
                r#"crossBetween val="unsupported""#,
            ),
        ] {
            let xml = generated_defaults.replacen(supported, unsupported, 1);
            assert!(
                unsupported_reasons(&xml)
                    .contains(&ChartUnsupportedReason::UnsupportedAxisPresentation),
                "{unsupported}"
            );
        }
    }

    #[test]
    fn chart_text_inheritance_merges_chart_list_paragraph_and_run_properties() {
        let xml = r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><title><tx><rich>
            <a:bodyPr/><a:lstStyle><a:lvl1pPr><a:defRPr sz="1200"><a:latin typeface="List Face"/></a:defRPr></a:lvl1pPr></a:lstStyle>
            <a:p><a:pPr lvl="0"><a:defRPr b="1"/></a:pPr><a:r><a:rPr i="1"><a:solidFill><a:srgbClr val="112233"/></a:solidFill></a:rPr><a:t>X</a:t></a:r></a:p>
        </rich></tx></title><plotArea><pieChart/></plotArea></chart>
        <txPr><a:bodyPr/><a:p><a:pPr><a:defRPr sz="1600"><a:latin typeface="Chart Face"/></a:defRPr></a:pPr></a:p></txPr>
        </chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert!(parsed.unsupported_reasons.is_empty());
        let title = parsed.text_styles.chart_title.unwrap();
        assert_eq!(title.latin_font_family, "List Face");
        assert_eq!(title.size_hundredths_of_point, 1_200);
        assert_eq!(title.color, Color::rgb(0x11, 0x22, 0x33));
        assert!(title.bold);
        assert!(title.italic);

        let category = parsed.text_styles.category_axis_labels.unwrap();
        assert_eq!(category.latin_font_family, "Chart Face");
        assert_eq!(category.size_hundredths_of_point, 1_600);
    }

    #[test]
    fn chart_text_ignores_unpainted_invalid_runs_but_rejects_late_properties() {
        let valid = r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><title><tx><rich><a:bodyPr/><a:p>
            <a:r><a:rPr sz="99"/><a:t/></a:r>
            <a:r><a:rPr sz="1400"><a:latin typeface="Face"/></a:rPr><a:t>X</a:t></a:r>
        </a:p></rich></tx></title><plotArea><pieChart/></plotArea></chart></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(valid, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert!(parsed.unsupported_reasons.is_empty());
        assert_eq!(
            parsed
                .text_styles
                .chart_title
                .as_ref()
                .map(|style| style.size_hundredths_of_point),
            Some(1_400)
        );

        for late in [
            r#"<a:r><a:t>X</a:t><a:rPr b="1"/></a:r>"#,
            r#"<a:r><a:t>X</a:t></a:r><a:pPr lvl="0"/>"#,
        ] {
            let xml = format!(
                r#"<chartSpace><chart><title><tx><rich><a:bodyPr/><a:p>{late}</a:p></rich></tx></title><plotArea><pieChart/></plotArea></chart></chartSpace>"#
            );
            let mut cache_points = 16;
            let mut chart_series = 16;
            let parsed =
                parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
            assert!(parsed
                .unsupported_reasons
                .contains(&ChartUnsupportedReason::UnsupportedTextStyle));
        }
    }

    #[test]
    fn chart_text_color_map_and_fact_budget_are_strict_and_semantic() {
        let theme = parse_theme(&complete_theme_xml(
            "Theme Major",
            "Theme Face",
            "4472C4",
            "102030",
        ));
        assert!(theme.source_valid);
        let entities = "&amp;".repeat(MAX_CHART_TEXT_STYLE_FACTS_PER_ROLE + 1);
        let xml = format!(
            r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><clrMapOvr><overrideClrMapping bg1="lt1" tx1="accent2" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/></clrMapOvr><chart><title><tx><rich><a:bodyPr/><a:p><a:r><a:t>{entities}</a:t></a:r></a:p></rich></tx></title><plotArea><pieChart/></plotArea></chart></chartSpace>"#
        );
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed = parse_chart_with_theme(
            &xml,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
            &theme,
        )
        .unwrap();
        assert!(!parsed.limit_exceeded);
        assert!(parsed.unsupported_reasons.is_empty());
        assert_eq!(
            parsed.text_styles.chart_title.unwrap().color,
            Color::rgb(0x10, 0x20, 0x30)
        );

        let partial = xml.replace(
            r#" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink""#,
            "",
        );
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed = parse_chart_with_theme(
            &partial,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
            &theme,
        )
        .unwrap();
        assert!(parsed
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::UnsupportedTextStyle));
    }

    #[test]
    fn chart_text_fields_use_exact_non_truncating_byte_limits() {
        for (length, retained) in [
            (MAX_XLSX_CHART_TEXT_FIELD_BYTES, true),
            (MAX_XLSX_CHART_TEXT_FIELD_BYTES + 1, false),
        ] {
            let title = "x".repeat(length);
            let xml = format!(
                r#"<chartSpace><chart><title><tx><rich><a:bodyPr/><a:p><a:r><a:t>{title}</a:t></a:r></a:p></rich></tx></title><plotArea><pieChart/></plotArea></chart></chartSpace>"#
            );
            let mut cache_points = 16;
            let mut chart_series = 16;
            let parsed =
                parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
            assert_eq!(
                parsed.chart.title.as_deref(),
                retained.then_some(title.as_str())
            );
            assert_eq!(parsed.limit_exceeded, !retained);
        }
    }

    #[test]
    fn chart_cache_and_series_sidecars_stop_at_exact_budgets() {
        let xml = r#"<chartSpace><chart><plotArea><lineChart>
            <ser><val><numRef><f>S!$A$1:$A$2</f><numCache>
                <pt idx="0"><v>1</v></pt><pt idx="1"><v>2</v></pt>
            </numCache></numRef></val></ser>
            <ser><val><numRef><f>S!$B$1:$B$2</f><numCache>
                <pt idx="0"><v>3</v></pt><pt idx="1"><v>4</v></pt>
            </numCache></numRef></val></ser>
        </lineChart></plotArea></chart></chartSpace>"#;
        let mut cache_points = 2;
        let mut chart_series = 1;
        let parsed =
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert_eq!(cache_points, 0);
        assert_eq!(chart_series, 0);
        assert!(parsed.limit_exceeded);
        assert_eq!(parsed.chart.series.len(), 1);
        assert_eq!(parsed.series_caches.len(), 1);
        assert_eq!(parsed.series_styles.len(), 1);
        assert_eq!(parsed.series_caches[0].values.len(), 2);
    }

    #[test]
    fn unsupported_chart_series_style_metadata_is_typed_and_bounded() {
        let xml = r#"<chartSpace><chart><plotArea><lineChart><ser>
            <marker><symbol val="picture"/><size val="255"/></marker>
            <spPr><a:ln w="20116801"><a:gradFill/><a:prstDash val="dash"/></a:ln></spPr>
            <val><numRef><f>S!$A$1:$A$2</f></numRef></val>
        </ser></lineChart></plotArea></chart></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert_eq!(parsed.series_styles.len(), 1);
        let style = &parsed.series_styles[0];
        assert_eq!(style.marker, ChartMarkerSymbol::Automatic);
        assert_eq!(style.marker_size, None);
        assert_eq!(
            style.losses,
            [
                ChartSeriesStyleLossKind::UnsupportedMarkerSymbol,
                ChartSeriesStyleLossKind::InvalidMarkerSize,
                ChartSeriesStyleLossKind::InvalidLineWidth,
                ChartSeriesStyleLossKind::UnsupportedLinePaint,
            ]
        );
    }

    #[test]
    fn visible_series_legend_and_line_semantics_are_never_dropped_silently() {
        fn parse_with_series_markup(markup: &str) -> ParsedChart {
            let xml = format!(
                r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><plotArea><barChart><barDir val="col"/><ser><idx val="0"/><order val="0"/>{markup}<val><numRef><f>S!$A$1:$A$2</f></numRef></val></ser></barChart></plotArea></chart></chartSpace>"#
            );
            let mut cache_points = 16;
            let mut chart_series = 16;
            parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).expect("chart")
        }

        for markup in [
            "<dPt><idx val=\"0\"/></dPt>",
            "<trendline/>",
            "<errBars/>",
            "<invertIfNegative/>",
            "<pictureOptions/>",
            "<marker><spPr/></marker>",
            r#"<spPr><a:solidFill><a:srgbClr val="112233"/></a:solidFill></spPr>"#,
        ] {
            assert!(parse_with_series_markup(markup)
                .unsupported_reasons
                .contains(&ChartUnsupportedReason::UnsupportedPlotSemantics));
        }

        let line = parse_with_series_markup(
            r#"<spPr><a:ln cap="rnd"><a:solidFill><a:srgbClr val="112233"><a:tint val="50000"/></a:srgbClr></a:solidFill><a:headEnd/></a:ln></spPr>"#,
        );
        assert!(line.series_styles[0]
            .losses
            .contains(&ChartSeriesStyleLossKind::UnsupportedLinePaint));

        let legend = r#"<chartSpace><chart><plotArea><pieChart/></plotArea><legend><spPr/></legend></chart></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed = parse_chart(
            legend,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
        )
        .expect("chart");
        assert!(parsed
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::UnsupportedLegend));
    }

    #[test]
    fn multiple_chart_text_color_transforms_fail_closed() {
        let xml = r#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><title><tx><rich><a:bodyPr/><a:p><a:r><a:rPr><a:solidFill><a:srgbClr val="204080"><a:tint val="50000"/><a:shade val="50000"/></a:srgbClr></a:solidFill><a:latin typeface="Face"/></a:rPr><a:t>X</a:t></a:r></a:p></rich></tx></title><plotArea><pieChart/></plotArea></chart></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).expect("chart");
        assert!(parsed
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::UnsupportedTextStyle));
    }

    #[test]
    fn chart_space_fill_and_axis_gridline_presence_are_retained() {
        let xml = r#"<chartSpace><chart><plotArea><lineChart><ser>
            <spPr><a:ln><a:noFill/></a:ln></spPr>
            <cat><strRef><f>S!$A$1:$A$2</f></strRef></cat>
            <val><numRef><f>S!$B$1:$B$2</f></numRef></val>
        </ser></lineChart>
        <catAx><majorGridlines/></catAx><valAx/>
        </plotArea></chart>
        <spPr>
            <a:solidFill><a:srgbClr val="123456"/></a:solidFill>
            <a:ln><a:noFill/></a:ln>
        </spPr></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();

        assert_eq!(
            parsed.frame_fill,
            ChartFrameFill::Solid(Color::rgb(0x12, 0x34, 0x56))
        );
        assert_eq!(
            parsed.frame_style_losses,
            [ChartFrameStyleLossKind::UnsupportedPaint]
        );
        assert!(parsed.category_major_gridlines);
        assert!(!parsed.value_major_gridlines);
        assert!(!parsed.series_styles[0].line_visible);
        assert_eq!(parsed.series_styles[0].line_width_emu, Some(12_700));

        let no_fill = xml.replace(
            r#"<a:solidFill><a:srgbClr val="123456"/></a:solidFill>"#,
            "<a:noFill/>",
        );
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed = parse_chart(
            &no_fill,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
        )
        .unwrap();
        assert_eq!(parsed.frame_fill, ChartFrameFill::NoFill);

        let unsupported = no_fill.replace("<a:noFill/>", "<a:gradFill/>");
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed = parse_chart(
            &unsupported,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
        )
        .unwrap();
        assert_eq!(parsed.frame_fill, ChartFrameFill::Automatic);
        assert_eq!(
            parsed.frame_style_losses,
            [ChartFrameStyleLossKind::UnsupportedPaint]
        );
    }

    #[test]
    fn chart_series_line_width_enforces_ooxml_bounds() {
        for (width, expected, invalid) in [
            (None, Some(12_700), false),
            (Some("0"), Some(0), false),
            (Some("20116800"), Some(20_116_800), false),
            (Some("-1"), None, true),
            (Some("20116801"), None, true),
            (Some("wide"), None, true),
        ] {
            let width = width.map_or(String::new(), |value| format!(r#" w="{value}""#));
            let xml = format!(
                r#"<chartSpace><chart><plotArea><lineChart><ser>
                    <spPr><a:ln{width}/></spPr>
                    <val><numRef><f>S!$A$1:$A$2</f></numRef></val>
                </ser></lineChart></plotArea></chart></chartSpace>"#
            );
            let mut cache_points = 16;
            let mut chart_series = 16;
            let parsed =
                parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
            let style = &parsed.series_styles[0];
            assert_eq!(style.line_width_emu, expected, "{width}");
            assert_eq!(
                style
                    .losses
                    .contains(&ChartSeriesStyleLossKind::InvalidLineWidth),
                invalid,
                "{width}"
            );
        }
    }

    #[test]
    fn bar_chart_direction_is_retained_without_changing_chart_kind() {
        for (value, expected) in [
            ("col", ChartBarDirection::Column),
            ("bar", ChartBarDirection::Horizontal),
        ] {
            let xml = format!(
                r#"<chartSpace><chart><plotArea><barChart><barDir val="{value}"/><ser><val><numRef><f>Data!$A$1:$A$2</f></numRef></val></ser></barChart></plotArea></chart></chartSpace>"#
            );
            let mut cache_points = 16;
            let mut chart_series = 16;
            let parsed =
                parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
            assert_eq!(parsed.chart.kind, ChartKind::Bar);
            assert_eq!(parsed.bar_direction, expected);
        }
    }

    #[test]
    fn chart_plot_order_style_and_legend_semantics_fail_closed() {
        let supported = r#"<chartSpace><style val="2"/><chart><plotArea>
            <lineChart><grouping val="standard"/><ser><idx val="0"/><order val="0"/>
                <val><numRef><f>Data!$A$1:$A$2</f></numRef></val>
            </ser><axId val="1"/><axId val="2"/></lineChart>
            <catAx><axId val="1"/><crossAx val="2"/></catAx>
            <valAx><axId val="2"/><crossAx val="1"/></valAx>
        </plotArea><legend><legendPos val="r"/><overlay val="0"/></legend>
        <plotVisOnly val="1"/><dispBlanksAs val="gap"/></chart></chartSpace>"#;

        let parse_reasons = |xml: &str| {
            let mut cache_points = 16;
            let mut chart_series = 16;
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series)
                .unwrap()
                .unsupported_reasons
        };

        assert!(parse_reasons(supported).is_empty());
        for xml in [
            supported.replace(r#"grouping val="standard""#, r#"grouping val="stacked""#),
            supported.replace(r#"order val="0""#, r#"order val="1""#),
            supported.replace(r#"<order val="0"/>"#, ""),
            supported.replace(
                r#"<grouping val="standard"/>"#,
                r#"<grouping val="standard"/><overlap val="0"/>"#,
            ),
        ] {
            assert!(
                parse_reasons(&xml).contains(&ChartUnsupportedReason::UnsupportedPlotSemantics),
                "{xml}"
            );
        }

        let nondefault_style = supported.replace(r#"style val="2""#, r#"style val="3""#);
        assert!(parse_reasons(&nondefault_style)
            .contains(&ChartUnsupportedReason::UnsupportedChartStyle));

        for xml in [
            supported.replace(r#"legendPos val="r""#, r#"legendPos val="l""#),
            supported.replace(r#"overlay val="0""#, r#"overlay val="1""#),
            supported.replace(
                r#"<legendPos val="r"/>"#,
                r#"<legendPos val="r"/><legendEntry><idx val="0"/></legendEntry>"#,
            ),
        ] {
            assert!(
                parse_reasons(&xml).contains(&ChartUnsupportedReason::UnsupportedLegend),
                "{xml}"
            );
        }
    }

    #[test]
    fn chart_kind_specific_plot_defaults_are_exact() {
        let supported = r#"<chartSpace><chart><plotArea>
            <bubbleChart><varyColors val="0"/><bubble3D val="0"/>
                <ser><idx val="0"/><order val="0"/><xVal><numRef><f>S!$A$1:$A$2</f></numRef></xVal><yVal><numRef><f>S!$B$1:$B$2</f></numRef></yVal><bubbleSize><numRef><f>S!$C$1:$C$2</f></numRef></bubbleSize></ser>
                <axId val="1"/><axId val="2"/>
            </bubbleChart>
            <valAx><axId val="1"/><crossAx val="2"/></valAx>
            <valAx><axId val="2"/><crossAx val="1"/></valAx>
        </plotArea></chart></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed = parse_chart(
            supported,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
        )
        .unwrap();
        assert!(parsed.unsupported_reasons.is_empty());

        for replacement in [
            (r#"bubble3D val="0""#, r#"bubble3D val="1""#),
            (r#"varyColors val="0""#, r#"varyColors val="1""#),
        ] {
            let xml = supported.replace(replacement.0, replacement.1);
            let mut cache_points = 16;
            let mut chart_series = 16;
            let parsed =
                parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
            assert!(parsed
                .unsupported_reasons
                .contains(&ChartUnsupportedReason::UnsupportedPlotSemantics));
        }
    }

    #[test]
    fn unsupported_combo_3d_pivot_and_external_charts_are_explicit() {
        let xml = r#"<chartSpace><pivotSource/><externalData/><chart><view3D/><plotArea>
            <barChart><ser><val><numRef><f>Data!$A$1:$A$2</f></numRef></val></ser></barChart>
            <lineChart><ser><val><numRef><f>'[Other.xlsx]Data'!$B$1:$B$2</f></numRef></val></ser></lineChart>
        </plotArea></chart></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert_eq!(parsed.chart.kind, ChartKind::Bar);
        assert_eq!(parsed.chart.series.len(), 2);
        for reason in [
            ChartUnsupportedReason::Combo,
            ChartUnsupportedReason::ThreeDimensional,
            ChartUnsupportedReason::Pivot,
            ChartUnsupportedReason::ExternalData,
        ] {
            assert!(parsed.unsupported_reasons.contains(&reason), "{reason:?}");
        }

        let mut cache_points = 16;
        let mut chart_series = 16;
        let surface = parse_chart(
            r#"<chartSpace><chart><plotArea><surface3DChart><ser><val><numRef><f>Data!$A$1:$A$2</f></numRef></val></ser></surface3DChart></plotArea></chart></chartSpace>"#,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
        )
        .unwrap();
        assert_eq!(surface.chart.kind, ChartKind::Area);
        assert!(surface
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::ThreeDimensional));
        assert!(surface
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::UnsupportedKind));
    }

    #[test]
    fn sheet_local_filter_database_defined_name_surfaces_autofilter() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm._FilterDatabase" localSheetId="0">Data!$B$3:$E$10</definedName></definedNames></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();

        assert!(wb.defined_names().is_empty());
        assert_eq!(wb.sheets[0].autofilter_range(), Some((2, 1, 9, 4)));
        assert_eq!(wb.sheets[0].page_setup(), None);
    }

    /// End-to-end `.xlsx` read: package document properties surface through the
    /// public `Workbook::properties` field instead of remaining writer-only.
    #[test]
    fn reads_xlsx_doc_properties_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
            (
                "docProps/core.xml",
                r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"><dc:title>Quarterly Report</dc:title><dc:subject>Procurement</dc:subject><dc:creator>rxls reader</dc:creator><cp:keywords>bid,report</cp:keywords><dc:description>Public bid report</dc:description><cp:lastModifiedBy>reviewer</cp:lastModifiedBy><dcterms:created>2024-01-02T03:04:05Z</dcterms:created></cp:coreProperties>"#,
            ),
            (
                "docProps/app.xml",
                r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>Excel</Application><Company>ACME</Company></Properties>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();

        assert_eq!(wb.properties.title.as_deref(), Some("Quarterly Report"));
        assert_eq!(wb.properties.subject.as_deref(), Some("Procurement"));
        assert_eq!(wb.properties.creator.as_deref(), Some("rxls reader"));
        assert_eq!(wb.properties.keywords.as_deref(), Some("bid,report"));
        assert_eq!(
            wb.properties.description.as_deref(),
            Some("Public bid report")
        );
        assert_eq!(wb.properties.last_modified_by.as_deref(), Some("reviewer"));
        assert_eq!(
            wb.properties.created.as_deref(),
            Some("2024-01-02T03:04:05Z")
        );
        assert_eq!(wb.properties.company.as_deref(), Some("ACME"));
    }

    #[test]
    fn chartsheet_is_not_marked_as_worksheet_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Data" r:id="rId1"/><sheet name="Chart" r:id="rId2"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>ok</t></is></c></row></sheetData></worksheet>"#,
            ),
            ("xl/chartsheets/sheet1.xml", r#"<chartsheet/>"#),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();

        assert_eq!(wb.sheets.len(), 2);
        assert!(wb.sheets[0].is_worksheet);
        assert!(!wb.sheets[1].is_worksheet);
        assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Text("ok".into())));
        assert_eq!(wb.text(), "# Data\nok\n");
    }

    #[test]
    fn dangling_sheet_ref_without_relationship_is_not_marked_as_worksheet() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/><sheet name="Module" r:id=""/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();

        assert_eq!(wb.sheets.len(), 2);
        assert!(wb.sheets[0].is_worksheet);
        assert!(!wb.sheets[1].is_worksheet);
        assert_eq!(wb.sheets[1].sheet_type(), SheetType::Vba);
        assert_eq!(wb.text(), "# Sheet1\n\n");
    }

    #[test]
    fn external_worksheet_relationship_never_dispatches_to_a_local_zip_part() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (name, body) in [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="External" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml" TargetMode="External"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>must-not-load</t></is></c></row></sheetData></worksheet>"#,
            ),
        ] {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }

        let workbook = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        assert_eq!(workbook.sheets[0].sheet_type(), SheetType::Vba);
        assert!(workbook.sheets[0].cells.is_empty());
    }

    #[test]
    fn internal_relationship_resolution_uses_only_the_uri_path_component() {
        assert_eq!(
            resolve_internal_relationship_part("xl/worksheets/sheet1.xml", "#Sheet2!A1"),
            Some("xl/worksheets/sheet1.xml".to_string())
        );
        assert_eq!(
            resolve_internal_relationship_part("xl/workbook.xml", "worksheets/sheet1.xml?q#f"),
            Some("xl/worksheets/sheet1.xml".to_string())
        );
        assert_eq!(
            resolve_internal_relationship_part("xl\\workbook.xml", "worksheets\\sheet1.xml"),
            Some("xl/worksheets/sheet1.xml".to_string())
        );
        assert_eq!(
            resolve_internal_relationship_part("xl/a.xml", "%2e%2e/kept.xml"),
            Some("xl/%2e%2e/kept.xml".to_string())
        );
        assert_eq!(
            resolve_internal_relationship_part("xl/workbook.xml", "https://evil.invalid/a.xml"),
            None
        );
        assert_eq!(
            resolve_internal_relationship_part("xl/workbook.xml", "//evil.invalid/a.xml"),
            None
        );
    }

    #[test]
    fn office_document_fragment_resolves_but_absolute_internal_uri_is_rejected() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let package = |target: &str, workbook_part: &str| {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opt = SimpleFileOptions::default();
            zw.start_file("_rels/.rels", opt).unwrap();
            zw.write_all(
                format!(r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{target}"/></Relationships>"#).as_bytes(),
            )
            .unwrap();
            zw.start_file(workbook_part, opt).unwrap();
            zw.write_all(b"<workbook><sheets/></workbook>").unwrap();
            zw.finish().unwrap().into_inner()
        };

        assert!(Workbook::open(&package("xl/workbook.xml#Sheet1", "xl/workbook.xml")).is_ok());
        assert!(Workbook::open(&package(
            "https://evil.invalid/workbook.xml",
            "https:/evil.invalid/workbook.xml"
        ))
        .is_err());
    }

    #[test]
    fn sheet_rels_path_inserts_rels_segment() {
        assert_eq!(
            sheet_rels_path("xl/worksheets/sheet1.xml"),
            "xl/worksheets/_rels/sheet1.xml.rels"
        );
        assert_eq!(sheet_rels_path("sheet1.xml"), "_rels/sheet1.xml.rels");
    }

    /// End-to-end `.xlsx` read: a worksheet `<hyperlink>` whose `r:id` resolves
    /// through the worksheet rels surfaces via the public `hyperlinks()` accessor
    /// as `(row, col, url)`.
    #[test]
    fn reads_xlsx_hyperlinks_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Links" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="2"><c r="B2" t="inlineStr"><is><t>click</t></is></c></row></sheetData><hyperlinks><hyperlink ref="B2:B4" r:id="rId1"/></hyperlinks></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/" TargetMode="External"/></Relationships>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        // The `B2:B4` range expands to every cell (0-based rows 1..=3, col 1), each
        // resolved to the URL from the worksheet rels.
        let url = "https://example.com/".to_string();
        assert_eq!(
            wb.sheets[0].hyperlinks(),
            &[(1u32, 1u16, url.clone()), (2, 1, url.clone()), (3, 1, url)]
        );
    }

    #[test]
    fn normalize_part_target_resolves_relative() {
        assert_eq!(
            normalize_part_target("xl/worksheets/sheet1.xml", "../comments1.xml"),
            "xl/comments1.xml"
        );
        assert_eq!(
            normalize_part_target("xl/worksheets/sheet1.xml", "comments1.xml"),
            "xl/worksheets/comments1.xml"
        );
        assert_eq!(
            normalize_part_target("xl/worksheets/sheet1.xml", "/xl/comments1.xml"),
            "xl/comments1.xml"
        );
        assert_eq!(
            normalize_part_target("xl/drawings/drawing1.xml", "../../xl/charts/chart1.xml"),
            "xl/charts/chart1.xml"
        );
        assert_eq!(
            normalize_part_target("xl/drawings/drawing1.xml", "../../../xl/charts/chart1.xml"),
            "xl/charts/chart1.xml"
        );
        assert_eq!(
            normalize_part_target("xl/drawings/drawing1.xml", "/../../xl/charts/chart1.xml"),
            "xl/charts/chart1.xml"
        );
    }

    #[test]
    fn parse_comments_resolves_author_and_ref() {
        let xml = r#"<comments>
            <authors><author>Alice</author><author>Bob</author></authors>
            <commentList>
                <comment ref="B2" authorId="1"><text><t>hello </t><t>world</t></text></comment>
                <comment ref="A1" authorId="0"><text><r><t>note</t></r></text></comment>
            </commentList>
        </comments>"#;
        let cs = parse_comments(xml);
        assert_eq!(cs.len(), 2);
        assert_eq!((cs[0].row, cs[0].col), (1, 1)); // B2
        assert_eq!(cs[0].text, "hello world");
        assert_eq!(cs[0].author.as_deref(), Some("Bob"));
        assert_eq!((cs[1].row, cs[1].col), (0, 0)); // A1
        assert_eq!(cs[1].text, "note");
        assert_eq!(cs[1].author.as_deref(), Some("Alice"));
    }

    /// End-to-end `.xlsx` read: a worksheet referencing a `comments1.xml` part
    /// via its rels (relationship Type `.../comments`) surfaces the notes via the
    /// public `comments()` accessor as `(row, col, text, author)`.
    #[test]
    fn reads_xlsx_comments_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Notes" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="3"><c r="C3" t="inlineStr"><is><t>x</t></is></c></row></sheetData></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing" Target="../drawings/vmlDrawing1.vml"/></Relationships>"#,
            ),
            (
                "xl/comments1.xml",
                r#"<comments><authors><author>심사위원</author></authors><commentList><comment ref="C3" authorId="0"><text><t>검토 필요</t></text></comment></commentList></comments>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let cs = wb.sheets[0].comments();
        assert_eq!(cs.len(), 1);
        // C3 → 0-based (row 2, col 2).
        assert_eq!((cs[0].row, cs[0].col), (2, 2));
        assert_eq!(cs[0].text, "검토 필요");
        assert_eq!(cs[0].author.as_deref(), Some("심사위원"));
    }

    #[test]
    fn parse_table_reads_name_range_columns() {
        // `displayName` is preferred over `name`; `ref` → 0-based inclusive range;
        // `<tableColumn name>` list → header columns; `<tableStyleInfo name>` → style.
        let xml = r#"<table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="Table1" displayName="가격표" ref="A1:C3"><autoFilter ref="A1:C3"/><tableColumns count="3"><tableColumn id="1" name="품목"/><tableColumn id="2" name="단가"/><tableColumn id="3" name="수량"/></tableColumns><tableStyleInfo name="TableStyleMedium2"/></table>"#;
        let parsed = parse_table(xml).unwrap();
        let t = parsed.table;
        assert_eq!(t.name, "가격표");
        assert_eq!(t.range, (0, 0, 2, 2)); // A1:C3
        assert_eq!(t.columns, vec!["품목", "단가", "수량"]);
        assert_eq!(t.style.as_deref(), Some("TableStyleMedium2"));
    }

    /// End-to-end `.xlsx` read: a worksheet referencing a `tables/table1.xml` part
    /// via its rels (relationship Type `.../table`) surfaces the table via the
    /// public `tables()` accessor with its name, range, and header columns.
    #[test]
    fn reads_xlsx_tables_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rIdStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
            ),
            (
                "xl/styles.xml",
                r#"<styleSheet><dxfs count="1"><dxf><font><b/><color rgb="FFFFFFFF"/></font><fill><patternFill patternType="solid"><fgColor rgb="FF123456"/></patternFill></fill><border><bottom style="medium"><color rgb="FFABCDEF"/></bottom></border><alignment horizontal="center" wrapText="1"/></dxf></dxfs><tableStyles count="1" defaultTableStyle="NamedBlue"><tableStyle name="NamedBlue" pivot="0" count="1"><tableStyleElement type="headerRow" dxfId="0"/></tableStyle></tableStyles></styleSheet>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>품목</t></is></c></row></sheetData></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
            ),
            (
                "xl/tables/table1.xml",
                r#"<table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="Table1" displayName="가격표" ref="A1:B2"><tableColumns count="2"><tableColumn id="1" name="품목"/><tableColumn id="2" name="단가"/></tableColumns><tableStyleInfo name="NamedBlue"/></table>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let tables = wb.sheets[0].tables();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "가격표");
        assert_eq!(tables[0].range, (0, 0, 1, 1)); // A1:B2
        assert_eq!(tables[0].columns, vec!["품목", "단가"]);
        let header = wb.sheets[0]
            .table_header_styles()
            .get("가격표")
            .expect("imported named table header style");
        assert_eq!(header.fill, Some(Color::rgb(0x12, 0x34, 0x56)));
        assert_eq!(
            header.font.as_ref().and_then(|font| font.color),
            Some(Color::rgb(0xFF, 0xFF, 0xFF))
        );
        assert!(header.font.as_ref().is_some_and(|font| font.bold));
        assert_eq!(
            header.border.as_ref().map(|border| border.bottom),
            Some(BorderStyle::Medium)
        );
        assert_eq!(
            header
                .border
                .as_ref()
                .and_then(|border| border.bottom_color),
            Some(Color::rgb(0xAB, 0xCD, 0xEF))
        );
        assert_eq!(
            header.align.as_ref().and_then(|align| align.horizontal),
            Some(HAlign::Center)
        );
        assert!(header.align.as_ref().is_some_and(|align| align.wrap));
    }

    #[test]
    fn built_in_table_style_header_uses_theme_accent() {
        let mut theme = ThemeColors::default();
        theme.colors[4] = Some(Color::rgb(1, 2, 3));
        let built_in = built_in_table_style("TableStyleMedium2", &theme).unwrap();
        let style = built_in_table_header_style("TableStyleMedium2", &theme).unwrap();
        assert_eq!(style.fill, Some(Color::rgb(1, 2, 3)));
        assert_eq!(
            style.font.as_ref().and_then(|font| font.color),
            Some(Color::rgb(0xFF, 0xFF, 0xFF))
        );
        assert!(style.font.as_ref().is_some_and(|font| font.bold));
        for region in [
            TableStyleRegion::HeaderRow,
            TableStyleRegion::TotalRow,
            TableStyleRegion::FirstColumn,
            TableStyleRegion::LastColumn,
            TableStyleRegion::FirstRowStripe,
            TableStyleRegion::FirstColumnStripe,
        ] {
            assert!(built_in.definition.get(region).is_some(), "{region:?}");
        }
        assert!(built_in_table_header_style("TableStyleMedium29", &theme).is_none());
    }

    #[test]
    fn direct_xf_masks_retain_explicit_resets_and_complete_builtin_formats() {
        let xml = r#"<styleSheet>
            <fonts count="2"><font><name val="Base"/></font><font><b/></font></fonts>
            <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
            <borders count="1"><border/></borders>
            <cellXfs count="3">
                <xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="0"/>
                <xf numFmtId="0" fontId="0" fillId="0" borderId="0"
                    applyFont="1" applyBorder="1" applyNumberFormat="1"
                    applyAlignment="1"><alignment wrapText="0"/></xf>
                <xf numFmtId="46" fontId="0" fillId="0" borderId="0"
                    applyNumberFormat="1"/>
            </cellXfs>
        </styleSheet>"#;
        let styles = parse_styles(xml, &ThemeColors::default());

        let disabled = styles.cell_style_overlay(0).expect("disabled overlay");
        assert!(!disabled.replace_font, "explicit applyFont=0 must win");
        assert!(overlay_is_empty(disabled));

        let reset = styles.cell_style_overlay(1).expect("reset overlay");
        assert!(reset.replace_font);
        assert!(reset.replace_border);
        assert!(reset.replace_num_fmt);
        assert!(reset.replace_alignment);
        assert!(reset
            .style
            .font
            .as_ref()
            .is_some_and(|font| font.name.as_deref() == Some("Base") && !font.bold));
        assert_eq!(reset.style.border, None, "borderId=0 clears the border");
        assert_eq!(reset.style.num_fmt, None, "numFmtId=0 means General");
        assert_eq!(reset.style.align, Some(Alignment::default()));

        assert_eq!(styles.cell_styles[2].num_fmt.as_deref(), Some("[h]:mm:ss"));
        assert_eq!(
            styles.cell_style_overlays[2].style.num_fmt.as_deref(),
            Some("[h]:mm:ss")
        );
    }

    #[test]
    fn xlsx_normal_font_provenance_requires_exact_integral_source_agreement() {
        let styles = |first_size: &str,
                      normal_size: &str,
                      first_family: &str,
                      normal_family: &str,
                      normal_descriptor: &str| {
            parse_styles(
                &format!(
                    r#"<styleSheet><fonts count="2"><font><sz val="{first_size}"/><name val="{first_family}"/></font><font><sz val="{normal_size}"/><name val="{normal_family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="1"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle {normal_descriptor} xfId="0"/></cellStyles></styleSheet>"#
                ),
                &ThemeColors::default(),
            )
        };

        for (value, expected) in [
            ("1", 1),
            ("00011", 11),
            ("11.0", 11),
            ("1.1e1", 11),
            ("4.09E2", 409),
        ] {
            assert_eq!(
                styles(value, value, "Verified", "Verified", r#"name="Normal""#)
                    .xlsx_normal_font_size_pt,
                Some(expected),
                "{value}"
            );
        }
        assert_eq!(
            styles("12", "12", "Verified", "Verified", r#"builtinId="0""#).xlsx_normal_font_size_pt,
            Some(12),
            "built-in Normal provenance must not depend on an English name"
        );

        for value in [
            "0",
            "-1",
            "11.5",
            "409.00000000000000001",
            "409.55",
            "410",
            "1e309",
            "NaN",
        ] {
            assert_eq!(
                styles(value, value, "Verified", "Verified", r#"name="Normal""#)
                    .xlsx_normal_font_size_pt,
                None,
                "{value}"
            );
        }
        assert_eq!(
            styles("12", "11", "Verified", "Verified", r#"name="Normal""#).xlsx_normal_font_size_pt,
            None,
            "the first cell XF and Normal style must agree on source size"
        );
        assert_eq!(
            styles("11", "11", "First", "Normal", r#"name="Normal""#).xlsx_normal_font_size_pt,
            None,
            "the first cell XF and Normal style must resolve to the same font"
        );
        assert_eq!(
            parse_styles(
                r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Verified"/></font></fonts><cellXfs count="1"><xf fontId="0"/></cellXfs></styleSheet>"#,
                &ThemeColors::default(),
            )
            .xlsx_normal_font_size_pt,
            None,
            "a missing named/built-in Normal style is ambiguous"
        );
        assert_eq!(
            parse_styles(
                r#"<styleSheet><fonts count="1"><font><sz val="11"/><sz val="12"/><name val="Verified"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
                &ThemeColors::default(),
            )
            .xlsx_normal_font_size_pt,
            None,
            "duplicate source size declarations are ambiguous"
        );
        assert_eq!(
            parse_styles(
                r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="2"><cellStyle name="Normal" xfId="0"/><cellStyle builtinId="0" xfId="0"/></cellStyles></styleSheet>"#,
                &ThemeColors::default(),
            )
            .xlsx_normal_font_size_pt,
            None,
            "duplicate Normal declarations are ambiguous"
        );
        assert_eq!(
            parse_styles(
                r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" builtinId="1" xfId="0"/></cellStyles></styleSheet>"#,
                &ThemeColors::default(),
            )
            .xlsx_normal_font_size_pt,
            None,
            "a contradictory built-in identifier must not identify Normal"
        );
    }

    #[test]
    fn xlsx_cell_xf_font_provenance_rejects_rounded_and_ambiguous_sources() {
        let styles = parse_styles(
            r#"<styleSheet>
                <fonts count="6">
                    <font><sz val="11"/><name val="Normal"/></font>
                    <font><sz val="14"/><name val="Exact"/></font>
                    <font><sz val="13.5"/><name val="Fractional"/></font>
                    <font><sz val="13.5"/><sz val="14"/><name val="Duplicate"/></font>
                    <font><sz val="malformed"/><name val="Malformed"/></font>
                    <font><sz val="410"/><name val="OutOfRange"/></font>
                </fonts>
                <cellXfs count="9">
                    <xf fontId="0"/>
                    <xf fontId="1" applyFont="1"/>
                    <xf fontId="2" applyFont="1"/>
                    <xf fontId="3" applyFont="1"/>
                    <xf fontId="4" applyFont="1"/>
                    <xf fontId="5" applyFont="1"/>
                    <xf fontId="1" fontId="0" applyFont="1"/>
                    <xf fontId="1" applyFont="0"/>
                    <xf applyFont="1"/>
                </cellXfs>
            </styleSheet>"#,
            &ThemeColors::default(),
        );

        assert_eq!(
            styles.xlsx_cell_xf_font_sizes_pt,
            [Some(11), Some(14), None, None, None, None, None, None, None,]
        );
    }

    #[test]
    fn xlsx_cell_xf_font_provenance_validates_implicit_parent_references() {
        let styles = parse_styles(
            r#"<styleSheet>
                <fonts count="1">
                    <font><sz val="11"/><name val="Verified"/></font>
                </fonts>
                <cellStyleXfs count="1">
                    <xf fontId="0"/>
                </cellStyleXfs>
                <cellXfs count="6">
                    <xf fontId="0" xfId="0"/>
                    <xf fontId="0"/>
                    <xf fontId="0" xfId="1"/>
                    <xf fontId="0" xfId="malformed"/>
                    <xf fontId="0" xfId="0" xfId="0"/>
                    <xf fontId="0" xfId="99" applyFont="1"/>
                </cellXfs>
            </styleSheet>"#,
            &ThemeColors::default(),
        );

        assert_eq!(
            styles.xlsx_cell_xf_font_sizes_pt,
            [Some(11), Some(11), None, None, None, Some(11)]
        );

        let missing_parent_table = parse_styles(
            r#"<styleSheet>
                <fonts count="1">
                    <font><sz val="11"/><name val="Verified"/></font>
                </fonts>
                <cellXfs count="1">
                    <xf fontId="0" xfId="0"/>
                </cellXfs>
            </styleSheet>"#,
            &ThemeColors::default(),
        );
        assert_eq!(missing_parent_table.xlsx_cell_xf_font_sizes_pt, [None]);
    }

    #[test]
    fn duplicate_cells_clear_stale_coordinate_sidecars() {
        let base = CellStyle::new().font_name("Base").set_font_size(11);
        let direct = CellStyle::new().font_name("Direct").set_font_size(14);
        let styles = Styles {
            cell_styles: vec![base, direct.clone()],
            xlsx_cell_xf_font_sizes_pt: vec![Some(11), Some(14)],
            cell_style_overlays: vec![
                CellStyleOverlay::default(),
                CellStyleOverlay {
                    style: direct,
                    replace_font: true,
                    ..CellStyleOverlay::default()
                },
            ],
            ..Styles::default()
        };
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            r#"<worksheet><sheetData><row r="1">
                <c r="A1" s="1" t="inlineStr"><is><r><rPr><b/></rPr><t>first</t></r></is></c>
                <c r="A1" t="inlineStr"><is><t>last</t></is></c>
            </row></sheetData></worksheet>"#,
            &[],
            &styles,
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.cells.len(), 2);
        assert_eq!(
            parsed.cells.last().map(|cell| cell.text.as_str()),
            Some("last")
        );
        assert_eq!(
            parsed.cells.last().and_then(|cell| cell.xlsx_font_size_pt),
            Some(11)
        );
        assert!(!parsed.direct_cell_formats.contains_key(&(0, 0)));
        assert!(!parsed.rich.contains_key(&(0, 0)));
    }

    #[test]
    fn xlsx_normal_font_provenance_fails_closed_on_malformed_metadata() {
        let invalid_documents = [
            (
                "malformed built-in identifier",
                r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" builtinId="bogus" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "duplicate source-size attribute",
                r#"<styleSheet><fonts><font><sz val="11" val="12"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "duplicate Normal-style reference",
                r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0" xfId="1"/></cellStyles></styleSheet>"#,
            ),
            (
                "duplicate local-name Normal-style reference",
                r#"<styleSheet xmlns:p="urn:test"><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0" p:xfId="1"/></cellStyles></styleSheet>"#,
            ),
            (
                "duplicate font table",
                r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><fonts/><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "nested font table",
                r#"<styleSheet><fonts><fonts/><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "font table below an extension",
                r#"<styleSheet><ext><fonts><font><sz val="11"/><name val="Verified"/></font></fonts></ext><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "font record below an extension",
                r#"<styleSheet><fonts><ext><font><sz val="11"/><name val="Verified"/></font></ext></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "font properties below an extension",
                r#"<styleSheet><fonts><font><ext><sz val="11"/><name val="Verified"/></ext></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "duplicate cell XF table",
                r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellXfs/><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "nested cell-style XF table",
                r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><cellStyleXfs><xf fontId="0"/></cellStyleXfs></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "cell-style XF below an extension",
                r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><ext><xf fontId="0"/></ext></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "cell XF below an extension",
                r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><ext><xf fontId="0" xfId="0"/></ext></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "Normal style below an extension",
                r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><ext><cellStyle name="Normal" xfId="0"/></ext></cellStyles></styleSheet>"#,
            ),
            (
                "cell XF table below an extension",
                r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><ext><cellXfs><xf fontId="0" xfId="0"/></cellXfs></ext><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#,
            ),
            (
                "truncated Normal-style table",
                r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font></fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/>"#,
            ),
        ];

        for (case, xml) in invalid_documents {
            assert_eq!(
                parse_styles(xml, &ThemeColors::default()).xlsx_normal_font_size_pt,
                None,
                "{case}"
            );
        }

        let truncated_fonts =
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font>"#;
        let mut losses = Vec::new();
        let (_, _, complete) =
            parse_font_table(truncated_fonts, &ThemeColors::default(), &[], &mut losses);
        assert!(!complete, "an open font table must not retain provenance");
    }

    #[test]
    fn xlsx_normal_font_provenance_rejects_over_limit_font_tables() {
        let overflow = "<font/>".repeat(MAX_XLSX_STYLE_RECORDS);
        let xml = format!(
            r#"<styleSheet><fonts><font><sz val="11"/><name val="Verified"/></font>{overflow}</fonts><cellStyleXfs><xf fontId="0"/></cellStyleXfs><cellXfs><xf fontId="0" xfId="0"/></cellXfs><cellStyles><cellStyle name="Normal" xfId="0"/></cellStyles></styleSheet>"#
        );
        let mut losses = Vec::new();
        let (fonts, exact_sizes, complete) =
            parse_font_table(&xml, &ThemeColors::default(), &[], &mut losses);

        assert_eq!(fonts.len(), MAX_XLSX_STYLE_RECORDS);
        assert_eq!(exact_sizes.first(), Some(&Some(11)));
        assert!(!complete);
        assert_eq!(
            verified_xlsx_normal_font_size(&xml, &fonts, &exact_sizes, complete),
            None,
            "retained leading records must not be trusted after table truncation"
        );
    }

    #[test]
    fn xlsx_style_table_limits_are_bounded_and_typed() {
        let colors = r#"<rgbColor rgb="FF010203"/>"#.repeat(MAX_XLSX_INDEXED_COLORS + 1);
        let overlong_format = "0".repeat(MAX_XLSX_FORMAT_CODE_BYTES + 1);
        let xml = format!(
            r#"<styleSheet><numFmts count="1"><numFmt numFmtId="164" formatCode="{overlong_format}"/></numFmts><colors><indexedColors>{colors}</indexedColors></colors><cellXfs count="1"><xf numFmtId="0"/></cellXfs></styleSheet>"#
        );
        let styles = parse_styles(&xml, &ThemeColors::default());
        assert!(styles.custom.is_empty());
        assert_eq!(styles.indexed_colors.len(), MAX_XLSX_INDEXED_COLORS);
        assert!(styles
            .losses
            .iter()
            .any(|loss| loss.kind == StyleLossKind::LimitExceeded && loss.occurrences >= 2));

        let mut records = vec![(); MAX_XLSX_STYLE_RECORDS];
        let mut losses = Vec::new();
        retain_xlsx_style_record(&mut records, (), &mut losses);
        assert_eq!(records.len(), MAX_XLSX_STYLE_RECORDS);
        assert_eq!(
            losses,
            vec![StyleLoss {
                kind: StyleLossKind::LimitExceeded,
                occurrences: 1,
            }]
        );
    }

    #[test]
    fn empty_direct_xf_mask_still_prevents_full_style_fallback() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
            ),
            (
                "xl/styles.xml",
                r#"<styleSheet><fonts count="2"><font><name val="Base"/></font><font><b/></font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="0"/></cellXfs></styleSheet>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>1</v></c></row></sheetData></worksheet>"#,
            ),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        let workbook = Workbook::open(&zip.finish().unwrap().into_inner()).unwrap();
        let sheet = &workbook.sheets[0];
        assert!(sheet
            .direct_cell_formats
            .get(&(0, 0))
            .is_some_and(overlay_is_empty));
        let font = sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.font)
            .expect("resolved base font");
        assert_eq!(font.name.as_deref(), Some("Base"));
        assert!(!font.bold, "explicit applyFont=0 must not use fontId=1");
    }

    #[test]
    fn custom_table_style_parser_retains_regions_sizes_and_typed_losses() {
        let regions = [
            TableStyleRegion::WholeTable,
            TableStyleRegion::FirstColumnStripe,
            TableStyleRegion::SecondColumnStripe,
            TableStyleRegion::FirstRowStripe,
            TableStyleRegion::SecondRowStripe,
            TableStyleRegion::FirstColumn,
            TableStyleRegion::LastColumn,
            TableStyleRegion::HeaderRow,
            TableStyleRegion::TotalRow,
            TableStyleRegion::FirstHeaderCell,
            TableStyleRegion::LastHeaderCell,
            TableStyleRegion::FirstTotalCell,
            TableStyleRegion::LastTotalCell,
        ];
        let dxfs = regions
            .iter()
            .enumerate()
            .map(|(index, _)| DifferentialStyle {
                style: CellStyle::new().background_color([index as u8, 1, 2]),
                losses: (index == 0)
                    .then_some(StyleLoss {
                        kind: StyleLossKind::UnresolvedColor,
                        occurrences: 1,
                    })
                    .into_iter()
                    .collect(),
            })
            .collect::<Vec<_>>();
        let xml = r#"<styleSheet><tableStyles count="1"><tableStyle name="AllRegions" count="17">
            <tableStyleElement type="wholeTable" dxfId="0"/>
            <tableStyleElement type="firstColumnStripe" size="3" dxfId="1"/>
            <tableStyleElement type="secondColumnStripe" size="9999999" dxfId="2"/>
            <tableStyleElement type="firstRowStripe" size="2" dxfId="3"/>
            <tableStyleElement type="secondRowStripe" dxfId="4"/>
            <tableStyleElement type="firstColumn" dxfId="5"/>
            <tableStyleElement type="lastColumn" dxfId="6"/>
            <tableStyleElement type="headerRow" dxfId="7"/>
            <tableStyleElement type="totalRow" dxfId="8"/>
            <tableStyleElement type="firstHeaderCell" dxfId="9"/>
            <tableStyleElement type="lastHeaderCell" dxfId="10"/>
            <tableStyleElement type="firstTotalCell" dxfId="11"/>
            <tableStyleElement type="lastTotalCell" dxfId="12"/>
            <tableStyleElement type="pageFieldLabels" dxfId="0"/>
            <tableStyleElement type="headerRow" dxfId="999"/>
            <tableStyleElement type="wholeTable" dxfId="1"/>
        </tableStyle></tableStyles></styleSheet>"#;
        let parsed = parse_table_styles(xml, &dxfs)
            .remove("AllRegions")
            .expect("parsed table style");

        for region in regions {
            assert!(
                parsed.definition.get(region).is_some(),
                "missing region {region:?}"
            );
        }
        assert_eq!(
            parsed
                .definition
                .get(TableStyleRegion::FirstColumnStripe)
                .map(|style| style.stripe_size),
            Some(3)
        );
        assert_eq!(
            parsed
                .definition
                .get(TableStyleRegion::FirstRowStripe)
                .map(|style| style.stripe_size),
            Some(2)
        );
        for kind in [
            StyleLossKind::UnsupportedProperty,
            StyleLossKind::MissingReference,
            StyleLossKind::LimitExceeded,
            StyleLossKind::UnresolvedColor,
        ] {
            assert!(
                parsed.losses.iter().any(|loss| loss.kind == kind),
                "missing typed loss {kind:?}: {:?}",
                parsed.losses
            );
        }
    }

    #[test]
    fn xlsx_table_regions_compose_with_sheet_column_row_and_direct_cell_styles() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let styles = r#"<styleSheet>
            <fonts count="3"><font><name val="Base"/></font><font><b/></font><font><i/></font></fonts>
            <fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="solid"><fgColor rgb="FF636363"/></patternFill></fill></fills>
            <borders count="1"><border/></borders>
            <cellXfs count="4">
                <xf numFmtId="2" fontId="0" fillId="0" borderId="0"/>
                <xf numFmtId="2" fontId="1" fillId="0" borderId="0" applyFont="1"/>
                <xf numFmtId="2" fontId="2" fillId="0" borderId="0" applyFont="1"/>
                <xf numFmtId="2" fontId="0" fillId="1" borderId="0" applyFill="1"/>
            </cellXfs>
            <dxfs count="11">
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF0A0A0A"/></patternFill></fill></dxf>
                <dxf><font><b/><color rgb="FFFFFFFF"/></font><fill><patternFill patternType="solid"><fgColor rgb="FF141414"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF1E1E1E"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF282828"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF323232"/></patternFill></fill></dxf>
                <dxf><font><color rgb="FF3C3C3C"/></font></dxf>
                <dxf><font><i/></font></dxf>
                <dxf><font><color rgb="FF505050"/></font></dxf>
                <dxf><font><color rgb="FF5A5A5A"/></font></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF464646"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF484848"/></patternFill></fill></dxf>
            </dxfs>
            <tableStyles count="1"><tableStyle name="Layered" count="12">
                <tableStyleElement type="wholeTable" dxfId="0"/>
                <tableStyleElement type="headerRow" dxfId="1"/>
                <tableStyleElement type="totalRow" dxfId="2"/>
                <tableStyleElement type="firstRowStripe" size="2" dxfId="3"/>
                <tableStyleElement type="secondRowStripe" dxfId="4"/>
                <tableStyleElement type="firstColumn" dxfId="5"/>
                <tableStyleElement type="lastColumn" dxfId="6"/>
                <tableStyleElement type="firstHeaderCell" dxfId="7"/>
                <tableStyleElement type="lastTotalCell" dxfId="8"/>
                <tableStyleElement type="firstColumnStripe" dxfId="9"/>
                <tableStyleElement type="secondColumnStripe" dxfId="10"/>
                <tableStyleElement type="pageFieldLabels" dxfId="0"/>
            </tableStyle></tableStyles>
        </styleSheet>"#;
        let worksheet = r#"<worksheet><cols><col min="1" max="1" style="1"/></cols><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>H1</t></is></c><c r="B1" t="inlineStr"><is><t>H2</t></is></c><c r="C1" t="inlineStr"><is><t>H3</t></is></c></row>
            <row r="2" s="2" customFormat="1"><c r="A2"><v>1</v></c><c r="B2" s="3"><v>2</v></c><c r="C2"><v>3</v></c></row>
            <row r="3"><c r="A3"><v>4</v></c><c r="B3"><v>5</v></c><c r="C3"><v>6</v></c></row>
            <row r="4"><c r="A4"><v>7</v></c><c r="B4"><v>8</v></c><c r="C4"><v>9</v></c></row>
            <row r="5"><c r="A5"><v>10</v></c><c r="B5"><v>11</v></c><c r="C5"><v>12</v></c></row>
        </sheetData><tableParts count="1"><tablePart r:id="rIdTable"/></tableParts></worksheet>"#;
        let table = r#"<table id="1" name="LayeredTable" displayName="LayeredTable" ref="A1:C5" headerRowCount="1" totalsRowCount="1"><tableColumns count="3"><tableColumn id="1" name="H1"/><tableColumn id="2" name="H2"/><tableColumn id="3" name="H3"/></tableColumns><tableStyleInfo name="Layered" showFirstColumn="1" showLastColumn="1" showRowStripes="1" showColumnStripes="1"/></table>"#;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
            ),
            ("xl/styles.xml", styles),
            ("xl/worksheets/sheet1.xml", worksheet),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rIdTable" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
            ),
            ("xl/tables/table1.xml", table),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        let workbook = Workbook::open(&zip.finish().unwrap().into_inner()).unwrap();
        let sheet = &workbook.sheets[0];
        let effective_fill = |row, col| {
            let style = sheet.resolved_cell_style(row, col)?;
            style
                .pattern_fill
                .and_then(|fill| fill.foreground.or(fill.background))
                .or(style.fill)
        };

        assert_eq!(effective_fill(0, 0), Some(Color::rgb(0x14, 0x14, 0x14)));
        assert_eq!(
            sheet
                .resolved_cell_style(0, 0)
                .and_then(|style| style.font)
                .and_then(|font| font.color),
            Some(Color::rgb(0x50, 0x50, 0x50))
        );
        assert_eq!(
            effective_fill(1, 1),
            Some(Color::rgb(0x63, 0x63, 0x63)),
            "direct cell fill must win over both row and column banding"
        );
        let direct = sheet.resolved_cell_style(1, 1).expect("direct style");
        assert!(direct.font.as_ref().is_some_and(|font| font.italic));
        assert_eq!(direct.num_fmt.as_deref(), Some("0.00"));
        assert_eq!(effective_fill(2, 1), Some(Color::rgb(0x28, 0x28, 0x28)));
        assert_eq!(effective_fill(3, 1), Some(Color::rgb(0x32, 0x32, 0x32)));
        assert_eq!(effective_fill(4, 2), Some(Color::rgb(0x1E, 0x1E, 0x1E)));
        assert_eq!(
            sheet
                .resolved_cell_style(4, 2)
                .and_then(|style| style.font)
                .and_then(|font| font.color),
            Some(Color::rgb(0x5A, 0x5A, 0x5A))
        );
        assert!(sheet.style_losses().iter().any(|loss| {
            loss.kind == StyleLossKind::UnsupportedProperty && loss.occurrences == 1
        }));
        assert_eq!(
            sheet.resolved_cell_style(2, 1),
            sheet.resolved_cell_style(2, 1)
        );
    }

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("A1:C3"), Some((0, 0, 2, 2)));
        assert_eq!(parse_range("B2"), Some((1, 1, 1, 1))); // lone ref = 1×1
        assert_eq!(parse_range("A1:"), None);
        assert_eq!(parse_range("junk"), None);
    }

    #[test]
    fn sheet_view_and_autofilter_metadata_is_parsed() {
        let xml = r#"<worksheet>
            <sheetViews>
                <sheetView showGridLines="0" showRowColHeaders="0" rightToLeft="1" zoomScale="125">
                    <pane xSplit="2" ySplit="1" state="frozen"/>
                </sheetView>
            </sheetViews>
            <sheetData/>
            <autoFilter ref="A1:C10"/>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.freeze, Some((1, 2)));
        assert_eq!(parsed.autofilter, Some((0, 0, 9, 2)));
        assert!(parsed.hide_gridlines);
        assert_eq!(parsed.show_headers, Some(false));
        assert!(parsed.right_to_left);
        assert_eq!(parsed.zoom, Some(125));
    }

    #[test]
    fn sheet_view_metadata_uses_primary_view_only() {
        let xml = r#"<worksheet>
            <sheetViews>
                <sheetView workbookViewId="0" zoomScale="110"/>
                <sheetView workbookViewId="1" showGridLines="0" showRowColHeaders="0" rightToLeft="1" zoomScale="125">
                    <pane xSplit="2" ySplit="1" state="frozen"/>
                </sheetView>
            </sheetViews>
            <sheetData/>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.freeze, None);
        assert!(!parsed.hide_gridlines);
        assert_eq!(parsed.show_headers, None);
        assert!(!parsed.right_to_left);
        assert_eq!(parsed.zoom, Some(110));
    }

    #[test]
    fn sheet_view_explicit_visible_headers_are_preserved() {
        let xml = r#"<worksheet>
            <sheetViews>
                <sheetView showRowColHeaders="1"/>
            </sheetViews>
            <sheetData/>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.show_headers, Some(true));
    }

    #[test]
    fn page_setup_first_page_number_requires_use_flag() {
        for (attrs, expected) in [
            (r#"firstPageNumber="7""#, None),
            (r#"firstPageNumber="7" useFirstPageNumber="0""#, None),
            (r#"firstPageNumber="7" useFirstPageNumber="1""#, Some(7)),
        ] {
            let xml = format!(r#"<worksheet><sheetData/><pageSetup {attrs}/></worksheet>"#);
            let mut budget = crate::MAX_TEXT_BYTES;
            let parsed = parse_sheet(
                &xml,
                &[],
                &Styles::default(),
                &ThemeColors::default(),
                false,
                &mut budget,
            );

            assert_eq!(
                parsed
                    .page_setup
                    .as_ref()
                    .and_then(|setup| setup.first_page_number),
                expected,
                "unexpected first_page_number for pageSetup attrs {attrs}"
            );
        }
    }

    #[test]
    fn page_setup_pr_is_the_authoritative_fit_mode_switch() {
        for (sheet_pr, expected) in [
            ("", false),
            ("<sheetPr><pageSetUpPr/></sheetPr>", false),
            (r#"<sheetPr><pageSetUpPr fitToPage="0"/></sheetPr>"#, false),
            (
                r#"<sheetPr><pageSetUpPr fitToPage="false"/></sheetPr>"#,
                false,
            ),
            (r#"<sheetPr><pageSetUpPr fitToPage="1"/></sheetPr>"#, true),
            (
                r#"<sheetPr><pageSetUpPr fitToPage="true"/></sheetPr>"#,
                true,
            ),
        ] {
            let xml = format!(
                r#"<worksheet>{sheet_pr}<sheetData/><pageSetup scale="85" fitToWidth="1" fitToHeight="1"/></worksheet>"#
            );
            let mut budget = crate::MAX_TEXT_BYTES;
            let parsed = parse_sheet(
                &xml,
                &[],
                &Styles::default(),
                &ThemeColors::default(),
                false,
                &mut budget,
            );

            assert_eq!(parsed.print_metadata.fit_to_page(), Some(expected));
            let setup = parsed.page_setup.expect("pageSetup must be retained");
            assert_eq!(setup.scale, Some(85));
            assert_eq!(setup.fit_to_width, Some(1));
            assert_eq!(setup.fit_to_height, Some(1));
        }
    }

    #[test]
    fn active_fit_retains_defaulted_and_zero_dimensions() {
        for (attrs, expected_width, expected_height) in [
            (r#"scale="85""#, None, None),
            (
                r#"scale="85" fitToWidth="1" fitToHeight="0""#,
                Some(1),
                Some(0),
            ),
            (
                r#"scale="85" fitToWidth="0" fitToHeight="0""#,
                Some(0),
                Some(0),
            ),
        ] {
            let xml = format!(
                r#"<worksheet><sheetPr><pageSetUpPr fitToPage="1"/></sheetPr><sheetData/><pageSetup {attrs}/></worksheet>"#
            );
            let mut budget = crate::MAX_TEXT_BYTES;
            let parsed = parse_sheet(
                &xml,
                &[],
                &Styles::default(),
                &ThemeColors::default(),
                false,
                &mut budget,
            );

            assert_eq!(parsed.print_metadata.fit_to_page(), Some(true));
            let setup = parsed.page_setup.expect("pageSetup must be retained");
            assert_eq!(setup.scale, Some(85));
            assert_eq!(setup.fit_to_width, expected_width);
            assert_eq!(setup.fit_to_height, expected_height);
        }
    }

    #[test]
    fn fit_count_attributes_saturate_numeric_overflow_and_report_typed_losses() {
        for attribute in ["fitToWidth", "fitToHeight"] {
            for (value, expected, expected_loss) in [
                ("0", Some(0), None),
                ("65535", Some(u16::MAX), None),
                ("65536", Some(u16::MAX), Some(PrintLossKind::LimitExceeded)),
                (
                    "4294967295",
                    Some(u16::MAX),
                    Some(PrintLossKind::LimitExceeded),
                ),
                (
                    "not-a-count",
                    None,
                    Some(PrintLossKind::UnsupportedProperty),
                ),
            ] {
                let xml = format!(
                    r#"<worksheet><sheetPr><pageSetUpPr fitToPage="1"/></sheetPr><sheetData/><pageSetup scale="85" {attribute}="{value}"/></worksheet>"#
                );
                let mut budget = crate::MAX_TEXT_BYTES;
                let parsed = parse_sheet(
                    &xml,
                    &[],
                    &Styles::default(),
                    &ThemeColors::default(),
                    false,
                    &mut budget,
                );

                assert_eq!(
                    parsed.print_metadata.fit_to_page(),
                    Some(true),
                    "{attribute}={value}"
                );
                let setup = parsed.page_setup.expect("pageSetup must be retained");
                let retained = if attribute == "fitToWidth" {
                    setup.fit_to_width
                } else {
                    setup.fit_to_height
                };
                assert_eq!(retained, expected, "{attribute}={value}");
                let other = if attribute == "fitToWidth" {
                    setup.fit_to_height
                } else {
                    setup.fit_to_width
                };
                assert_eq!(other, None, "{attribute}={value}");
                match expected_loss {
                    Some(kind) => {
                        assert_eq!(
                            parsed.print_metadata.fidelity(),
                            crate::PrintFidelity::Partial,
                            "{attribute}={value}"
                        );
                        assert_eq!(
                            parsed
                                .print_metadata
                                .losses()
                                .iter()
                                .find(|loss| loss.kind == kind)
                                .map(|loss| loss.occurrences),
                            Some(1),
                            "{attribute}={value}"
                        );
                    }
                    None => assert!(
                        parsed.print_metadata.losses().is_empty(),
                        "{attribute}={value}"
                    ),
                }
            }
        }
    }

    #[test]
    fn malformed_fit_mode_fails_closed_with_typed_loss() {
        let xml = r#"<worksheet><sheetPr><pageSetUpPr fitToPage="maybe"/></sheetPr>
            <sheetData/><pageSetup scale="85" fitToWidth="1" fitToHeight="1"/>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.print_metadata.fit_to_page(), Some(false));
        assert_eq!(
            parsed.print_metadata.fidelity(),
            crate::PrintFidelity::Partial
        );
        assert!(parsed
            .print_metadata
            .losses()
            .iter()
            .any(|loss| loss.kind == PrintLossKind::UnsupportedProperty));
    }

    #[test]
    fn first_header_footer_falls_back_to_page_setup_metadata() {
        let xml = r#"<worksheet>
            <sheetData/>
            <headerFooter>
                <firstHeader>&amp;CFirst page</firstHeader>
                <firstFooter>&amp;RFirst footer</firstFooter>
            </headerFooter>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        let page_setup = parsed.page_setup.expect("page setup metadata");
        assert_eq!(page_setup.header.as_deref(), Some("&CFirst page"));
        assert_eq!(page_setup.footer.as_deref(), Some("&RFirst footer"));
    }

    #[test]
    fn even_header_footer_falls_back_to_page_setup_metadata() {
        let xml = r#"<worksheet>
            <sheetData/>
            <headerFooter>
                <evenHeader>&amp;LEven pages</evenHeader>
                <evenFooter>&amp;REven footer</evenFooter>
            </headerFooter>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        let page_setup = parsed.page_setup.expect("page setup metadata");
        assert_eq!(page_setup.header.as_deref(), Some("&LEven pages"));
        assert_eq!(page_setup.footer.as_deref(), Some("&REven footer"));
    }

    #[test]
    fn odd_header_footer_overrides_first_even_fallback_metadata() {
        let xml = r#"<worksheet>
            <sheetData/>
            <headerFooter>
                <firstHeader>&amp;CFirst page</firstHeader>
                <evenHeader>&amp;LEven pages</evenHeader>
                <oddHeader>&amp;COdd pages</oddHeader>
                <firstFooter>&amp;RFirst footer</firstFooter>
                <evenFooter>&amp;REven footer</evenFooter>
                <oddFooter>&amp;COdd footer</oddFooter>
            </headerFooter>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        let page_setup = parsed.page_setup.expect("page setup metadata");
        assert_eq!(page_setup.header.as_deref(), Some("&COdd pages"));
        assert_eq!(page_setup.footer.as_deref(), Some("&COdd footer"));
    }

    #[test]
    fn print_sidecar_retains_exact_ooxml_source_metadata() {
        let xml = r#"<worksheet>
            <sheetData/>
            <printOptions gridLines="0" headings="1" horizontalCentered="1" verticalCentered="0"/>
            <pageSetup pageOrder="overThenDown"/>
            <headerFooter differentOddEven="1" differentFirst="1" scaleWithDoc="0" alignWithMargins="1">
                <oddHeader>&amp;COdd</oddHeader><oddFooter>&amp;LOddF</oddFooter>
                <evenHeader>&amp;CEven</evenHeader><evenFooter>&amp;LEvenF</evenFooter>
                <firstHeader>&amp;CFirst</firstHeader><firstFooter>&amp;LFirstF</firstFooter>
            </headerFooter>
            <rowBreaks count="3" manualBreakCount="2">
                <brk id="20" min="0" max="16383" man="1"/>
                <brk id="5" min="0" max="16383" man="1"/>
                <brk id="8" min="0" max="16383" man="0"/>
            </rowBreaks>
            <colBreaks count="2" manualBreakCount="2">
                <brk id="7" min="0" max="1048575" man="1"/>
                <brk id="3" min="0" max="1048575" man="1"/>
            </colBreaks>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let mut parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        let names = [SheetDefinedName {
            local_sheet_id: 0,
            name: "_xlnm.Print_Area".to_string(),
            refers_to: "'Print Sheet'!$A$1:$B$2,'Print Sheet'!$D$4:$F$9".to_string(),
        }];
        apply_sheet_defined_names(
            &mut parsed.page_setup,
            &mut parsed.print_metadata,
            &mut parsed.autofilter,
            names.iter(),
        );

        let metadata = &parsed.print_metadata;
        assert_eq!(metadata.fidelity(), crate::PrintFidelity::Retained);
        assert_eq!(metadata.print_areas(), &[(0, 0, 1, 1), (3, 3, 8, 5)]);
        assert_eq!(metadata.manual_row_breaks(), &[5, 20]);
        assert_eq!(metadata.manual_col_breaks(), &[3, 7]);
        assert_eq!(metadata.page_order(), Some(PrintPageOrder::OverThenDown));
        assert_eq!(metadata.print_gridlines(), Some(false));
        assert_eq!(metadata.print_headings(), Some(true));
        assert_eq!(metadata.center_horizontally(), Some(true));
        assert_eq!(metadata.center_vertically(), Some(false));
        let header_footer = metadata.header_footer();
        assert_eq!(header_footer.odd_header(), Some("&COdd"));
        assert_eq!(header_footer.odd_footer(), Some("&LOddF"));
        assert_eq!(header_footer.even_header(), Some("&CEven"));
        assert_eq!(header_footer.even_footer(), Some("&LEvenF"));
        assert_eq!(header_footer.first_header(), Some("&CFirst"));
        assert_eq!(header_footer.first_footer(), Some("&LFirstF"));
        assert_eq!(header_footer.different_odd_even(), Some(true));
        assert_eq!(header_footer.different_first(), Some(true));
        assert_eq!(header_footer.scale_with_document(), Some(false));
        assert_eq!(header_footer.align_with_margins(), Some(true));
        assert_eq!(
            parsed
                .page_setup
                .as_ref()
                .and_then(|setup| setup.print_area),
            Some((0, 0, 1, 1))
        );
    }

    #[test]
    fn malformed_ooxml_print_state_is_typed_not_flattened() {
        let xml = r#"<worksheet><sheetData/><pageSetup pageOrder="sideways"/>
            <rowBreaks><brk id="bad" man="1"/></rowBreaks>
            <headerFooter differentFirst="maybe"><firstHeader>first</firstHeader></headerFooter>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        assert_eq!(
            parsed.print_metadata.fidelity(),
            crate::PrintFidelity::Partial
        );
        assert!(parsed
            .print_metadata
            .losses()
            .iter()
            .any(|loss| loss.kind == PrintLossKind::InvalidPageBreak));
        assert!(parsed
            .print_metadata
            .losses()
            .iter()
            .any(|loss| loss.kind == PrintLossKind::MalformedHeaderFooter));
        assert!(parsed
            .print_metadata
            .losses()
            .iter()
            .any(|loss| loss.kind == PrintLossKind::UnsupportedProperty));
    }

    #[test]
    fn self_closing_ooxml_break_container_does_not_capture_stray_breaks() {
        let xml = r#"<worksheet><sheetData/><rowBreaks/><brk id="9" man="1"/></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        assert!(parsed.print_metadata.manual_row_breaks().is_empty());
        assert!(parsed.print_metadata.manual_col_breaks().is_empty());
    }

    #[test]
    fn data_validations_metadata_is_parsed() {
        let xml = r#"<worksheet>
            <sheetData/>
            <dataValidations count="3">
                <dataValidation type="list" allowBlank="1" showInputMessage="1" sqref="A1 A3:A4" promptTitle="Pick" prompt="Choose one">
                    <formula1>"Yes,No"</formula1>
                </dataValidation>
                <dataValidation type="whole" operator="between" allowBlank="0" showErrorMessage="1" sqref="B1:B2" errorTitle="Bounds" error="1..9 only">
                    <formula1>1</formula1><formula2>9</formula2>
                </dataValidation>
                <dataValidation type="custom" sqref="C1"><formula1>ISNUMBER(C1)</formula1></dataValidation>
            </dataValidations>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.data_validations.len(), 4);
        assert_eq!(parsed.data_validations[0].sqref, (0, 0, 0, 0));
        assert_eq!(parsed.data_validations[1].sqref, (2, 0, 3, 0));
        assert_eq!(parsed.data_validations[0].kind, DvKind::List);
        assert_eq!(parsed.data_validations[0].formula1, "\"Yes,No\"");
        assert_eq!(
            parsed.data_validations[0].prompt.as_ref(),
            Some(&("Pick".to_string(), "Choose one".to_string()))
        );
        assert!(parsed.data_validations[0].show_input_message);
        assert!(!parsed.data_validations[0].show_error_message);
        let whole = &parsed.data_validations[2];
        assert_eq!(whole.kind, DvKind::Whole);
        assert_eq!(whole.operator, DvOp::Between);
        assert!(!whole.allow_blank);
        assert!(!whole.show_input_message);
        assert!(whole.show_error_message);
        assert_eq!(whole.formula1, "1");
        assert_eq!(whole.formula2.as_deref(), Some("9"));
        assert_eq!(
            whole.error.as_ref(),
            Some(&("Bounds".to_string(), "1..9 only".to_string()))
        );
        assert_eq!(parsed.data_validations[3].kind, DvKind::Custom);
        assert_eq!(parsed.data_validations[3].formula1, "ISNUMBER(C1)");
    }

    #[test]
    fn data_validation_missing_allow_blank_defaults_false() {
        let xml = r#"<worksheet><sheetData/>
            <dataValidations count="1">
                <dataValidation type="whole" sqref="A1" promptTitle="Prompt" prompt="Text" errorTitle="Error" error="Text"><formula1>1</formula1></dataValidation>
            </dataValidations>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.data_validations.len(), 1);
        assert!(!parsed.data_validations[0].allow_blank);
        assert!(!parsed.data_validations[0].show_input_message);
        assert!(!parsed.data_validations[0].show_error_message);
        assert_eq!(
            parsed.data_validations[0].prompt.as_ref(),
            Some(&("Prompt".to_string(), "Text".to_string()))
        );
        assert_eq!(
            parsed.data_validations[0].error.as_ref(),
            Some(&("Error".to_string(), "Text".to_string()))
        );
    }

    #[test]
    fn shared_string_excludes_phonetic_ruby() {
        // `<rPh>` carries East Asian ruby (furigana) guide text, not part of the
        // displayed string — it must not be concatenated into the value.
        let xml = r#"<sst><si><t>東京</t><rPh sb="0" eb="2"><t>とうきょう</t></rPh></si></sst>"#;
        assert_eq!(shared_texts(xml), vec!["東京"]);
    }

    #[test]
    fn formula_without_cached_value_is_surfaced() {
        // An uncalculated formula (`<f>` but no `<v>`) must still surface its
        // source as Cell::Formula, not be silently dropped.
        let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\"><f>SUM(B1:B2)</f></c></row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        assert_eq!(cells.len(), 1);
        match &cells[0].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM(B1:B2)");
                assert_eq!(**cached, Cell::Text(String::new()));
            }
            other => panic!("expected a formula cell, got {other:?}"),
        }
    }

    #[test]
    fn self_closing_formula_keeps_cached_value() {
        // A self-closing `<f/>` (e.g. a shared-formula follower) has no formula
        // text and no End event; the following `<v>` must be read as the value,
        // not captured as formula text (which would surface an empty-cached
        // formula and swallow the 42).
        let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\"><f t=\"shared\" si=\"0\"/><v>42</v></c></row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].value, Cell::Number(42.0));
    }

    #[test]
    fn shift_formula_engine() {
        assert_eq!(shift_formula("A1+B1", 1, 0), "A2+B2");
        assert_eq!(shift_formula("$A$1+B1", 1, 1), "$A$1+C2");
        assert_eq!(shift_formula("SUM(A1:A3)", 2, 0), "SUM(A3:A5)");
        assert_eq!(shift_formula("LOG10(A1)", 1, 0), "LOG10(A2)"); // function, not a ref
        assert_eq!(shift_formula("\"A1\"&B2", 1, 0), "\"A1\"&B3"); // string literal untouched
        assert_eq!(shift_formula("$A1", 5, 9), "$A6"); // col absolute, row shifts
        assert_eq!(shift_formula("A1", 0, -1), "#REF!"); // shifted off-grid
        assert_eq!(shift_formula("Z9", 1, 1), "AA10"); // column carry
        assert_eq!(shift_formula("'My Sheet'!A1", 1, 0), "'My Sheet'!A2"); // sheet name kept
        assert_eq!(shift_formula("'A1'!B1", 1, 0), "'A1'!B2"); // ref inside '…' not shifted
        assert_eq!(shift_formula("XFE1+1", 1, 0), "XFE1+1"); // off-grid A1-shaped name kept
        assert_eq!(shift_formula("SUM(1:2)", 1, 0), "SUM(2:3)");
        assert_eq!(shift_formula("SUM($1:2)", 1, 0), "SUM($1:3)");
        assert_eq!(shift_formula("SUM(A:B)", 0, 1), "SUM(B:C)");
        assert_eq!(shift_formula("SUM($A:B)", 0, 1), "SUM($A:C)");
    }

    #[test]
    fn shared_formula_follower_with_whitespace() {
        // A pretty-printed follower (whitespace between the self-closing `<f/>` and
        // `<v>`) must not capture that whitespace as formula text and be mis-registered
        // as a master.
        let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\"><f t=\"shared\" ref=\"A1:A2\" si=\"0\">B1*2</f><v>2</v></c></row>\
            <row r=\"2\"><c r=\"A2\"><f t=\"shared\" si=\"0\"/>\n            <v>4</v></c></row>\
            </sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        match &cells[1].value {
            Cell::Formula { formula, .. } => assert_eq!(formula, "B2*2"),
            o => panic!("whitespace follower not reconstructed: {o:?}"),
        }
    }

    #[test]
    fn shared_formula_follower_is_reconstructed() {
        // Master at A1 defines si=0; follower at A2 must surface the relative-shifted
        // formula (B1 -> B2), not a bare cached value.
        let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\"><f t=\"shared\" ref=\"A1:A2\" si=\"0\">B1*2</f><v>2</v></c></row>\
            <row r=\"2\"><c r=\"A2\"><f t=\"shared\" si=\"0\"/><v>4</v></c></row>\
            </sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        assert_eq!(cells.len(), 2);
        match &cells[0].value {
            Cell::Formula { formula, .. } => assert_eq!(formula, "B1*2"),
            o => panic!("master not a formula: {o:?}"),
        }
        match &cells[1].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "B2*2");
                assert_eq!(**cached, Cell::Number(4.0));
            }
            o => panic!("follower not reconstructed: {o:?}"),
        }
    }

    #[test]
    fn iso_date_cell_t_d() {
        // A `t="d"` ISO date cell (emitted by some non-Excel writers) must read as
        // a Date, not be dropped by the numeric fallback.
        let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\" t=\"d\"><v>2024-03-15</v></c></row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].value, Cell::Date(45366.0));
    }

    #[test]
    fn iso_date_cell_t_d_renders_datetime_and_time_only_values() {
        let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\" s=\"1\" t=\"d\"><v>2021-01-01T10:10:10</v></c>\
            <c r=\"A2\" s=\"2\" t=\"d\"><v>10:10:10</v></c>\
            </row></sheetData></worksheet>";
        let styles = Styles {
            xf_numfmt: vec![0, 22, 20],
            ..Default::default()
        };
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &styles,
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;

        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].text, "2021-01-01 10:10:10");
        assert_eq!(cells[1].text, "10:10:10");
    }

    #[test]
    fn reads_merged_ranges_and_formula() {
        let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\"><f>SUM(B1:B2)</f><v>30</v></c>\
            <c r=\"B1\"><v>10</v></c></row>\
            <row r=\"2\"><c r=\"B2\"><v>20</v></c></row>\
            </sheetData>\
            <mergeCells count=\"1\"><mergeCell ref=\"A1:C1\"/></mergeCells>\
            </worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        let cells = parsed.cells;
        assert_eq!(parsed.merges, vec![(0, 0, 0, 2)]); // A1:C1

        // The formula cell exposes both the source text and the cached value.
        let a1 = cells.iter().find(|c| c.row == 0 && c.col == 0).unwrap();
        match &a1.value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM(B1:B2)");
                assert_eq!(**cached, Cell::Number(30.0));
            }
            other => panic!("expected a formula cell, got {other:?}"),
        }
        assert_eq!(a1.text, "30"); // display text is the cached value
    }

    #[test]
    fn conditional_metadata_retains_priority_stop_full_dxf_and_losses() {
        let styles_xml = r#"<styleSheet>
            <dxfs count="1"><dxf>
                <font><b/><color rgb="FF112233"/><outline/></font>
                <fill><patternFill patternType="solid"><fgColor rgb="FF445566"/></patternFill></fill>
                <border><left style="thin"><color rgb="FF778899"/></left><diagonal style="thin"/></border>
                <numFmt numFmtId="166" formatCode="0.000"/>
                <alignment horizontal="center" wrapText="1" readingOrder="2"/>
                <protection locked="0" hidden="1"/>
            </dxf></dxfs>
        </styleSheet>"#;
        let theme = ThemeColors::default();
        let styles = parse_styles(styles_xml, &theme);
        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>2</v></c></row></sheetData>
            <conditionalFormatting sqref="A1">
                <cfRule type="cellIs" dxfId="0" priority="7" stopIfTrue="1" operator="greaterThan"><formula>1</formula></cfRule>
            </conditionalFormatting></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(xml, &[], &styles, &theme, false, &mut budget);

        assert_eq!(parsed.cond_formats.len(), 1);
        assert_eq!(parsed.cond_format_metadata.len(), 1);
        let metadata = &parsed.cond_format_metadata[0];
        assert_eq!(metadata.priority, Some(7));
        assert!(metadata.stop_if_true);
        let dxf = metadata
            .differential_style
            .as_ref()
            .expect("retained differential style");
        assert_eq!(dxf.fill, Some(Color::rgb(0x44, 0x55, 0x66)));
        assert_eq!(
            dxf.font.as_ref().and_then(|font| font.color),
            Some(Color::rgb(0x11, 0x22, 0x33))
        );
        assert!(dxf.font.as_ref().is_some_and(|font| font.bold));
        assert_eq!(
            dxf.border.as_ref().map(|border| border.left),
            Some(BorderStyle::Thin)
        );
        assert_eq!(dxf.num_fmt.as_deref(), Some("0.000"));
        assert!(dxf.align.as_ref().is_some_and(|align| align.wrap));
        assert!(dxf
            .protection
            .as_ref()
            .is_some_and(|protection| protection.locked == Some(false) && protection.hidden));
        assert!(metadata.style_losses.iter().any(|loss| {
            loss.kind == StyleLossKind::UnsupportedProperty && loss.occurrences >= 2
        }));
    }
}
#[test]
fn zero_print_title_rows_are_rejected_without_panicking() {
    assert_eq!(parse_repeat_rows("0:1"), None);
    assert_eq!(parse_repeat_rows("$0:$1"), None);
    assert_eq!(parse_repeat_rows("1:1048577"), None);
}
