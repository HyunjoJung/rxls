//! Release-mode package-preserving edit/save performance consumer.
//!
//! ```text
//! cargo run --release --example edit_save_benchmark -- input.xlsx output.xlsx
//! ```

use std::io::{Error as IoError, ErrorKind};

use rxls::{Cell, Spreadsheet};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = args.next().ok_or_else(|| {
        IoError::new(
            ErrorKind::InvalidInput,
            "usage: edit_save_benchmark INPUT.xlsx OUTPUT.xlsx",
        )
    })?;
    let output = args.next().ok_or_else(|| {
        IoError::new(
            ErrorKind::InvalidInput,
            "usage: edit_save_benchmark INPUT.xlsx OUTPUT.xlsx",
        )
    })?;
    if args.next().is_some() {
        return Err(IoError::new(
            ErrorKind::InvalidInput,
            "usage: edit_save_benchmark INPUT.xlsx OUTPUT.xlsx",
        )
        .into());
    }

    let bytes = std::fs::read(input)?;
    let mut spreadsheet = Spreadsheet::open(&bytes)?;
    let sheet_name = spreadsheet
        .workbook()
        .sheet_names()
        .first()
        .ok_or_else(|| IoError::new(ErrorKind::InvalidData, "workbook has no worksheets"))?
        .to_string();
    spreadsheet.set_cell_value(
        &sheet_name,
        0,
        0,
        Cell::Text("rxls package-preserving edit benchmark".to_string()),
    )?;
    std::fs::write(output, spreadsheet.save()?)?;
    Ok(())
}
