//! Print workbook-level and sheet-level metadata without loading caller code
//! with parser internals.
//!
//! ```text
//! cargo run -p rxls --example metadata -- path/to/book.xlsx
//! ```

use std::process::ExitCode;

use rxls::Workbook;

fn print_optional(label: &str, value: Option<&str>) {
    if let Some(value) = value {
        println!("{label}: {value}");
    }
}

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: metadata <file.xls|.xlsx|.xlsb|.ods>");
        return ExitCode::from(64);
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("read {path}: {e}");
            return ExitCode::from(66);
        }
    };
    let workbook = match Workbook::open(&bytes) {
        Ok(workbook) => workbook,
        Err(e) => {
            eprintln!("parse {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let metadata = workbook.metadata();
    println!("file: {path}");
    println!("date1904: {}", metadata.date1904);
    println!("partial: {}", metadata.text_truncated);

    let properties = metadata.properties;
    print_optional("title", properties.title.as_deref());
    print_optional("subject", properties.subject.as_deref());
    print_optional("creator", properties.creator.as_deref());
    print_optional("keywords", properties.keywords.as_deref());
    print_optional("description", properties.description.as_deref());
    print_optional("company", properties.company.as_deref());
    print_optional("created", properties.created.as_deref());

    println!("defined_names: {}", metadata.defined_names.len());
    for (name, refers_to) in metadata.defined_names {
        println!("defined-name\t{name}\t{refers_to}");
    }

    println!("sheets: {}", metadata.sheets.len());
    for (idx, sheet) in metadata.sheets.iter().enumerate() {
        println!(
            "sheet\t{}\t{}\t{:?}\t{:?}",
            idx + 1,
            sheet.name,
            sheet.typ,
            sheet.visible
        );
    }

    ExitCode::SUCCESS
}
