//! Build a styled bid-comparison `.xlsx` report (the PRD north-star scenario) and
//! write it to the given path. Exercises the authoring API end to end.
//!
//! ```text
//! cargo run --example author_report -- report.xlsx
//! ```

use rxls::{Cell, CellStyle, HAlign, Workbook};

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "report.xlsx".to_string());

    let mut wb = Workbook::new();
    let sheet = wb.add_sheet("입찰공고");

    // Merged, colored title row.
    let title = CellStyle::new()
        .bold()
        .size(14)
        .color([255, 255, 255])
        .fill([0x2F, 0x54, 0x96])
        .align(HAlign::Center);
    sheet.write_styled(0, 0, "나라장터 입찰공고 비교", &title);
    sheet.merge(0, 0, 0, 4);

    // Shaded, bold, wrapped header row.
    let hdr = CellStyle::new()
        .bold()
        .fill([0xDD, 0xEB, 0xF7])
        .align(HAlign::Center)
        .wrap();
    for (c, h) in ["공고명", "기관", "추정가격", "마감일시", "자격"]
        .iter()
        .enumerate()
    {
        sheet.write_styled(1, c as u16, *h, &hdr);
    }

    // Data row: hyperlinked 공고명, ₩ amount, date.
    let won = CellStyle::new().num_fmt("₩#,##0");
    let date = CellStyle::new().num_fmt("yyyy-mm-dd");
    sheet.write_url(
        2,
        0,
        "https://www.g2b.go.kr/co/cobs/0001",
        "26~27시즌 KT농구단 뉴미디어 콘텐츠 제작",
    );
    sheet.write(2, 1, "케이티스포츠");
    sheet.write_styled(2, 2, 150_000_000.0, &won);
    sheet.write_styled(2, 3, Cell::date(46_000.0), &date);
    sheet.write(2, 4, "제한경쟁");

    // Layout: column widths, frozen header, autofilter over the table.
    sheet.set_col_width(0, 42.0);
    sheet.set_col_width(1, 16.0);
    sheet.set_col_width(2, 16.0);
    sheet.set_col_width(3, 16.0);
    sheet.set_col_width(4, 12.0);
    sheet.freeze_panes(2, 0);
    sheet.autofilter(1, 0, 2, 4);

    std::fs::write(&out, wb.to_xlsx()).expect("write report");
    eprintln!("wrote {out}");
}
