//! Deserialize a header and two rows into typed records without input files.
//!
//! ```text
//! cargo +1.85.0 run --locked --features serde --example typed_rows
//! ```
//!
//! Requires `serde` and `xlsx`; default features already enable `xlsx`.

use rxls::{RangeDeserializerBuilder, Workbook};
use serde::Deserialize;

#[derive(Deserialize)]
struct BidRow {
    name: String,
    price: f64,
    awarded: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut authored = Workbook::new();
    let sheet = authored.add_sheet("Data");
    sheet.write(0, 0, "name");
    sheet.write(0, 1, "price");
    sheet.write(0, 2, "awarded");
    sheet.write(1, 0, "Road");
    sheet.write(1, 1, 125.5);
    sheet.write(1, 2, true);
    sheet.write(2, 0, "Bridge");
    sheet.write(2, 1, 88.0);
    sheet.write(2, 2, false);

    // Exercise the checked XLSX writer and reader entirely in memory.
    let bytes = authored.to_xlsx_checked()?;
    let workbook = Workbook::open(&bytes)?;
    let range = workbook
        .worksheet_range("Data")
        .ok_or("the Data worksheet is missing")?;

    // The first nonempty row supplies the struct's field names.
    // Header construction and each row conversion can fail; propagate both.
    let rows: Vec<BidRow> = RangeDeserializerBuilder::new()
        .from_range(&range)?
        .collect::<Result<_, _>>()?;
    for row in rows {
        println!("{}: price={}, awarded={}", row.name, row.price, row.awarded);
    }
    Ok(())
}
