#![no_main]
//! Fuzz the `.xlsx` AUTHORING path: drive the public builder API from arbitrary
//! bytes, then serialize. The panic-free / no-OOM / no-hang / valid-OOXML contract
//! means no sequence of authoring ops may panic, abort, hang, or amplify — the
//! coordinate clamps, under-merge drop, dedup, sheet-name sanitize, and the
//! `to_xlsx` allocation budget must hold for any input.
//!
//! ```text
//! cargo +nightly fuzz run author
//! ```

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use rxls::{Cell, CellStyle, Workbook};

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let mut wb = Workbook::new();

    let n_sheets = u.int_in_range(0u8..=4).unwrap_or(0);
    for _ in 0..n_sheets {
        let name = String::arbitrary(&mut u).unwrap_or_default();
        let sheet = wb.add_sheet(&name);
        let n_ops = u.int_in_range(0u16..=300).unwrap_or(0);
        for _ in 0..n_ops {
            if u.is_empty() {
                break;
            }
            let row = u32::arbitrary(&mut u).unwrap_or(0);
            let col = u16::arbitrary(&mut u).unwrap_or(0);
            match u.int_in_range(0u8..=6).unwrap_or(0) {
                0 => sheet.write(row, col, String::arbitrary(&mut u).unwrap_or_default()),
                1 => sheet.write(row, col, f64::arbitrary(&mut u).unwrap_or(0.0)),
                2 => sheet.write(row, col, Cell::Date(f64::arbitrary(&mut u).unwrap_or(0.0))),
                3 => {
                    let r1 = u32::arbitrary(&mut u).unwrap_or(row);
                    let c1 = u16::arbitrary(&mut u).unwrap_or(col);
                    sheet.merge(row, col, r1, c1);
                }
                4 => {
                    let url = String::arbitrary(&mut u).unwrap_or_default();
                    let text = String::arbitrary(&mut u).unwrap_or_default();
                    sheet.write_url(row, col, &url, &text);
                }
                5 => {
                    sheet.set_col_width(col, f32::arbitrary(&mut u).unwrap_or(10.0));
                    sheet.set_row_height(row, f32::arbitrary(&mut u).unwrap_or(15.0));
                    sheet.freeze_panes(row, col);
                    sheet.autofilter(row, col, row, col);
                }
                _ => {
                    let style = CellStyle::new()
                        .num_fmt(&String::arbitrary(&mut u).unwrap_or_default())
                        .fill(<[u8; 3]>::arbitrary(&mut u).unwrap_or([0, 0, 0]));
                    sheet.write_styled(row, col, String::arbitrary(&mut u).unwrap_or_default(), &style);
                }
            }
        }
    }

    let _ = wb.to_xlsx();
});
