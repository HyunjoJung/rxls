use std::collections::BTreeMap;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::model::{TableStyleApplication, TableStyleRegion};
use crate::{CellStyle, StyleLoss, StyleLossKind, Table};

use super::refs::parse_range;
use super::relationships::{parse_ooxml_relationships, relationship_type_matches};
use super::style::{add_differential_loss, Styles};
use super::theme::ThemeColors;
use super::{attr, local, parse_bool_attr};

/// Collect every internal table part target from worksheet relationships.
pub(super) fn table_targets(xml: &str) -> Vec<String> {
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
            // A mixed internal/external table relationship set is malformed;
            // fail closed instead of returning only a trusted-looking subset.
            if relationship.external {
                return Vec::new();
            }
            out.push(relationship.target);
        }
    }
    out
}

/// Parsed table metadata awaiting workbook style resolution.
#[derive(Debug)]
pub(super) struct ParsedTable {
    pub(super) table: Table,
    application: TableStyleApplication,
    losses: Vec<StyleLoss>,
}

/// Tables and resolved table-style state ready for the sheet aggregate.
pub(super) struct ResolvedTables {
    pub(super) tables: Vec<Table>,
    pub(super) table_header_formats: BTreeMap<String, CellStyle>,
    pub(super) table_region_formats: BTreeMap<String, TableStyleApplication>,
    pub(super) style_losses: Vec<StyleLoss>,
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

/// Parse one `xl/tables/tableN.xml` part.
pub(super) fn parse_table(xml: &str) -> Option<ParsedTable> {
    const MAX_TABLE_COLUMNS: usize = 16_384;
    let mut r = Reader::from_str(xml);
    let mut range = None;
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

/// Resolve parsed table style names against the workbook style and theme tables.
pub(super) fn resolve_table_styles(
    parsed_tables: Vec<ParsedTable>,
    styles: &Styles,
    theme: &ThemeColors,
) -> ResolvedTables {
    let tables = parsed_tables
        .iter()
        .map(|parsed| parsed.table.clone())
        .collect();
    let mut table_header_formats = BTreeMap::new();
    let mut table_region_formats = BTreeMap::new();
    let mut style_losses = styles.losses.clone();

    for parsed in parsed_tables {
        for loss in parsed.losses {
            add_style_loss(&mut style_losses, loss.kind, loss.occurrences);
        }
        let Some(style_name) = parsed.table.style.as_deref() else {
            continue;
        };
        let Some(table_style) = styles.table_style(style_name, theme) else {
            add_style_loss(&mut style_losses, StyleLossKind::MissingReference, 1);
            continue;
        };
        for loss in table_style.losses {
            add_style_loss(&mut style_losses, loss.kind, loss.occurrences);
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

    ResolvedTables {
        tables,
        table_header_formats,
        table_region_formats,
        style_losses,
    }
}

fn add_style_loss(losses: &mut Vec<StyleLoss>, kind: StyleLossKind, occurrences: u32) {
    if occurrences == 0 {
        return;
    }
    if let Some(loss) = losses.iter_mut().find(|loss| loss.kind == kind) {
        loss.occurrences = loss.occurrences.saturating_add(occurrences);
    } else {
        losses.push(StyleLoss { kind, occurrences });
    }
}
