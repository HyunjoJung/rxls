#![no_main]
//! Exercise bounded worksheet snapshot construction through the public renderer API.

mod support;

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use rxls_render::{build_scene, render_scene_svg};

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(support::input(data));
    let workbook = support::workbook(&mut unstructured);
    let options = support::render_options(&mut unstructured);
    if let Ok(build) = build_scene(&workbook, 0, &options) {
        let _ = render_scene_svg(&build.scene, options.limits.max_output_bytes);
        let _ = build.report.to_json();
    }
});
