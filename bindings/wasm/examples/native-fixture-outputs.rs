//! Native oracle used by the JavaScript distribution smoke tests.
//!
//! This example is intentionally not a public product surface. It runs the
//! ordinary native workbook/report/export APIs so the generated WASM package
//! can be compared with a separately compiled native executable.

use std::env;
use std::fs;
use std::path::PathBuf;

use rxls::{Workbook, WorkbookReport};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let output =
        PathBuf::from(args.next().ok_or(
            "usage: native-fixture-outputs OUTPUT_DIR ID FORMAT FILE [ID FORMAT FILE ...]",
        )?);
    let remaining: Vec<_> = args.collect();
    if remaining.is_empty() || remaining.len() % 3 != 0 {
        return Err(
            "usage: native-fixture-outputs OUTPUT_DIR ID FORMAT FILE [ID FORMAT FILE ...]".into(),
        );
    }
    fs::create_dir_all(&output)?;

    for entry in remaining.chunks_exact(3) {
        let id = entry[0].to_string_lossy();
        let format = entry[1].to_string_lossy();
        if !id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-') {
            return Err(format!("invalid fixture id {id:?}").into());
        }
        let bytes = fs::read(&entry[2])?;
        let workbook = Workbook::open(&bytes)?;
        let text = rxls::extract_text(&bytes)?;
        let csv = workbook.to_csv(0).ok_or("fixture has no worksheet 0")?;
        let html = workbook.to_html(0).ok_or("fixture has no worksheet 0")?;
        let report = WorkbookReport::from_workbook_with_package(format.as_ref(), &workbook, &bytes);

        fs::write(output.join(format!("{id}.text")), text)?;
        fs::write(output.join(format!("{id}.csv")), csv)?;
        fs::write(output.join(format!("{id}.html")), html)?;
        fs::write(output.join(format!("{id}.report.json")), report.to_json())?;
    }

    Ok(())
}
