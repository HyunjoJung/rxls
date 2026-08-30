//! Build a styled monthly operations `.xlsx` report (an end-to-end authoring scenario) and
//! write it to the given path. Exercises the authoring API end to end.
//!
//! ```text
//! cargo run --example author_report -- report.xlsx
//! ```

use rxls::{Cell, CellStyle, HAlign, PageSetup, Workbook};

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "report.xlsx".to_string());

    let mut wb = Workbook::new();
    let sheet = wb.add_sheet("Operations");

    // Merged, colored title row.
    let title = CellStyle::new()
        .bold()
        .size(14)
        .color([255, 255, 255])
        .fill([0x17, 0x6B, 0x3A])
        .align(HAlign::Center);
    sheet.write_styled(0, 0, "Q3 Operations Snapshot", &title);
    sheet.merge(0, 0, 0, 4);
    sheet.set_row_height(0, 25.0);

    // Shaded, bold, wrapped header row.
    let hdr = CellStyle::new()
        .bold()
        .fill([0xE1, 0xEC, 0xE5])
        .align(HAlign::Center)
        .wrap();
    for (c, h) in ["Workstream", "Owner", "Budget", "Due", "Status"]
        .iter()
        .enumerate()
    {
        sheet.write_styled(2, c as u16, *h, &hdr);
    }

    let money = CellStyle::new().num_fmt("$#,##0");
    let date = CellStyle::new().num_fmt("mmm d, yyyy");
    let on_track = CellStyle::new().bold().color([0x17, 0x6B, 0x3A]);
    let watch = CellStyle::new().bold().color([0xA1, 0x47, 0x00]);
    let workstreams = [
        (
            "Platform reliability",
            "Core",
            420_000.0,
            46_250.0,
            "On track",
        ),
        (
            "Customer migration",
            "Success",
            185_000.0,
            46_264.0,
            "On track",
        ),
        ("Data quality", "Data", 235_000.0, 46_295.0, "Watch"),
        (
            "Security review",
            "Security",
            96_000.0,
            46_295.0,
            "On track",
        ),
        ("Regional launch", "Growth", 310_000.0, 46_325.0, "Watch"),
        ("Support automation", "Ops", 128_000.0, 46_325.0, "On track"),
    ];
    for (offset, (workstream, owner, budget, due, status)) in workstreams.iter().enumerate() {
        let row = offset as u32 + 3;
        if offset == 0 {
            sheet.write_url(row, 0, "https://github.com/HyunjoJung/rxls", *workstream);
        } else {
            sheet.write(row, 0, *workstream);
        }
        sheet.write(row, 1, *owner);
        sheet.write_styled(row, 2, *budget, &money);
        sheet.write_styled(row, 3, Cell::date(*due), &date);
        sheet.write_styled(
            row,
            4,
            *status,
            if *status == "Watch" {
                &watch
            } else {
                &on_track
            },
        );
    }

    let total = CellStyle::new()
        .bold()
        .fill([0xEC, 0xF2, 0xEE])
        .num_fmt("$#,##0");
    sheet.write_styled(9, 0, "Portfolio total", &total);
    sheet.merge(9, 0, 9, 1);
    sheet.write_styled(9, 2, Cell::formula("SUM(C4:C9)", 1_374_000.0), &total);
    sheet.write_styled(9, 3, "6 initiatives", &total);
    sheet.write_styled(9, 4, "4 on track", &total);

    // Layout: column widths, frozen header, autofilter over the table.
    sheet.set_col_width(0, 25.0);
    sheet.set_col_width(1, 14.0);
    sheet.set_col_width(2, 15.0);
    sheet.set_col_width(3, 15.0);
    sheet.set_col_width(4, 13.0);
    sheet.freeze_panes(3, 0);
    sheet.autofilter(2, 0, 8, 4);
    sheet.set_page_setup(
        PageSetup::new()
            .with_landscape()
            .with_fit_to_pages(1, 1)
            .with_print_area((0, 0, 9, 4))
            .with_header("&Lrxls&COperations report&RQ3 2026")
            .with_footer("&CPage &P of &N"),
    );

    let metrics = wb.add_sheet("Metrics");
    metrics.write_styled(0, 0, "Delivery metrics", &title);
    metrics.merge(0, 0, 0, 3);
    for (col, heading) in ["Metric", "Current", "Target", "Health"].iter().enumerate() {
        metrics.write_styled(2, col as u16, *heading, &hdr);
    }
    let percent = CellStyle::new().num_fmt("0.00%");
    let metric_rows = [
        (
            "Availability",
            Cell::Number(0.9998),
            Cell::Number(0.9995),
            "Ahead",
        ),
        (
            "Automated checks",
            Cell::Number(1_092.0),
            Cell::Number(1_000.0),
            "Ahead",
        ),
        (
            "Corpus files",
            Cell::Number(916.0),
            Cell::Number(900.0),
            "Ahead",
        ),
        (
            "Unexpected failures",
            Cell::Number(0.0),
            Cell::Number(0.0),
            "Clear",
        ),
    ];
    for (offset, (metric, current, target, health)) in metric_rows.iter().enumerate() {
        let row = offset as u32 + 3;
        metrics.write(row, 0, *metric);
        if offset == 0 {
            metrics.write_styled(row, 1, current.clone(), &percent);
            metrics.write_styled(row, 2, target.clone(), &percent);
        } else {
            metrics.write(row, 1, current.clone());
            metrics.write(row, 2, target.clone());
        }
        metrics.write_styled(row, 3, *health, &on_track);
    }
    metrics.set_col_width(0, 25.0);
    metrics.set_col_width(1, 14.0);
    metrics.set_col_width(2, 14.0);
    metrics.set_col_width(3, 14.0);
    metrics.freeze_panes(3, 0);
    metrics.set_page_setup(
        PageSetup::new()
            .with_fit_to_pages(1, 1)
            .with_print_area((0, 0, 6, 3))
            .with_footer("&CPage &P of &N"),
    );

    std::fs::write(&out, wb.to_xlsx()).expect("write report");
    eprintln!("wrote {out}");
}
