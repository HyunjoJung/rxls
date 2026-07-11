//! JavaScript bindings for the portable `rxls::wasm` byte adapters.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use wasm_bindgen::prelude::*;

fn js_error(err: rxls::Error) -> JsValue {
    JsValue::from_str(&err.to_string())
}

/// Extract workbook text from spreadsheet bytes.
#[wasm_bindgen(js_name = extractText)]
pub fn extract_text(bytes: &[u8]) -> std::result::Result<String, JsValue> {
    rxls::wasm::extract_text_bytes(bytes).map_err(js_error)
}

/// Export one worksheet as CSV from spreadsheet bytes.
#[wasm_bindgen(js_name = toCsv)]
pub fn to_csv(bytes: &[u8], sheet_index: usize) -> std::result::Result<String, JsValue> {
    rxls::wasm::to_csv_bytes(bytes, sheet_index).map_err(js_error)
}

/// Export one worksheet as an HTML table fragment from spreadsheet bytes.
#[wasm_bindgen(js_name = toHtml)]
pub fn to_html(bytes: &[u8], sheet_index: usize) -> std::result::Result<String, JsValue> {
    rxls::wasm::to_html_bytes(bytes, sheet_index).map_err(js_error)
}

/// Build the machine-readable diagnose JSON report from spreadsheet bytes.
#[wasm_bindgen(js_name = reportJson)]
pub fn report_json(bytes: &[u8]) -> std::result::Result<String, JsValue> {
    rxls::wasm::report_json_bytes(bytes).map_err(js_error)
}
