//! Generate a synthetic XLSX and matching CLI-style CSV files.
//!
//! cargo +1.85.0 run --locked --example csv_export
//! See docs/compatibility.md for equivalent CLI commands.

use rxls::{export_csv, CsvExportError, CsvFormulaPolicy, CsvNewline, CsvOptions, Workbook};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    for (col, header) in ["name", "note", "input"].iter().enumerate() {
        sheet.write(0, col as u16, *header);
    }
    sheet.write(1, 0, "a,\"b\"");
    sheet.write(1, 1, "line1\nline2");
    // This is text, not a workbook formula.
    sheet.write(1, 2, "=1+1");

    let directory = std::path::Path::new("target/csv-recipe");
    std::fs::create_dir_all(directory)?;
    std::fs::write(directory.join("source.xlsx"), workbook.to_xlsx_checked()?)?;
    // Export the reopened workbook, just as the CLI reads the generated file.
    let reopened = Workbook::open(&std::fs::read(directory.join("source.xlsx"))?)?;
    let sheet = &reopened.sheets[0];
    let preserve = CsvOptions {
        max_output_bytes: 1024,
        ..CsvOptions::default()
    };
    let escape = CsvOptions {
        formula_policy: CsvFormulaPolicy::Escape,
        ..preserve
    };
    let excel = CsvOptions {
        delimiter: ';',
        newline: CsvNewline::CrLf,
        bom: true,
        ..escape
    };
    for (name, options, ending) in [
        ("preserve", preserve, "\n"),
        ("escape", escape, "\n"),
        ("excel", excel, "\r\n"),
    ] {
        let mut csv = export_csv(sheet, options)?;
        // The library omits the final separator. The CLI adds one to nonempty
        // output and includes those bytes in the same output limit.
        if !csv.is_empty() {
            if csv.len().saturating_add(ending.len()) > options.max_output_bytes {
                return Err(CsvExportError::OutputTooLarge {
                    limit: options.max_output_bytes,
                }
                .into());
            }
            csv.push_str(ending);
        }
        std::fs::write(directory.join(format!("{name}.rust.csv")), csv)?;
    }

    let limited = CsvOptions {
        max_output_bytes: 8,
        ..preserve
    };
    match export_csv(sheet, limited) {
        Err(CsvExportError::OutputTooLarge { limit: 8 }) => {
            println!("8-byte export rejected before any CSV was returned");
        }
        other => return Err(format!("unexpected limited export result: {other:?}").into()),
    }
    Ok(())
}
