//! Bounded, opt-in formula-cache refresh inside an uncommitted MCP edit.

use std::collections::BTreeSet;

use rxls::{Cell, FormulaEvaluation, Spreadsheet, Workbook};

use crate::model::RecalculationSummary;

const MAX_FORMULA_CELLS: usize = 10_000;
const MAX_INDEX_RECORDS: usize = 1_000_000;
const MAX_SHEETS: usize = 4_096;
const MAX_SHEET_NAME_BYTES: usize = 64 * 1024;
const MAX_DIAGNOSTIC_REASONS: usize = 32;
const MAX_SUMMARY_BYTES: usize = 4 * 1024;

/// Refresh only changed computed caches; fallback caches remain untouched.
pub(crate) fn apply(candidate: &mut Spreadsheet) -> Result<RecalculationSummary, String> {
    let workbook = candidate.workbook();
    // Bound index construction before display_cells initializes it. Own sheet
    // names once so cache updates do not borrow candidate's parsed workbook.
    let names = preflight(workbook)?;
    let mut targets = Vec::new();
    let mut originals = Vec::new();
    for (sheet, name) in workbook.sheets.iter().zip(&names) {
        for cell in sheet.display_cells() {
            if let Cell::Formula { cached, .. } = cell.value {
                if targets.len() == MAX_FORMULA_CELLS {
                    return Err(failure("formula_target_limit"));
                }
                targets.push((name.as_str(), cell.row, cell.col));
                originals.push(cached.as_ref());
            }
        }
    }
    // A single batch shares operation/range/text/dependency budgets and memo.
    let evaluated = workbook
        .evaluate_cells(&targets)
        .map_err(|reason| failure(reason.code()))?;
    if evaluated.len() != targets.len() {
        return Err(failure("invalid_evaluation_count"));
    }
    let mut summary = RecalculationSummary::default();
    let mut updates = Vec::new();
    for ((target, cached), evaluation) in targets.into_iter().zip(originals).zip(evaluated) {
        match evaluation {
            FormulaEvaluation::Computed(value) => {
                match &value {
                    Cell::Number(number) | Cell::Date(number) if !number.is_finite() => {
                        return Err(failure("nonfinite_result"));
                    }
                    Cell::Formula { .. } => return Err(failure("non_scalar_result")),
                    _ => {}
                }
                summary.computed_cells += 1;
                if same_cache(&value, cached) {
                    summary.unchanged_cells += 1;
                } else {
                    updates.push((target.0, target.1, target.2, value));
                }
            }
            FormulaEvaluation::Fallback { reason, .. } => {
                summary.unsupported_cells += 1;
                let code = reason.code();
                if !summary.reasons.iter().any(|reason| reason == code) {
                    if summary.reasons.len() == MAX_DIAGNOSTIC_REASONS
                        || code.len() > MAX_SUMMARY_BYTES
                    {
                        return Err(failure("diagnostic_limit"));
                    }
                    summary.reasons.push(code.to_string());
                }
            }
            _ => return Err(failure("unsupported_evaluation")),
        }
    }
    summary.reasons.sort_unstable();
    let summary_bytes =
        serde_json::to_vec(&summary).map_err(|_| failure("summary_serialization_failed"))?;
    if summary_bytes.len() > MAX_SUMMARY_BYTES {
        return Err(failure("diagnostic_limit"));
    }
    if !updates.is_empty() {
        candidate
            .set_formula_cached_values(&updates)
            .map_err(|source| {
                format!(
                    "RXLS_MCP_RECALC_CACHE_FAILED: formula caches could not be updated: {source}"
                )
            })?;
    }
    Ok(summary)
}

fn preflight(workbook: &Workbook) -> Result<Vec<String>, String> {
    if workbook.sheets.len() > MAX_SHEETS {
        return Err(failure("sheet_limit"));
    }
    let mut names = Vec::with_capacity(workbook.sheets.len());
    let mut folded = BTreeSet::new();
    let mut name_bytes = 0usize;
    let mut records = 0usize;
    for sheet in &workbook.sheets {
        name_bytes = name_bytes
            .checked_add(sheet.name.len())
            .filter(|&total| total <= MAX_SHEET_NAME_BYTES)
            .ok_or_else(|| failure("sheet_name_limit"))?;
        if !folded.insert(sheet.name.to_ascii_lowercase()) {
            return Err(failure("ambiguous_sheet_names"));
        }
        // The display index also allocates entries for read hyperlinks.
        records = records
            .checked_add(sheet.hyperlinks().len())
            .filter(|&total| total <= MAX_INDEX_RECORDS)
            .ok_or_else(|| failure("formula_scan_limit"))?;
        for _ in sheet.cells() {
            if records == MAX_INDEX_RECORDS {
                return Err(failure("formula_scan_limit"));
            }
            records += 1;
        }
        names.push(sheet.name.clone());
    }
    Ok(names)
}

fn same_cache(left: &Cell, right: &Cell) -> bool {
    match (left, right) {
        (Cell::Number(left) | Cell::Date(left), Cell::Number(right) | Cell::Date(right)) => {
            left == right
        }
        _ => left == right,
    }
}

fn failure(reason: &str) -> String {
    format!("RXLS_MCP_RECALC_FAILED: formula recalculation could not complete ({reason}); the edit was not applied")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_case_ambiguous_names_even_without_any_cache_updates() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write_number(0, 0, 1.0);
        workbook
            .add_sheet("data")
            .write_formula(0, 0, "NOW()", 42.0);
        assert!(preflight(&workbook)
            .unwrap_err()
            .contains("ambiguous_sheet_names"));
    }

    #[test]
    fn sheet_metadata_work_is_bounded_before_display_index_construction() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("x".repeat(MAX_SHEET_NAME_BYTES));
        assert!(preflight(&workbook).is_ok());
        workbook.add_sheet("y");
        assert!(preflight(&workbook)
            .unwrap_err()
            .contains("sheet_name_limit"));
        let mut workbook = Workbook::new();
        for index in 0..=MAX_SHEETS {
            workbook.add_sheet(index.to_string());
        }
        assert!(preflight(&workbook).unwrap_err().contains("sheet_limit"));
    }
}
