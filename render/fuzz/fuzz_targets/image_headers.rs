#![no_main]
//! Exercise malformed and oversized PNG/JPEG headers without permitting amplification.

mod support;

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use rxls::{Image, ImageFmt, Workbook};
use rxls_render::{build_scene, render_scene_svg, RenderLimits, RenderOptions};

const VALID_RGBA_PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x56, 0xc7, 0x2f, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn png_crc(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn push_png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    output.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(payload);
    let mut checked = Vec::with_capacity(4 + payload.len());
    checked.extend_from_slice(kind);
    checked.extend_from_slice(payload);
    output.extend_from_slice(&png_crc(&checked).to_be_bytes());
}

fn header(unstructured: &mut Unstructured<'_>, format: ImageFmt) -> Vec<u8> {
    let width = u32::arbitrary(unstructured).unwrap_or(u32::MAX);
    let height = u32::arbitrary(unstructured).unwrap_or(u32::MAX);
    let retained = unstructured.len().min(32 << 10);
    let tail = unstructured.bytes(retained).unwrap_or_default();
    match format {
        ImageFmt::Png => {
            let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
            let mut ihdr = Vec::with_capacity(13);
            ihdr.extend_from_slice(&width.to_be_bytes());
            ihdr.extend_from_slice(&height.to_be_bytes());
            ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
            push_png_chunk(&mut bytes, b"IHDR", &ihdr);
            push_png_chunk(&mut bytes, b"IDAT", tail);
            push_png_chunk(&mut bytes, b"IEND", &[]);
            bytes
        }
        ImageFmt::Jpeg => {
            let width = (width as u16).max(1);
            let height = (height as u16).max(1);
            let mut bytes = b"\xff\xd8\xff\xc0\x00\x11\x08".to_vec();
            bytes.extend_from_slice(&height.to_be_bytes());
            bytes.extend_from_slice(&width.to_be_bytes());
            bytes.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
            bytes.extend_from_slice(tail);
            bytes.extend_from_slice(b"\xff\xd9");
            bytes
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(support::input(data));
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("image-header");
    sheet.write(0, 0, "bounded image header");
    sheet.add_image(Image::new(VALID_RGBA_PNG_1X1, ImageFmt::Png, (20, 1)).with_to((21, 2)));
    let count = unstructured.int_in_range(2u8..=8).unwrap_or(2);
    for index in 0..count {
        let format = if index % 2 == 0 {
            ImageFmt::Png
        } else {
            ImageFmt::Jpeg
        };
        let from = (u32::from(index) * 2, 1);
        sheet.add_image(
            Image::new(header(&mut unstructured, format), format, from)
                .with_to((from.0.saturating_add(1), 3)),
        );
    }
    let base = RenderOptions {
        limits: RenderLimits {
            max_output_bytes: 4 << 20,
            max_path_commands: 262_144,
            max_scene_nodes: 65_536,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    };
    let mut pixel_limited = base.clone();
    pixel_limited.limits.max_image_dimension = u64::MAX;
    pixel_limited.limits.max_image_pixels = 1;
    let mut decoded_limited = base.clone();
    decoded_limited.limits.max_image_dimension = u64::MAX;
    decoded_limited.limits.max_image_pixels = u64::MAX;
    decoded_limited.limits.max_decoded_media_bytes = 3;
    for options in [&base, &pixel_limited, &decoded_limited] {
        if let Ok(build) = build_scene(&workbook, 0, options) {
            let _ = render_scene_svg(&build.scene, options.limits.max_output_bytes);
        }
    }
    let encoded = workbook.to_xlsx();
    if encoded.len() <= 8 << 20 {
        if let Ok(reopened) = Workbook::open(&encoded) {
            let _ = build_scene(&reopened, 0, &base);
        }
    }
});
