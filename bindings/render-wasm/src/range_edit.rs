//! Bounded rectangular edits share the single-edit atomic commit boundary.

use super::*;

const MAX_RANGE_EDIT_CELLS: usize = 10_000;
const MAX_RANGE_EDIT_REQUEST_BYTES: usize = 1 << 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Request {
    sheet_index: usize,
    start_row: u32,
    start_col: u16,
    values: Vec<Vec<EditableCell>>,
}

impl RenderSession {
    pub(super) fn set_range_recalculate_json_core(
        &mut self,
        text: &str,
    ) -> Result<String, FacadeError> {
        ensure_editable(&self.spreadsheet)?;
        if text.len() > MAX_RANGE_EDIT_REQUEST_BYTES {
            return Err(FacadeError::limit(
                "rangeEditRequestBytes",
                MAX_RANGE_EDIT_REQUEST_BYTES as u64,
                text.len() as u64,
            ));
        }
        let request: Request =
            serde_json::from_str(text).map_err(|_| invalid("invalid rectangular edit request"))?;
        let columns = request.values.first().map_or(0, Vec::len);
        let count = request.values.len().saturating_mul(columns);
        if count > MAX_RANGE_EDIT_CELLS {
            return Err(FacadeError::limit(
                "rangeEditCells",
                MAX_RANGE_EDIT_CELLS as u64,
                count as u64,
            ));
        }
        if columns == 0 || request.values.iter().any(|row| row.len() != columns) {
            return Err(invalid("cell range must be nonempty and rectangular"));
        }
        let last_row = u64::from(request.start_row) + request.values.len() as u64 - 1;
        let last_col = u64::from(request.start_col) + columns as u64 - 1;
        if last_row > 1_048_575 || last_col > 16_383 {
            return Err(invalid("cell range is outside the Excel grid"));
        }
        let sheet = self
            .workbook
            .sheets
            .get(request.sheet_index)
            .ok_or_else(|| invalid("sheet index is out of range"))?;
        for &(r0, c0, r1, c1) in sheet.merged_ranges() {
            let top = r0.max(request.start_row);
            let left = c0.max(request.start_col);
            let bottom = r1.min(last_row as u32);
            let right = c1.min(last_col as u16);
            if top <= bottom
                && left <= right
                && (top != r0 || left != c0 || bottom != r0 || right != c0)
            {
                return Err(invalid("cell range includes a merged-cell interior"));
            }
        }
        let name = sheet.name.clone();
        let mut required = std::collections::BTreeSet::new();
        let mut values = Vec::with_capacity(request.values.len());
        for (row_offset, row) in request.values.into_iter().enumerate() {
            let mut output = Vec::with_capacity(columns);
            for (col_offset, value) in row.into_iter().enumerate() {
                let cell_bytes = serde_json::to_string(&value)
                    .map_err(|_| invalid("invalid cell value"))?
                    .len();
                if cell_bytes > 128 * 1024 {
                    return Err(FacadeError::limit(
                        "editCellBytes",
                        128 * 1024,
                        cell_bytes as u64,
                    ));
                }
                let cell = match value {
                    EditableCell::Blank => None,
                    EditableCell::Text { value } => Some(Cell::Text(value)),
                    EditableCell::Number { value } => Some(Cell::Number(value)),
                    EditableCell::Date { value } => Some(Cell::Date(value)),
                    EditableCell::Boolean { value } => Some(Cell::Bool(value)),
                    EditableCell::Error { value } => Some(Cell::Error(value)),
                    EditableCell::Formula { formula, cached } => {
                        Some(formula_cell(formula, cached.into_cell())?)
                    }
                    EditableCell::FormulaAuto { formula } => {
                        required.insert((
                            request.sheet_index,
                            request.start_row + row_offset as u32,
                            request.start_col + col_offset as u16,
                        ));
                        // This placeholder is never committed: the shared evaluator must
                        // compute every requested automatic formula before the swap.
                        Some(formula_cell(formula, Cell::Error("#N/A".into()))?)
                    }
                };
                output.push(cell);
            }
            values.push(output);
        }
        self.apply_edit_checked(
            move |candidate| {
                candidate.set_cell_range_values(
                    &name,
                    request.start_row,
                    request.start_col,
                    &values,
                )
            },
            true,
            &required,
        )
    }
}

fn formula_cell(formula: String, cached: Cell) -> Result<Cell, FacadeError> {
    let body = formula.trim().trim_start_matches('=').trim();
    if body.is_empty() {
        return Err(invalid("formula cannot be empty"));
    }
    Ok(Cell::Formula {
        formula: body.into(),
        cached: Box::new(cached),
    })
}

fn invalid(message: &str) -> FacadeError {
    FacadeError::simple("invalid_edit", message, "edit.range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> RenderSession {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data").write(0, 0, 1.0);
        workbook
            .add_sheet("Total")
            .write_formula(0, 0, "Data!A1+Data!B1", 1.0);
        RenderSession::new_core(&workbook.to_xlsx_checked().unwrap(), &[]).unwrap()
    }

    #[test]
    fn rectangular_paste_recalculates_post_paste_formulas_once_and_undoes_together() {
        let mut session = session();
        let before = session.save_document_bytes_core().unwrap();
        let result: serde_json::Value = serde_json::from_str(
            &session
                .set_range_recalculate_json_core(
                    &json!({"sheetIndex":0,"startRow":0,"startCol":0,"values":[
                        [{"kind":"number","value":4},{"kind":"formula-auto","formula":"=A1*2"}],
                        [{"kind":"text","value":"hello"},{"kind":"boolean","value":true}]
                    ]})
                    .to_string(),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["editState"]["undoDepth"], 1);
        assert_eq!(result["recalculation"]["computedCells"], 2);
        assert_eq!(
            session.workbook.sheets[1].cell(0, 0).unwrap().as_f64(),
            Some(12.0)
        );
        let saved = session.save_document_bytes_core().unwrap();
        let reopened = Workbook::open(&saved).unwrap();
        assert_eq!(reopened.sheets[0].cell(0, 1).unwrap().as_f64(), Some(8.0));
        session.undo_edit_core().unwrap();
        assert_eq!(session.save_document_bytes_core().unwrap(), before);
        session.redo_edit_core().unwrap();
        assert_eq!(session.save_document_bytes_core().unwrap(), saved);
    }

    #[test]
    fn rectangular_paste_fails_atomically_and_preserves_redo() {
        let mut session = session();
        session
            .set_cell_json_core(
                r#"{"sheetIndex":0,"row":0,"col":0,"value":{"kind":"number","value":2}}"#,
            )
            .unwrap();
        session.undo_edit_core().unwrap();
        let before = session.save_document_bytes_core().unwrap();
        let history = session.edit_state_json();
        for values in [
            json!([]),
            json!([[]]),
            json!([[{"kind":"blank"}],[]]),
            json!([[{"kind":"number","value":4},{"kind":"formula-auto","formula":"=UNKNOWNFUNCTION()"}]]),
            json!([[{"kind":"formula-auto","formula":"=B1"},{"kind":"formula-auto","formula":"=A1"}]]),
            json!([[{"kind":"formula-auto","formula":"="}]]),
            json!([[{"kind":"text","value":"bad\u{0}xml"}]]),
            json!([[{"kind":"formula","formula":"x".repeat(128*1024),"cached":{"kind":"number","value":0}}]]),
            json!([[{"kind":"text","value":"x".repeat(32768)}]]),
        ] {
            let request = json!({"sheetIndex":0,"startRow":0,"startCol":0,"values":values});
            assert!(session
                .set_range_recalculate_json_core(&request.to_string())
                .is_err());
            assert_eq!(session.save_document_bytes_core().unwrap(), before);
            assert_eq!(session.edit_state_json(), history);
        }
    }

    #[test]
    fn rectangular_paste_limits_ranges_and_request_bytes_before_mutation() {
        let mut session = session();
        let before = session.save_document_bytes_core().unwrap();
        for request in [
            json!({"sheetIndex":0,"startRow":1048575,"startCol":0,"values":[[{"kind":"blank"}],[{"kind":"blank"}]]}),
            json!({"sheetIndex":0,"startRow":0,"startCol":16383,"values":[[{"kind":"blank"},{"kind":"blank"}]]}),
            json!({"sheetIndex":1,"startRow":0,"startCol":0,"values":[vec![json!({"kind":"blank"});10001]]}),
            json!({"sheetIndex":2,"startRow":0,"startCol":0,"values":[[{"kind":"blank"}]]}),
            json!({"sheetIndex":0,"startRow":0,"startCol":0,"unknown":true,"values":[[{"kind":"blank"}]]}),
        ] {
            assert!(session
                .set_range_recalculate_json_core(&request.to_string())
                .is_err());
            assert_eq!(session.save_document_bytes_core().unwrap(), before);
        }
        let request = json!({"sheetIndex":0,"startRow":0,"startCol":0,"values":[[{"kind":"number","value":9}]]}).to_string();
        let exact = format!(
            "{request}{}",
            " ".repeat(MAX_RANGE_EDIT_REQUEST_BYTES - request.len())
        );
        let error = session
            .set_range_recalculate_json_core(&(exact.clone() + " "))
            .unwrap_err();
        assert_eq!(error.resource, Some("rangeEditRequestBytes"));
        assert_eq!(session.save_document_bytes_core().unwrap(), before);
        session.set_range_recalculate_json_core(&exact).unwrap();
        assert_eq!(
            session.workbook.sheets[0].cell(0, 0),
            Some(&Cell::Number(9.0))
        );
        session.set_range_recalculate_json_core(&json!({
            "sheetIndex":0,"startRow":0,"startCol":0,"values":vec![vec![json!({"kind":"blank"});100];100]
        }).to_string()).unwrap();
        assert_eq!(session.edit_state_value()["undoDepth"], 2);
    }

    #[test]
    fn rectangular_paste_rejects_merged_interiors_but_accepts_original_anchor() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Merged").merge(1, 1, 2, 2);
        let mut session =
            RenderSession::new_core(&workbook.to_xlsx_checked().unwrap(), &[]).unwrap();
        let before = session.save_document_bytes_core().unwrap();
        for (row, col, values) in [
            (1, 2, json!([[{"kind":"blank"}]])),
            (
                1,
                1,
                json!([[{"kind":"number","value":1},{"kind":"blank"}]]),
            ),
            (
                0,
                0,
                json!([[{"kind":"blank"},{"kind":"blank"},{"kind":"blank"}],[{"kind":"blank"},{"kind":"blank"},{"kind":"blank"}]]),
            ),
        ] {
            assert!(session
                .set_range_recalculate_json_core(
                    &json!({"sheetIndex":0,"startRow":row,"startCol":col,"values":values})
                        .to_string()
                )
                .is_err());
            assert_eq!(session.save_document_bytes_core().unwrap(), before);
        }
        session.set_range_recalculate_json_core(r#"{"sheetIndex":0,"startRow":1,"startCol":1,"values":[[{"kind":"number","value":3}]]}"#).unwrap();
        assert_eq!(
            session.workbook.sheets[0].cell(1, 1),
            Some(&Cell::Number(3.0))
        );
    }

    #[test]
    fn rectangular_paste_preserves_existing_fallbacks_and_fails_closed_on_shared_budget() {
        let mut workbook = Workbook::new();
        workbook
            .add_sheet("Data")
            .write_formula(0, 0, "UNKNOWNFUNCTION()", 42.0);
        let mut session =
            RenderSession::new_core(&workbook.to_xlsx_checked().unwrap(), &[]).unwrap();
        let request = r#"{"sheetIndex":0,"startRow":0,"startCol":1,"values":[[{"kind":"number","value":3}]]}"#;
        let result: serde_json::Value =
            serde_json::from_str(&session.set_range_recalculate_json_core(request).unwrap())
                .unwrap();
        assert_eq!(result["recalculation"]["unsupportedCells"], 1);
        assert_eq!(
            session.workbook.sheets[0].cell(0, 0).unwrap().as_f64(),
            Some(42.0)
        );
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        for row in 0..10001 {
            sheet.write_formula(row, 0, "1+1", 2.0);
        }
        let mut session =
            RenderSession::new_core(&workbook.to_xlsx_checked().unwrap(), &[]).unwrap();
        let before = session.save_document_bytes_core().unwrap();
        assert_eq!(
            session
                .set_range_recalculate_json_core(request)
                .unwrap_err()
                .code,
            "recalculation_failed"
        );
        assert_eq!(session.save_document_bytes_core().unwrap(), before);
        assert_eq!(session.edit_state_value()["undoDepth"], 0);
    }

    #[test]
    fn rectangular_paste_rejects_legacy_formats_and_preserves_xlsm_history() {
        for bytes in [
            include_bytes!("../../../tests/fixtures/xls/reader-basic.xls").as_slice(),
            include_bytes!("../../../tests/fixtures/ods/repeated-hidden.ods").as_slice(),
            include_bytes!("../../../tests/fixtures/xlsb/reader-basic.xlsb").as_slice(),
        ] {
            let mut session = RenderSession::new_core(bytes, &[]).unwrap();
            assert_eq!(session.set_range_recalculate_json_core(r#"{"sheetIndex":0,"startRow":0,"startCol":0,"values":[[{"kind":"number","value":3}]]}"#).unwrap_err().code, "edit_read_only");
            assert_eq!(session.edit_state_value()["undoDepth"], 0);
        }
        let bytes = include_bytes!("../../../viewer/samples/apache-poi-simple-macro.xlsm");
        let mut session = RenderSession::new_core(bytes, &[]).unwrap();
        let before = session.save_document_bytes_core().unwrap();
        session.set_range_recalculate_json_core(r#"{"sheetIndex":0,"startRow":0,"startCol":0,"values":[[{"kind":"number","value":3},{"kind":"text","value":"batch"}]]}"#).unwrap();
        assert!(session
            .edited_parts
            .iter()
            .all(|part| part.starts_with("xl/worksheets/")));
        let saved = session.save_document_bytes_core().unwrap();
        session.undo_edit_core().unwrap();
        assert_eq!(session.save_document_bytes_core().unwrap(), before);
        session.redo_edit_core().unwrap();
        assert_eq!(session.save_document_bytes_core().unwrap(), saved);
    }
}
