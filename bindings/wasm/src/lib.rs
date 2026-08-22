//! JavaScript bindings for the portable `rxls::wasm` byte adapters.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use wasm_bindgen::prelude::*;

/// Maximum JavaScript input accepted by the synchronous byte adapters.
///
/// `wasm-bindgen` copies a `Uint8Array` into WebAssembly linear memory before
/// Rust receives the slice. Keeping the public limit explicit prevents a
/// parser call from multiplying an already-large allocation unexpectedly.
pub const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;

#[wasm_bindgen(typescript_custom_section)]
const ERROR_TYPES: &str = r#"
/** Stable shape thrown by synchronous rxls-wasm adapter functions. */
export interface RxlsErrorObject extends Error {
  readonly name: "RxlsError";
  readonly kind: string;
  readonly location: string;
  readonly cause: string | null;
}
"#;

#[derive(Debug, PartialEq, Eq)]
struct ErrorDetails {
    kind: &'static str,
    location: &'static str,
    cause: Option<String>,
}

fn error_details(err: &rxls::Error) -> ErrorDetails {
    use rxls::Error;

    let (kind, location, cause) = match err {
        Error::NotOle2 => ("not_ole2", "container", None),
        Error::LegacyBiff => ("legacy_biff", "workbook", None),
        Error::Cfb(cause) => ("cfb", "container", Some(cause.to_string())),
        Error::InvalidCfb(cause) => ("invalid_cfb", "container", Some((*cause).into())),
        Error::MissingWorkbook => ("missing_workbook", "workbook", None),
        Error::Biff(cause) => ("biff", "workbook", Some((*cause).into())),
        Error::Zip(cause) => ("zip", "container", Some((*cause).into())),
        Error::UnsupportedCompression { part, method } => (
            "unsupported_compression",
            "container",
            Some(format!("part {part} uses ZIP compression method {method}")),
        ),
        Error::Xml(cause) => ("xml", "xml", Some((*cause).into())),
        Error::Encrypted => ("encrypted", "workbook", None),
        Error::EncryptedPackage => ("encrypted_package", "container", None),
        Error::EncryptedOpenDocument => ("encrypted_open_document", "container", None),
        Error::NoText => ("no_text", "workbook", None),
        Error::SheetOutOfRange => ("sheet_out_of_range", "sheet_index", None),
        Error::ExportInvalidDelimiter { format, delimiter } => (
            "invalid_export_delimiter",
            "output",
            Some(format!("{format} delimiter {delimiter:?}")),
        ),
        Error::ExportOutputTooLarge { format, limit } => (
            "output_too_large",
            "output",
            Some(format!("{format} output limit is {limit} bytes")),
        ),
        Error::ExportWorkLimitExceeded { format, limit } => (
            "work_limit_exceeded",
            "output",
            Some(format!("{format} work limit is {limit} operations")),
        ),
        #[allow(unreachable_patterns)]
        _ => ("unknown", "workbook", None),
    };
    ErrorDetails {
        kind,
        location,
        cause,
    }
}

fn js_error(err: rxls::Error) -> JsValue {
    let details = error_details(&err);
    make_js_error(
        details.kind,
        &err.to_string(),
        details.location,
        details.cause.as_deref(),
    )
}

fn make_js_error(kind: &str, message: &str, location: &str, cause: Option<&str>) -> JsValue {
    let error = js_sys::Error::new(message);
    error.set_name("RxlsError");
    let object: &JsValue = error.as_ref();
    let _ = js_sys::Reflect::set(object, &JsValue::from_str("kind"), &JsValue::from_str(kind));
    let _ = js_sys::Reflect::set(
        object,
        &JsValue::from_str("location"),
        &JsValue::from_str(location),
    );
    let cause = cause.map_or(JsValue::NULL, JsValue::from_str);
    let _ = js_sys::Reflect::set(object, &JsValue::from_str("cause"), &cause);
    error.into()
}

fn check_input(bytes: &[u8]) -> std::result::Result<(), JsValue> {
    if bytes.len() <= MAX_INPUT_BYTES {
        return Ok(());
    }
    Err(make_js_error(
        "input_too_large",
        &format!(
            "input is {} bytes; rxls-wasm accepts at most {MAX_INPUT_BYTES} bytes",
            bytes.len()
        ),
        "input",
        None,
    ))
}

/// Return the maximum accepted input size in bytes.
#[wasm_bindgen(js_name = maxInputBytes)]
pub fn max_input_bytes() -> usize {
    MAX_INPUT_BYTES
}

/// Return the maximum CSV, HTML, or Markdown output size in UTF-8 bytes.
#[wasm_bindgen(js_name = maxExportOutputBytes)]
pub fn max_export_output_bytes() -> usize {
    rxls::wasm::MAX_EXPORT_OUTPUT_BYTES
}

/// Extract workbook text from spreadsheet bytes.
#[wasm_bindgen(js_name = extractText)]
pub fn extract_text(bytes: &[u8]) -> std::result::Result<String, JsValue> {
    check_input(bytes)?;
    rxls::wasm::extract_text_bytes(bytes).map_err(js_error)
}

/// Export one worksheet as CSV from spreadsheet bytes.
#[wasm_bindgen(js_name = toCsv)]
pub fn to_csv(bytes: &[u8], sheet_index: usize) -> std::result::Result<String, JsValue> {
    check_input(bytes)?;
    rxls::wasm::to_csv_bytes(bytes, sheet_index).map_err(js_error)
}

/// Export one worksheet as an HTML table fragment from spreadsheet bytes.
#[wasm_bindgen(js_name = toHtml)]
pub fn to_html(bytes: &[u8], sheet_index: usize) -> std::result::Result<String, JsValue> {
    check_input(bytes)?;
    rxls::wasm::to_html_bytes(bytes, sheet_index).map_err(js_error)
}

/// Export one worksheet as Markdown from spreadsheet bytes.
#[wasm_bindgen(js_name = toMarkdown)]
pub fn to_markdown(bytes: &[u8], sheet_index: usize) -> std::result::Result<String, JsValue> {
    check_input(bytes)?;
    rxls::wasm::to_markdown_bytes(bytes, sheet_index).map_err(js_error)
}

/// Build the machine-readable diagnose JSON report from spreadsheet bytes.
#[wasm_bindgen(js_name = reportJson)]
pub fn report_json(bytes: &[u8]) -> std::result::Result<String, JsValue> {
    check_input(bytes)?;
    rxls::wasm::report_json_bytes(bytes).map_err(js_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_error_details_preserve_kind_location_and_cause() {
        assert_eq!(
            error_details(&rxls::Error::Biff("truncated record")),
            ErrorDetails {
                kind: "biff",
                location: "workbook",
                cause: Some("truncated record".into()),
            }
        );
        assert_eq!(
            error_details(&rxls::Error::SheetOutOfRange),
            ErrorDetails {
                kind: "sheet_out_of_range",
                location: "sheet_index",
                cause: None,
            }
        );
        assert_eq!(
            error_details(&rxls::Error::UnsupportedCompression {
                part: "xl/media.bin".into(),
                method: 99,
            }),
            ErrorDetails {
                kind: "unsupported_compression",
                location: "container",
                cause: Some("part xl/media.bin uses ZIP compression method 99".into()),
            }
        );
        assert_eq!(
            error_details(&rxls::Error::ExportOutputTooLarge {
                format: "HTML",
                limit: 16,
            }),
            ErrorDetails {
                kind: "output_too_large",
                location: "output",
                cause: Some("HTML output limit is 16 bytes".into()),
            }
        );
        assert_eq!(
            error_details(&rxls::Error::ExportWorkLimitExceeded {
                format: "Markdown",
                limit: 4_000_000,
            }),
            ErrorDetails {
                kind: "work_limit_exceeded",
                location: "output",
                cause: Some("Markdown work limit is 4000000 operations".into()),
            }
        );
    }

    #[test]
    fn input_limit_is_fixed_and_browser_conservative() {
        assert_eq!(max_input_bytes(), 32 * 1024 * 1024);
        assert_eq!(max_export_output_bytes(), 16 * 1024 * 1024);
    }
}
