//! Convert any Excel file (legacy `.xls` or modern `.xlsx`) to a clean `.xlsx`
//! through the typed workbook model.
//!
//! ```text
//! cargo run --example to_xlsx -- input.xls [output.xlsx]
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(input) = args.next() else {
        eprintln!("usage: to_xlsx <input.xls|.xlsx> [output.xlsx]");
        return ExitCode::from(2);
    };
    let out = args.next().map(PathBuf::from).unwrap_or_else(|| {
        let mut p = PathBuf::from(&input);
        p.set_extension("out.xlsx");
        p
    });

    let bytes = match std::fs::read(&input) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let wb = match rxls::Workbook::open(&bytes) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("parse {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::write(&out, wb.to_xlsx()) {
        eprintln!("write {}: {e}", out.display());
        return ExitCode::FAILURE;
    }
    eprintln!("wrote {} ({} sheets)", out.display(), wb.sheets.len());
    ExitCode::SUCCESS
}
