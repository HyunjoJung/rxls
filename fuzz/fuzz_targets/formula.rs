#![no_main]
//! Fuzz the public deterministic formula-evaluation surface with both formulas
//! recovered from arbitrary spreadsheet bytes and bounded formula text projected
//! from arbitrary input. The BIFF/XLSB Ptg decompiler is crate-private, so parsed
//! workbooks are the public end-to-end route through decompilation into evaluation.
//!
//! ```text
//! cargo +nightly fuzz run formula
//! ```

use libfuzzer_sys::fuzz_target;
use rxls::{Cell, Workbook, WorkbookReport};

const MAX_INPUT_FORMULA_BYTES: usize = 4_096;
const MAX_PARSED_FORMULAS: usize = 128;

fn projected_formula(data: &[u8]) -> String {
    const TOKENS: &[&str] = &[
        "0",
        "1",
        "-1",
        "+",
        "-",
        "*",
        "/",
        "^",
        "&",
        "=",
        "<>",
        "(",
        ")",
        ",",
        "%",
        "A1",
        "$B$2",
        "A1:B4",
        "1:3",
        "B:D",
        "Data!A1",
        "'Input Data'!A1:B4",
        "GlobalRate",
        "LocalRate",
        "SUM",
        "AVERAGE",
        "IF",
        "IFERROR",
        "AND",
        "OR",
        "ABS",
        "ROUND",
        "LEN",
        "\"text\"",
        "#N/A",
        "NOW",
        "SEQUENCE",
        "[external.xlsx]Sheet1!A1",
    ];

    let mut formula = String::new();
    for &byte in data.iter().take(512) {
        formula.push_str(TOKENS[usize::from(byte) % TOKENS.len()]);
    }
    formula
}

fn evaluate_parsed_formulas(data: &[u8]) {
    let Ok(workbook) = Workbook::open(data) else {
        return;
    };
    let mut evaluated = 0usize;
    for sheet in &workbook.sheets {
        for (row, col, cell) in sheet.cells() {
            if matches!(cell, Cell::Formula { .. }) {
                let _ = workbook.evaluate_cell(&sheet.name, row, col);
                evaluated += 1;
                if evaluated == MAX_PARSED_FORMULAS {
                    return;
                }
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    evaluate_parsed_formulas(data);

    let bounded = &data[..data.len().min(MAX_INPUT_FORMULA_BYTES)];
    let raw_formula = String::from_utf8_lossy(bounded).into_owned();
    let generated_formula = projected_formula(bounded);

    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("Data");
        for row in 0..8u32 {
            for col in 0..4u16 {
                let byte = bounded
                    .get((row as usize * 4 + usize::from(col)) % bounded.len().max(1))
                    .copied()
                    .unwrap_or(0);
                match byte % 5 {
                    0 => sheet.write(row, col, f64::from(byte) - 64.0),
                    1 => sheet.write(row, col, byte % 2 == 0),
                    2 => sheet.write(row, col, format!("t{byte}")),
                    3 => sheet.write(row, col, Cell::Error("#N/A".into())),
                    _ => sheet.write(row, col, Cell::Date(f64::from(byte))),
                }
            }
        }
    }
    workbook.add_sheet("Input Data").write(0, 0, 7.0);
    workbook.define_name("GlobalRate", "Data!$A$1");
    workbook.define_local_name("Data", "LocalRate", "Data!$B$1");

    let formulas = [
        raw_formula,
        generated_formula,
        "SUM(Data!A1:B4)".to_string(),
        "IF(Data!A1>0,ABS(Data!A1),0)".to_string(),
        "SUM('Input Data'!A1:B4)".to_string(),
        "GlobalRate+LocalRate".to_string(),
        "IFERROR(1/0,#N/A)".to_string(),
        "SUM(Data!1:3)+SUM(Data!B:D)".to_string(),
    ];
    for (row, formula) in formulas.into_iter().enumerate() {
        workbook.sheets[0].write(
            row as u32 + 10,
            0,
            Cell::Formula {
                formula,
                cached: Box::new(Cell::Number(0.0)),
            },
        );
    }

    for row in 10..18 {
        let _ = workbook.evaluate_cell("Data", row, 0);
    }
    let _ = WorkbookReport::from_workbook("fuzz", &workbook).to_json();
});
