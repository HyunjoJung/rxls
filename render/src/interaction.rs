//! Exact sheet-selection geometry, separate from backend paint and exported SVG.

use rxls::Workbook;

use crate::{CellCoordinate, Rect, RenderError, RenderOptions, RenderOutput};

/// One selectable visible cell or merged-cell region in final scene coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellInteractionRegion {
    /// Original source cell; a merged region always targets its original anchor.
    pub source: CellCoordinate,
    /// Positive visible selection rectangle, in scene fixed-point coordinates.
    pub rect: Rect,
}

/// Normal SVG output and its same-pass, text-free cell interaction geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveRenderOutput {
    /// Unmodified scene, SVG, and report produced by worksheet rendering.
    pub output: RenderOutput,
    /// Visible cell regions, bounded by the render cell limit.
    pub cells: Vec<CellInteractionRegion>,
}

/// Render a sheet with exact selectable-cell geometry without modifying its SVG.
///
/// Blank cells inside the selected extent are included. Hidden axes are omitted
/// unless requested, merges resolve to their original source anchor, and bounds
/// include automatic sizing, viewport offsets, and right-to-left placement.
/// The metadata describes sheet coordinates, not projected print-page geometry.
pub fn render_sheet_interactive_svg(
    workbook: &Workbook,
    sheet_index: usize,
    options: &RenderOptions,
) -> Result<InteractiveRenderOutput, RenderError> {
    let (build, cells) =
        crate::layout::build_scene_with_interaction(workbook, sheet_index, options)?;
    let svg = crate::render_scene_svg(&build.scene, options.limits.max_output_bytes)?;
    let mut report = build.report;
    report.svg_bytes = svg.len() as u64;
    Ok(InteractiveRenderOutput {
        output: RenderOutput {
            scene: build.scene,
            svg,
            report,
        },
        cells,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Fixed, LimitKind, RenderRange, RenderSelection, SceneNode};
    use std::io::Write;

    fn options(range: RenderRange) -> RenderOptions {
        RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            ..RenderOptions::default()
        }
    }

    fn selected(output: &InteractiveRenderOutput, row: u32, col: u16) -> Rect {
        output
            .cells
            .iter()
            .find(|cell| cell.source == CellCoordinate { row, col })
            .unwrap()
            .rect
    }

    #[test]
    fn interaction_preserves_normal_svg_and_maps_blank_variable_and_hidden_axes() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("selection");
        sheet.write(0, 0, "first");
        sheet.write(2, 2, "last");
        sheet.set_col_width(0, 8.0);
        sheet.set_col_width(2, 19.0);
        sheet.set_row_height(0, 12.0);
        sheet.set_row_height(2, 27.0);
        sheet.hide_row(1);
        sheet.hide_column(1);
        let options = options(RenderRange::new(0, 0, 2, 2));
        let interactive = render_sheet_interactive_svg(&workbook, 0, &options).unwrap();
        assert_eq!(
            interactive.output,
            crate::render_sheet_svg(&workbook, 0, &options).unwrap()
        );
        assert_eq!(interactive.cells.len(), 4);
        assert_eq!(
            interactive
                .cells
                .iter()
                .map(|cell| (cell.source.row, cell.source.col))
                .collect::<Vec<_>>(),
            vec![(0, 0), (0, 2), (2, 0), (2, 2)]
        );
        let first = selected(&interactive, 0, 0);
        let blank = selected(&interactive, 0, 2);
        let last = selected(&interactive, 2, 2);
        assert_eq!(first.height, Fixed::from_pixels(16));
        assert_eq!(last.height, Fixed::from_pixels(36));
        assert_eq!(blank.x, first.width);
        assert_eq!(last.y, first.height);
        assert!(blank.width > first.width);
        for node in &interactive.output.scene.nodes {
            if let SceneNode::Text(node) = node {
                let expected = if node.text == "first" { first } else { last };
                // Text placement has its own vertical inset; selection must use
                // the actual cell rectangle, not the text's paint bounds.
                assert_eq!(node.bounds.x, expected.x);
                assert_eq!(node.bounds.width, expected.width);
                assert_eq!(node.bounds.height, expected.height);
            }
        }
    }

    #[test]
    fn interaction_merged_regions_keep_hidden_and_outside_range_source_anchors() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("merged");
        sheet.write(0, 0, "merged anchor");
        sheet.merge(0, 0, 2, 2);
        sheet.hide_row(0);
        sheet.hide_column(0);
        let output =
            render_sheet_interactive_svg(&workbook, 0, &options(RenderRange::new(1, 1, 2, 2)))
                .unwrap();
        assert_eq!(output.cells.len(), 1);
        assert_eq!(output.cells[0].source, CellCoordinate { row: 0, col: 0 });
        assert_eq!(
            output.cells[0].rect,
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: output.output.scene.width,
                height: output.output.scene.height
            }
        );
    }

    #[test]
    fn interaction_rtl_reflects_coordinates_without_changing_source_identity() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("rtl");
        sheet.write(0, 0, "left");
        sheet.write(0, 2, "right");
        sheet.set_col_width(0, 7.0);
        sheet.set_col_width(2, 17.0);
        let options = options(RenderRange::new(0, 0, 0, 2));
        let ltr = render_sheet_interactive_svg(&workbook, 0, &options).unwrap();
        workbook.sheets[0].set_right_to_left(true);
        let rtl = render_sheet_interactive_svg(&workbook, 0, &options).unwrap();
        assert_eq!(ltr.cells.len(), 3);
        for original in &ltr.cells {
            let reflected = selected(&rtl, original.source.row, original.source.col);
            assert_eq!(
                reflected.x.raw(),
                rtl.output.scene.width.raw() - original.rect.x.raw() - original.rect.width.raw()
            );
            assert_eq!(reflected.width, original.rect.width);
            assert_eq!(reflected.height, original.rect.height);
        }
    }

    #[test]
    fn interaction_empty_used_sheet_has_no_fabricated_cells_and_limits_stay_typed() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("empty");
        let empty = render_sheet_interactive_svg(&workbook, 0, &RenderOptions::default()).unwrap();
        assert!(empty.cells.is_empty());
        let mut options = options(RenderRange::new(0, 0, 0, 1));
        options.limits.max_cells = 2;
        assert_eq!(
            render_sheet_interactive_svg(&workbook, 0, &options)
                .unwrap()
                .cells
                .len(),
            2
        );
        options.limits.max_cells = 1;
        assert!(matches!(
            render_sheet_interactive_svg(&workbook, 0, &options),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::Cells,
                limit: 1,
                actual: 2
            })
        ));
    }

    #[test]
    fn interaction_tracks_automatic_height_and_can_explicitly_include_hidden_axes() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("automatic");
        sheet.set_col_width(0, 6.0);
        sheet.write_styled(
            0,
            0,
            "wrapped text on several separate lines",
            &rxls::CellStyle::new().wrap(),
        );
        sheet.hide_row(1);
        sheet.hide_column(1);
        let mut options = options(RenderRange::new(0, 0, 2, 2));
        options.font_pack = Some(crate::font::synthetic_test_pack());
        let output = render_sheet_interactive_svg(&workbook, 0, &options).unwrap();
        let first = selected(&output, 0, 0);
        assert!(first.height > options.default_row_height);
        assert_eq!(selected(&output, 2, 0).y, first.height);
        assert_eq!(
            output.output,
            crate::render_sheet_svg(&workbook, 0, &options).unwrap()
        );
        options.include_hidden = true;
        let included = render_sheet_interactive_svg(&workbook, 0, &options).unwrap();
        assert_eq!(included.cells.len(), 9);
        assert!(selected(&included, 1, 1).width > Fixed::ZERO);
    }

    #[test]
    fn interaction_preserves_drawing_expanded_viewport_offsets_and_rtl() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="offset"><table:shapes><draw:frame text:anchor-type="page" svg:x="0in" svg:y="0in" svg:width="1in" svg:height="0.5in"><draw:image xlink:href="Pictures/pixel.png"/></draw:frame></table:shapes><table:table-row table:number-rows-repeated="4"><table:table-cell/></table:table-row><table:table-row><table:table-cell table:number-columns-repeated="3"/><table:table-cell office:value-type="string"><text:p>offset cell</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let mut image = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut image, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[40, 80, 120, 255])
                .unwrap();
        }
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in [
            (
                "mimetype",
                b"application/vnd.oasis.opendocument.spreadsheet".as_slice(),
            ),
            ("content.xml", content.as_bytes()),
            ("Pictures/pixel.png", image.as_slice()),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        let mut workbook = Workbook::open(&zip.finish().unwrap().into_inner()).unwrap();
        let options = RenderOptions {
            gridlines: false,
            ..RenderOptions::default()
        };
        let ltr = render_sheet_interactive_svg(&workbook, 0, &options).unwrap();
        let rect = selected(&ltr, 4, 3);
        assert!(rect.x > Fixed::ZERO);
        assert!(rect.y > Fixed::ZERO);
        assert_eq!(
            ltr.output,
            crate::render_sheet_svg(&workbook, 0, &options).unwrap()
        );
        workbook.sheets[0].set_right_to_left(true);
        let rtl = render_sheet_interactive_svg(&workbook, 0, &options).unwrap();
        let reflected = selected(&rtl, 4, 3);
        assert_eq!(
            reflected.x.raw(),
            rtl.output.scene.width.raw() - rect.x.raw() - rect.width.raw()
        );
        assert_eq!(reflected.y, rect.y);
        assert_eq!(
            rtl.output,
            crate::render_sheet_svg(&workbook, 0, &options).unwrap()
        );
    }
}
