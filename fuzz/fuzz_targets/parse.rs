#![no_main]
//! Fuzz the `.xls` / `.xlsx` / `.xlsb` / `.ods` readers on arbitrary bytes: the panic-free,
//! bounds-checked contract means no input may panic, abort, OOM, or hang — only
//! return an `Err` or a bounded workbook.
//!
//! ```text
//! cargo +nightly fuzz run parse
//! ```

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rxls::extract_text(data);
    if let Ok(wb) = rxls::Workbook::open(data) {
        for sheet in &wb.sheets {
            let _ = sheet.to_text();
            for _ in sheet.cells() {}
        }
        // Also fuzz the write path: serializing a parsed workbook must never
        // panic or amplify (the `xlsx` feature is enabled in fuzz/Cargo.toml).
        let _ = wb.to_xlsx();
    }
});
