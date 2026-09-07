//! Atomic refresh of deterministic formula caches after an opt-in cell edit.

use rxls::{Cell, FormulaEvaluation, Spreadsheet, Workbook};
use serde::Serialize;

use super::{enforce_output, map_edit_error, FacadeError, MAX_OUTPUT_BYTES};

const MAX_FORMULA_CELLS: usize = 10_000;

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Summary {
    computed_cells: usize,
    unchanged_cells: usize,
    unsupported_cells: usize,
    reasons: Vec<&'static str>,
}

impl Summary {
    pub(super) fn changed_cells(&self) -> usize {
        self.computed_cells - self.unchanged_cells
    }

    pub(super) fn append_to_result(self, output: String) -> Result<String, FacadeError> {
        let mut result: serde_json::Value =
            serde_json::from_str(&output).map_err(|_| failure("serialization_failed"))?;
        result["recalculation"] =
            serde_json::to_value(self).map_err(|_| failure("serialization_failed"))?;
        let result = result.to_string();
        enforce_output(result.len(), MAX_OUTPUT_BYTES)?;
        Ok(result)
    }
}

pub(super) fn apply(
    candidate: &mut Spreadsheet,
    workbook: &Workbook,
) -> Result<Summary, FacadeError> {
    let mut targets = Vec::new();
    let mut originals = Vec::new();
    let mut sheet_names = std::collections::BTreeSet::new();
    for sheet in &workbook.sheets {
        if !sheet_names.insert(sheet.name.to_ascii_lowercase()) {
            return Err(failure("ambiguous_sheet_names"));
        }
        // The display iterator resolves duplicate source records exactly as
        // cell lookup does; stale formula records must not be recalculated.
        for cell in sheet.display_cells() {
            if let Cell::Formula { cached, .. } = cell.value {
                if targets.len() == MAX_FORMULA_CELLS {
                    return Err(failure("operation_limit_exceeded"));
                }
                targets.push((sheet.name.as_str(), cell.row, cell.col));
                originals.push(cached.as_ref());
            }
        }
    }
    let evaluated = workbook
        .evaluate_cells(&targets)
        .map_err(|reason| failure(reason.code()))?;
    let mut summary = Summary::default();
    let mut updates = Vec::new();
    for (((sheet, row, col), cached), evaluation) in
        targets.into_iter().zip(originals).zip(evaluated)
    {
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
                    updates.push((sheet, row, col, value));
                }
            }
            FormulaEvaluation::Fallback { reason, .. } => {
                summary.unsupported_cells += 1;
                if !summary.reasons.contains(&reason.code()) {
                    summary.reasons.push(reason.code());
                }
            }
            _ => return Err(failure("unsupported_evaluation")),
        }
    }
    summary.reasons.sort_unstable();
    if !updates.is_empty() {
        candidate
            .set_formula_cached_values(&updates)
            .map_err(map_edit_error)?;
    }
    Ok(summary)
}

fn same_cache(left: &Cell, right: &Cell) -> bool {
    match (left, right) {
        (Cell::Number(left) | Cell::Date(left), Cell::Number(right) | Cell::Date(right)) => {
            left == right
        }
        _ => left == right,
    }
}

fn failure(reason: &str) -> FacadeError {
    FacadeError::simple(
        "recalculation_failed",
        format!("Formula recalculation could not complete ({reason}); the edit was not applied."),
        "recalculation",
    )
}
