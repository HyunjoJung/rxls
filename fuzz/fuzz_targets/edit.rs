#![no_main]
//! Fuzz the package-preserving edit engine on arbitrary bytes: if the input
//! opens as a `Spreadsheet`, run a short scripted edit sequence covering every
//! public edit method and save. Every edit may fail on malformed/garbage
//! input; the invariant is no panic, abort, OOM, or hang while exercising
//! `XmlTree` promotion, serialization, and rollback paths (`src/xmltree.rs`,
//! `src/package.rs`, `src/spreadsheet.rs`).
//!
//! ```text
//! cargo +nightly fuzz run edit
//! ```

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use rxls::{Cell, Color, DocProperties, SheetVisible, Spreadsheet};

/// Bound on Unstructured-derived loop counts so the target stays fast.
const MAX_ITERS: u8 = 8;

fn arbitrary_cell(u: &mut Unstructured) -> Cell {
    match u.int_in_range(0u8..=5).unwrap_or(0) {
        0 => Cell::Text(String::arbitrary(u).unwrap_or_default()),
        1 => Cell::Number(f64::arbitrary(u).unwrap_or(0.0)),
        2 => Cell::Date(f64::arbitrary(u).unwrap_or(0.0)),
        3 => Cell::Bool(bool::arbitrary(u).unwrap_or(false)),
        4 => Cell::Error(String::arbitrary(u).unwrap_or_default()),
        _ => Cell::Formula {
            formula: String::arbitrary(u).unwrap_or_default(),
            cached: Box::new(Cell::Number(f64::arbitrary(u).unwrap_or(0.0))),
        },
    }
}

/// A row/col pair that occasionally lands beyond the Excel grid max
/// (1_048_575 rows / 16_383 cols) to exercise the out-of-grid rejection path.
fn arbitrary_coord(u: &mut Unstructured) -> (u32, u16) {
    if u.ratio(1u8, 4).unwrap_or(false) {
        // Beyond-grid: bias toward values just past (and well past) the max.
        let row = u.int_in_range(1_048_576u32..=u32::MAX).unwrap_or(u32::MAX);
        let col = u.int_in_range(16_384u16..=u16::MAX).unwrap_or(u16::MAX);
        (row, col)
    } else {
        let row = u.int_in_range(0u32..=2000).unwrap_or(0);
        let col = u.int_in_range(0u16..=200).unwrap_or(0);
        (row, col)
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(mut sheet) = Spreadsheet::open(data) else {
        return;
    };

    // Read-only introspection surface — cheap, locks it into the fuzz corpus.
    let _ = sheet.edit_capability();
    let _ = sheet.edited_parts();

    // Derive scripted-edit parameters from a leading slice of the input, same
    // pattern as fuzz_targets/author.rs.
    let mut u = Unstructured::new(&data[..data.len().min(4096)]);

    let real_name = sheet
        .workbook()
        .sheet_names()
        .first()
        .map(|s| s.to_string());
    let garbage_name = String::arbitrary(&mut u).unwrap_or_default();

    let names: Vec<String> = match &real_name {
        Some(n) => vec![n.clone(), garbage_name.clone()],
        None => vec![garbage_name.clone()],
    };

    let n_ops = u.int_in_range(0u8..=MAX_ITERS).unwrap_or(0);
    for _ in 0..n_ops {
        if u.is_empty() {
            break;
        }
        let name = &names[u.int_in_range(0usize..=names.len() - 1).unwrap_or(0)];
        let (row, col) = arbitrary_coord(&mut u);
        match u.int_in_range(0u8..=9).unwrap_or(0) {
            0 => {
                let value = arbitrary_cell(&mut u);
                let _ = sheet.set_cell_value(name, row, col, value);
            }
            1 => {
                let formula = String::arbitrary(&mut u).unwrap_or_default();
                let cached = arbitrary_cell(&mut u);
                let _ = sheet.set_cell_formula(name, row, col, formula, cached);
            }
            2 => {
                let n_cells = u.int_in_range(0u8..=MAX_ITERS).unwrap_or(0);
                let values: Vec<Cell> = (0..n_cells).map(|_| arbitrary_cell(&mut u)).collect();
                let _ = sheet.append_row(name, values);
            }
            3 => {
                let (row1, col1) = arbitrary_coord(&mut u);
                let _ = sheet.clear_range(name, row, col, row1, col1);
            }
            4 => {
                let mut properties = DocProperties::new();
                if let Some(title) = Option::<String>::arbitrary(&mut u).unwrap_or(None) {
                    properties = properties.with_title(title);
                }
                if let Some(subject) = Option::<String>::arbitrary(&mut u).unwrap_or(None) {
                    properties = properties.with_subject(subject);
                }
                if let Some(creator) = Option::<String>::arbitrary(&mut u).unwrap_or(None) {
                    properties = properties.with_creator(creator);
                }
                if let Some(keywords) = Option::<String>::arbitrary(&mut u).unwrap_or(None) {
                    properties = properties.with_keywords(keywords);
                }
                if let Some(description) = Option::<String>::arbitrary(&mut u).unwrap_or(None) {
                    properties = properties.with_description(description);
                }
                if let Some(last_modified_by) = Option::<String>::arbitrary(&mut u).unwrap_or(None)
                {
                    properties = properties.with_last_modified_by(last_modified_by);
                }
                if let Some(company) = Option::<String>::arbitrary(&mut u).unwrap_or(None) {
                    properties = properties.with_company(company);
                }
                if let Some(created) = Option::<String>::arbitrary(&mut u).unwrap_or(None) {
                    properties = properties.with_created(created);
                }
                let _ = sheet.set_document_properties(properties);
            }
            5 => {
                let dn_name = String::arbitrary(&mut u).unwrap_or_default();
                let refers_to = String::arbitrary(&mut u).unwrap_or_default();
                let _ = sheet.set_defined_name(dn_name, refers_to);
            }
            6 => {
                let new_name = String::arbitrary(&mut u).unwrap_or_default();
                let _ = sheet.rename_sheet(name, &new_name);
            }
            7 => {
                let visible = match u.int_in_range(0u8..=2).unwrap_or(0) {
                    0 => SheetVisible::Visible,
                    1 => SheetVisible::Hidden,
                    _ => SheetVisible::VeryHidden,
                };
                let _ = sheet.set_sheet_visibility(name, visible);
            }
            8 => {
                let _ = sheet.set_active_sheet(name);
            }
            _ => {
                let rgb = <[u8; 3]>::arbitrary(&mut u).unwrap_or([0, 0, 0]);
                let _ = sheet.set_sheet_tab_color(name, Color::from(rgb));
            }
        }
    }

    let Ok(bytes) = sheet.save() else {
        return;
    };
    // Open-edit-save-reopen-save exercises promotion, serialization, and
    // re-parse of self-produced output in one run.
    if let Ok(reopened) = Spreadsheet::open(&bytes) {
        let _ = reopened.save();
    }
});
