//! Round-trip self-test: read an Excel file, write it back as `.xlsx`, re-read
//! the result, and report how many typed cells survived. Used to validate the
//! write layer at corpus scale.
//!
//! ```text
//! cargo run --example roundtrip -- input.xls
//! ```
//! Prints `PASS <name> cells=<a>/<b> sheets=<a>/<b>` or `FAIL <name> <why>`.

use std::process::ExitCode;

use rxls::Workbook;

fn cell_count(wb: &Workbook) -> usize {
    wb.sheets.iter().map(|s| s.cells().count()).sum()
}

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: roundtrip <input.xls|.xlsx>");
        return ExitCode::from(2);
    };
    let name = path.rsplit(['/', '\\']).next().unwrap_or(&path).to_string();
    let Ok(bytes) = std::fs::read(&path) else {
        println!("FAIL {name} read-error");
        return ExitCode::FAILURE;
    };
    let w1 = match Workbook::open(&bytes) {
        Ok(w) => w,
        Err(e) => {
            println!("FAIL {name} open1: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (c1, s1) = (cell_count(&w1), w1.sheets.len());

    let xlsx = w1.to_xlsx();
    let w2 = match Workbook::open(&xlsx) {
        Ok(w) => w,
        Err(e) => {
            println!("FAIL {name} reopen: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (c2, s2) = (cell_count(&w2), w2.sheets.len());

    println!("PASS {name} cells={c1}/{c2} sheets={s1}/{s2}");
    ExitCode::SUCCESS
}
