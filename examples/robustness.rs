//! Robustness + structure harness: run the full read pipeline (open → text →
//! typed cells) over many `.xls` files, distinguishing a clean [`rxls::Error`]
//! from a real panic (the crate's `#![forbid(unsafe_code)]`, panic-free
//! contract), and tallying the structure produced.
//!
//! ```text
//! cargo run -p rxls --example robustness -- file1.xls file2.xls ...
//! ```
//!
//! Output is one line per file (`OK` / `ERR <kind>` / `PANIC`) plus a summary
//! to stderr with `ok` / `clean_err` (broken down by [`rxls::Error`] variant) /
//! `PANIC` counts and aggregate cell-kind structure. Any `PANIC` is a contract
//! violation: this binary is the fuzz oracle for the panic-free claim.

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};

use rxls::{Cell, Error};

/// Short, stable label for each [`Error`] variant (for the by-kind tally).
fn err_kind(e: &Error) -> &'static str {
    match e {
        Error::NotOle2 => "NotOle2",
        Error::LegacyBiff => "LegacyBiff",
        Error::Cfb(_) => "Cfb",
        Error::InvalidCfb(_) => "InvalidCfb",
        Error::MissingWorkbook => "MissingWorkbook",
        Error::Biff(_) => "Biff",
        Error::Zip(_) => "Zip",
        Error::UnsupportedCompression { .. } => "UnsupportedCompression",
        Error::Xml(_) => "Xml",
        Error::Encrypted => "Encrypted",
        Error::EncryptedPackage => "EncryptedPackage",
        Error::EncryptedOpenDocument => "EncryptedOpenDocument",
        Error::NoText => "NoText",
        Error::SheetOutOfRange => "SheetOutOfRange",
        _ => "Other",
    }
}

fn main() {
    std::panic::set_hook(Box::new(|_| {})); // silence default backtrace noise
    let (mut ok, mut err, mut panic) = (0u32, 0u32, 0u32);
    let mut kinds: BTreeMap<&'static str, u32> = BTreeMap::new();
    let (mut sheets, mut cells) = (0u64, 0u64);
    let (mut texts, mut numbers, mut dates, mut bools, mut errs) = (0u64, 0u64, 0u64, 0u64, 0u64);

    for path in std::env::args().skip(1) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        // Exercise the whole read surface inside the unwind boundary: container
        // parse, text flattening, and a full typed-cell walk of every sheet.
        let result = catch_unwind(AssertUnwindSafe(|| match rxls::Workbook::open(&bytes) {
            Ok(wb) => {
                let _ = wb.text();
                let mut s = 0u64;
                let (mut n, mut t, mut nu, mut da, mut bo, mut er) = (0, 0, 0, 0, 0, 0);
                for sheet in &wb.sheets {
                    s += 1;
                    let _ = sheet.to_text();
                    let _ = sheet.dimensions();
                    for (_, _, cell) in sheet.cells() {
                        n += 1;
                        match cell {
                            Cell::Text(_) => t += 1,
                            Cell::Number(_) => nu += 1,
                            Cell::Date(_) => da += 1,
                            Cell::Bool(_) => bo += 1,
                            Cell::Error(_) => er += 1,
                            Cell::Formula { .. } => {}
                        }
                    }
                }
                Ok::<_, &'static str>((s, n, t, nu, da, bo, er))
            }
            Err(e) => Err(err_kind(&e)),
        }));
        match result {
            Ok(Ok((s, n, t, nu, da, bo, er))) => {
                ok += 1;
                sheets += s;
                cells += n;
                texts += t;
                numbers += nu;
                dates += da;
                bools += bo;
                errs += er;
                println!("OK\t{path}\tsheets={s} cells={n}");
            }
            Ok(Err(kind)) => {
                err += 1;
                *kinds.entry(kind).or_default() += 1;
                println!("ERR\t{path}\t{kind}");
            }
            Err(_) => {
                panic += 1;
                println!("PANIC\t{path}");
            }
        }
    }
    let by_kind: Vec<String> = kinds.iter().map(|(k, v)| format!("{k}={v}")).collect();
    eprintln!(
        "=== files: ok={ok} clean_err={err} PANIC={panic} \
         | clean_err breakdown: [{}] \
         | structure: sheets={sheets} cells={cells} \
         (text={texts} number={numbers} date={dates} bool={bools} error={errs}) ===",
        by_kind.join(" ")
    );
}
