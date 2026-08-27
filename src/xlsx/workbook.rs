use quick_xml::events::Event;
use quick_xml::Reader;

use crate::{PageSetup, PrintLossKind, PrintMetadata};

use super::refs::{letters_col, parse_range, SheetRange};
use super::{append_general_ref, attr, attr_true, local, text_of};

/// A worksheet's visibility from the `<sheet state>` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Visibility {
    #[default]
    Visible,
    Hidden,
    VeryHidden,
}

/// One ordered `<sheet>` entry from workbook metadata.
pub(super) struct SheetRef {
    pub(super) name: String,
    pub(super) rid: String,
    pub(super) visibility: Visibility,
}

/// A sheet-local built-in defined name used for print and filter metadata.
pub(super) struct SheetDefinedName {
    pub(super) local_sheet_id: usize,
    pub(super) name: String,
    pub(super) refers_to: String,
}

/// Workbook metadata parsed before individual worksheet parts are opened.
pub(super) struct ParsedWorkbook {
    pub(super) sheets: Vec<SheetRef>,
    pub(super) date1904: bool,
    pub(super) structure_protected: bool,
    pub(super) active_sheet: Option<usize>,
    pub(super) defined_names: Vec<(String, String)>,
    pub(super) local_defined_names: Vec<crate::LocalDefinedName>,
    pub(super) sheet_defined_names: Vec<SheetDefinedName>,
}

enum DefinedNameCapture {
    GlobalUser(String),
    LocalUser { local_sheet_id: usize, name: String },
    LocalBuiltin { local_sheet_id: usize, name: String },
}

/// Parse workbook properties, ordered sheets, and defined names.
pub(super) fn parse_workbook(xml: &str) -> ParsedWorkbook {
    let mut r = Reader::from_str(xml);
    let mut sheets = Vec::new();
    let mut date1904 = false;
    let mut structure_protected = false;
    let mut active_sheet = None;
    let mut defined_names = Vec::new();
    let mut raw_local_defined_names = Vec::new();
    let mut sheet_defined_names = Vec::new();
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
                    let rid = attr(&e, b"id").unwrap_or_default();
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

/// Apply sheet-local built-in names to print and autofilter metadata.
pub(super) fn apply_sheet_defined_names<'a, I>(
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

pub(super) fn parse_repeat_rows(value: &str) -> Option<(u32, u32)> {
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
