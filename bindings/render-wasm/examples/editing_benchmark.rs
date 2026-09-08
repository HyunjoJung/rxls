//! Repeatable native timings for a bounded large-workbook editing workload.

use std::time::Instant;

use rxls::Workbook;
use rxls_render_wasm::RenderSession;
use serde_json::json;

fn main() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    for row in 0..2_000_u32 {
        for col in 0..32_u16 {
            sheet.write(row, col, f64::from(row * 32 + u32::from(col)));
        }
    }
    for row in 0..200_u32 {
        sheet.write_formula(row, 32, format!("A{}+B{}", row + 1, row + 1), 0.0);
    }
    let bytes = workbook.to_xlsx_checked().unwrap();
    let mut samples = Vec::new();
    let mut batch_samples = Vec::new();
    let mut tall_samples = Vec::new();
    let mut wide_samples = Vec::new();
    for _ in 0..3 {
        let mut session = RenderSession::new(&bytes, &[]).unwrap();
        let started = Instant::now();
        for col in 0..8_u16 {
            session
                .set_cell_recalculate_json(
                    &json!({"sheetIndex":0,"row":0,"col":col,"value":{"kind":"number","value":99}})
                        .to_string(),
                )
                .unwrap();
        }
        samples.push(started.elapsed().as_secs_f64());
        let reopened = Workbook::open(&session.save_document_bytes().unwrap()).unwrap();
        assert_eq!(
            reopened.sheets[0].cell(0, 7),
            Some(&rxls::Cell::Number(99.0))
        );
        let mut batch = RenderSession::new(&bytes, &[]).unwrap();
        let request = json!({"sheetIndex":0,"startRow":0,"startCol":0,"values":[vec![json!({"kind":"number","value":99});8]]}).to_string();
        let started = Instant::now();
        batch.set_range_recalculate_json(&request).unwrap();
        batch_samples.push(started.elapsed().as_secs_f64());
        assert_eq!(
            batch.save_document_bytes().unwrap(),
            session.save_document_bytes().unwrap()
        );
        for (rows, cols, samples) in [
            (10_000, 1, &mut tall_samples),
            (1, 10_000, &mut wide_samples),
        ] {
            let mut batch = RenderSession::new(&bytes, &[]).unwrap();
            let request = json!({"sheetIndex":0,"startRow":0,"startCol":0,"values":vec![vec![json!({"kind":"number","value":99});cols];rows]}).to_string();
            let started = Instant::now();
            batch.set_range_recalculate_json(&request).unwrap();
            samples.push(started.elapsed().as_secs_f64());
            let reopened = Workbook::open(&batch.save_document_bytes().unwrap()).unwrap();
            assert_eq!(
                reopened.sheets[0].cell(rows as u32 - 1, cols as u16 - 1),
                Some(&rxls::Cell::Number(99.0))
            );
        }
    }
    println!(
        "{}",
        json!({"schema":"rxls.editing-benchmark.v1","rows":2000,"columns":33,"formulaCells":200,"edits":8,"inputBytes":bytes.len(),"singleSeconds":samples,"batchSeconds":batch_samples,"tall10000Seconds":tall_samples,"wide10000Seconds":wide_samples})
    );
}
