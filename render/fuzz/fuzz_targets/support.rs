#![allow(dead_code)]

use arbitrary::{Arbitrary, Unstructured};
use rxls::{Border, BorderStyle, Cell, CellStyle, Color, HAlign, VAlign, Workbook};
use rxls_render::{Fixed, RenderLimits, RenderOptions, RenderRange, RenderSelection, Rgb};

pub const MAX_FUZZ_INPUT_BYTES: usize = 64 << 10;

pub fn input(data: &[u8]) -> &[u8] {
    &data[..data.len().min(MAX_FUZZ_INPUT_BYTES)]
}

pub fn bounded_text(u: &mut Unstructured<'_>, maximum: usize) -> String {
    let available = u.len().min(maximum);
    let length = u.int_in_range(0usize..=available).unwrap_or(0);
    String::from_utf8_lossy(u.bytes(length).unwrap_or_default()).into_owned()
}

pub fn coordinate(u: &mut Unstructured<'_>, max_row: u32, max_col: u16) -> (u32, u16) {
    (
        u.int_in_range(0..=max_row).unwrap_or(0),
        u.int_in_range(0..=max_col).unwrap_or(0),
    )
}

pub fn workbook(u: &mut Unstructured<'_>) -> Workbook {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("fuzz-render");
    let operations = u.int_in_range(1u16..=256).unwrap_or(1);
    for _ in 0..operations {
        if u.is_empty() {
            break;
        }
        let (row, col) = coordinate(u, 63, 31);
        match u.int_in_range(0u8..=9).unwrap_or(0) {
            0 => sheet.write(row, col, bounded_text(u, 256)),
            1 => sheet.write(row, col, f64::arbitrary(u).unwrap_or(0.0)),
            2 => sheet.write(row, col, bool::arbitrary(u).unwrap_or(false)),
            3 => sheet.write(row, col, Cell::Error(bounded_text(u, 32))),
            4 => {
                let (other_row, other_col) = coordinate(u, 63, 31);
                sheet.merge(
                    row.min(other_row),
                    col.min(other_col),
                    row.max(other_row),
                    col.max(other_col),
                );
            }
            5 => sheet.set_col_width(col, f32::arbitrary(u).unwrap_or(8.0)),
            6 => sheet.set_row_height(row, f32::arbitrary(u).unwrap_or(15.0)),
            7 => {
                let style = CellStyle::new()
                    .font_name(bounded_text(u, 64))
                    .size(u.int_in_range(1u16..=96).unwrap_or(11))
                    .color(Color::rgb(
                        u8::arbitrary(u).unwrap_or(0),
                        u8::arbitrary(u).unwrap_or(0),
                        u8::arbitrary(u).unwrap_or(0),
                    ))
                    .fill(Color::rgb(
                        u8::arbitrary(u).unwrap_or(255),
                        u8::arbitrary(u).unwrap_or(255),
                        u8::arbitrary(u).unwrap_or(255),
                    ))
                    .align(match u.int_in_range(0u8..=2).unwrap_or(0) {
                        0 => HAlign::Left,
                        1 => HAlign::Center,
                        _ => HAlign::Right,
                    })
                    .valign(match u.int_in_range(0u8..=2).unwrap_or(0) {
                        0 => VAlign::Top,
                        1 => VAlign::Middle,
                        _ => VAlign::Bottom,
                    })
                    .border(
                        Border::new().with_all(match u.int_in_range(0u8..=3).unwrap_or(0) {
                            0 => BorderStyle::None,
                            1 => BorderStyle::Thin,
                            2 => BorderStyle::Medium,
                            _ => BorderStyle::Double,
                        }),
                    );
                sheet.write_styled(row, col, bounded_text(u, 256), &style);
            }
            8 => {
                let target = bounded_text(u, 128);
                sheet.write_url(row, col, &target, bounded_text(u, 64));
            }
            _ => {
                if bool::arbitrary(u).unwrap_or(false) {
                    sheet.hide_row(row);
                } else {
                    sheet.hide_column(col);
                }
            }
        }
    }
    workbook
}

pub fn render_options(u: &mut Unstructured<'_>) -> RenderOptions {
    let (first_row, first_col) = coordinate(u, 31, 15);
    let (last_row, last_col) = coordinate(u, 63, 31);
    RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(
            first_row.min(last_row),
            first_col.min(last_col),
            first_row.max(last_row),
            first_col.max(last_col),
        )),
        gridlines: bool::arbitrary(u).unwrap_or(true),
        include_hidden: bool::arbitrary(u).unwrap_or(false),
        background: Rgb::new(
            u8::arbitrary(u).unwrap_or(255),
            u8::arbitrary(u).unwrap_or(255),
            u8::arbitrary(u).unwrap_or(255),
        ),
        default_column_width: Fixed::from_pixels(i64::from(
            u.int_in_range(1u16..=256).unwrap_or(64),
        )),
        default_row_height: Fixed::from_pixels(i64::from(u.int_in_range(1u16..=256).unwrap_or(20))),
        limits: RenderLimits {
            max_rows: 128,
            max_columns: 64,
            max_cells: 8_192,
            max_text_bytes: 1 << 20,
            max_glyphs: 131_072,
            max_text_runs: 65_536,
            max_text_lines: 65_536,
            max_path_commands: 524_288,
            max_scene_nodes: 131_072,
            max_dimension_raw: 65_536 * 1_024,
            max_output_bytes: 4 << 20,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    }
}
