//! Compile-checked public API journeys: create, read, inspect, evaluate, export,
//! diagnose, edit, save, and reopen.
//!
//! ```text
//! cargo run --example api_journeys
//! cargo run --example api_journeys -- target/api-journeys-edited.xlsx
//! ```

use rxls::{
    export_csv, Cell, CsvFormulaPolicy, CsvOptions, FormulaEvaluation, Spreadsheet, Workbook,
    WorkbookReport,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let edited_output = args.next();
    if args.next().is_some() {
        return Err("usage: api_journeys [EDITED_XLSX_OUTPUT]".into());
    }

    // Create and write through the checked authoring path.
    let mut authored = Workbook::new();
    let sheet = authored.add_sheet("Data");
    sheet.write(0, 0, "amount");
    sheet.write(1, 0, 21.0);
    sheet.write_formula(1, 1, "A2*2", 42.0);
    let bytes = authored.to_xlsx_checked()?;

    // Read and inspect borrowed metadata/cells.
    let workbook = Workbook::open(&bytes)?;
    let metadata = workbook.metadata();
    assert_eq!(metadata.sheets().len(), 1);
    assert_eq!(workbook.sheets[0].cell(1, 0), Some(&Cell::Number(21.0)));

    // Evaluate with a future-proof wildcard for the non-exhaustive result.
    match workbook.evaluate_cell("Data", 1, 1) {
        FormulaEvaluation::Computed(Cell::Number(value)) => assert_eq!(value, 42.0),
        FormulaEvaluation::Computed(other) => panic!("unexpected value: {other:?}"),
        FormulaEvaluation::Fallback { reason, .. } => panic!("unexpected fallback: {reason:?}"),
        _ => panic!("unknown formula-evaluation result"),
    }

    // Export and diagnose through stable public output contracts.
    let csv_options = CsvOptions {
        formula_policy: CsvFormulaPolicy::Escape,
        ..CsvOptions::default()
    };
    let csv = export_csv(&workbook.sheets[0], csv_options)?;
    assert!(csv.contains("amount"));
    let report = WorkbookReport::from_workbook_with_package("xlsx", &workbook, &bytes);
    assert_eq!(report.schema_version, rxls::REPORT_SCHEMA_VERSION);

    // Package-preserving edit, save, and reopen.
    let mut editable = Spreadsheet::open(&bytes)?;
    editable.set_cell_value("Data", 1, 0, Cell::Number(84.0))?;
    let edited = editable.save()?;
    let reopened = Workbook::open(&edited)?;
    assert_eq!(reopened.sheets[0].cell(1, 0), Some(&Cell::Number(84.0)));
    if let Some(path) = edited_output {
        std::fs::write(path, &edited)?;
    }
    Ok(())
}
