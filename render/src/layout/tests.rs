use super::charts::{
    chart_calc_imported_line_value_axis, chart_category_data_plot,
    chart_category_label_is_retained, chart_category_label_stride, chart_category_ratio,
    chart_frame_padding, chart_nice_value_axis, chart_nice_x_axis, chart_x_data_bounds,
    max_chart_text_metrics_with_style, measure_chart_text, measure_chart_text_with_style,
    push_area_chart, push_bubble_chart, push_chart_marker, push_chart_text, push_line_chart,
    push_radar_chart, push_scatter_chart, resolve_label_a1_range, resolve_numeric_a1_range,
    try_push_chart, try_push_chart_with_layout, ChartTextMetrics, ChartTextRole,
    ResolvedChartSeries, ResolvedChartTextStyle, CALC_MISSING_THEME_CHART_LATIN_FAMILY,
    CHART_TEXT_SINE_Q62_CEIL, CHART_TEXT_TRIG_SCALE, MAX_CHART_CATEGORY_LABELS,
};
use super::conditional::{
    active_color_only_conditional_cells, calc_line_layout_available,
    has_conditional_text_layout_overlay,
};
use super::edges::{
    remap_calc_metafile_grid_edges, CellEdge, ComposedEdgeKey, ComposedEdgeOrientation, EdgeClaim,
    EdgeClaimKind,
};
use super::geometry::{
    apply_calc_single_page_axis_fit, apply_calc_single_page_metafile_grid_axis_fit,
    calc_hmm_to_fixed, calc_inclusive_rectangle_extent, calc_metafile_grid_hmm_to_fixed,
    calc_twips_position_to_fixed, calc_twips_position_to_hmm, character_width_ratio_to_pixels,
    column_chars_to_fixed, fallback_row_height, imported_axis_measure_twips, maximum_digit_width,
    points_to_fixed, round_positive_mul_div, verified_implicit_ooxml,
    verified_ooxml_normal_font_size, xlsb_digits_to_fixed, SourceAxisCursor,
};
use super::text::{
    append_styled_shaped_outlines, build_glyph_run, calc_automatic_cell_height,
    calc_automatic_height_from_engine_mm100, calc_cell_text_layout_bounds,
    calc_draw_strings_edit_character, calc_draw_strings_short_utf16_len,
    calc_draw_strings_visible_width, calc_edit_engine_semantic_groups,
    calc_edit_engine_uses_only_complex_role, calc_line_height_mm100, calc_script_class,
    calc_script_class_summary_bounded, calc_verified_complex_role_face,
    combine_styled_line_metrics, expand_draw_strings_cutoff_to_cluster_boundary,
    expand_draw_strings_cutoff_to_visible_cluster, has_mixed_calc_script_classes, inner_width,
    line_height_from_metrics, measure_automatic_cell_height, multiply_fixed, prepare_styled_text,
    prepared_asian_face, resolve_rich_styles, scale_font_units, shape_styled_range,
    shape_text_with_kerning, shaped_width, single_face_line_metrics, styled_line_metrics,
    styled_shaped_width, text_base_direction, text_style, utf16_prefix_byte_len,
    utf16_suffix_byte_len, vertical_block_top, CalcCellScriptAnalysis, CalcScriptClass,
    CombinedLineMetrics, PreparedAsianFace, ResolvedRunStyle, StyledSourceSpan,
};

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::ops::Range;

use crate::error::{LimitKind, RenderError};
use crate::font::{BaseDirection, FontPack, FontRequest, ShapedText};
use crate::scene::{
    Fixed, GlyphCluster, GlyphClusterMetrics, GlyphRunNode, GlyphSemanticGroup, LineNode,
    PathCommand, Rect, RectNode, Rgb, Scene, SceneNode, TextAnchor, TextBaseline,
    FIXED_UNITS_PER_PIXEL,
};
use crate::typography::{wrap_text_lines, CellLineLayoutPolicy};
use rxls::{
    Border, BorderStyle, Cell, CellStyle, CfRule, Chart, ChartBarDirection, ChartKind,
    ChartMarkerSymbol, ChartSeriesStyle, Color, DrawingAnchorBehavior, DrawingMetadata,
    DrawingObjectKind, DvOp, FormatScript, HAlign, ImportedAxisMeasure, OoxmlImplicitRowHeight,
    Sheet, Sparkline, SparklineKind, StyleFidelity, StyleLossKind, VAlign, Workbook,
    XlsbDefaultColumnWidth,
};

use rxls::{CondFormat, Format, Image, ImageFmt, PageSetup, Series};
use zip::write::SimpleFileOptions;

use super::*;
use crate::font::{
    synthetic_kerning_test_pack, synthetic_test_pack, FontId, ShapedGlyph, ShapedRun,
};
use crate::{
    build_print_document, render_print_document_pdf, render_print_document_pdf_with_fonts,
    render_print_document_png_pages, render_sheet_svg, PrintOptions,
};

fn outlined_options(range: RenderRange) -> RenderOptions {
    let pack = synthetic_test_pack();
    RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: pack.default_family().to_string(),
        font_pack: Some(pack),
        ..RenderOptions::default()
    }
}

fn imported_xlsx(styles: &str, worksheet: &str) -> Workbook {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (name, body) in [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
        ),
        ("xl/styles.xml", styles),
        ("xl/worksheets/sheet1.xml", worksheet),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    Workbook::open(&zip.finish().unwrap().into_inner()).expect("imported OOXML workbook")
}

fn imported_biff8_default_row(manual: bool, text: &str) -> Workbook {
    fn record(kind: u16, body: &[u8]) -> Vec<u8> {
        let mut output = kind.to_le_bytes().to_vec();
        output.extend_from_slice(&(body.len() as u16).to_le_bytes());
        output.extend_from_slice(body);
        output
    }

    fn bof(substream: u16) -> Vec<u8> {
        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&0x0600_u16.to_le_bytes());
        body.extend_from_slice(&substream.to_le_bytes());
        body.extend_from_slice(&[0; 12]);
        body
    }

    let mut boundsheet = vec![0, 0, 0, 0, 0, 0, 8, 0];
    boundsheet.extend_from_slice(b"Geometry");
    let mut label = vec![0, 0, 0, 0, 0, 0];
    label.extend_from_slice(&(text.len() as u16).to_le_bytes());
    label.push(0);
    label.extend_from_slice(text.as_bytes());
    let flags = u16::from(manual);
    let default_row = [flags.to_le_bytes(), 300_u16.to_le_bytes()].concat();

    let mut stream = record(0x0809, &bof(0x0005));
    stream.extend_from_slice(&record(0x0085, &boundsheet));
    stream.extend_from_slice(&record(0x000A, &[]));
    stream.extend_from_slice(&record(0x0809, &bof(0x0010)));
    stream.extend_from_slice(&record(0x0225, &default_row));
    stream.extend_from_slice(&record(0x0204, &label));
    stream.extend_from_slice(&record(0x000A, &[]));

    let mut compound =
        cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).expect("create CFB");
    compound
        .create_stream("/Workbook")
        .expect("create Workbook stream")
        .write_all(&stream)
        .expect("write Workbook stream");
    compound.flush().expect("flush CFB");
    Workbook::open(&compound.into_inner().into_inner()).expect("imported BIFF8 workbook")
}

fn xlsb_record(record_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    if record_type < 0x80 {
        output.push(record_type as u8);
    } else {
        output.push((record_type & 0x7f) as u8 | 0x80);
        output.push(((record_type >> 7) & 0x7f) as u8);
    }
    let mut size = payload.len();
    loop {
        let mut byte = (size & 0x7f) as u8;
        size >>= 7;
        if size != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if size == 0 {
            break;
        }
    }
    output.extend_from_slice(payload);
    output
}

fn xlsb_wide_string(value: &str) -> Vec<u8> {
    let units = value.encode_utf16().collect::<Vec<_>>();
    let mut output = (units.len() as u32).to_le_bytes().to_vec();
    for unit in units {
        output.extend_from_slice(&unit.to_le_bytes());
    }
    output
}

fn imported_width_xlsb(
    sheet_default: Option<(u32, u16)>,
    columns: &[(u16, u16, u32, bool)],
) -> Workbook {
    let mut bundle = vec![0_u8; 8];
    bundle.extend_from_slice(&xlsb_wide_string("rId1"));
    bundle.extend_from_slice(&xlsb_wide_string("Widths"));
    let workbook = xlsb_record(156, &bundle);

    let mut sheet = Vec::new();
    if let Some((width_256, base_characters)) = sheet_default {
        let mut format = Vec::new();
        format.extend_from_slice(&width_256.to_le_bytes());
        format.extend_from_slice(&base_characters.to_le_bytes());
        format.extend_from_slice(&300_u16.to_le_bytes());
        format.extend_from_slice(&0_u16.to_le_bytes());
        format.extend_from_slice(&[0, 0]);
        sheet.extend_from_slice(&xlsb_record(0x01E5, &format));
    }
    for &(first, last, width_256, hidden) in columns {
        let mut column = Vec::new();
        column.extend_from_slice(&u32::from(first).to_le_bytes());
        column.extend_from_slice(&u32::from(last).to_le_bytes());
        column.extend_from_slice(&width_256.to_le_bytes());
        column.extend_from_slice(&0_u32.to_le_bytes());
        column.extend_from_slice(&u16::from(hidden).to_le_bytes());
        sheet.extend_from_slice(&xlsb_record(60, &column));
    }

    let relationships = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", workbook.as_slice()),
        ("xl/_rels/workbook.bin.rels", relationships.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        zip.start_file(path, options).unwrap();
        zip.write_all(body).unwrap();
    }
    Workbook::open(&zip.finish().unwrap().into_inner()).expect("imported XLSB workbook")
}

fn imported_table_xlsx(styles: &str, worksheet: &str, table: &str) -> Workbook {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (name, body) in [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
        ),
        ("xl/styles.xml", styles),
        ("xl/worksheets/sheet1.xml", worksheet),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdTable" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
        ),
        ("xl/tables/table1.xml", table),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    Workbook::open(&zip.finish().unwrap().into_inner()).expect("imported OOXML table workbook")
}

fn imported_two_cell_drawing(kind: DrawingObjectKind, to_offset: (i64, i64)) -> Workbook {
    imported_two_cell_drawing_with_worksheet(
        kind,
        to_offset,
        r#"<worksheet><sheetData/><drawing r:id="rIdDrawing"/></worksheet>"#,
    )
}

fn imported_two_cell_drawing_with_worksheet(
    kind: DrawingObjectKind,
    to_offset: (i64, i64),
    worksheet: &str,
) -> Workbook {
    let (drawing_object, object_relationship, object_part) = match kind {
        DrawingObjectKind::Image => (
            r#"<pic><blipFill><blip r:embed="rIdObject"/></blipFill></pic>"#,
            r#"<Relationship Id="rIdObject" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>"#,
            ("xl/media/image1.png", b"\x89PNG\r\n\x1a\n".as_slice()),
        ),
        DrawingObjectKind::Chart => (
            r#"<graphicFrame><graphic><graphicData><chart r:id="rIdObject"/></graphicData></graphic></graphicFrame>"#,
            r#"<Relationship Id="rIdObject" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/>"#,
            (
                "xl/charts/chart1.xml",
                br#"<chartSpace><chart><plotArea><lineChart><ser><idx val="0"/><order val="0"/><cat><strRef><f>Sheet1!$A$1:$A$4</f><strCache><pt idx="0"><v>Q1</v></pt><pt idx="1"><v>Q2</v></pt><pt idx="2"><v>Q3</v></pt><pt idx="3"><v>Q4</v></pt></strCache></strRef></cat><val><numRef><f>Sheet1!$B$1:$B$4</f><numCache><pt idx="0"><v>10</v></pt><pt idx="1"><v>20</v></pt><pt idx="2"><v>30</v></pt><pt idx="3"><v>40</v></pt></numCache></numRef></val></ser><axId val="1"/><axId val="2"/></lineChart><catAx><axId val="1"/><crossAx val="2"/></catAx><valAx><axId val="2"/><crossAx val="1"/></valAx></plotArea></chart></chartSpace>"#
                    .as_slice(),
            ),
        ),
        _ => panic!("test helper only supports images and charts"),
    };
    let drawing = format!(
        r#"<wsDr><twoCellAnchor><from><col>2</col><colOff>0</colOff><row>3</row><rowOff>0</rowOff></from><to><col>5</col><colOff>{}</colOff><row>7</row><rowOff>{}</rowOff></to>{drawing_object}</twoCellAnchor></wsDr>"#,
        to_offset.0, to_offset.1
    );
    let drawing_relationships = format!("<Relationships>{object_relationship}</Relationships>");
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (name, body) in [
        (
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="Drawing" r:id="rId1"/></sheets></workbook>"#
                .as_slice(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#
                .as_slice(),
        ),
        (
            "xl/worksheets/sheet1.xml",
            worksheet.as_bytes(),
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            br#"<Relationships><Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#
                .as_slice(),
        ),
        ("xl/drawings/drawing1.xml", drawing.as_bytes()),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            drawing_relationships.as_bytes(),
        ),
        object_part,
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body).unwrap();
    }
    Workbook::open(&zip.finish().unwrap().into_inner()).expect("two-cell drawing workbook")
}

fn imported_single_page_terminal_column_drawing(
    hidden_terminal_column: bool,
    explicit_prefix_widths: bool,
    from_column: u16,
    to_column: u16,
) -> Workbook {
    let mut columns = String::new();
    if explicit_prefix_widths {
        columns.push_str(
            r#"<col min="1" max="1" width="18" customWidth="1"/><col min="2" max="5" width="14" customWidth="1"/>"#,
        );
    }
    if hidden_terminal_column {
        columns.push_str(&format!(
            r#"<col min="{to_column}" max="{to_column}" hidden="1"/>"#
        ));
    }
    let columns = if columns.is_empty() {
        String::new()
    } else {
        format!("<cols>{columns}</cols>")
    };
    let worksheet =
        format!(r#"<worksheet>{columns}<sheetData/><drawing r:id="rIdDrawing"/></worksheet>"#);
    let drawing = format!(
        r#"<wsDr><twoCellAnchor><from><col>{from_column}</col><colOff>0</colOff><row>0</row><rowOff>0</rowOff></from><to><col>{to_column}</col><colOff>0</colOff><row>1</row><rowOff>0</rowOff></to><sp><nvSpPr><cNvPr id="1" name="Terminal column"/></nvSpPr></sp></twoCellAnchor></wsDr>"#
    );
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (name, body) in [
        (
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="Drawing" r:id="rId1"/></sheets></workbook>"#
                .as_slice(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#
                .as_slice(),
        ),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            br#"<Relationships><Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#
                .as_slice(),
        ),
        ("xl/drawings/drawing1.xml", drawing.as_bytes()),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body).unwrap();
    }
    Workbook::open(&zip.finish().unwrap().into_inner())
        .expect("single-page terminal-column drawing workbook")
}

fn imported_hidden_two_cell_drawing(
    kind: DrawingObjectKind,
    move_only: bool,
    right_to_left: bool,
) -> Workbook {
    let edit_as = if move_only {
        r#" editAs="oneCell""#
    } else {
        ""
    };
    let right_to_left = if right_to_left { "1" } else { "0" };
    let worksheet = format!(
        r#"<worksheet><sheetViews><sheetView rightToLeft="{right_to_left}"/></sheetViews><sheetFormatPr defaultColWidth="8.8571428571" defaultRowHeight="15"/><cols><col min="4" max="6" hidden="1"/></cols><sheetData><row r="6" hidden="1"/><row r="7" hidden="1"/><row r="8" hidden="1"/></sheetData><drawing r:id="rIdDrawing"/></worksheet>"#
    );
    let (drawing_object, object_relationship, object_part) = match kind {
        DrawingObjectKind::Image => (
            r#"<pic><nvPicPr><cNvPr id="1" name="Hidden-axis image"/></nvPicPr><blipFill><blip r:embed="rIdObject"/></blipFill><spPr><xfrm><ext cx="1619250" cy="666750"/></xfrm></spPr></pic>"#,
            Some(r#"<Relationship Id="rIdObject" Target="../media/image1.png"/>"#),
            Some(("xl/media/image1.png", b"\x89PNG\r\n\x1a\n".as_slice())),
        ),
        DrawingObjectKind::Chart => (
            r#"<graphicFrame><nvGraphicFramePr><cNvPr id="1" name="Hidden-axis chart"/></nvGraphicFramePr><xfrm><ext cx="1619250" cy="666750"/></xfrm><graphic><graphicData><chart r:id="rIdObject"/></graphicData></graphic></graphicFrame>"#,
            Some(r#"<Relationship Id="rIdObject" Target="../charts/chart1.xml"/>"#),
            Some((
                "xl/charts/chart1.xml",
                br#"<chartSpace><chart><plotArea><lineChart/></plotArea></chart></chartSpace>"#
                    .as_slice(),
            )),
        ),
        DrawingObjectKind::Shape => (
            r#"<sp><nvSpPr><cNvPr id="1" name="Hidden-axis callout"/></nvSpPr><spPr><xfrm><ext cx="1619250" cy="666750"/></xfrm></spPr></sp>"#,
            None,
            None,
        ),
        _ => panic!("unsupported drawing test kind"),
    };
    let drawing = format!(
        r#"<wsDr><twoCellAnchor{edit_as}><from><col>2</col><colOff>0</colOff><row>3</row><rowOff>0</rowOff></from><to><col>5</col><colOff>0</colOff><row>7</row><rowOff>0</rowOff></to>{drawing_object}</twoCellAnchor></wsDr>"#
    );
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (name, body) in [
        (
            "xl/workbook.xml",
            br#"<workbook><sheets><sheet name="Drawing" r:id="rId1"/></sheets></workbook>"#
                .as_slice(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#
                .as_slice(),
        ),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            br#"<Relationships><Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#
                .as_slice(),
        ),
        ("xl/drawings/drawing1.xml", drawing.as_bytes()),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body).unwrap();
    }
    if let Some(relationship) = object_relationship {
        zip.start_file("xl/drawings/_rels/drawing1.xml.rels", options)
            .unwrap();
        zip.write_all(format!("<Relationships>{relationship}</Relationships>").as_bytes())
            .unwrap();
    }
    if let Some((name, body)) = object_part {
        zip.start_file(name, options).unwrap();
        zip.write_all(body).unwrap();
    }
    Workbook::open(&zip.finish().unwrap().into_inner())
        .expect("hidden-axis two-cell drawing workbook")
}

fn fixed_drawing_outer_rect(nodes: &[SceneNode]) -> Rect {
    for node in nodes {
        match node {
            SceneNode::Rect(RectNode { rect, .. })
                if rect.width == Fixed::from_pixels(170)
                    && rect.height == Fixed::from_pixels(70) =>
            {
                return *rect;
            }
            SceneNode::ClipGroup(group) => {
                if let Some(rect) = group.nodes.iter().find_map(|node| match node {
                    SceneNode::Rect(RectNode { rect, .. })
                        if rect.width == Fixed::from_pixels(170)
                            && rect.height == Fixed::from_pixels(70) =>
                    {
                        Some(*rect)
                    }
                    _ => None,
                }) {
                    return rect;
                }
            }
            _ => {}
        }
    }
    panic!("fixed drawing outer frame")
}

fn image_placeholder_rect(nodes: &[SceneNode]) -> Option<Rect> {
    nodes.iter().find_map(|node| match node {
        SceneNode::Rect(RectNode {
            rect,
            fill: Some(color),
            ..
        }) if *color == Rgb::new(242, 242, 242) => Some(*rect),
        SceneNode::ClipGroup(group) => image_placeholder_rect(&group.nodes),
        _ => None,
    })
}

fn chart_outer_rect(nodes: &[SceneNode]) -> Option<Rect> {
    nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Rect(RectNode {
                rect,
                stroke: Some(_),
                ..
            }) => Some(*rect),
            SceneNode::ClipGroup(group) => chart_outer_rect(&group.nodes),
            _ => None,
        })
        .max_by_key(|rect| i128::from(rect.width.raw()) * i128::from(rect.height.raw()))
}

fn shape_placeholder_rect(nodes: &[SceneNode]) -> Option<Rect> {
    nodes.iter().find_map(|node| match node {
        SceneNode::Rect(RectNode {
            rect,
            fill: Some(color),
            ..
        }) if *color == Rgb::new(221, 235, 247) => Some(*rect),
        SceneNode::ClipGroup(group) => shape_placeholder_rect(&group.nodes),
        _ => None,
    })
}

fn glyph_run<'a>(scene: &'a Scene, text: &str) -> &'a GlyphRunNode {
    scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::GlyphRun(run) if run.text == text => Some(run),
            _ => None,
        })
        .expect("outlined text node")
}

fn path_x_span(run: &GlyphRunNode) -> i64 {
    let mut minimum = i64::MAX;
    let mut maximum = i64::MIN;
    let mut include = |value: Fixed| {
        minimum = minimum.min(value.raw());
        maximum = maximum.max(value.raw());
    };
    for command in &run.commands {
        match *command {
            PathCommand::MoveTo { x, .. } | PathCommand::LineTo { x, .. } => include(x),
            PathCommand::QuadraticTo { control_x, x, .. } => {
                include(control_x);
                include(x);
            }
            PathCommand::CubicTo {
                control1_x,
                control2_x,
                x,
                ..
            } => {
                include(control1_x);
                include(control2_x);
                include(x);
            }
            PathCommand::Close => {}
        }
    }
    maximum - minimum
}
#[test]
fn a_cell_without_vertical_alignment_sits_on_the_row_bottom() {
    // ECMA-376 Part 1 section 18.8.1 defaults `vertical` to `bottom`, and
    // both Excel and Calc render it that way. Centring instead lifts every
    // unaligned cell off the baseline Calc puts it on. Calc then applies
    // the ordinary 20-twip bottom ATTR_MARGIN before positioning the block.
    let rect = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: Fixed::from_pixels(100),
        height: Fixed::from_pixels(40),
    };
    let block = Fixed::from_pixels(12);
    assert_eq!(
        CALC_CELL_VERTICAL_MARGIN,
        points_to_fixed(1.0).expect("one point"),
        "20 twips are exactly one point"
    );
    let bottom =
        calc_cell_text_layout_bounds(rect, TextBaseline::Bottom, CALC_CELL_VERTICAL_MARGIN)
            .unwrap();
    let top =
        calc_cell_text_layout_bounds(rect, TextBaseline::Top, CALC_CELL_VERTICAL_MARGIN).unwrap();
    let middle =
        calc_cell_text_layout_bounds(rect, TextBaseline::Middle, CALC_CELL_VERTICAL_MARGIN)
            .unwrap();
    assert_eq!(
        vertical_block_top(bottom, block, TextBaseline::Bottom).unwrap(),
        Fixed::from_pixels(28)
            .checked_sub(CALC_CELL_VERTICAL_MARGIN)
            .unwrap()
    );
    assert_eq!(
        vertical_block_top(top, block, TextBaseline::Top).unwrap(),
        CALC_CELL_VERTICAL_MARGIN
    );
    assert_eq!(
        vertical_block_top(middle, block, TextBaseline::Middle).unwrap(),
        Fixed::from_pixels(14)
    );
    assert_eq!(
        middle, rect,
        "equal top and bottom margins cancel at center"
    );
}

#[test]
fn calc_vertical_cell_margin_is_shared_by_scene_text_and_outlined_glyphs() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("vertical-margins");
    for row in 0..4 {
        sheet.set_row_height(row, 30.0);
    }
    sheet.write(0, 0, "default");
    sheet.write_styled(1, 0, "bottom", &CellStyle::new().valign(VAlign::Bottom));
    sheet.write_styled(2, 0, "top", &CellStyle::new().valign(VAlign::Top));
    sheet.write_styled(3, 0, "middle", &CellStyle::new().valign(VAlign::Middle));
    let range = RenderRange::new(0, 0, 3, 0);
    let approximate = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let text_node = |text: &str| {
        approximate
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Text(node) if node.text == text => Some(node),
                _ => None,
            })
            .expect("cell text node")
    };
    let default = text_node("default");
    let bottom = text_node("bottom");
    let top = text_node("top");
    let middle = text_node("middle");
    assert_eq!(default.style.baseline, TextBaseline::Bottom);
    assert_eq!(bottom.style.baseline, TextBaseline::Bottom);
    assert_eq!(top.style.baseline, TextBaseline::Top);
    assert_eq!(middle.style.baseline, TextBaseline::Middle);
    for node in [default, bottom] {
        assert_eq!(node.bounds.height, node.clip_bounds.height);
        assert_eq!(
            node.bounds.y.checked_add(CALC_CELL_VERTICAL_MARGIN),
            Some(node.clip_bounds.y),
            "default and explicit bottom alignment share the exact bottom inset"
        );
    }
    assert_eq!(top.bounds.height, top.clip_bounds.height);
    assert_eq!(
        top.bounds.y,
        top.clip_bounds
            .y
            .checked_add(CALC_CELL_VERTICAL_MARGIN)
            .unwrap()
    );
    assert_eq!(middle.bounds, middle.clip_bounds);

    let pack = synthetic_test_pack();
    let outlined = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: pack.default_family().to_string(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let default = glyph_run(&outlined.scene, "default");
    let bottom = glyph_run(&outlined.scene, "bottom");
    let top = glyph_run(&outlined.scene, "top");
    let relative_baseline =
        |run: &GlyphRunNode| run.cluster_metrics[0].baseline_y.raw() - run.clip_bounds.y.raw();
    assert_eq!(relative_baseline(default), relative_baseline(bottom));
    assert_eq!(
        top.cluster_metrics[0].baseline_y.raw() - top.cluster_metrics[0].ascent.raw(),
        top.clip_bounds.y.raw() + CALC_CELL_VERTICAL_MARGIN.raw(),
        "outlined top text starts exactly one point inside the original clip"
    );
}

#[test]
fn imported_biff_cells_use_their_two_point_default_margin() {
    let biff = imported_biff8_default_row(false, ".");
    assert_eq!(
        calc_cell_vertical_margin(&biff.sheets[0]),
        CALC_BIFF_CELL_VERTICAL_MARGIN
    );

    let mut authored = Workbook::new();
    authored.add_sheet("authored");
    assert_eq!(
        calc_cell_vertical_margin(&authored.sheets[0]),
        CALC_CELL_VERTICAL_MARGIN
    );
}

#[test]
fn calc_print_text_uses_page_vertical_clip_for_automatic_rows() {
    let automatic = imported_biff8_default_row(false, ".");
    let manual = imported_biff8_default_row(true, ".");
    let options = outlined_options(RenderRange::new(0, 0, 1, 0));
    let build = |workbook: &Workbook| {
        build_sheet_scene_for_print(&workbook.sheets[0], 0, &options).unwrap()
    };

    let automatic = build(&automatic);
    let automatic_run = glyph_run(&automatic.scene, ".");
    assert_eq!(automatic_run.clip_bounds.y, Fixed::ZERO);
    assert_eq!(automatic_run.clip_bounds.height, automatic.scene.height);

    let manual = build(&manual);
    let manual_run = glyph_run(&manual.scene, ".");
    assert_eq!(manual_run.clip_bounds.y, Fixed::ZERO);
    assert!(manual_run.clip_bounds.height < manual.scene.height);
}

#[test]
fn calc_vertical_cell_margin_keeps_too_short_row_clips_bounded() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("short-margin");
    sheet.set_row_height(0, 0.5);
    sheet.write_styled(0, 0, "top", &CellStyle::new().valign(VAlign::Top));
    sheet.write_styled(0, 1, "bottom", &CellStyle::new().valign(VAlign::Bottom));
    sheet.write_styled(0, 2, "middle", &CellStyle::new().valign(VAlign::Middle));
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 2)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let text_node = |text: &str| {
        build
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Text(node) if node.text == text => Some(node),
                _ => None,
            })
            .expect("short-row text node")
    };
    let top = text_node("top");
    let bottom = text_node("bottom");
    let middle = text_node("middle");
    assert!(top.clip_bounds.height < CALC_CELL_VERTICAL_MARGIN);
    assert_eq!(top.bounds.height, top.clip_bounds.height);
    assert_eq!(bottom.bounds.height, bottom.clip_bounds.height);
    assert_eq!(middle.bounds, middle.clip_bounds);
    assert!(
        top.bounds.y
            > top
                .clip_bounds
                .y
                .checked_add(top.clip_bounds.height)
                .unwrap(),
        "the exact top inset may fall beyond a sub-point row, while clipping stays on the row"
    );
    assert!(
        bottom.bounds.y.checked_add(bottom.bounds.height).unwrap() < bottom.clip_bounds.y,
        "the exact bottom inset may fall above a sub-point row, while clipping stays on the row"
    );
}

#[test]
fn render_options_layer_verified_packs_and_report_every_selected_face_hash() {
    let caller = synthetic_test_pack();
    let fallback = synthetic_test_pack();
    let expected_stack = caller.with_fallback(&fallback).unwrap();
    let expected_source_pack = caller.pack_sha256().to_string();
    let expected_face_sha = caller.face_identities().next().unwrap().sha256.to_string();
    let mut workbook = Workbook::new();
    workbook.add_sheet("fonts").write_styled(
        0,
        0,
        "caller alias",
        &CellStyle::new().font_name("Legacy Sans"),
    );
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        gridlines: false,
        default_font_family: "Wide Sans".to_string(),
        font_pack: Some(caller),
        ..RenderOptions::default()
    }
    .with_fallback_font_pack(&fallback)
    .unwrap();
    let output = render_sheet_svg(&workbook, 0, &options).unwrap();
    assert_eq!(output.report.schema_version, 2);
    assert_eq!(
        output.report.font_pack_sha256.as_deref(),
        Some(expected_stack.pack_sha256())
    );
    assert_eq!(output.report.font_faces.len(), 1);
    let selected = &output.report.font_faces[0];
    assert_eq!(selected.source_pack_sha256, expected_source_pack);
    assert_eq!(selected.face_sha256, expected_face_sha);
    assert_eq!(selected.family, "Wide Sans");
    assert_eq!(selected.weight, 400);
    assert!(!selected.italic);
    assert!(selected.substituted);
    let json = output.report.to_json();
    assert!(json.contains("\"font_pack_sha256\":"));
    assert!(json.contains(&expected_face_sha));
    assert!(json.contains("\"substituted\":true"));
}

fn test_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
    }
    output
}

#[test]
fn multilingual_layout_is_outlined_wrapped_shrunk_and_deterministic() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("typography");
    sheet.set_col_width(0, 8.0);
    sheet.set_row_height(3, 54.0);
    let base = CellStyle::new().font_name("Wide Sans").size(11);
    sheet.write_styled(0, 0, "Latin 123", &base);
    sheet.write_styled(1, 0, "한글 日本 中文", &base);
    sheet.write_styled(2, 0, "العربية עברית 123", &base);
    let wrapped = "한글中文日本 wrapped words";
    sheet.write_styled(3, 0, wrapped, &base.clone().wrap());
    let shrunk = "shrink-to-fit-long-text";
    let plain = "plain-unshrunk-long-text";
    sheet.write_styled(4, 0, shrunk, &base.clone().shrink_to_fit());
    sheet.write_styled(5, 0, plain, &base);
    sheet.write_styled(
        6,
        0,
        "decorated",
        &base
            .clone()
            .italic()
            .underline()
            .strikethrough()
            .font_script(FormatScript::Superscript),
    );
    sheet.write_url_with_text_and_format(
        7,
        0,
        "https://example.com/?a=1&b=2",
        "linked",
        &Format::new().font_name("Wide Sans").size(11),
    );

    let options = outlined_options(RenderRange::new(0, 0, 7, 3));
    let first = render_sheet_svg(&workbook, 0, &options).unwrap();
    let second = render_sheet_svg(&workbook, 0, &options).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        first
            .scene
            .nodes
            .iter()
            .filter(|node| matches!(node, SceneNode::GlyphRun(_)))
            .count(),
        8
    );
    assert!(!first
        .scene
        .nodes
        .iter()
        .any(|node| matches!(node, SceneNode::Text(_))));
    assert!(first
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::FontFamilySubstituted));
    assert!(first.svg.contains("<g role=\"text\""));
    assert!(first.svg.contains("<path d=\""));
    assert!(!first.svg.contains("<text "));
    assert!(!first.svg.contains("font-family="));
    assert!(first
        .svg
        .contains("href=\"https://example.com/?a=1&amp;b=2\""));

    let wrapped_run = glyph_run(&first.scene, wrapped);
    let baselines = wrapped_run
        .commands
        .iter()
        .filter_map(|command| match command {
            PathCommand::MoveTo { y, .. } => Some(y.raw()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert!(
        baselines.len() >= 2,
        "wrapped text must occupy multiple lines"
    );
    assert!(
        path_x_span(glyph_run(&first.scene, shrunk)) < path_x_span(glyph_run(&first.scene, plain))
    );
    assert_eq!(glyph_run(&first.scene, "decorated").decorations.len(), 2);
}

#[test]
fn calc_line_height_excludes_gap_while_native_retains_it() {
    let metrics = CombinedLineMetrics {
        ascent: Fixed::from_pixels(8),
        descent: Fixed::from_pixels(-2),
        line_gap: Fixed::from_pixels(3),
        placement_ascent: Fixed::from_pixels(8),
        placement_descent: Fixed::from_pixels(-2),
        calc_ascent_pixels: 8,
        calc_descent_pixels: -2,
        calc_portion_height_mm100: 265,
    };
    assert_eq!(
        line_height_from_metrics(metrics, CalcLinePlacementPolicy::Native).unwrap(),
        Fixed::from_pixels(13)
    );
    assert_eq!(
        line_height_from_metrics(metrics, CalcLinePlacementPolicy::RequestedFace).unwrap(),
        Fixed::from_pixels(10)
    );
    assert_eq!(
        line_height_from_metrics(
            metrics,
            CalcLinePlacementPolicy::Imported(CalcImportProvenance::Biff),
        )
        .unwrap(),
        Fixed::from_pixels(13),
        "BIFF keeps the selected fallback face's ink block height"
    );
    for provenance in [
        CalcImportProvenance::Ods,
        CalcImportProvenance::Xlsb,
        CalcImportProvenance::Xlsx,
    ] {
        assert_eq!(
            line_height_from_metrics(metrics, CalcLinePlacementPolicy::Imported(provenance),)
                .unwrap(),
            Fixed::from_pixels(10),
            "Calc package imports use the role face's block height"
        );
    }
}

#[test]
fn calc_placement_metrics_do_not_replace_fallback_ink_metrics() {
    let fallback_ink = CombinedLineMetrics {
        ascent: Fixed::from_pixels(14),
        descent: Fixed::from_pixels(-8),
        line_gap: Fixed::from_pixels(2),
        placement_ascent: Fixed::from_pixels(14),
        placement_descent: Fixed::from_pixels(-8),
        calc_ascent_pixels: 14,
        calc_descent_pixels: -8,
        calc_portion_height_mm100: 582,
    };
    let requested_face = CombinedLineMetrics {
        ascent: Fixed::from_pixels(10),
        descent: Fixed::from_pixels(-3),
        line_gap: Fixed::from_pixels(1),
        placement_ascent: Fixed::from_pixels(10),
        placement_descent: Fixed::from_pixels(-3),
        calc_ascent_pixels: 10,
        calc_descent_pixels: -3,
        calc_portion_height_mm100: 344,
    };
    let mut combined = None;
    combine_styled_line_metrics(&mut combined, fallback_ink, requested_face);
    let combined = combined.unwrap();

    assert_eq!(combined.ascent, fallback_ink.ascent);
    assert_eq!(combined.descent, fallback_ink.descent);
    assert_eq!(combined.line_gap, fallback_ink.line_gap);
    assert_eq!(combined.placement_ascent, requested_face.ascent);
    assert_eq!(combined.placement_descent, requested_face.descent);
    assert_eq!(
        line_height_from_metrics(combined, CalcLinePlacementPolicy::Native).unwrap(),
        Fixed::from_pixels(24),
        "Native layout must continue to use the selected fallback face"
    );
    assert_eq!(
        line_height_from_metrics(combined, CalcLinePlacementPolicy::RequestedFace).unwrap(),
        Fixed::from_pixels(13),
        "Calc placement must use the requested face without rewriting ink metrics"
    );
    assert_eq!(combined.calc_ascent_pixels, 10);
    assert_eq!(combined.calc_descent_pixels, -3);
    assert_eq!(combined.calc_portion_height_mm100, 344);
}

#[test]
fn calc_mixed_arabic_cjk_uses_requested_face_for_line_placement() {
    let pack = synthetic_test_pack();
    let text = "한국어 العربية";
    let style = ResolvedRunStyle {
        family: "Wide Sans".to_string(),
        size: points_to_fixed(11.0).unwrap(),
        color: Rgb::BLACK,
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
        script: FormatScript::None,
    };
    let styles = vec![style.clone()];
    let spans = vec![StyledSourceSpan {
        source: 0..text.len(),
        style_index: 0,
    }];
    let options = RenderOptions::default();
    let shaped = shape_styled_range(
        &pack,
        text,
        0..text.len(),
        &spans,
        &styles,
        BaseDirection::Auto,
        true,
        &options,
    )
    .unwrap();
    assert_eq!(
        shaped
            .runs
            .iter()
            .map(|run| run.font_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([FontId(0), FontId(1)]),
        "the fixture must select the CJK face and an Arabic fallback face"
    );

    let requested = single_face_line_metrics(
        &pack,
        pack.resolve(style.request()).id,
        &style,
        CellLineLayoutPolicy::CalcEditEngine,
        1,
        1,
    )
    .unwrap();
    let calc = styled_line_metrics(
        &pack,
        &shaped,
        &styles,
        CellLineLayoutPolicy::CalcEditEngine,
        CalcLinePlacementPolicy::RequestedFace,
        text,
        1,
        1,
        &options,
    )
    .unwrap();
    assert_eq!(calc.placement_ascent, requested.ascent);
    assert_eq!(calc.placement_descent, requested.descent);
    assert_eq!(calc.calc_ascent_pixels, requested.calc_ascent_pixels);
    assert_eq!(calc.calc_descent_pixels, requested.calc_descent_pixels);

    let native = styled_line_metrics(
        &pack,
        &shaped,
        &styles,
        CellLineLayoutPolicy::Native,
        CalcLinePlacementPolicy::Native,
        text,
        1,
        1,
        &options,
    )
    .unwrap();
    assert_eq!(native.placement_ascent, native.ascent);
    assert_eq!(native.placement_descent, native.descent);
}

#[test]
fn calc_rich_superscript_keeps_its_explicit_requested_face_metrics() {
    let pack = synthetic_test_pack();
    let text = "한국어 العربية";
    let arabic_start = "한국어 ".len();
    let base = ResolvedRunStyle {
        family: "Wide Sans".to_string(),
        size: points_to_fixed(11.0).unwrap(),
        color: Rgb::BLACK,
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
        script: FormatScript::None,
    };
    let superscript = ResolvedRunStyle {
        family: "RTL Sans".to_string(),
        size: points_to_fixed(18.0).unwrap(),
        script: FormatScript::Superscript,
        ..base.clone()
    };
    let styles = vec![base.clone(), superscript.clone()];
    let spans = vec![
        StyledSourceSpan {
            source: 0..arabic_start,
            style_index: 0,
        },
        StyledSourceSpan {
            source: arabic_start..text.len(),
            style_index: 1,
        },
    ];
    let shaped = shape_styled_range(
        &pack,
        text,
        0..text.len(),
        &spans,
        &styles,
        BaseDirection::Auto,
        true,
        &RenderOptions::default(),
    )
    .unwrap();
    let metrics = styled_line_metrics(
        &pack,
        &shaped,
        &styles,
        CellLineLayoutPolicy::CalcEditEngine,
        CalcLinePlacementPolicy::RequestedFace,
        text,
        1,
        1,
        &RenderOptions::default(),
    )
    .unwrap();
    let base_metrics = single_face_line_metrics(
        &pack,
        pack.resolve(base.request()).id,
        &base,
        CellLineLayoutPolicy::CalcEditEngine,
        1,
        1,
    )
    .unwrap();
    let superscript_metrics = single_face_line_metrics(
        &pack,
        pack.resolve(superscript.request()).id,
        &superscript,
        CellLineLayoutPolicy::CalcEditEngine,
        1,
        1,
    )
    .unwrap();
    assert_eq!(
        metrics.placement_ascent,
        base_metrics.ascent.max(superscript_metrics.ascent)
    );
    assert_eq!(
        metrics.placement_descent,
        base_metrics.descent.min(superscript_metrics.descent)
    );
}

#[test]
fn calc_mixed_fallback_wrapping_steps_by_requested_face_height() {
    let pack = synthetic_test_pack();
    let mut options = outlined_options(RenderRange::new(0, 0, 0, 0));
    options.font_pack = Some(pack.clone());
    let text = "한국어 العربية 한국어 العربية 한국어 العربية";
    let region = Region {
        source: CellCoordinate { row: 0, col: 0 },
        rect: Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(100),
            height: Fixed::from_pixels(120),
        },
        is_merged: false,
        line_layout_policy: CellLineLayoutPolicy::CalcEditEngine,
        line_placement_policy: CalcLinePlacementPolicy::RequestedFace,
        calc_wrap_space: Some(CalcWrapSpace {
            paper_width_mm100: 2_000,
        }),
        style: Some(
            CellStyle::new()
                .font_name("Wide Sans")
                .size(11)
                .wrap()
                .valign(VAlign::Top),
        ),
        conditional: ConditionalPaint::default(),
        text: text.to_string(),
        rich_text: None,
        hyperlink: None,
        numeric_default: false,
        text_can_overflow: false,
        fixed_height_row: false,
        ods_fixed_height_row: false,
        print_vertical_overflow: false,
        vertical_margin: CALC_CELL_VERTICAL_MARGIN,
    };
    let style = text_style(&region, &options);
    let mut statistics = TypographyStats::default();
    let prepared = prepare_styled_text(
        &pack,
        &region,
        &style,
        false,
        true,
        &options,
        &mut statistics,
    )
    .unwrap();
    assert!(prepared.lines.len() > 1, "the fixture must wrap");
    let requested = single_face_line_metrics(
        &pack,
        pack.resolve(prepared.styles[0].request()).id,
        &prepared.styles[0],
        CellLineLayoutPolicy::CalcEditEngine,
        1,
        1,
    )
    .unwrap();
    assert!(prepared.lines.iter().all(|line| {
        line.metrics.placement_ascent == requested.ascent
            && line.metrics.placement_descent == requested.descent
    }));

    let mut statistics = TypographyStats::default();
    let run = build_glyph_run(
        &pack,
        &region,
        region.rect,
        region.rect,
        None,
        &style,
        false,
        true,
        false,
        &options,
        &mut statistics,
        &mut Warnings::default(),
    )
    .unwrap();
    let baselines = run
        .cluster_metrics
        .iter()
        .map(|metrics| metrics.baseline_y.raw())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(baselines.len(), prepared.lines.len());
    let expected_step = requested
        .ascent
        .checked_sub(requested.descent)
        .unwrap()
        .raw();
    assert!(baselines
        .windows(2)
        .all(|window| window[1] - window[0] == expected_step));
}

#[test]
fn calc_automatic_height_replays_virtual_device_quantization() {
    let locked_metrics = CombinedLineMetrics {
        ascent: Fixed::from_raw(17_422),
        descent: Fixed::from_raw(-4_325),
        line_gap: Fixed::ZERO,
        placement_ascent: Fixed::from_raw(17_422),
        placement_descent: Fixed::from_raw(-4_325),
        calc_ascent_pixels: 17,
        calc_descent_pixels: -4,
        calc_portion_height_mm100: 556,
    };
    assert_eq!(calc_line_height_mm100(locked_metrics).unwrap(), 556);
    for (lines, expected_raw) in [(1_u64, 23_415_i64), (21, 451_311), (24, 515_550)] {
        assert_eq!(
            calc_automatic_height_from_engine_mm100(556 * lines).unwrap(),
            Fixed::from_raw(expected_raw),
            "{lines} lines"
        );
    }

    let ctl_metrics = CombinedLineMetrics {
        ascent: Fixed::from_pixels(16),
        descent: Fixed::from_pixels(-4),
        line_gap: Fixed::ZERO,
        placement_ascent: Fixed::from_pixels(16),
        placement_descent: Fixed::from_pixels(-4),
        calc_ascent_pixels: 16,
        calc_descent_pixels: -4,
        calc_portion_height_mm100: 529,
    };
    assert_eq!(calc_line_height_mm100(ctl_metrics).unwrap(), 529);
    assert_eq!(
        calc_automatic_height_from_engine_mm100(529).unwrap(),
        Fixed::from_raw(22_391),
        "Calc's 20px CTL line plus two padding pixels is 328 twips"
    );
}

#[test]
fn pinned_calc_ctl_base_face_produces_the_verified_mixed_rtl_row_height() {
    let Some(manifest) = std::env::var_os("RXLS_TEST_FONT_PACK_MANIFEST") else {
        return;
    };
    let pack = FontPack::load_manifest(manifest).expect("load pinned render font pack");
    for bold in [false, true] {
        for (requested, selected, ascent, descent, mm100, row_raw) in [
            ("Noto Sans CJK KR", "Noto Sans Hebrew", 16, -4, 529, 22_391),
            ("Noto Sans Arabic", "Noto Sans Arabic", 21, -11, 847, 34_611),
            ("Arial", "Arimo", 14, -3, 450, 19_319),
        ] {
            let style = ResolvedRunStyle {
                family: requested.to_string(),
                size: points_to_fixed(11.0).unwrap(),
                color: Rgb::BLACK,
                bold,
                italic: false,
                underline: false,
                strikethrough: false,
                script: FormatScript::None,
            };
            let requested_resolution = pack.resolve(style.request());
            assert!(requested_resolution.exact_family || requested_resolution.declared_alias);
            assert!(requested_resolution.exact_style);
            let font_id = calc_verified_complex_role_face(&pack, &style, requested_resolution.id)
                .unwrap()
                .expect("verified pack must retain Calc's CTL metric role");
            let identity = pack.selected_face_identity(font_id).unwrap();
            assert_eq!(identity.family, selected, "{requested}");
            assert_eq!(identity.weight, if bold { 700 } else { 400 });
            assert_eq!(identity.source_pack_sha256, pack.pack_sha256());
            let metrics = single_face_line_metrics(
                &pack,
                font_id,
                &style,
                CellLineLayoutPolicy::CalcEditEngine,
                1,
                1,
            )
            .unwrap();
            assert_eq!(metrics.calc_ascent_pixels, ascent, "{requested}");
            assert_eq!(metrics.calc_descent_pixels, descent, "{requested}");
            assert_eq!(
                calc_line_height_mm100(metrics).unwrap(),
                mm100,
                "{requested}"
            );
            assert_eq!(
                calc_automatic_height_from_engine_mm100(mm100).unwrap(),
                Fixed::from_raw(row_raw),
                "{requested}"
            );
        }
    }

    for (family, expected_raw) in [
        ("Noto Sans CJK KR", 22_391),
        ("Noto Sans Arabic", 34_611),
        ("Arial", 19_319),
    ] {
        let styles = format!(
            r#"<styleSheet><fonts count="2"><font><b/><sz val="11"/><name val="{family}"/></font><font><b/><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let workbook = imported_xlsx(
            &styles,
            r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>مرحبا بالعالم 0009</t></is></c></row></sheetData></worksheet>"#,
        );
        let sheet = &workbook.sheets[0];
        let range = RenderRange::new(0, 0, 0, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family.to_string(),
            font_pack: Some(pack.clone()),
            ..RenderOptions::default()
        };
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap();
        assert_eq!(
            measured.rows[0].size,
            Fixed::from_raw(expected_raw),
            "verified XLSX Arabic-plus-digits row for {family}"
        );
    }
}

#[test]
fn pinned_imported_mixed_rtl_digits_use_the_calc_complex_role_face() {
    let Some(manifest) = std::env::var_os("RXLS_TEST_FONT_PACK_MANIFEST") else {
        return;
    };
    let pack = FontPack::load_manifest(manifest).expect("load pinned render font pack");
    let text = "مرحبا بالعالم 0007";
    let styles = r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Carlito"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#;
    let workbook = imported_xlsx(
        styles,
        &format!(
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>{text}</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>Plain 0007</t></is></c></row></sheetData></worksheet>"#
        ),
    );
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 0)),
        gridlines: false,
        default_font_family: "Carlito".to_string(),
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let build = build_scene(&workbook, 0, &options).unwrap();
    let run = glyph_run(&build.scene, text);
    let digit_start = text.find("0007").unwrap() as u64;
    let digit_clusters = run
        .clusters
        .iter()
        .enumerate()
        .filter_map(|(index, cluster)| {
            (cluster.source_start >= digit_start).then_some(index as u32)
        })
        .collect::<BTreeSet<_>>();
    assert!(!digit_clusters.is_empty());
    let digit_faces = run
        .glyphs
        .iter()
        .filter(|glyph| digit_clusters.contains(&glyph.cluster))
        .map(|glyph| &run.font_faces[glyph.face as usize].family)
        .collect::<Vec<_>>();
    assert!(!digit_faces.is_empty());
    assert!(
        digit_faces
            .iter()
            .all(|family| family.as_str() == "Noto Sans Hebrew"),
        "{digit_faces:?}"
    );
    let plain = glyph_run(&build.scene, "Plain 0007");
    assert!(
        plain.font_faces.iter().all(|face| face.family == "Carlito"),
        "plain imported text must retain its Western role"
    );
}

#[test]
fn calc_wrap_space_replays_per_column_device_truncation() {
    let imported = imported_xlsx(
        "<styleSheet/>",
        r#"<worksheet><cols><col min="1" max="1" width="24" customWidth="1"/></cols><sheetData/></worksheet>"#,
    );
    let sheet = &imported.sheets[0];
    assert_eq!(calc_ooxml_wrap_column_twips(sheet, 0, 122), Some(2_928));
    assert_eq!(calc_ooxml_wrap_column_twips(sheet, 1, 122), Some(1_037));
    assert_eq!(
        calc_ooxml_wrap_space(sheet, [0], Fixed::from_raw(8_329))
            .unwrap()
            .unwrap()
            .paper_width_mm100,
        5_106
    );
    assert_eq!(
        calc_ooxml_wrap_space(sheet, [0], Fixed::ZERO).unwrap(),
        None
    );
    let half_twip = imported_xlsx(
        "<styleSheet/>",
        r#"<worksheet><cols><col min="1" max="1" width="8.25" customWidth="1"/></cols><sheetData/></worksheet>"#,
    );
    assert_eq!(
        calc_ooxml_wrap_column_twips(&half_twip.sheets[0], 0, 122),
        Some(1_007)
    );
    let unsupported_default = imported_xlsx(
        "<styleSheet/>",
        r#"<worksheet><sheetFormatPr defaultColWidth="8.5"/><sheetData/></worksheet>"#,
    );
    assert_eq!(
        calc_ooxml_wrap_column_twips(&unsupported_default.sheets[0], 0, 122),
        None
    );

    let narrow = calc_wrap_space_from_column_twips([1_037]).unwrap().unwrap();
    let merged = calc_wrap_space_from_column_twips([1_037, 1_037])
        .unwrap()
        .unwrap();
    // Calc imports an explicit OOXML width directly in default-font digit
    // units, without the ECMA screen-width projection used for painting.
    let wide = calc_wrap_space_from_column_twips([2_928]).unwrap().unwrap();
    assert_eq!(narrow.paper_width_mm100, 1_746); // 66 device pixels
    assert_eq!(merged.paper_width_mm100, 3_572); // 135 device pixels
    assert_eq!(wide.paper_width_mm100, 5_106); // 193 device pixels
    assert_eq!(
        CalcWrapSpace::physical_width_mm100(Fixed::from_pixels(191))
            .unwrap()
            .raw(),
        5_054
    );

    let separately_truncated = calc_wrap_space_from_column_twips([1_010, 1_010])
        .unwrap()
        .unwrap();
    let trunc_after_sum = calc_wrap_space_from_column_twips([2_020]).unwrap().unwrap();
    assert_eq!(separately_truncated.paper_width_mm100, 3_466); // 131 pixels
    assert_eq!(trunc_after_sum.paper_width_mm100, 3_493); // 132 pixels

    assert_eq!(calc_wrap_space_from_column_twips([0]).unwrap(), None);
}

#[test]
fn calc_wrap_space_locks_narrow_merged_and_wide_endpoints() {
    const TEXT: &str = concat!(
        "한국어 자동 줄바꿈 English 日本語 中文 0123456789 ",
        "한국어 자동 줄바꿈 English 日本語 中文 0123456789 ",
        "한국어 자동 줄바꿈 English 日本語 中文 0123456789"
    );
    let measure_physical = |range: Range<usize>| {
        let value = TEXT.get(range).ok_or(RenderError::Typography {
            reason: "invalid_line_break_range",
        })?;
        let raw = value.chars().try_fold(0_i64, |total, ch| {
            let advance = match ch {
                '한' | '국' | '자' | '줄' | '바' => 13_817,
                '어' | '동' | '꿈' => 13_818,
                'E' => 7_780,
                'n' | 'g' => 8_500,
                'l' | 'i' => 3_500,
                's' => 7_500,
                'h' => 11_740,
                '日' | '本' | '語' | '中' | '文' => 15_019,
                '0' | '1' | '2' | '7' => 8_335,
                '3' | '4' | '5' | '6' | '8' | '9' => 8_336,
                ' ' => 3_364,
                _ => {
                    return Err(RenderError::Typography {
                        reason: "unexpected_locked_probe_character",
                    })
                }
            };
            total
                .checked_add(advance)
                .ok_or(RenderError::CoordinateOverflow)
        })?;
        CalcWrapSpace::measure_physical_width(Fixed::from_raw(raw), Fixed::from_raw(15_019))
    };

    for (columns, expected) in [
        (
            vec![1_037],
            vec![
                10, 17, 27, 35, 48, 52, 59, 63, 73, 80, 90, 98, 111, 115, 122, 126, 136, 143, 153,
                161, 174, 178, 185, 188,
            ],
        ),
        (
            vec![1_037, 1_037],
            vec![27, 52, 73, 98, 115, 136, 161, 178, 188],
        ),
        (vec![2_928], vec![38, 63, 101, 126, 164, 188]),
    ] {
        let space = calc_wrap_space_from_column_twips(columns).unwrap().unwrap();
        let lines = wrap_text_lines(
            TEXT,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            space.line_width().unwrap(),
            100,
            1_000,
            measure_physical,
        )
        .unwrap();
        assert_eq!(
            lines.iter().map(|line| line.source.end).collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn calc_merge_wrap_space_uses_full_source_span_and_rejects_hidden_anchor_ambiguity() {
    let maximum_digit_width = Fixed::from_raw(8_329);
    let options = RenderOptions::default();
    let visible = imported_xlsx(
        "<styleSheet/>",
        r#"<worksheet><sheetData/><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#,
    );
    assert_eq!(
        calc_ooxml_merge_wrap_space(&visible.sheets[0], 0, 1, maximum_digit_width, &options,)
            .unwrap(),
        calc_wrap_space_from_column_twips([1_037, 1_037]).unwrap()
    );

    let hidden_anchor = imported_xlsx(
        "<styleSheet/>",
        r#"<worksheet><cols><col min="1" max="1" hidden="1"/></cols><sheetData/><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#,
    );
    assert_eq!(
        calc_ooxml_merge_wrap_space(
            &hidden_anchor.sheets[0],
            0,
            1,
            maximum_digit_width,
            &options,
        )
        .unwrap(),
        None
    );

    let hidden_tail = imported_xlsx(
        "<styleSheet/>",
        r#"<worksheet><cols><col min="2" max="2" hidden="1"/></cols><sheetData/><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#,
    );
    assert_eq!(
        calc_ooxml_merge_wrap_space(&hidden_tail.sheets[0], 0, 1, maximum_digit_width, &options,)
            .unwrap(),
        calc_wrap_space_from_column_twips([1_037]).unwrap()
    );
    let include_hidden = RenderOptions {
        include_hidden: true,
        ..RenderOptions::default()
    };
    assert_eq!(
        calc_ooxml_merge_wrap_space(
            &hidden_tail.sheets[0],
            0,
            1,
            maximum_digit_width,
            &include_hidden,
        )
        .unwrap(),
        None
    );

    assert_eq!(calc_wrap_space_from_column_twips([u64::MAX]).unwrap(), None);
}

#[test]
fn calc_suppressed_space_does_not_advance_paint_or_decoration() {
    let pack = synthetic_test_pack();
    let mut options = outlined_options(RenderRange::new(0, 0, 0, 0));
    options.font_pack = Some(pack.clone());
    options.horizontal_padding = Fixed::ZERO;
    let text = "ab cd";
    let style = CellStyle::new()
        .font_name(pack.default_family())
        .size(11)
        .wrap()
        .underline();
    let mut region = Region {
        source: CellCoordinate { row: 0, col: 0 },
        rect: Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(100),
            height: Fixed::from_pixels(100),
        },
        is_merged: false,
        line_layout_policy: CellLineLayoutPolicy::CalcEditEngine,
        line_placement_policy: CalcLinePlacementPolicy::RequestedFace,
        calc_wrap_space: Some(CalcWrapSpace {
            paper_width_mm100: 1,
        }),
        style: Some(style),
        conditional: ConditionalPaint::default(),
        text: text.to_string(),
        rich_text: None,
        hyperlink: None,
        numeric_default: false,
        text_can_overflow: false,
        fixed_height_row: false,
        ods_fixed_height_row: false,
        print_vertical_overflow: false,
        vertical_margin: CALC_CELL_VERTICAL_MARGIN,
    };
    let base = text_style(&region, &options);
    let (styles, spans) = resolve_rich_styles(&region, &base).unwrap();
    let direction = text_base_direction(text, false);
    let full_atom = shape_styled_range(
        &pack,
        text,
        0..3,
        &spans,
        &styles,
        direction,
        true,
        &options,
    )
    .unwrap();
    let prefix = shape_styled_range(
        &pack,
        text,
        0..2,
        &spans,
        &styles,
        direction,
        true,
        &options,
    )
    .unwrap();
    let full_width = styled_shaped_width(&pack, &full_atom, &styles, 1, 1).unwrap();
    let prefix_width = styled_shaped_width(&pack, &prefix, &styles, 1, 1).unwrap();
    assert!(full_width > prefix_width);
    region.rect.width = full_width;
    region.calc_wrap_space = Some(CalcWrapSpace {
        paper_width_mm100: u64::try_from(
            CalcWrapSpace::measure_physical_width(full_width, points_to_fixed(11.0).unwrap())
                .unwrap()
                .raw(),
        )
        .unwrap(),
    });

    let mut statistics = TypographyStats::default();
    let prepared = prepare_styled_text(
        &pack,
        &region,
        &base,
        false,
        true,
        &options,
        &mut statistics,
    )
    .unwrap();
    assert_eq!(prepared.lines[0].source, 0..3);
    assert_eq!(prepared.lines[0].width, prefix_width);
    assert!(prepared.lines[0]
        .shaped
        .runs
        .iter()
        .all(|run| run.source.end <= 2));

    let mut statistics = TypographyStats::default();
    let run = build_glyph_run(
        &pack,
        &region,
        region.rect,
        region.rect,
        None,
        &base,
        false,
        true,
        false,
        &options,
        &mut statistics,
        &mut Warnings::default(),
    )
    .unwrap();
    assert!(run.metadata_is_valid());
    assert!(run.decorations.len() >= 2);
    assert_eq!(
        run.decorations[0].x2.raw() - run.decorations[0].x1.raw(),
        prefix_width.raw()
    );
    let (suppressed_index, suppressed) = run
        .clusters
        .iter()
        .enumerate()
        .find(|(_, cluster)| cluster.source_start == 2 && cluster.source_end == 3)
        .expect("zero-advance trailing-space cluster");
    assert_eq!(suppressed.command_start, suppressed.command_end);
    assert_eq!(run.cluster_metrics[suppressed_index].advance_x, Fixed::ZERO);
    assert!(run
        .clusters
        .iter()
        .filter(|cluster| cluster.source_start < 2)
        .all(|cluster| cluster.source_end <= 2));
}

#[test]
fn rich_text_styles_clusters_backends_and_auto_height_are_exact() {
    let mut workbook = Workbook::new();
    let wrapped_text = "Latin 한글 אב a\u{301}";
    let transformed_text = "shrink 한글 אב";
    {
        let sheet = workbook.add_sheet("rich-typography");
        sheet.set_col_width(0, 7.0);
        sheet.write_rich_styled(
            0,
            0,
            [
                rxls::TextRun::new("Latin ", rxls::Font::new()),
                rxls::TextRun::new(
                    "한글 ",
                    rxls::Font::new()
                        .with_size(24)
                        .with_color([200, 10, 20])
                        .bold()
                        .underline(),
                ),
                rxls::TextRun::new(
                    "אב ",
                    rxls::Font::new()
                        .with_name("Rtl Sans")
                        .with_color([10, 160, 40])
                        .italic()
                        .strikethrough(),
                ),
                rxls::TextRun::new(
                    "a\u{301}",
                    rxls::Font::new()
                        .with_color([20, 30, 220])
                        .with_script(FormatScript::Superscript),
                ),
            ],
            &CellStyle::new()
                .font_name("Wide Sans")
                .size(11)
                .color([1, 2, 3])
                .wrap()
                .valign(VAlign::Top),
        );
        sheet.set_row_height(1, 60.0);
        sheet.write_rich_styled(
            1,
            0,
            [
                rxls::TextRun::new("shrink ", rxls::Font::new().with_color([1, 2, 3])),
                rxls::TextRun::new(
                    "한글 ",
                    rxls::Font::new().with_size(18).with_color([200, 10, 20]),
                ),
                rxls::TextRun::new(
                    "אב",
                    rxls::Font::new()
                        .with_name("Rtl Sans")
                        .with_color([10, 160, 40]),
                ),
            ],
            &CellStyle::new()
                .font_name("Wide Sans")
                .size(11)
                .shrink_to_fit()
                .indent(2)
                .text_rotation(30)
                .valign(VAlign::Bottom),
        );
    }

    let range = RenderRange::new(0, 0, 1, 0);
    let options = outlined_options(range);
    let output = render_sheet_svg(&workbook, 0, &options).unwrap();
    assert_eq!(output, render_sheet_svg(&workbook, 0, &options).unwrap());
    assert!(!output
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::RichTextFlattened));
    let run = glyph_run(&output.scene, wrapped_text);
    assert!(run.metadata_is_valid());
    assert!(run.clusters.len() <= wrapped_text.chars().count());
    assert_eq!(run.paints.first().unwrap().command_start, 0);
    assert_eq!(
        run.paints.last().unwrap().command_end,
        run.commands.len() as u64
    );
    for color in [
        Rgb::new(1, 2, 3),
        Rgb::new(200, 10, 20),
        Rgb::new(10, 160, 40),
        Rgb::new(20, 30, 220),
    ] {
        assert!(run.paints.iter().any(|paint| paint.color == color));
    }
    assert!(run
        .clusters
        .windows(2)
        .any(|pair| pair[1].source_start < pair[0].source_start));
    assert!(run.clusters.iter().any(|cluster| {
        &wrapped_text[cluster.source_start as usize..cluster.source_end as usize] == "a\u{301}"
    }));
    assert!(run
        .decorations
        .iter()
        .any(|line| line.color == Rgb::new(200, 10, 20)));
    assert!(run
        .decorations
        .iter()
        .any(|line| line.color == Rgb::new(10, 160, 40)));
    assert!(output.svg.contains("fill=\"#C80A14\""));
    assert!(output.svg.contains("fill=\"#0AA028\""));
    assert!(output.svg.contains("fill=\"#141EDC\""));

    let transformed = glyph_run(&output.scene, transformed_text);
    assert_eq!(transformed.rotation_degrees, 30);
    assert!(transformed.metadata_is_valid());
    assert!(path_x_span(transformed) <= transformed.clip_bounds.width.raw());

    let (rows, _) = measure_sheet_axes(&workbook.sheets[0], range, &options).unwrap();
    assert!(rows[0].size > options.default_row_height);
    assert_eq!(
        output.scene.height,
        sum_fixed(rows.iter().map(|slot| slot.size)).unwrap()
    );

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            render: options.clone(),
            single_page_sheets: true,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    let pdf = render_print_document_pdf(&document).unwrap();
    assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
    let png = render_print_document_png_pages(&document, 96).unwrap();
    assert_eq!(png, render_print_document_png_pages(&document, 96).unwrap());
    assert_eq!(png.len(), document.pages.len());
    let pdf_source = String::from_utf8_lossy(&pdf);
    assert!(pdf_source.contains("/Subtype /Type3"));
    assert!(!pdf_source.contains("/Helvetica"));
    assert!(pdf_source.contains("0.784314 0.039216 0.078431 rg"));

    // The same document rendered with the pack in hand embeds real font
    // programs instead of outlining every glyph.
    let pack = options
        .font_pack
        .as_ref()
        .expect("test renders with a pack");
    let embedded = render_print_document_pdf_with_fonts(&document, pack).unwrap();
    assert_eq!(
        embedded,
        render_print_document_pdf_with_fonts(&document, pack).unwrap(),
        "embedded output must be byte-deterministic"
    );
    let embedded_source = String::from_utf8_lossy(&embedded);
    assert!(embedded_source.contains("/Subtype /Type0"));
    assert!(embedded_source.contains("/Encoding /Identity-H"));
    assert!(embedded_source.contains("/Subtype /CIDFontType2"));
    assert!(embedded_source.contains("/FontFile2"));
    assert!(
        embedded.len() < pdf.len() + 128,
        "embedding must remain within bounded metadata overhead: {} vs {}",
        embedded.len(),
        pdf.len()
    );

    if std::process::Command::new("pdftotext")
        .arg("-v")
        .output()
        .is_ok()
    {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("rxls-rich-pdf-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let pdf_path = directory.join("rich.pdf");
        let text_path = directory.join("rich.txt");
        std::fs::write(&pdf_path, &pdf).unwrap();
        let status = std::process::Command::new("pdftotext")
            .arg(&pdf_path)
            .arg(&text_path)
            .status()
            .unwrap();
        assert!(status.success());
        let extracted = std::fs::read_to_string(text_path).unwrap();
        for fragment in ["Latin", "한글", "a\u{301}"] {
            assert!(extracted.contains(fragment), "{extracted:?}");
        }
        // Poppler preserves the logical RTL source order inside its
        // directional controls without inventing a leading gap.
        assert!(extracted.contains("\u{202b}אב\u{202c}"), "{extracted:?}");
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn ligature_cluster_metadata_spans_all_source_bytes_and_hard_limits() {
    let pack = synthetic_test_pack();
    let styles = vec![ResolvedRunStyle {
        family: "Wide Sans".to_string(),
        size: Fixed::from_pixels(16),
        color: Rgb::BLACK,
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
        script: FormatScript::None,
    }];
    // One glyph whose HarfBuzz cluster starts at byte zero models a
    // two-source-character ligature without requiring a host test font.
    let shaped = ShapedText {
        runs: vec![ShapedRun {
            font_id: FontId(0),
            direction: BaseDirection::LeftToRight,
            source: 0..2,
            style_index: 0,
            glyphs: vec![ShapedGlyph {
                glyph_id: 1,
                cluster: 0,
                x_advance: 600,
                y_advance: 0,
                x_offset: 0,
                y_offset: 0,
            }],
        }],
        glyph_count: 1,
        missing_glyphs: 0,
        requested_family_matched: true,
        selected_faces: Vec::new(),
        base_direction: BaseDirection::LeftToRight,
    };
    let options = RenderOptions::default();
    let mut stats = TypographyStats::default();
    let mut commands = Vec::new();
    let mut clusters = Vec::new();
    let mut cluster_metrics = Vec::new();
    let mut paints = Vec::new();
    let mut decorations = Vec::new();
    let mut glyphs = Vec::new();
    let mut font_faces = Vec::new();
    append_styled_shaped_outlines(
        &pack,
        "fi",
        0,
        &shaped,
        Fixed::ZERO,
        Fixed::from_pixels(16),
        &styles,
        1,
        1,
        &options,
        &mut stats,
        &mut commands,
        &mut clusters,
        &mut cluster_metrics,
        &mut paints,
        &mut decorations,
        &mut glyphs,
        &mut font_faces,
    )
    .unwrap();
    assert_eq!(font_faces.len(), 1);
    assert_eq!(font_faces[0].units_per_em, 1_000);
    assert_eq!(glyphs.len(), 1, "the ligature shapes to a single glyph");
    assert_eq!(glyphs[0].face, 0);
    assert_eq!(glyphs[0].origin_y, Fixed::from_pixels(16));
    assert_eq!(glyphs[0].size, Fixed::from_pixels(16));
    assert!(!glyphs[0].synthetic);
    assert_eq!(clusters.len(), 1);
    assert_eq!(clusters[0].source_start, 0);
    assert_eq!(clusters[0].source_end, 2);
    assert_eq!(clusters[0].command_start, 0);
    assert_eq!(clusters[0].command_end, commands.len() as u64);
    assert_eq!(cluster_metrics.len(), 1);
    assert_eq!(cluster_metrics[0].baseline_y, Fixed::from_pixels(16));
    assert_eq!(cluster_metrics[0].ascent, Fixed::from_raw(13_107));
    assert_eq!(cluster_metrics[0].descent, Fixed::from_raw(-3_277));
    assert_eq!(cluster_metrics[0].advance_x, Fixed::from_raw(9_830));

    let mut limited = options;
    limited.limits.max_path_commands = commands.len() as u64 - 1;
    let error = append_styled_shaped_outlines(
        &pack,
        "fi",
        0,
        &shaped,
        Fixed::ZERO,
        Fixed::from_pixels(16),
        &styles,
        1,
        1,
        &limited,
        &mut TypographyStats::default(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RenderError::LimitExceeded {
            kind: LimitKind::PathCommands,
            ..
        }
    ));
}

#[test]
fn merged_source_clusters_retain_the_actual_shaped_glyph_cluster_index() {
    let pack = synthetic_test_pack();
    let styles = vec![ResolvedRunStyle {
        family: "Wide Sans".to_string(),
        size: Fixed::from_pixels(16),
        color: Rgb::BLACK,
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
        script: FormatScript::None,
    }];
    let glyph = ShapedGlyph {
        glyph_id: 1,
        cluster: 0,
        x_advance: 600,
        y_advance: 0,
        x_offset: 0,
        y_offset: 0,
    };
    // Layout normally produces disjoint run sources, but the cluster
    // builder deliberately supports adjacent command ranges for one
    // source cluster. Exercise that supported merge directly so retained
    // shaped-glyph identity cannot point at the next/nonexistent cluster.
    let shaped = ShapedText {
        runs: vec![
            ShapedRun {
                font_id: FontId(0),
                direction: BaseDirection::LeftToRight,
                source: 0..1,
                style_index: 0,
                glyphs: vec![glyph],
            },
            ShapedRun {
                font_id: FontId(0),
                direction: BaseDirection::LeftToRight,
                source: 0..1,
                style_index: 0,
                glyphs: vec![glyph],
            },
        ],
        glyph_count: 2,
        missing_glyphs: 0,
        requested_family_matched: true,
        selected_faces: Vec::new(),
        base_direction: BaseDirection::LeftToRight,
    };
    let mut stats = TypographyStats::default();
    let mut commands = Vec::new();
    let mut clusters = Vec::new();
    let mut cluster_metrics = Vec::new();
    let mut paints = Vec::new();
    let mut decorations = Vec::new();
    let mut glyphs = Vec::new();
    let mut font_faces = Vec::new();
    append_styled_shaped_outlines(
        &pack,
        "A",
        0,
        &shaped,
        Fixed::ZERO,
        Fixed::from_pixels(16),
        &styles,
        1,
        1,
        &RenderOptions::default(),
        &mut stats,
        &mut commands,
        &mut clusters,
        &mut cluster_metrics,
        &mut paints,
        &mut decorations,
        &mut glyphs,
        &mut font_faces,
    )
    .unwrap();

    assert_eq!(clusters.len(), 1);
    assert_eq!(cluster_metrics.len(), 1);
    assert_eq!(glyphs.len(), 2);
    assert!(glyphs.iter().all(|glyph| glyph.cluster == 0));
    assert_eq!(clusters[0].command_end, commands.len() as u64);
}

#[test]
fn automatic_row_heights_are_sparse_exact_and_shared_with_scene_layout() {
    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_sheet("auto-heights");
        sheet.set_col_width(0, 2.0);
        sheet.write_styled(0, 0, "한글中文", &CellStyle::new().wrap());
        // The selected axis below only contains column A. Row measurement is a
        // worksheet property, so mandatory breaks in B still affect row 2.
        sheet.write(1, 1, "A\nB\n");
        sheet.write_rich(
            2,
            2,
            [rxls::TextRun::new("large", rxls::Font::new().with_size(24))],
        );
        sheet.set_row_height(3, 12.0);
        sheet.write_styled(3, 0, "한글中文", &CellStyle::new().wrap());
        sheet.hide_row(4);
        sheet.write(4, 0, "hidden\nrow");
    }

    let range = RenderRange::new(0, 0, 4, 0);
    let options = outlined_options(range);
    let sheet = &workbook.sheets[0];
    let (first, columns) = measure_sheet_axes(sheet, range, &options).unwrap();
    let (second, _) = measure_sheet_axes(sheet, range, &options).unwrap();
    assert_eq!(first, second);
    assert_eq!(columns.len(), 1);
    assert_eq!(
        first
            .iter()
            .map(|slot| (slot.index, slot.size.raw()))
            .collect::<Vec<_>>(),
        [
            (0, 74_140), // four shaped CJK lines
            (1, 56_117), // two mandatory breaks plus trailing empty line
            (2, 41_370), // retained 24pt rich-run metrics
            (3, 16_384), // explicit 12pt source height is authoritative
        ]
    );

    let first_scene = build_scene(&workbook, 0, &options).unwrap();
    let second_scene = build_scene(&workbook, 0, &options).unwrap();
    assert_eq!(first_scene, second_scene);
    assert_eq!(
        first_scene.scene.height,
        sum_fixed(first.iter().map(|slot| slot.size)).unwrap()
    );

    let included = RenderOptions {
        include_hidden: true,
        ..options
    };
    let (rows, _) = measure_sheet_axes(sheet, range, &included).unwrap();
    assert_eq!(
        rows.last().map(|slot| (slot.index, slot.size.raw())),
        Some((4, 38_094))
    );
}

#[test]
fn prepared_geometry_replays_nonlocal_wrapped_row_height_in_print_tiles() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("prepared-geometry");
    sheet.set_col_width(0, 100.0);
    sheet.set_col_width(1, 2.0);
    sheet.write(0, 0, "plain");
    sheet.write_styled(0, 1, "한글中文한글中文", &CellStyle::new().wrap());

    let full_range = RenderRange::new(0, 0, 0, 1);
    let full_options = outlined_options(full_range);
    let (prepared_rows, prepared_columns) =
        measure_sheet_axes(sheet, full_range, &full_options).unwrap();
    let prepared_height = prepared_rows[0].size;

    let tile_options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        ..full_options
    };
    let tile_local = build_sheet_scene(sheet, 0, &tile_options).unwrap();
    assert_eq!(
        tile_local.scene.height, prepared_height,
        "automatic row height is sheet-row geometry even outside the tile columns"
    );

    let replayed = build_sheet_scene_with_geometry(
        sheet,
        0,
        &tile_options,
        SheetGeometryOverride::new(&prepared_rows, &prepared_columns),
    )
    .unwrap();
    assert_eq!(replayed.scene.height, prepared_height);
}

#[test]
fn merged_auto_height_is_independent_of_selected_columns() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("merged-row-geometry");
    sheet.set_col_width(0, 8.0);
    for col in 1..=3 {
        sheet.set_col_width(col, 2.0);
    }
    sheet.write_styled(
        0,
        1,
        "merged wrapped text must use the complete B:D width",
        &CellStyle::new().wrap(),
    );
    sheet.merge(0, 1, 0, 3);

    let full_options = outlined_options(RenderRange::new(0, 0, 0, 3));
    let tile_options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        ..full_options.clone()
    };
    let full = build_sheet_scene(&workbook.sheets[0], 0, &full_options).unwrap();
    let tile = build_sheet_scene(&workbook.sheets[0], 0, &tile_options).unwrap();
    assert_eq!(
        tile.scene.height, full.scene.height,
        "worksheet-global row height must use the full merged width outside the selected tile"
    );
}

#[test]
fn merged_auto_height_uses_visible_width_without_materializing_covered_cells() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("merged-height");
    sheet.set_col_width(0, 2.0);
    sheet.set_col_width(1, 2.0);
    sheet.hide_column(1);
    sheet.merge(0, 0, 0, 1);
    sheet.write_styled(0, 0, "한글中文", &CellStyle::new().wrap());
    let range = RenderRange::new(0, 0, 0, 0);

    let hidden = outlined_options(range);
    let (rows, _) = measure_sheet_axes(sheet, range, &hidden).unwrap();
    assert_eq!(rows[0].size.raw(), 74_140);

    let visible = RenderOptions {
        include_hidden: true,
        ..hidden
    };
    let (rows, _) = measure_sheet_axes(sheet, range, &visible).unwrap();
    assert_eq!(rows[0].size.raw(), 38_094);
}

#[test]
fn automatic_height_limits_fail_before_unbounded_line_growth() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("height-limit").write(0, 0, "A\nB");
    let range = RenderRange::new(0, 0, 0, 0);
    let mut options = outlined_options(range);
    options.limits.max_text_lines = 1;
    assert_eq!(
        measure_sheet_axes(&workbook.sheets[0], range, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TextLines,
            limit: 1,
            actual: 2,
        })
    );
}

#[test]
fn horizontal_overflow_respects_empty_cells_blockers_wrap_and_rtl() {
    let mut ltr = Workbook::new();
    let sheet = ltr.add_sheet("ltr");
    sheet.write(0, 0, "spills across empty cells");
    sheet.write(0, 3, "blocker");
    sheet.write_styled(1, 0, "wrapped", &CellStyle::new().wrap());
    let options = outlined_options(RenderRange::new(0, 0, 1, 3));
    let scene = build_scene(&ltr, 0, &options).unwrap().scene;
    assert_eq!(
        glyph_run(&scene, "spills across empty cells").clip_bounds,
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(255),
            height: Fixed::from_pixels(20),
        }
    );
    assert_eq!(
        glyph_run(&scene, "wrapped").clip_bounds.width,
        Fixed::from_pixels(85)
    );

    let mut rtl = Workbook::new();
    let sheet = rtl.add_sheet("rtl");
    sheet.set_right_to_left(true);
    sheet.write(0, 2, "עברית");
    sheet.write(0, 3, "blocker");
    let scene = build_scene(&rtl, 0, &outlined_options(RenderRange::new(0, 0, 0, 3)))
        .unwrap()
        .scene;
    assert_eq!(
        glyph_run(&scene, "עברית").clip_bounds,
        Rect {
            x: Fixed::from_pixels(85),
            y: Fixed::ZERO,
            width: Fixed::from_pixels(85),
            height: Fixed::from_pixels(20),
        }
    );
}

#[test]
fn rtl_axis_measurement_remains_logical_ascending_geometry() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("rtl-axis");
    sheet.set_right_to_left(true);
    sheet.set_col_width(1, 5.0);
    sheet.set_col_width(2, 11.0);
    sheet.set_col_width(3, 19.0);
    sheet.set_col_width(4, 8.0);
    sheet.hide_column(2);

    let range = RenderRange::new(0, 1, 0, 4);
    let (_, columns) = measure_sheet_axes(sheet, range, &RenderOptions::default()).unwrap();
    assert_eq!(
        columns.iter().map(|slot| slot.index).collect::<Vec<_>>(),
        [1, 3, 4]
    );
    assert_eq!(columns[0].offset, Fixed::ZERO);
    for pair in columns.windows(2) {
        assert_eq!(
            pair[1].offset,
            pair[0].offset.checked_add(pair[0].size).unwrap()
        );
    }
}

#[test]
fn verified_font_metrics_drive_ecma_column_width_geometry() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("widths");
    sheet.set_default_col_width(10.0);
    sheet.write(0, 0, "A");
    let range = RenderRange::new(0, 0, 0, 0);

    let approximate = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let outlined = build_scene(&workbook, 0, &outlined_options(range)).unwrap();
    assert_eq!(approximate.scene.width, Fixed::from_pixels(72));
    assert_eq!(outlined.scene.width, Fixed::from_pixels(82));

    let mut empty = Workbook::new();
    empty.add_sheet("empty-widths").set_default_col_width(10.0);
    let mut empty_options = outlined_options(range);
    empty_options.selection = RenderSelection::Used;
    let empty = build_scene(&empty, 0, &empty_options).unwrap();
    assert_eq!(empty.scene.width, Fixed::from_pixels(82));
    assert_eq!(empty.scene.height, Fixed::from_pixels(1));

    let mut no_width_metadata = Workbook::new();
    no_width_metadata.add_sheet("defaults").write(0, 4, "A");
    let five_columns = RenderRange::new(0, 0, 0, 4);
    let approximate = build_scene(
        &no_width_metadata,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(five_columns),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let outlined = build_scene(&no_width_metadata, 0, &outlined_options(five_columns)).unwrap();
    assert_eq!(approximate.scene.width, Fixed::from_pixels(320));
    assert_eq!(outlined.scene.width, Fixed::from_pixels(425));

    let mut imported_widths = Workbook::new();
    let sheet = imported_widths.add_sheet("calc-import");
    sheet.set_col_width(0, 18.0);
    for col in 1..=4 {
        sheet.set_col_width(col, 14.0);
    }
    sheet.write(0, 4, "A");
    let outlined = build_scene(
        &imported_widths,
        0,
        &outlined_options(RenderRange::new(0, 0, 0, 4)),
    )
    .unwrap();
    assert_eq!(outlined.scene.width, Fixed::from_pixels(602));
}

#[test]
fn implicit_biff_columns_use_calcs_fixed_sixty_four_point_default() {
    let candidates = [
        include_bytes!("../../../tests/fixtures/xls/reader-basic.xls").as_slice(),
        include_bytes!("../../../tests/fixtures/xls/korean-unicode-biff8.xls").as_slice(),
        include_bytes!("../../../tests/fixtures/xls/korean-cp949-biff5.xls").as_slice(),
    ];
    let workbook = candidates
        .into_iter()
        .map(|bytes| Workbook::open(bytes).expect("imported BIFF fixture"))
        .find(|workbook| {
            workbook
                .sheets
                .first()
                .is_some_and(|sheet| sheet.biff_uses_application_default_column_width())
        })
        .expect("at least one BIFF fixture without a sheet-wide width record");
    let sheet = &workbook.sheets[0];
    let first_col = (0_u16..=251)
        .find(|first| (*first..=*first + 4).all(|col| !sheet.column_widths().contains_key(&col)))
        .expect("five implicit BIFF columns");
    let range = RenderRange::new(0, first_col, 0, first_col + 4);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        include_hidden: true,
        gridlines: false,
        ..RenderOptions::default()
    };

    let (_, columns) = measure_sheet_axes(sheet, range, &options).unwrap();
    assert_eq!(columns.len(), 5);
    assert!(columns
        .iter()
        .all(|column| column.size == BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH));
    assert_eq!(
        columns.iter().map(|column| column.size.raw()).sum::<i64>(),
        436_905
    );
    assert_eq!(
        build_single_page_sheet_scene(sheet, 0, &options)
            .unwrap()
            .scene
            .width,
        Fixed::from_raw(436_950),
        "SinglePageSheets must convert the cumulative 6,400-twip extent and retain Calc's inclusive rectangle unit"
    );

    let authored = {
        let mut workbook = Workbook::new();
        workbook.add_sheet("authored").write(0, 4, "A");
        workbook
    };
    let (_, authored_columns) =
        measure_sheet_axes(&authored.sheets[0], RenderRange::new(0, 0, 0, 4), &options).unwrap();
    assert!(authored_columns
        .iter()
        .all(|column| column.size == options.default_column_width));
}

#[test]
fn ooxml_defaults_distinguish_absent_defaulted_base_and_explicit_widths() {
    const PINNED_NOTO_SANS_CJK_KR_11_MDW: Fixed = Fixed::from_raw(8_336);
    let imported = |sheet_format: &str| {
        imported_xlsx(
            "<styleSheet/>",
            &format!(
                r#"<worksheet>{sheet_format}<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
            ),
        )
    };
    let absent = imported("");
    let defaulted_base = imported(r#"<sheetFormatPr/>"#);
    let explicit_8_5 = imported(r#"<sheetFormatPr defaultColWidth="8.5"/>"#);
    let explicit_8 = imported(r#"<sheetFormatPr defaultColWidth="8"/>"#);
    let base_8 = imported(r#"<sheetFormatPr baseColWidth="8"/>"#);
    let range = RenderRange::new(0, 0, 0, 0);

    assert_eq!(absent.sheets[0].default_column_width(), None);
    assert_eq!(absent.sheets[0].implicit_ooxml_column_width(), Some(None));
    assert!(!absent.sheets[0].ooxml_uses_defaulted_base_column_width());
    assert_eq!(
        defaulted_base.sheets[0].implicit_ooxml_column_width(),
        Some(None)
    );
    assert!(defaulted_base.sheets[0].ooxml_uses_defaulted_base_column_width());
    assert_eq!(
        calc_ooxml_wrap_column_twips(&absent.sheets[0], 0, 122),
        Some(1_037)
    );
    assert_eq!(
        calc_ooxml_wrap_column_twips(&defaulted_base.sheets[0], 0, 122),
        Some(1_051)
    );
    assert_eq!(explicit_8_5.sheets[0].default_column_width(), Some(8.5));
    assert_eq!(explicit_8_5.sheets[0].implicit_ooxml_column_width(), None);
    assert_eq!(base_8.sheets[0].default_column_width(), None);
    assert_eq!(
        base_8.sheets[0].implicit_ooxml_column_width(),
        Some(Some(8.0))
    );
    assert_eq!(
        xlsb_digits_to_fixed(
            OOXML_APPLICATION_DEFAULT_COLUMN_WIDTH_256,
            PINNED_NOTO_SANS_CJK_KR_11_MDW,
            0,
        ),
        Some(Fixed::from_raw(70_793))
    );

    let approximate = build_scene(
        &absent,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert_eq!(approximate.scene.width, Fixed::from_pixels(64));

    let absent_width = build_scene(&absent, 0, &outlined_options(range))
        .unwrap()
        .scene
        .width;
    let explicit_8_5_width = build_scene(&explicit_8_5, 0, &outlined_options(range))
        .unwrap()
        .scene
        .width;
    let explicit_8_width = build_scene(&explicit_8, 0, &outlined_options(range))
        .unwrap()
        .scene
        .width;
    let base_8_width = build_scene(&base_8, 0, &outlined_options(range))
        .unwrap()
        .scene
        .width;
    assert_eq!(absent_width, Fixed::from_raw(76_049));
    assert_eq!(explicit_8_5_width, Fixed::from_pixels(70));
    assert_eq!(explicit_8_width, Fixed::from_pixels(66));
    assert_eq!(base_8_width, Fixed::from_pixels(71));
    assert_eq!(
        base_8_width.checked_sub(explicit_8_width),
        Some(Fixed::from_pixels(5))
    );

    let five_columns = RenderRange::new(0, 0, 0, 4);
    let pack = synthetic_test_pack();
    let exact_options = RenderOptions {
        selection: RenderSelection::Range(five_columns),
        gridlines: false,
        default_font_family: pack.default_family().to_string(),
        default_font_size: Fixed::from_raw(13_893),
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let ordinary_total = |sheet: &Sheet| {
        measure_sheet_axes(sheet, five_columns, &exact_options)
            .unwrap()
            .1
            .iter()
            .map(|column| column.size.raw())
            .sum::<i64>()
    };
    assert_eq!(ordinary_total(&absent.sheets[0]), 353_965);
    assert_eq!(ordinary_total(&defaulted_base.sheets[0]), 358_740);
    assert_eq!(
        build_single_page_sheet_scene(&absent.sheets[0], 0, &exact_options)
            .unwrap()
            .scene
            .width,
        Fixed::from_raw(354_011)
    );
    assert_eq!(
        build_single_page_sheet_scene(&defaulted_base.sheets[0], 0, &exact_options)
            .unwrap()
            .scene
            .width,
        Fixed::from_raw(358_771)
    );

    let fallback_options = RenderOptions {
        selection: RenderSelection::Range(five_columns),
        gridlines: false,
        ..RenderOptions::default()
    };
    for sheet in [&absent.sheets[0], &defaulted_base.sheets[0]] {
        let ordinary = measure_sheet_axes(sheet, five_columns, &fallback_options)
            .unwrap()
            .1
            .iter()
            .map(|column| column.size.raw())
            .sum::<i64>();
        assert_eq!(ordinary, 5 * Fixed::from_pixels(64).raw());
        // Calc still converts the cumulative 4,800-twip position to
        // Map100thMM and applies tools::Rectangle's inclusive endpoint,
        // even when every source track used the renderer fallback.
        assert_eq!(
            build_single_page_sheet_scene(sheet, 0, &fallback_options)
                .unwrap()
                .scene
                .width,
            Fixed::from_raw(327_732)
        );
    }
}

#[test]
fn xlsb_digit_widths_match_calc_twips_for_explicit_defaults_hidden_and_font_mdw() {
    const CARLITO_11_MDW: Fixed = Fixed::from_raw(7_612);
    const CARLITO_EQUIVALENT_SYNTHETIC_SIZE: Fixed = Fixed::from_raw(12_687);
    let from_twips = |twips: i64| {
        Fixed::from_raw(
            (twips * FIXED_UNITS_PER_PIXEL + i64::try_from(TWIPS_PER_CSS_PIXEL / 2).unwrap())
                / i64::try_from(TWIPS_PER_CSS_PIXEL).unwrap(),
        )
    };
    let ceil_pixels =
        |width: Fixed| (width.raw() + FIXED_UNITS_PER_PIXEL - 1) / FIXED_UNITS_PER_PIXEL;

    assert_eq!(
        xlsb_digits_to_fixed(18 * 256, CARLITO_11_MDW, 0),
        Some(from_twips(1_998))
    );
    assert_eq!(
        xlsb_digits_to_fixed(14 * 256, CARLITO_11_MDW, 0),
        Some(from_twips(1_554))
    );
    assert_eq!(
        xlsb_digits_to_fixed(8 * 256 + 128, CARLITO_11_MDW, 0),
        Some(from_twips(944))
    );
    assert_eq!(
        xlsb_digits_to_fixed(8 * 256, CARLITO_11_MDW, 5),
        Some(from_twips(963))
    );
    assert_eq!(
        xlsb_digits_to_fixed(18 * 256, Fixed::from_pixels(8), 0),
        Some(Fixed::from_pixels(144))
    );

    let explicit = imported_width_xlsb(None, &[(0, 0, 18 * 256, false), (1, 4, 14 * 256, false)]);
    assert_eq!(
        explicit.sheets[0].xlsb_column_widths_256().get(&0),
        Some(&(18 * 256))
    );
    assert_eq!(
        explicit.sheets[0].xlsb_column_widths_256().get(&4),
        Some(&(14 * 256))
    );
    assert_eq!(
        explicit.sheets[0].xlsb_default_column_width(),
        Some(XlsbDefaultColumnWidth::ApplicationDefault)
    );

    let range = RenderRange::new(0, 0, 0, 4);
    let pack = synthetic_test_pack();
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: pack.default_family().to_string(),
        default_font_size: CARLITO_EQUIVALENT_SYNTHETIC_SIZE,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let (_, explicit_columns) = measure_sheet_axes(&explicit.sheets[0], range, &options).unwrap();
    assert_eq!(
        explicit_columns
            .iter()
            .map(|column| column.size.raw())
            .collect::<Vec<_>>(),
        [136_397, 106_086, 106_086, 106_086, 106_086]
    );
    let explicit_total = explicit_columns
        .iter()
        .map(|column| column.size.raw())
        .sum::<i64>();
    assert_eq!(explicit_total, 560_741);
    assert_eq!(ceil_pixels(Fixed::from_raw(explicit_total)), 548);

    let implicit = imported_width_xlsb(None, &[]);
    let (_, implicit_columns) = measure_sheet_axes(&implicit.sheets[0], range, &options).unwrap();
    assert!(implicit_columns
        .iter()
        .all(|column| column.size.raw() == 64_444));
    let implicit_total = implicit_columns
        .iter()
        .map(|column| column.size.raw())
        .sum::<i64>();
    assert_eq!(implicit_total, 322_220);
    assert_eq!(ceil_pixels(Fixed::from_raw(implicit_total)), 315);
    assert_eq!(
        build_single_page_sheet_scene(&implicit.sheets[0], 0, &options)
            .unwrap()
            .scene
            .width,
        Fixed::from_raw(322_275),
        "SinglePageSheets must convert the cumulative XLSB twips and retain Calc's inclusive rectangle unit"
    );

    let numeric_default = imported_width_xlsb(Some((14 * 256, 42)), &[]);
    assert_eq!(
        numeric_default.sheets[0].xlsb_default_column_width(),
        Some(XlsbDefaultColumnWidth::Digits256(14 * 256))
    );
    let (_, default_columns) =
        measure_sheet_axes(&numeric_default.sheets[0], range, &options).unwrap();
    assert!(default_columns
        .iter()
        .all(|column| column.size.raw() == 106_086));
    assert_eq!(
        ceil_pixels(Fixed::from_raw(
            default_columns.iter().map(|column| column.size.raw()).sum()
        )),
        518
    );

    let base_default = imported_width_xlsb(Some((u32::MAX, 8)), &[]);
    assert_eq!(
        base_default.sheets[0].xlsb_default_column_width(),
        Some(XlsbDefaultColumnWidth::BaseCharacters(8))
    );
    let (_, base_columns) = measure_sheet_axes(&base_default.sheets[0], range, &options).unwrap();
    assert!(base_columns
        .iter()
        .all(|column| column.size.raw() == 65_741));

    let hidden = imported_width_xlsb(
        None,
        &[
            (0, 0, 18 * 256, false),
            (1, 1, 14 * 256, true),
            (2, 4, 14 * 256, false),
        ],
    );
    let (_, visible_columns) = measure_sheet_axes(&hidden.sheets[0], range, &options).unwrap();
    assert_eq!(
        visible_columns
            .iter()
            .map(|column| column.index)
            .collect::<Vec<_>>(),
        [0, 2, 3, 4]
    );
    assert_eq!(
        ceil_pixels(Fixed::from_raw(
            visible_columns.iter().map(|column| column.size.raw()).sum()
        )),
        444
    );
    let mut include_hidden = options.clone();
    include_hidden.include_hidden = true;
    let (_, all_columns) = measure_sheet_axes(&hidden.sheets[0], range, &include_hidden).unwrap();
    assert_eq!(
        all_columns
            .iter()
            .map(|column| column.size.raw())
            .sum::<i64>(),
        560_741
    );

    let mut wider_font = options.clone();
    wider_font.default_font_size = Fixed::from_raw(13_653);
    let (_, wider_columns) = measure_sheet_axes(&explicit.sheets[0], range, &wider_font).unwrap();
    assert_eq!(
        wider_columns
            .iter()
            .map(|column| column.size)
            .collect::<Vec<_>>(),
        [
            Fixed::from_pixels(144),
            Fixed::from_pixels(112),
            Fixed::from_pixels(112),
            Fixed::from_pixels(112),
            Fixed::from_pixels(112),
        ]
    );
}

#[test]
fn ooxml_implicit_row_height_is_calc_specific_not_a_global_fallback() {
    let imported = |sheet_format: &str| {
        imported_xlsx(
            "<styleSheet/>",
            &format!(
                r#"<worksheet>{sheet_format}<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
            ),
        )
    };
    let implicit = imported("");
    let explicit = imported(r#"<sheetFormatPr defaultRowHeight="15"/>"#);
    let mut overridden = imported("");
    overridden.sheets[0].set_default_row_height(12.0);
    let mut authored = Workbook::new();
    authored.add_sheet("authored").write(0, 0, 1.0);

    assert_eq!(implicit.sheets[0].default_row_height(), None);
    assert!(implicit.sheets[0].has_implicit_ooxml_row_height());
    assert_eq!(explicit.sheets[0].default_row_height(), Some(15.0));
    assert!(!explicit.sheets[0].has_implicit_ooxml_row_height());
    assert_eq!(overridden.sheets[0].default_row_height(), Some(12.0));
    assert!(!overridden.sheets[0].has_implicit_ooxml_row_height());
    assert!(!authored.sheets[0].has_implicit_ooxml_row_height());

    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        gridlines: false,
        default_row_height: Fixed::from_pixels(37),
        ..RenderOptions::default()
    };
    let implicit_rows =
        measure_sheet_axes(&implicit.sheets[0], RenderRange::new(0, 0, 0, 0), &options)
            .unwrap()
            .0;
    let explicit_rows =
        measure_sheet_axes(&explicit.sheets[0], RenderRange::new(0, 0, 0, 0), &options)
            .unwrap()
            .0;
    let overridden_rows = measure_sheet_axes(
        &overridden.sheets[0],
        RenderRange::new(0, 0, 0, 0),
        &options,
    )
    .unwrap()
    .0;
    let authored_rows =
        measure_sheet_axes(&authored.sheets[0], RenderRange::new(0, 0, 0, 0), &options)
            .unwrap()
            .0;

    assert_eq!(implicit_rows[0].size, OOXML_APPLICATION_DEFAULT_ROW_HEIGHT);
    assert_eq!(implicit_rows[0].size.raw(), 19_351);
    assert_eq!(explicit_rows[0].size, Fixed::from_pixels(20));
    assert_eq!(overridden_rows[0].size, Fixed::from_pixels(16));
    assert_eq!(authored_rows[0].size, Fixed::from_pixels(37));
}

#[test]
fn ooxml_implicit_row_defaults_round_only_cumulative_single_page_boundaries() {
    let unverified = |sheet_format: &str| {
        imported_xlsx(
            "<styleSheet/>",
            &format!(r#"<worksheet>{sheet_format}<sheetData/></worksheet>"#),
        )
    };
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let verified_styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let verified = |sheet_format: &str| {
        imported_xlsx(
            &verified_styles,
            &format!(r#"<worksheet>{sheet_format}<sheetData/></worksheet>"#),
        )
    };
    let range = RenderRange::new(0, 0, 4, 0);
    let fallback_options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        ..RenderOptions::default()
    };
    let verified_options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };

    for sheet_format in ["", "<sheetFormatPr/>"] {
        let unverified = unverified(sheet_format);
        let unverified_sheet = &unverified.sheets[0];
        let ordinary = measure_sheet_axes(unverified_sheet, range, &fallback_options)
            .unwrap()
            .0
            .iter()
            .map(|row| row.size.raw())
            .sum::<i64>();
        assert_eq!(ordinary, 96_755);
        assert_eq!(
            build_single_page_sheet_scene(unverified_sheet, 0, &fallback_options)
                .unwrap()
                .scene
                .height,
            Fixed::from_raw(96_640)
        );

        let verified = verified(sheet_format);
        let verified_sheet = &verified.sheets[0];
        let ordinary = measure_sheet_axes(verified_sheet, range, &verified_options)
            .unwrap()
            .0
            .iter()
            .map(|row| row.size.raw())
            .sum::<i64>();
        assert_eq!(ordinary, 94_210);
        assert_eq!(
            build_single_page_sheet_scene(verified_sheet, 0, &verified_options)
                .unwrap()
                .scene
                .height,
            Fixed::from_raw(94_240)
        );
    }
}

#[test]
fn verified_wrapped_implicit_xlsx_quantizes_height_and_shares_wrap_with_painting() {
    const TEXT: &str = "aa aa aa aa aa aa";

    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let workbook = |row_attributes: &str| {
        imported_xlsx(
            &styles,
            &format!(
                r#"<worksheet><cols><col min="1" max="1" width="4" customWidth="1"/></cols><sheetData><row r="1"{row_attributes}><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c></row></sheetData></worksheet>"#
            ),
        )
    };
    let range = RenderRange::new(0, 0, 0, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family.clone(),
        font_pack: Some(pack),
        ..RenderOptions::default()
    };

    let automatic = workbook("");
    let sheet = &automatic.sheets[0];
    let style = sheet.resolved_cell_style(0, 0).expect("wrapped style");
    assert_eq!(sheet.verified_xlsx_cell_font_size_pt(0, 0), Some(11));
    assert_eq!(
        cell_line_layout_policy(
            sheet,
            CellCoordinate { row: 0, col: 0 },
            Some(&style),
            None,
            CalcLineLayoutEvidence {
                is_plain_text: true,
                has_adjustable_row: true,
                wrap_space_available: true,
            },
            &options,
        ),
        CellLineLayoutPolicy::CalcEditEngine
    );
    assert_eq!(
        cell_line_layout_policy(
            sheet,
            CellCoordinate { row: 0, col: 0 },
            Some(&style),
            None,
            CalcLineLayoutEvidence {
                is_plain_text: true,
                has_adjustable_row: true,
                wrap_space_available: false,
            },
            &options,
        ),
        CellLineLayoutPolicy::Native
    );
    let mut indented_style = style.clone();
    indented_style.align.as_mut().unwrap().indent = 1;
    assert_eq!(
        cell_line_layout_policy(
            sheet,
            CellCoordinate { row: 0, col: 0 },
            Some(&indented_style),
            None,
            CalcLineLayoutEvidence {
                is_plain_text: true,
                has_adjustable_row: true,
                wrap_space_available: true,
            },
            &options,
        ),
        CellLineLayoutPolicy::Native
    );

    let (rows, columns) = measure_sheet_axes(sheet, range, &options).unwrap();
    let mut digit_warnings = Warnings::default();
    let mut digit_statistics = TypographyStats::default();
    let maximum_digit_width = maximum_digit_width(
        &RenderStyleSnapshot::new(sheet),
        &options,
        &mut digit_warnings,
        &mut digit_statistics,
    )
    .unwrap();
    let region = Region {
        source: CellCoordinate { row: 0, col: 0 },
        rect: Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: columns[0].size,
            height: rows[0].size,
        },
        is_merged: false,
        line_layout_policy: CellLineLayoutPolicy::CalcEditEngine,
        line_placement_policy: CalcLinePlacementPolicy::RequestedFace,
        calc_wrap_space: calc_ooxml_wrap_space(sheet, [0], maximum_digit_width).unwrap(),
        style: Some(style.clone()),
        conditional: ConditionalPaint::default(),
        text: TEXT.to_string(),
        rich_text: None,
        hyperlink: None,
        numeric_default: false,
        text_can_overflow: false,
        fixed_height_row: false,
        ods_fixed_height_row: false,
        print_vertical_overflow: false,
        vertical_margin: CALC_CELL_VERTICAL_MARGIN,
    };
    let text_style = text_style(&region, &options);
    let mut statistics = TypographyStats::default();
    let prepared = prepare_styled_text(
        options.font_pack.as_ref().unwrap(),
        &region,
        &text_style,
        false,
        true,
        &options,
        &mut statistics,
    )
    .unwrap();
    assert!(prepared.lines.len() >= 2);
    let line_heights = prepared
        .lines
        .iter()
        .map(|line| line_height_from_metrics(line.metrics, CalcLinePlacementPolicy::RequestedFace))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let expected_height = calc_automatic_cell_height(&prepared.lines).unwrap();
    assert_eq!(rows[0].size, expected_height);
    assert_eq!(
        prepared.available_width,
        inner_width(region.rect.width, prepared.horizontal_padding).unwrap()
    );

    let automatic_scene = build_scene(&automatic, 0, &options).unwrap();
    let automatic_run = glyph_run(&automatic_scene.scene, TEXT);
    let automatic_baselines = automatic_run
        .cluster_metrics
        .iter()
        .map(|metric| metric.baseline_y.raw())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(automatic_baselines.len(), prepared.lines.len());
    for (window, expected) in automatic_baselines.windows(2).zip(line_heights) {
        assert_eq!(window[1] - window[0], expected.raw());
    }
    let mut painted_partitions = BTreeMap::<i64, (u64, u64)>::new();
    for (cluster, metrics) in automatic_run
        .clusters
        .iter()
        .zip(&automatic_run.cluster_metrics)
    {
        let partition = painted_partitions
            .entry(metrics.baseline_y.raw())
            .or_insert((cluster.source_start, cluster.source_end));
        partition.0 = partition.0.min(cluster.source_start);
        partition.1 = partition.1.max(cluster.source_end);
    }
    assert_eq!(
        painted_partitions.into_values().collect::<Vec<_>>(),
        prepared
            .lines
            .iter()
            .map(|line| {
                (
                    u64::try_from(line.source.start).unwrap(),
                    u64::try_from(line.source.end).unwrap(),
                )
            })
            .collect::<Vec<_>>()
    );

    let merged = imported_xlsx(
        &styles,
        &format!(
            r#"<worksheet><cols><col min="1" max="2" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#
        ),
    );
    let painted_source_partitions = |selection: RenderRange| {
        let scene = build_scene(
            &merged,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(selection),
                ..options.clone()
            },
        )
        .unwrap();
        let mut partitions = BTreeMap::<i64, (u64, u64)>::new();
        for (cluster, metrics) in glyph_run(&scene.scene, TEXT)
            .clusters
            .iter()
            .zip(&glyph_run(&scene.scene, TEXT).cluster_metrics)
        {
            let partition = partitions
                .entry(metrics.baseline_y.raw())
                .or_insert((cluster.source_start, cluster.source_end));
            partition.0 = partition.0.min(cluster.source_start);
            partition.1 = partition.1.max(cluster.source_end);
        }
        partitions.into_values().collect::<Vec<_>>()
    };
    assert_eq!(
        painted_source_partitions(RenderRange::new(0, 0, 0, 0)),
        painted_source_partitions(RenderRange::new(0, 0, 0, 1)),
        "selection clipping must not change a merged cell's source paper"
    );

    let explicit = workbook(r#" ht="42" customHeight="1""#);
    let explicit_sheet = &explicit.sheets[0];
    let explicit_style = explicit_sheet
        .resolved_cell_style(0, 0)
        .expect("explicit wrapped style");
    assert_eq!(
        cell_line_layout_policy(
            explicit_sheet,
            CellCoordinate { row: 0, col: 0 },
            Some(&explicit_style),
            None,
            CalcLineLayoutEvidence {
                is_plain_text: true,
                has_adjustable_row: false,
                wrap_space_available: true,
            },
            &options,
        ),
        CellLineLayoutPolicy::Native
    );
    assert_eq!(
        calc_line_placement_policy(
            explicit_sheet,
            CellCoordinate { row: 0, col: 0 },
            Some(&explicit_style),
            None,
            true,
            &options,
        ),
        CalcLinePlacementPolicy::Imported(CalcImportProvenance::Xlsx),
        "a fixed row keeps native wrapping but still replays Calc's imported role placement"
    );
    let (explicit_rows, _) = measure_sheet_axes(explicit_sheet, range, &options).unwrap();
    assert_eq!(explicit_rows[0].size, points_to_fixed(42.0).unwrap());
    let explicit_scene = build_scene(&explicit, 0, &options).unwrap();
    let explicit_baselines = glyph_run(&explicit_scene.scene, TEXT)
        .cluster_metrics
        .iter()
        .map(|metric| metric.baseline_y.raw())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let imported_step = line_height_from_metrics(
        prepared.lines[0].metrics,
        CalcLinePlacementPolicy::Imported(CalcImportProvenance::Xlsx),
    )
    .unwrap();
    assert!(explicit_baselines.len() >= 2);
    assert_eq!(
        explicit_baselines[1] - explicit_baselines[0],
        imported_step.raw()
    );

    let huge_width = imported_xlsx(
        &styles,
        &format!(
            r#"<worksheet><cols><col min="1" max="1" width="10000000000000000" customWidth="1"/></cols><sheetData><row r="1" ht="42" customHeight="1"><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c></row></sheetData></worksheet>"#
        ),
    );
    let huge_width_scene = build_scene(&huge_width, 0, &options).unwrap();
    assert_eq!(
        huge_width_scene.scene.height,
        points_to_fixed(42.0).unwrap()
    );

    assert_eq!(
        cell_line_layout_policy(
            sheet,
            CellCoordinate { row: 0, col: 0 },
            Some(&style),
            None,
            CalcLineLayoutEvidence {
                is_plain_text: false,
                has_adjustable_row: true,
                wrap_space_available: true,
            },
            &options,
        ),
        CellLineLayoutPolicy::Native,
        "numeric and other non-text cells cannot enter Calc's text wrapper"
    );
    let filtered = imported_xlsx(
        &styles,
        &format!(
            r#"<worksheet><cols><col min="1" max="1" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c></row></sheetData><autoFilter ref="A1:A10"/></worksheet>"#
        ),
    );
    let filtered_sheet = &filtered.sheets[0];
    let filtered_style = filtered_sheet
        .resolved_cell_style(0, 0)
        .expect("filtered wrapped style");
    assert_eq!(
        cell_line_layout_policy(
            filtered_sheet,
            CellCoordinate { row: 0, col: 0 },
            Some(&filtered_style),
            None,
            CalcLineLayoutEvidence {
                is_plain_text: true,
                has_adjustable_row: true,
                wrap_space_available: true,
            },
            &options,
        ),
        CellLineLayoutPolicy::Native,
        "Calc reserves the filter-button width in header cells"
    );
    build_scene(&filtered, 0, &options).unwrap();

    let conditional_styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><alignment textRotation="30"/></dxf></dxfs></styleSheet>"#
    );
    let conditional = imported_xlsx(
        &conditional_styles,
        &format!(
            r#"<worksheet><cols><col min="1" max="1" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="expression" dxfId="0" priority="1"><formula>1=1</formula></cfRule></conditionalFormatting></worksheet>"#
        ),
    );
    let conditional_sheet = &conditional.sheets[0];
    assert!(has_conditional_text_layout_overlay(conditional_sheet));
    let conditional_style = conditional_sheet
        .resolved_cell_style(0, 0)
        .expect("conditionally overlaid wrapped style");
    let calc_line_layout_available = verified_implicit_ooxml(conditional_sheet, &options)
        && !has_conditional_text_layout_overlay(conditional_sheet);
    assert_eq!(
        cell_line_layout_policy(
            conditional_sheet,
            CellCoordinate { row: 0, col: 0 },
            Some(&conditional_style),
            None,
            CalcLineLayoutEvidence {
                is_plain_text: true,
                has_adjustable_row: true,
                wrap_space_available: calc_line_layout_available,
            },
            &options,
        ),
        CellLineLayoutPolicy::Native
    );
    build_scene(&conditional, 0, &options).unwrap();
}

#[test]
fn geometry_conditional_outside_selection_keeps_measurement_and_painting_on_native_wrap() {
    const TEXT: &str = "aa aa aa aa aa aa";

    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><font><sz val="24"/></font></dxf></dxfs></styleSheet>"#
    );
    let workbook = imported_xlsx(
        &styles,
        &format!(
            r#"<worksheet><cols><col min="1" max="1" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c><c r="B1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="B1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting></worksheet>"#
        ),
    );
    let sheet = &workbook.sheets[0];
    let range = RenderRange::new(0, 0, 0, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    assert!(has_conditional_text_layout_overlay(sheet));
    assert!(!calc_line_layout_available(sheet, &options));

    let (rows, columns) = measure_sheet_axes(sheet, range, &options).unwrap();
    let style = sheet.resolved_cell_style(0, 0).expect("wrapped style");
    let mut digit_warnings = Warnings::default();
    let mut digit_statistics = TypographyStats::default();
    let maximum_digit_width = maximum_digit_width(
        &RenderStyleSnapshot::new(sheet),
        &options,
        &mut digit_warnings,
        &mut digit_statistics,
    )
    .unwrap();
    let calc_wrap_space = calc_ooxml_wrap_space(sheet, [0], maximum_digit_width)
        .unwrap()
        .expect("verified Calc wrap space");
    let native_region = Region {
        source: CellCoordinate { row: 0, col: 0 },
        rect: Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: columns[0].size,
            height: rows[0].size,
        },
        is_merged: false,
        line_layout_policy: CellLineLayoutPolicy::Native,
        line_placement_policy: CalcLinePlacementPolicy::Native,
        calc_wrap_space: None,
        style: Some(style),
        conditional: ConditionalPaint::default(),
        text: TEXT.to_string(),
        rich_text: None,
        hyperlink: None,
        numeric_default: false,
        text_can_overflow: false,
        fixed_height_row: false,
        ods_fixed_height_row: false,
        print_vertical_overflow: false,
        vertical_margin: CALC_CELL_VERTICAL_MARGIN,
    };
    let mut native_statistics = TypographyStats::default();
    let native_height = measure_automatic_cell_height(
        options.font_pack.as_ref().unwrap(),
        &native_region,
        false,
        &options,
        &mut native_statistics,
        None,
        None,
    )
    .unwrap();
    let mut calc_region = native_region.clone();
    calc_region.line_layout_policy = CellLineLayoutPolicy::CalcEditEngine;
    calc_region.line_placement_policy = CalcLinePlacementPolicy::RequestedFace;
    calc_region.calc_wrap_space = Some(calc_wrap_space);
    let mut calc_statistics = TypographyStats::default();
    let calc_height = measure_automatic_cell_height(
        options.font_pack.as_ref().unwrap(),
        &calc_region,
        false,
        &options,
        &mut calc_statistics,
        None,
        None,
    )
    .unwrap();
    assert_ne!(
        native_height, calc_height,
        "the fixture must distinguish the native and Calc measurement paths"
    );
    assert_eq!(
        rows[0].size, native_height,
        "automatic-row measurement must use the same conservative native wrapper as painting"
    );

    let native_style = text_style(&native_region, &options);
    let mut prepared_statistics = TypographyStats::default();
    let prepared = prepare_styled_text(
        options.font_pack.as_ref().unwrap(),
        &native_region,
        &native_style,
        false,
        true,
        &options,
        &mut prepared_statistics,
    )
    .unwrap();
    let scene = build_scene(&workbook, 0, &options).unwrap();
    let baselines = glyph_run(&scene.scene, TEXT)
        .cluster_metrics
        .iter()
        .map(|metric| metric.baseline_y.raw())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(baselines.len(), prepared.lines.len());
    for (window, line) in baselines.windows(2).zip(&prepared.lines) {
        assert_eq!(
            window[1] - window[0],
            line_height_from_metrics(line.metrics, CalcLinePlacementPolicy::Native)
                .unwrap()
                .raw()
        );
    }
}

#[test]
fn verified_ooxml_normal_font_drives_calc_implicit_row_twips() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let imported = |family: &str, points: u16, worksheet: &str| {
        imported_xlsx(
            &format!(
                r#"<styleSheet><fonts count="1"><font><sz val="{points}"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
            ),
            worksheet,
        )
    };
    let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>plain</t></is></c></row></sheetData></worksheet>"#;
    let explicit_default_column_worksheet = r#"<worksheet><sheetFormatPr defaultColWidth="8.5"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>plain</t></is></c></row></sheetData></worksheet>"#;
    let range = RenderRange::new(0, 0, 0, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family.clone(),
        font_pack: Some(pack.clone()),
        ..RenderOptions::default()
    };

    for (points, expected_twips, expected_raw) in [(11, 276_i64, 18_842_i64), (12, 300, 20_480)] {
        for source in [worksheet, explicit_default_column_worksheet] {
            let workbook = imported(&family, points, source);
            let sheet = &workbook.sheets[0];
            assert!(sheet.has_implicit_ooxml_row_height());
            assert_eq!(sheet.verified_xlsx_normal_font_size_pt(), Some(points));
            assert_eq!(
                calc_ooxml_implicit_row_height(sheet, &options),
                Some(Fixed::from_raw(expected_raw))
            );
            let (rows, _) = measure_sheet_axes(sheet, range, &options).unwrap();
            assert_eq!(rows[0].size, Fixed::from_raw(expected_raw));
            assert_eq!(
                rows[0].size.raw(),
                (expected_twips * FIXED_UNITS_PER_PIXEL
                    + i64::try_from(TWIPS_PER_CSS_PIXEL / 2).unwrap())
                    / i64::try_from(TWIPS_PER_CSS_PIXEL).unwrap()
            );
        }
    }

    for source_size in ["0", "-1", "11.5", "409.55", "410", "1e309"] {
        let workbook = imported_xlsx(
            &format!(
                r#"<styleSheet><fonts count="1"><font><sz val="{source_size}"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
            ),
            worksheet,
        );
        assert_eq!(
            workbook.sheets[0].verified_xlsx_normal_font_size_pt(),
            None,
            "{source_size}"
        );
        assert_eq!(
            calc_ooxml_implicit_row_height(&workbook.sheets[0], &options),
            None,
            "{source_size}"
        );
        assert_eq!(
            fallback_row_height(&workbook.sheets[0], &options),
            OOXML_APPLICATION_DEFAULT_ROW_HEIGHT,
            "{source_size}"
        );
    }

    let mismatched_normal = imported_xlsx(
        &format!(
            r#"<styleSheet><fonts count="2"><font><sz val="12"/><name val="{family}"/></font><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="1"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        ),
        worksheet,
    );
    assert_eq!(
        mismatched_normal.sheets[0].verified_xlsx_normal_font_size_pt(),
        None
    );
    assert_eq!(
        calc_ooxml_implicit_row_height(&mismatched_normal.sheets[0], &options),
        None
    );

    let unavailable_style = imported_xlsx(
        &format!(
            r#"<styleSheet><fonts count="1"><font><sz val="12"/><name val="{family}"/><b/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        ),
        worksheet,
    );
    assert_eq!(
        unavailable_style.sheets[0].verified_xlsx_normal_font_size_pt(),
        Some(12)
    );
    assert_eq!(
        verified_ooxml_normal_font_size(&unavailable_style.sheets[0], &options),
        None,
        "an exact family with a substituted style is not verified"
    );

    let substituted = imported("Unavailable Normal", 12, worksheet);
    assert_eq!(
        calc_ooxml_implicit_row_height(&substituted.sheets[0], &options),
        None
    );
    assert_eq!(
        fallback_row_height(&substituted.sheets[0], &options),
        OOXML_APPLICATION_DEFAULT_ROW_HEIGHT
    );

    let mut authored = Workbook::new();
    authored
        .add_sheet("authored")
        .set_default_format(&Format::new().set_font_name(&family).set_font_size(12));
    assert_eq!(
        verified_ooxml_normal_font_size(&authored.sheets[0], &options),
        None,
        "authored and non-OOXML sheets must retain their prior auto-height path"
    );

    let mut xlsb = Workbook::open(include_bytes!(
        "../../../tests/fixtures/xlsb/reader-basic.xlsb"
    ))
    .expect("imported XLSB fixture");
    assert!(xlsb.sheets[0].has_implicit_ooxml_row_height());
    xlsb.sheets[0].set_default_col_width(8.5);
    assert_eq!(
        verified_ooxml_normal_font_size(&xlsb.sheets[0], &options),
        None,
        "mutable XLSB width provenance must not masquerade as XLSX"
    );

    let verified = imported(&family, 12, worksheet);
    let mut mutated_default = verified.clone();
    mutated_default.sheets[0]
        .set_default_format(&Format::new().set_font_name(&family).set_font_size(12));
    assert_eq!(
        mutated_default.sheets[0].verified_xlsx_normal_font_size_pt(),
        None
    );
    assert_eq!(
        calc_ooxml_implicit_row_height(&mutated_default.sheets[0], &options),
        None,
        "authoring a new default format invalidates retained source-font evidence"
    );
    let explicit_default_row = imported(
        &family,
        12,
        r#"<worksheet><sheetFormatPr defaultRowHeight="15"/><sheetData/></worksheet>"#,
    );
    assert_eq!(
        calc_ooxml_implicit_row_height(&explicit_default_row.sheets[0], &options),
        None,
        "an explicit default row height must not be recalibrated"
    );
    let without_pack = RenderOptions {
        font_pack: None,
        ..options
    };
    assert_eq!(
        calc_ooxml_implicit_row_height(&verified.sheets[0], &without_pack),
        None
    );
    assert_eq!(
        measure_sheet_axes(&verified.sheets[0], range, &without_pack)
            .unwrap()
            .0[0]
            .size,
        OOXML_APPLICATION_DEFAULT_ROW_HEIGHT
    );
}

#[test]
fn verified_normal_size_suppresses_only_default_plain_single_line_expansion() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="2"><font><sz val="12"/><name val="{family}"/></font><font><sz val="14"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="3"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0"/><xf fontId="0" xfId="0"><alignment wrapText="1"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let worksheet = r#"<worksheet><sheetFormatPr defaultRowHeight="15" customHeight="0"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>plain Normal</t></is></c></row><row r="2"><c r="A2" s="1" t="inlineStr"><is><t>larger plain</t></is></c></row><row r="3"><c r="A3" s="2" t="inlineStr"><is><t>wrapped</t></is></c></row><row r="4" ht="21" customHeight="1"><c r="A4" s="2" t="inlineStr"><is><t>explicit wrapped</t></is></c></row><row r="5" hidden="1"><c r="A5" s="2" t="inlineStr"><is><t>hidden wrapped</t></is></c></row></sheetData></worksheet>"#;
    let workbook = imported_xlsx(&styles, worksheet);
    let workbook_with_explicit_column = imported_xlsx(
        &styles,
        &worksheet.replace(
            r#"defaultRowHeight="15""#,
            r#"defaultRowHeight="15" defaultColWidth="8.5""#,
        ),
    );
    let range = RenderRange::new(0, 0, 4, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let (rows, _) = measure_sheet_axes(&workbook.sheets[0], range, &options).unwrap();
    let (rows_with_explicit_column, _) =
        measure_sheet_axes(&workbook_with_explicit_column.sheets[0], range, &options).unwrap();

    assert_eq!(rows[0].size, Fixed::from_pixels(20));
    assert!(rows[1].size > Fixed::from_pixels(20));
    assert!(rows[2].size > Fixed::from_pixels(20));
    assert_eq!(rows[3].size, Fixed::from_pixels(28));
    assert!(rows.iter().all(|row| row.index != 4));
    assert_eq!(
        rows_with_explicit_column, rows,
        "column defaults must not change automatic row-font identity"
    );

    let included = RenderOptions {
        include_hidden: true,
        ..options
    };
    let (rows, _) = measure_sheet_axes(&workbook.sheets[0], range, &included).unwrap();
    assert!(rows[4].size > Fixed::from_pixels(20));
}

#[test]
fn implicit_xlsx_plain_rows_use_declared_font_height_across_font_facets() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="5"><font><sz val="11"/><name val="{family}"/></font><font><b/><sz val="11"/><name val="{family}"/></font><font><i/><sz val="11"/><name val="{family}"/></font><font><sz val="11"/><name val="RTL Sans"/></font><font><sz val="14"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="5"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0" applyFont="1"/><xf fontId="2" xfId="0" applyFont="1"/><xf fontId="3" xfId="0" applyFont="1"/><xf fontId="4" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>regular automatic row</t></is></c></row><row r="2"><c r="A2" s="1" t="inlineStr"><is><t>bold automatic row</t></is></c></row><row r="3"><c r="A3" s="2" t="inlineStr"><is><t>italic automatic row</t></is></c></row><row r="4"><c r="A4" s="3" t="inlineStr"><is><t>שלום</t></is></c></row><row r="5"><c r="A5" s="4" t="inlineStr"><is><t>large automatic row</t></is></c></row></sheetData></worksheet>"#;
    let workbook = imported_xlsx(&styles, worksheet);
    let range = RenderRange::new(0, 0, 4, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };

    let (rows, _) = measure_sheet_axes(&workbook.sheets[0], range, &options).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.size).collect::<Vec<_>>(),
        [
            Fixed::from_raw(18_842),
            Fixed::from_raw(18_842),
            Fixed::from_raw(18_842),
            Fixed::from_raw(18_842),
            Fixed::from_raw(23_689),
        ]
    );
    assert_eq!(
        calc_ooxml_row_height_from_points(14),
        Some(Fixed::from_raw(23_689)),
        "14pt must use Calc's 347-twip declared-font row height"
    );
}

#[test]
fn implicit_xlsx_declared_height_requires_exact_cell_font_provenance() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let range = RenderRange::new(0, 0, 0, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family.clone(),
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let measure = |font_record: &str, cell_xf: &str| {
        let styles = format!(
            r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="{family}"/></font><font>{font_record}<name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/>{cell_xf}</cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let workbook = imported_xlsx(
            &styles,
            r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>plain automatic row</t></is></c></row></sheetData></worksheet>"#,
        );
        let sheet = &workbook.sheets[0];
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap();
        (
            sheet.verified_xlsx_cell_font_size_pt(0, 0),
            sheet
                .resolved_cell_style(0, 0)
                .and_then(|style| style.font)
                .and_then(|font| font.size_pt),
            measured,
        )
    };

    let (provenance, rounded_points, exact) =
        measure(r#"<sz val="14"/>"#, r#"<xf fontId="1" xfId="0"/>"#);
    assert_eq!(provenance, Some(14));
    assert_eq!(rounded_points, Some(14));
    assert_eq!(exact.rows[0].size, Fixed::from_raw(23_689));
    assert_eq!(exact.typography.shaped_runs, 0);

    for (label, font_record, cell_xf, expected_rounded) in [
        (
            "fractional",
            r#"<sz val="13.5"/>"#,
            r#"<xf fontId="1" xfId="0"/>"#,
            Some(14),
        ),
        (
            "duplicate size",
            r#"<sz val="13.5"/><sz val="14"/>"#,
            r#"<xf fontId="1" xfId="0"/>"#,
            Some(14),
        ),
        (
            "malformed size",
            r#"<sz val="malformed"/>"#,
            r#"<xf fontId="1" xfId="0"/>"#,
            None,
        ),
        (
            "ambiguous cell XF",
            r#"<sz val="14"/>"#,
            r#"<xf fontId="1" fontId="0" xfId="0"/>"#,
            Some(14),
        ),
    ] {
        let (provenance, rounded_points, measured) = measure(font_record, cell_xf);
        assert_eq!(provenance, None, "{label}");
        assert_eq!(rounded_points, expected_rounded, "{label}");
        assert!(
            measured.typography.shaped_runs > 0,
            "{label} must take the bounded shaped-height path"
        );
    }
}

#[test]
fn inherited_fractional_row_font_cannot_reuse_exact_cell_xf_provenance() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="{family}"/></font><font><sz val="10.5"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let workbook = imported_xlsx(
        &styles,
        r#"<worksheet><sheetData><row r="1" s="1" customFormat="1"><c r="A1" t="inlineStr"><is><t>plain automatic row</t></is></c></row></sheetData></worksheet>"#,
    );
    let sheet = &workbook.sheets[0];
    assert_eq!(
        sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.font)
            .and_then(|font| font.size_pt),
        Some(11),
        "the public model deliberately rounds the inherited 10.5pt source"
    );
    assert_eq!(sheet.verified_xlsx_cell_font_size_pt(0, 0), None);

    let range = RenderRange::new(0, 0, 0, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let mut snapshot = RenderStyleSnapshot::new(sheet);
    snapshot.capture_range(sheet, range, &options).unwrap();
    let measured = measure_sheet_axes_inner(
        sheet,
        range,
        &snapshot,
        &options,
        None,
        &mut Warnings::default(),
    )
    .unwrap();
    assert!(measured.typography.shaped_runs > 0);
}

#[test]
fn later_style_zero_duplicate_clears_direct_font_provenance() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="{family}"/></font><font><sz val="10.5"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="3"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0"/><xf fontId="0" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let workbook = imported_xlsx(
        &styles,
        r#"<worksheet><sheetData><row r="1" s="1" customFormat="1"><c r="A1" s="2" t="inlineStr"><is><t>earlier direct cell</t></is></c><c r="A1" t="inlineStr"><is><t>effective plain cell</t></is></c></row></sheetData></worksheet>"#,
    );
    let sheet = &workbook.sheets[0];
    assert_eq!(
        sheet.display_cells().next().map(|cell| cell.formatted),
        Some("effective plain cell")
    );
    assert_eq!(
        sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.font)
            .and_then(|font| font.size_pt),
        Some(11),
        "the inherited 10.5pt row font collides after public rounding"
    );
    assert_eq!(sheet.verified_xlsx_cell_font_size_pt(0, 0), None);

    let range = RenderRange::new(0, 0, 0, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let mut snapshot = RenderStyleSnapshot::new(sheet);
    snapshot.capture_range(sheet, range, &options).unwrap();
    let measured = measure_sheet_axes_inner(
        sheet,
        range,
        &snapshot,
        &options,
        None,
        &mut Warnings::default(),
    )
    .unwrap();
    assert!(measured.typography.shaped_runs > 0);
}

#[test]
fn implicit_xlsx_mixed_western_asian_normal_row_uses_primary_calc_metrics() {
    const MIXED_TEXT: &str = "한국어 자동 줄바꿈 English 日本語 中文 0123456789 한국어 자동 줄바꿈 English 日本語 中文 0123456789 한국어 자동 줄바꿈 English 日本語 中文 0123456789";

    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="{family}"/></font><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let worksheet = format!(
        r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{MIXED_TEXT}</t></is></c></row></sheetData></worksheet>"#
    );
    let workbook = imported_xlsx(&styles, &worksheet);
    let sheet = &workbook.sheets[0];
    let range = RenderRange::new(0, 0, 0, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let mut snapshot = RenderStyleSnapshot::new(sheet);
    snapshot.capture_range(sheet, range, &options).unwrap();
    let measured = measure_sheet_axes_inner(
        sheet,
        range,
        &snapshot,
        &options,
        None,
        &mut Warnings::default(),
    )
    .unwrap();

    assert!(has_mixed_calc_script_classes(MIXED_TEXT));
    assert!(!has_mixed_calc_script_classes("bold automatic row"));
    assert!(!has_mixed_calc_script_classes("日本語かなカナ"));
    assert!(!has_mixed_calc_script_classes("한국어 中文"));
    assert!(!has_mixed_calc_script_classes("Latin Ελληνικά"));
    assert!(!has_mixed_calc_script_classes("שלום العربية"));
    assert!(has_mixed_calc_script_classes("123 한국어"));
    assert!(has_mixed_calc_script_classes("Latin。"));
    assert!(has_mixed_calc_script_classes("Latin Ａ"));
    assert!(!has_mixed_calc_script_classes(
        " \u{00a0}\u{00b2}\u{00b3}\u{00b9}한국어"
    ));
    assert!(!has_mixed_calc_script_classes(
        "\u{0001}\u{0002}\u{02c7}\u{02ca}\u{02cb}\u{02d9}한국어"
    ));
    assert!(!has_mixed_calc_script_classes("Latin \u{2c80}"));
    assert_eq!(calc_script_class('1'), Some(CalcScriptClass::Western));
    assert_eq!(calc_script_class('１'), Some(CalcScriptClass::Asian));
    assert_eq!(
        calc_script_class('\u{2c80}'),
        Some(CalcScriptClass::Western)
    );
    let semantic_text = "한국어 렌더링 사례 0006";
    let groups = calc_edit_engine_semantic_groups(semantic_text, &[]).unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(
        &semantic_text[groups[0].source_start as usize..groups[0].source_end as usize],
        "한국어 렌더링 사례 "
    );
    assert_eq!(
        &semantic_text[groups[1].source_start as usize..groups[1].source_end as usize],
        "0006"
    );
    assert!(calc_edit_engine_semantic_groups("Latin only 0006", &[])
        .unwrap()
        .is_empty());
    assert_eq!(
        measured.rows[0].size,
        calc_ooxml_row_height_from_points(11).unwrap(),
        "the synthetic primary font has Calc's declared 11pt row height"
    );
    assert_eq!(
        measured.typography.shaped_runs, 1,
        "mixed-script rows must still shape their glyphs before applying primary Calc metrics"
    );
    assert!(
        measured.typography.text_work >= MIXED_TEXT.chars().count() as u64,
        "script calibration and shaping must account for every inspected scalar"
    );
}

#[test]
fn heading_row_mixed_only_across_uniform_cells_keeps_the_pattern_row_height() {
    // Each cell is internally single-script, so Calc keeps every one of
    // them on the pattern font height and the row stays exactly as tall as
    // the same sheet without it. Only a cell that itself mixes script
    // classes (or one re-resolved by a conditional format) leaves the
    // pattern, so a row that is "mixed" merely because separate cells carry
    // separate scripts must not grow.
    //
    // This is a consistency check, not the regression gate: the synthetic
    // pack resolves every script to one face, so shaped and pattern metrics
    // coincide here and the assertion below holds either way. The behaviour
    // is gated for real by the hosted OOXML row-diagnostic ratchet, whose
    // `auto_heading_western_asian`/`auto_heading_western_complex` cohorts
    // measure this against Calc with the pinned multi-face font pack.
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let worksheet = concat!(
        r#"<worksheet><sheetData><row r="1">"#,
        r#"<c r="A1" t="inlineStr"><is><t>Project review</t></is></c>"#,
        r#"<c r="B1" t="inlineStr"><is><t>한국어 검토</t></is></c>"#,
        r#"<c r="C1" t="inlineStr"><is><t>日本語確認</t></is></c>"#,
        r#"<c r="D1" t="inlineStr"><is><t>中文复核</t></is></c>"#,
        r#"</row></sheetData></worksheet>"#
    )
    .to_string();
    let workbook = imported_xlsx(&styles, &worksheet);
    let sheet = &workbook.sheets[0];
    let range = RenderRange::new(0, 0, 0, 3);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let mut snapshot = RenderStyleSnapshot::new(sheet);
    snapshot.capture_range(sheet, range, &options).unwrap();
    let measured = measure_sheet_axes_inner(
        sheet,
        range,
        &snapshot,
        &options,
        None,
        &mut Warnings::default(),
    )
    .unwrap();

    // Every heading cell is internally uniform even though the row is not.
    assert!(!has_mixed_calc_script_classes("Project review"));
    assert!(!has_mixed_calc_script_classes("한국어 검토"));
    assert!(!has_mixed_calc_script_classes("日本語確認"));
    assert!(!has_mixed_calc_script_classes("中文复核"));
    assert_eq!(
        measured.rows[0].size,
        calc_ooxml_row_height_from_points(11).unwrap(),
        "a row mixed only across internally-uniform cells keeps Calc's pattern row height"
    );
}

#[test]
fn implicit_xlsx_unattested_complex_base_falls_back_without_inflation() {
    const MIXED_TEXT: &str = "العربية 0123456789";

    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="{family}"/></font><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let worksheet = format!(
        r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{MIXED_TEXT}</t></is></c></row></sheetData></worksheet>"#
    );
    let workbook = imported_xlsx(&styles, &worksheet);
    let sheet = &workbook.sheets[0];
    let range = RenderRange::new(0, 0, 0, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let mut snapshot = RenderStyleSnapshot::new(sheet);
    snapshot.capture_range(sheet, range, &options).unwrap();
    let measured = measure_sheet_axes_inner(
        sheet,
        range,
        &snapshot,
        &options,
        None,
        &mut Warnings::default(),
    )
    .unwrap();

    assert!(has_mixed_calc_script_classes(MIXED_TEXT));
    assert!(
        measured.typography.shaped_runs > 0,
        "a Western+Complex mix must stay on the glyph-aware height path"
    );
    assert_eq!(
        measured.rows[0].size,
        calc_ooxml_row_height_from_points(11).unwrap(),
        "an unattested synthetic pack must keep the conservative measured fallback"
    );
}

#[test]
fn color_only_conditional_format_preserves_calc_layout_and_sizes_affected_rows() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="3"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyFont="1"/><xf fontId="0" xfId="0" applyFont="1" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><fill><patternFill patternType="solid"><fgColor rgb="FFFFC7CE"/><bgColor indexed="64"/></patternFill></fill><font><color rgb="FFFF0000"/></font></dxf></dxfs></styleSheet>"#
    );
    let conditional_worksheet = r#"<worksheet><cols><col min="2" max="2" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1"><v>123</v></c><c r="B1" s="2" t="inlineStr"><is><t>wrapped text remains on Calc line layout</t></is></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting></worksheet>"#;
    let inactive_worksheet = r#"<worksheet><cols><col min="2" max="2" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1"><v>123</v></c><c r="B1" s="2" t="inlineStr"><is><t>wrapped text remains on Calc line layout</t></is></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>1000</formula></cfRule></conditionalFormatting></worksheet>"#;
    let control_worksheet = r#"<worksheet><cols><col min="2" max="2" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1"><v>123</v></c><c r="B1" s="2" t="inlineStr"><is><t>wrapped text remains on Calc line layout</t></is></c></row></sheetData></worksheet>"#;
    let conditional = imported_xlsx(&styles, conditional_worksheet);
    let inactive = imported_xlsx(&styles, inactive_worksheet);
    let control = imported_xlsx(&styles, control_worksheet);
    let sheet = &conditional.sheets[0];
    assert!(
        !has_conditional_text_layout_overlay(sheet),
        "font color does not change text geometry"
    );
    let wrapped_style = sheet.resolved_cell_style(0, 1).expect("wrapped cell style");
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let candidates = sheet.display_cells().collect::<Vec<_>>();
    let active =
        active_color_only_conditional_cells(sheet, &candidates, &options, &mut Warnings::default())
            .unwrap();
    assert!(active.requires_individual_metrics(CellCoordinate { row: 0, col: 0 }));
    assert!(!active.requires_individual_metrics(CellCoordinate { row: 0, col: 1 }));
    let inactive_candidates = inactive.sheets[0].display_cells().collect::<Vec<_>>();
    assert!(
        !active_color_only_conditional_cells(
            &inactive.sheets[0],
            &inactive_candidates,
            &options,
            &mut Warnings::default(),
        )
        .unwrap()
        .requires_individual_metrics(CellCoordinate { row: 0, col: 0 }),
        "a false color-only rule must not request individual automatic height"
    );
    assert_eq!(
        cell_line_layout_policy(
            sheet,
            CellCoordinate { row: 0, col: 1 },
            Some(&wrapped_style),
            None,
            CalcLineLayoutEvidence {
                is_plain_text: true,
                has_adjustable_row: true,
                wrap_space_available: true,
            },
            &options,
        ),
        CellLineLayoutPolicy::CalcEditEngine,
        "an unrelated color-only rule must not disable Calc wrapping"
    );

    let measure = |workbook: &Workbook| {
        let sheet = &workbook.sheets[0];
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot
            .capture_range(sheet, RenderRange::new(0, 0, 0, 1), &options)
            .unwrap();
        measure_sheet_axes_inner(
            sheet,
            RenderRange::new(0, 0, 0, 1),
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap()
    };
    let conditional_measured = measure(&conditional);
    let inactive_measured = measure(&inactive);
    let control_measured = measure(&control);
    assert!(
        conditional_measured.typography.shaped_runs > control_measured.typography.shaped_runs,
        "the conditionally affected automatic numeric cell must use individual Calc metrics"
    );
    assert_eq!(
        conditional_measured.rows[0].size, control_measured.rows[0].size,
        "color-only paint must not inflate the row in the synthetic primary font"
    );
    assert_eq!(
        inactive_measured.typography.shaped_runs, control_measured.typography.shaped_runs,
        "an inactive color-only rule must preserve the declared-height shortcut"
    );
}

#[test]
fn lossy_conditional_font_cannot_use_the_color_only_exemption() {
    let styles = r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Test Sans"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><font><b val="0"/><color rgb="FFFF0000"/></font></dxf></dxfs></styleSheet>"#;
    let reset_only_styles = r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Test Sans"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><font><b val="0"/></font></dxf></dxfs></styleSheet>"#;
    let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting></worksheet>"#;
    let workbook = imported_xlsx(styles, worksheet);
    let reset_only_workbook = imported_xlsx(reset_only_styles, worksheet);
    let sheet = &workbook.sheets[0];
    let [metadata] = sheet.conditional_format_metadata() else {
        panic!("one conditional metadata row");
    };
    assert!(!metadata.style_losses.is_empty());
    assert!(
        has_conditional_text_layout_overlay(sheet),
        "an ambiguous font reset must conservatively disable Calc text layout"
    );
    let reset_only_sheet = &reset_only_workbook.sheets[0];
    let [reset_only_metadata] = reset_only_sheet.conditional_format_metadata() else {
        panic!("one reset-only conditional metadata row");
    };
    assert!(reset_only_metadata
        .differential_style
        .as_ref()
        .is_some_and(|style| style.font.is_none()));
    assert!(reset_only_metadata
        .style_losses
        .iter()
        .any(|loss| loss.kind == StyleLossKind::UnsupportedProperty));
    assert!(
        has_conditional_text_layout_overlay(reset_only_sheet),
        "an unretained reset-only font must conservatively disable Calc text layout"
    );
}

#[test]
fn conditional_layout_styles_drive_axes_and_unresolved_text_styles_fail_closed() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let style_sheet = |dxf: &str| {
        format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf>{dxf}</dxf></dxfs></styleSheet>"#
        )
    };
    let conditional_sheet = |formula: &str| {
        format!(
            r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>{formula}</formula></cfRule></conditionalFormatting></worksheet>"#
        )
    };
    let control = imported_xlsx(
        &style_sheet("<font><sz val=\"24\"/></font>"),
        r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>1</v></c></row></sheetData></worksheet>"#,
    );
    let active = imported_xlsx(
        &style_sheet("<font><sz val=\"24\"/></font>"),
        &conditional_sheet("0"),
    );
    let inactive = imported_xlsx(
        &style_sheet("<font><sz val=\"24\"/></font>"),
        &conditional_sheet("100"),
    );
    let reset = imported_xlsx(
        &style_sheet("<font><b val=\"0\"/></font>"),
        &conditional_sheet("0"),
    );
    let inactive_reset = imported_xlsx(
        &style_sheet("<font><b val=\"0\"/></font>"),
        &conditional_sheet("100"),
    );
    let unresolved_color = imported_xlsx(
        &style_sheet("<font><color theme=\"99\"/></font>"),
        &conditional_sheet("0"),
    );
    let inactive_unresolved_color = imported_xlsx(
        &style_sheet("<font><color theme=\"99\"/></font>"),
        &conditional_sheet("100"),
    );
    let number_format = imported_xlsx(
        &style_sheet(r#"<numFmt numFmtId="165" formatCode="yyyy-mm-dd hh:mm:ss"/>"#),
        &conditional_sheet("0"),
    );
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let measure = |workbook: &Workbook| {
        let sheet = &workbook.sheets[0];
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot
            .capture_range(sheet, RenderRange::new(0, 0, 0, 0), &options)
            .unwrap();
        measure_sheet_axes_inner(
            sheet,
            RenderRange::new(0, 0, 0, 0),
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
    };
    let control = measure(&control).unwrap();
    let active = measure(&active).unwrap();
    let inactive = measure(&inactive).unwrap();
    assert!(
        active.rows[0].size > control.rows[0].size,
        "an active 24pt differential font must grow the automatic row"
    );
    assert_eq!(
        inactive.rows[0].size, control.rows[0].size,
        "an inactive differential font must not alter row geometry"
    );
    assert_eq!(
        measure(&reset).map(|_| ()),
        Err(RenderError::Typography {
            reason: "conditional_text_layout_unresolved",
        })
    );
    assert_eq!(
        measure(&inactive_reset).unwrap().rows[0].size,
        control.rows[0].size,
        "a proven-inactive reset-only rule must not block measurement"
    );
    assert_eq!(
        measure(&unresolved_color).map(|_| ()),
        Err(RenderError::Typography {
            reason: "conditional_text_layout_unresolved",
        })
    );
    assert_eq!(
        measure(&inactive_unresolved_color).unwrap().rows[0].size,
        control.rows[0].size,
        "a proven-inactive unresolved font color must not block measurement"
    );
    assert_eq!(
        measure(&number_format).map(|_| ()),
        Err(RenderError::Typography {
            reason: "conditional_number_format_layout_unresolved",
        })
    );
}

#[test]
fn conditional_evaluation_budget_is_shared_by_axes_and_paint() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><font><color rgb="FFFF0000"/></font></dxf></dxfs></styleSheet>"#
    );
    let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting></worksheet>"#;
    let workbook = imported_xlsx(&styles, worksheet);
    let mut options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    options.limits.max_conditional_evaluations = 1;
    assert_eq!(
        render_sheet_svg(&workbook, 0, &options).map(|_| ()),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::ConditionalEvaluations,
            limit: 1,
            actual: 2,
        })
    );
    options.limits.max_conditional_evaluations = 2;
    render_sheet_svg(&workbook, 0, &options).unwrap();
}

#[test]
fn prepared_asian_metric_face_ignores_complex_only_fallback_runs() {
    let pack = synthetic_test_pack();
    let options = RenderOptions {
        default_font_family: "Legacy Sans".to_string(),
        font_pack: Some(pack.clone()),
        ..RenderOptions::default()
    };
    let make_region = |text: &str| Region {
        source: CellCoordinate { row: 0, col: 0 },
        rect: Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(400),
            height: Fixed::from_pixels(20),
        },
        is_merged: false,
        line_layout_policy: CellLineLayoutPolicy::Native,
        line_placement_policy: CalcLinePlacementPolicy::Native,
        calc_wrap_space: None,
        style: Some(CellStyle::new().font_name("Legacy Sans").size(11)),
        conditional: ConditionalPaint::default(),
        text: text.to_string(),
        rich_text: None,
        hyperlink: None,
        numeric_default: false,
        text_can_overflow: false,
        fixed_height_row: false,
        ods_fixed_height_row: false,
        print_vertical_overflow: false,
        vertical_margin: CALC_CELL_VERTICAL_MARGIN,
    };
    let mut statistics = TypographyStats::default();
    let region = make_region("Latin 한국어 العربية");
    let base = text_style(&region, &options);
    let prepared = prepare_styled_text(
        &pack,
        &region,
        &base,
        false,
        true,
        &options,
        &mut statistics,
    )
    .unwrap();
    assert_eq!(
        prepared_asian_face(&prepared, &region.text, &options, &mut statistics).unwrap(),
        PreparedAsianFace::Verified(FontId(0)),
        "the actual Asian run selects Wide Sans while the Arabic run's RTL face is ignored"
    );

    let mut statistics = TypographyStats::default();
    let region = make_region("Latin العربية");
    let base = text_style(&region, &options);
    let prepared = prepare_styled_text(
        &pack,
        &region,
        &base,
        false,
        true,
        &options,
        &mut statistics,
    )
    .unwrap();
    assert_eq!(
        prepared_asian_face(&prepared, &region.text, &options, &mut statistics).unwrap(),
        PreparedAsianFace::None
    );
}

#[test]
fn bounded_script_summary_inspects_late_complex_scalars() {
    let mut options = RenderOptions::default();
    options.limits.max_text_runs = 3;
    let mut typography = TypographyStats::default();
    let summary = calc_script_class_summary_bounded("한1ع", &options, &mut typography).unwrap();
    assert!(summary.mixed);
    assert!(summary.has_complex);
    assert!(!calc_edit_engine_uses_only_complex_role("한1ع", summary, &options).unwrap());
    assert_eq!(typography.text_work, 3);

    options.limits.max_text_runs = 2;
    let mut typography = TypographyStats::default();
    assert_eq!(
        calc_script_class_summary_bounded("한1ع", &options, &mut typography).map(|_| ()),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TextRuns,
            limit: 2,
            actual: 3,
        })
    );
}

#[test]
fn calc_complex_role_metric_source_replays_bounded_logical_bidi_runs() {
    let options = RenderOptions::default();
    let analyze = |text: &str| {
        let summary =
            calc_script_class_summary_bounded(text, &options, &mut TypographyStats::default())
                .unwrap();
        let edit_engine_uses_only_complex_role =
            calc_edit_engine_uses_only_complex_role(text, summary, &options).unwrap();
        (
            summary,
            CalcCellScriptAnalysis {
                edit_engine_uses_only_complex_role,
            },
        )
    };
    for text in [
        "مرحبا بالعالم 0009",
        "0009 مرحبا بالعالم",
        "مرحبا!",
        "مرحبا-0009",
        "עברית (123)",
    ] {
        let (summary, analysis) = analyze(text);
        assert!(analysis.edit_engine_uses_only_complex_role, "{text}");
        for source in [
            OoxmlImplicitRowHeight::XlsxApplicationDefault,
            OoxmlImplicitRowHeight::XlsbApplicationDefault,
        ] {
            assert_eq!(
                calc_automatic_metric_source(
                    Some(source),
                    true,
                    true,
                    Some(&summary),
                    Some(&analysis),
                ),
                Some(CalcAutomaticMetricSource::CalcComplexRole),
                "{source:?}: {text}"
            );
        }
    }

    for text in [
        "مرحبا بالعالم",
        "0009",
        "Latin مرحبا 0009",
        "مرحبا 0009 한국어",
        "हिन्दी 0009",
        "مرحبا abc 0009",
        "مرحبا \u{2066}0009\u{2069}",
        "مرحبا \u{2067}0009\u{2069}",
        "مرحبا \u{2068}0009\u{2069}",
        "مرحبا \u{202a}0009\u{202c}",
        "مرحبا \u{202b}0009\u{202c}",
        "مرحبا \u{202d}0009\u{202c}",
        "مرحبا \u{202e}0009\u{202c}",
    ] {
        let (_, analysis) = analyze(text);
        assert!(
            !analysis.edit_engine_uses_only_complex_role,
            "unsupported mixed form must fail closed: {text}"
        );
    }
    let (qualified, qualified_analysis) = analyze("مرحبا 0009");
    assert_eq!(
        calc_automatic_metric_source(
            Some(OoxmlImplicitRowHeight::XlsxApplicationDefault),
            false,
            true,
            Some(&qualified),
            Some(&qualified_analysis),
        ),
        None,
        "the source-specific metric never bypasses normal eligibility"
    );
    assert_eq!(
        calc_automatic_metric_source(
            Some(OoxmlImplicitRowHeight::XlsbApplicationDefault),
            true,
            false,
            Some(&qualified),
            Some(&qualified_analysis),
        ),
        None,
        "an unattested workbook font never selects the pinned CTL metric"
    );

    let text = "مرحبا!";
    let mut limited = RenderOptions::default();
    limited.limits.max_text_bytes = text.len() as u64 - 1;
    let summary =
        calc_script_class_summary_bounded(text, &limited, &mut TypographyStats::default()).unwrap();
    assert_eq!(
        calc_edit_engine_uses_only_complex_role(text, summary, &limited),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TextBytes,
            limit: text.len() as u64 - 1,
            actual: text.len() as u64,
        })
    );
}

#[test]
fn declared_height_classification_and_off_range_candidates_obey_exact_limits() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let workbook = imported_xlsx(
        &styles,
        r#"<worksheet><sheetData><row r="1"><c r="F1" t="inlineStr"><is><t>Latin</t></is></c></row></sheetData></worksheet>"#,
    );
    let sheet = &workbook.sheets[0];
    let range = RenderRange::new(0, 0, 0, 0);
    let mut options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let mut snapshot = RenderStyleSnapshot::new(sheet);
    snapshot.capture_range(sheet, range, &options).unwrap();
    let candidates = sheet.display_cells().collect::<Vec<_>>();

    options.limits.max_cells = 1;
    options.limits.max_text_bytes = 5;
    options.limits.max_text_runs = 5;
    let measured = measure_sheet_axes_inner(
        sheet,
        range,
        &snapshot,
        &options,
        Some(&candidates),
        &mut Warnings::default(),
    )
    .unwrap();
    assert_eq!(measured.typography.text_bytes, 5);
    assert_eq!(measured.typography.text_work, 5);
    assert_eq!(measured.typography.shaped_runs, 0);

    let mut limited = options.clone();
    limited.limits.max_text_runs = 4;
    assert_eq!(
        measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &limited,
            Some(&candidates),
            &mut Warnings::default(),
        )
        .map(|_| ()),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TextRuns,
            limit: 4,
            actual: 5,
        })
    );

    let mut limited = options.clone();
    limited.limits.max_text_bytes = 4;
    assert_eq!(
        measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &limited,
            Some(&candidates),
            &mut Warnings::default(),
        )
        .map(|_| ()),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TextBytes,
            limit: 4,
            actual: 5,
        })
    );

    let mut limited = options;
    limited.limits.max_cells = 0;
    assert_eq!(
        measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &limited,
            Some(&candidates),
            &mut Warnings::default(),
        )
        .map(|_| ()),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Cells,
            limit: 0,
            actual: 1,
        })
    );
}

#[test]
fn skipped_default_plain_candidates_are_charged_before_text_scans() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let mut workbook = Workbook::new();
    workbook.add_sheet("bounded").write(0, 5, "bounded");
    let sheet = &workbook.sheets[0];
    let range = RenderRange::new(0, 0, 0, 0);
    let mut options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let mut snapshot = RenderStyleSnapshot::new(sheet);
    snapshot.capture_range(sheet, range, &options).unwrap();
    let candidates = sheet.display_cells().collect::<Vec<_>>();

    options.limits.max_cells = 1;
    options.limits.max_text_bytes = 7;
    let measured = measure_sheet_axes_inner(
        sheet,
        range,
        &snapshot,
        &options,
        Some(&candidates),
        &mut Warnings::default(),
    )
    .unwrap();
    assert_eq!(measured.typography.text_bytes, 7);
    assert_eq!(measured.typography.text_work, 0);
    assert_eq!(measured.typography.shaped_runs, 0);

    let mut limited = options.clone();
    limited.limits.max_text_bytes = 6;
    assert_eq!(
        measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &limited,
            Some(&candidates),
            &mut Warnings::default(),
        )
        .map(|_| ()),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TextBytes,
            limit: 6,
            actual: 7,
        })
    );

    let mut limited = options;
    limited.limits.max_cells = 0;
    assert_eq!(
        measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &limited,
            Some(&candidates),
            &mut Warnings::default(),
        )
        .map(|_| ()),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Cells,
            limit: 0,
            actual: 1,
        })
    );
}

#[test]
fn implicit_xlsx_rotation_shapes_but_shrink_uses_declared_auto_height() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="3"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment textRotation="30"/></xf><xf fontId="0" xfId="0" applyAlignment="1"><alignment shrinkToFit="1"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>rotated automatic row</t></is></c></row><row r="2"><c r="A2" s="2" t="inlineStr"><is><t>shrunk automatic row</t></is></c></row></sheetData></worksheet>"#;
    let workbook = imported_xlsx(&styles, worksheet);
    let sheet = &workbook.sheets[0];
    let range = RenderRange::new(0, 0, 1, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let mut snapshot = RenderStyleSnapshot::new(sheet);
    snapshot.capture_range(sheet, range, &options).unwrap();
    let measured = measure_sheet_axes_inner(
        sheet,
        range,
        &snapshot,
        &options,
        None,
        &mut Warnings::default(),
    )
    .unwrap();

    assert!(
        measured.rows[0].size > Fixed::from_raw(18_842),
        "rotation must remain on the shaped height path"
    );
    assert_eq!(
        measured.rows[1].size,
        Fixed::from_raw(18_842),
        "Calc's standard-height path does not exclude shrink-to-fit"
    );
    assert!(measured.typography.shaped_runs >= 1);
}

#[test]
fn implicit_xlsx_superscript_and_subscript_keep_measured_auto_height() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="3"><font><sz val="11"/><name val="{family}"/></font><font><vertAlign val="superscript"/><sz val="11"/><name val="{family}"/></font><font><vertAlign val="subscript"/><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="3"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0" applyFont="1"/><xf fontId="2" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>superscript automatic row</t></is></c></row><row r="2"><c r="A2" s="2" t="inlineStr"><is><t>subscript automatic row</t></is></c></row></sheetData></worksheet>"#;
    let workbook = imported_xlsx(&styles, worksheet);
    let sheet = &workbook.sheets[0];
    assert_eq!(
        sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.font)
            .map(|font| font.script),
        Some(FormatScript::Superscript)
    );
    assert_eq!(
        sheet
            .resolved_cell_style(1, 0)
            .and_then(|style| style.font)
            .map(|font| font.script),
        Some(FormatScript::Subscript)
    );

    let range = RenderRange::new(0, 0, 1, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let mut snapshot = RenderStyleSnapshot::new(sheet);
    snapshot.capture_range(sheet, range, &options).unwrap();
    let measured = measure_sheet_axes_inner(
        sheet,
        range,
        &snapshot,
        &options,
        None,
        &mut Warnings::default(),
    )
    .unwrap();

    assert!(
        measured.typography.shaped_runs >= 2,
        "superscript and subscript must not take the declared-font shortcut"
    );
}

#[test]
fn verified_normal_auto_height_skip_requires_the_same_effective_font() {
    let pack = synthetic_test_pack();
    let styles = r#"<styleSheet><fonts count="2"><font><sz val="12"/><name val="Wide Sans"/></font><font><sz val="12"/><name val="RTL Sans"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#;
    let workbook = |style_index: u8| {
        imported_xlsx(
            styles,
            &format!(
                r#"<worksheet><sheetFormatPr defaultRowHeight="15"/><sheetData><row r="1"><c r="A1" s="{style_index}" t="inlineStr"><is><t>plain</t></is></c></row></sheetData></worksheet>"#
            ),
        )
    };
    let range = RenderRange::new(0, 0, 0, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: "Wide Sans".to_string(),
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let measure = |workbook: &Workbook| {
        let sheet = &workbook.sheets[0];
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap()
    };

    let default_font = measure(&workbook(0));
    let alternate_font = measure(&workbook(1));
    assert!(
        alternate_font.typography.shaped_runs > default_font.typography.shaped_runs,
        "a same-size alternate family must still be measured for automatic height"
    );
    assert_eq!(default_font.rows[0].size, Fixed::from_pixels(20));
    assert!(alternate_font.rows[0].size >= Fixed::from_pixels(20));
}

#[test]
fn verified_ooxml_implicit_rows_keep_sparse_origin_and_explicit_hidden_geometry() {
    let pack = synthetic_test_pack();
    let family = pack.default_family().to_string();
    let styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="4" ht="21" customHeight="1"/><row r="5" hidden="1"/><row r="8"><c r="A8"><v>8</v></c></row></sheetData></worksheet>"#;
    let workbook = imported_xlsx(&styles, worksheet);
    let sheet = &workbook.sheets[0];
    let range = RenderRange::new(0, 0, 7, 0);
    let options = RenderOptions {
        selection: RenderSelection::Range(range),
        gridlines: false,
        default_font_family: family,
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let (rows, _) = measure_sheet_axes(sheet, range, &options).unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| (row.index, row.size))
            .collect::<Vec<_>>(),
        [
            (0, Fixed::from_raw(18_842)),
            (1, Fixed::from_raw(18_842)),
            (2, Fixed::from_raw(18_842)),
            (3, Fixed::from_pixels(28)),
            (5, Fixed::from_raw(18_842)),
            (6, Fixed::from_raw(18_842)),
            (7, Fixed::from_raw(18_842)),
        ]
    );
    let mut warnings = Warnings::default();
    assert_eq!(
        sheet_grid_origin(
            sheet,
            RenderRange::new(7, 0, 7, 0),
            Fixed::from_pixels(7),
            &options,
            &mut warnings,
        )
        .unwrap()
        .1,
        Fixed::from_raw(18_842 * 5 + 28 * FIXED_UNITS_PER_PIXEL)
    );
}

#[test]
fn biff_application_default_row_height_is_calc_specific_not_a_global_fallback() {
    let candidates = [
        include_bytes!("../../../tests/fixtures/xls/reader-basic.xls").as_slice(),
        include_bytes!("../../../tests/fixtures/xls/korean-unicode-biff8.xls").as_slice(),
        include_bytes!("../../../tests/fixtures/xls/korean-cp949-biff5.xls").as_slice(),
    ];
    let implicit = candidates
        .into_iter()
        .map(|bytes| Workbook::open(bytes).expect("imported BIFF fixture"))
        .find(|workbook| {
            workbook
                .sheets
                .first()
                .is_some_and(|sheet| sheet.biff_uses_application_default_row_height())
        })
        .expect("at least one BIFF fixture without DEFAULTROWHEIGHT");
    let mut overridden = implicit.clone();
    overridden.sheets[0].set_default_row_height(12.0);

    assert!(implicit.sheets[0].biff_uses_application_default_row_height());
    assert!(!overridden.sheets[0].biff_uses_application_default_row_height());

    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        gridlines: false,
        default_row_height: Fixed::from_pixels(37),
        ..RenderOptions::default()
    };
    let measured = |workbook: &Workbook| {
        measure_sheet_axes(&workbook.sheets[0], RenderRange::new(0, 0, 0, 0), &options)
            .unwrap()
            .0[0]
            .size
    };

    assert_eq!(measured(&implicit), BIFF_APPLICATION_DEFAULT_ROW_HEIGHT);
    assert_eq!(measured(&overridden), Fixed::from_pixels(16));
}

#[test]
fn biff_default_funsynced_controls_multiline_auto_height() {
    let automatic = imported_biff8_default_row(false, "first line\nsecond line");
    let manual = imported_biff8_default_row(true, "first line\nsecond line");
    assert!(!automatic.sheets[0].default_row_height_is_manual());
    assert!(manual.sheets[0].default_row_height_is_manual());

    let pack = synthetic_test_pack();
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        gridlines: false,
        default_font_family: pack.default_family().to_string(),
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let measured = |workbook: &Workbook| {
        measure_sheet_axes(&workbook.sheets[0], RenderRange::new(0, 0, 0, 0), &options)
            .unwrap()
            .0[0]
            .size
    };

    assert!(
        measured(&automatic) > Fixed::from_pixels(20),
        "automatic BIFF default must allow multiline expansion"
    );
    assert_eq!(
        measured(&manual),
        Fixed::from_pixels(20),
        "manual BIFF default must retain its exact 300-twip height"
    );
}

#[test]
fn implicit_ooxml_row_height_drives_image_and_chart_anchor_geometry() {
    for kind in [DrawingObjectKind::Image, DrawingObjectKind::Chart] {
        let workbook = imported_two_cell_drawing(kind, (0, 0));
        assert!(workbook.sheets[0].has_implicit_ooxml_row_height());

        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                gridlines: false,
                default_row_height: Fixed::from_pixels(37),
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert_eq!(build.report.range, RenderRange::new(3, 2, 6, 4));
        assert_eq!(
            build.scene.height,
            Fixed::from_raw(OOXML_APPLICATION_DEFAULT_ROW_HEIGHT.raw() * 4),
            "{kind:?}"
        );
        assert!(
            build.scene.nodes.iter().any(|node| matches!(
                node,
                SceneNode::Rect(RectNode {
                    rect: Rect {
                        x: Fixed::ZERO,
                        y: Fixed::ZERO,
                        width,
                        height,
                    },
                    ..
                }) if *width == build.scene.width && *height == build.scene.height
            )),
            "{kind:?} anchor did not retain the exact implicit-row rectangle"
        );
    }
}

#[test]
fn imported_ooxml_drawing_anchors_use_calc_standard_row_tracks() {
    let mut workbook = imported_xlsx("<styleSheet/>", "<worksheet><sheetData/></worksheet>");
    workbook.sheets[0]
        .add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (0, 0)).with_to((4, 4)));
    assert!(workbook.sheets[0].has_implicit_ooxml_row_height());
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let frame = build
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Rect(RectNode {
                rect,
                fill: Some(color),
                ..
            }) if *color == Rgb::new(242, 242, 242) => Some(*rect),
            _ => None,
        })
        .expect("imported image placeholder frame");
    assert_eq!(
        frame.height,
        Fixed::from_raw(CALC_OOXML_DRAWING_DEFAULT_ROW_HEIGHT.raw() * 4)
    );
}

#[test]
fn prepared_print_geometry_uses_calc_rows_for_implicit_ooxml_images_only() {
    let prepared_rect = |workbook: &Workbook, kind: DrawingObjectKind, tile: RenderRange| {
        let sheet = &workbook.sheets[0];
        let geometry_range = RenderRange::new(3, 2, 7, 5);
        let geometry_options = outlined_options(geometry_range);
        let (rows, columns) = measure_sheet_axes(sheet, geometry_range, &geometry_options).unwrap();
        let build = build_sheet_scene_with_geometry_for_print(
            sheet,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(tile),
                ..geometry_options
            },
            SheetGeometryOverride::new(&rows, &columns),
        )
        .unwrap();
        match kind {
            DrawingObjectKind::Image => image_placeholder_rect(&build.scene.nodes),
            DrawingObjectKind::Chart => chart_outer_rect(&build.scene.nodes),
            _ => unreachable!("fixture only creates images or charts"),
        }
        .expect("prepared drawing frame")
    };

    let implicit_image = imported_two_cell_drawing(DrawingObjectKind::Image, (0, 0));
    let implicit_chart = imported_two_cell_drawing(DrawingObjectKind::Chart, (0, 0));
    assert_eq!(
        implicit_image.sheets[0].implicit_ooxml_row_height_source(),
        Some(OoxmlImplicitRowHeight::XlsxApplicationDefault)
    );
    let full_tile = RenderRange::new(3, 2, 6, 4);
    let image = prepared_rect(&implicit_image, DrawingObjectKind::Image, full_tile);
    let chart = prepared_rect(&implicit_chart, DrawingObjectKind::Chart, full_tile);
    assert_eq!(
        image.height,
        Fixed::from_raw(CALC_OOXML_DRAWING_DEFAULT_ROW_HEIGHT.raw() * 4)
    );
    assert_eq!(
        chart.height,
        Fixed::from_raw(OOXML_APPLICATION_DEFAULT_ROW_HEIGHT.raw() * 4)
    );

    let explicit_worksheet = r#"<worksheet><sheetFormatPr defaultRowHeight="15"/><sheetData/><drawing r:id="rIdDrawing"/></worksheet>"#;
    let explicit_image = imported_two_cell_drawing_with_worksheet(
        DrawingObjectKind::Image,
        (0, 0),
        explicit_worksheet,
    );
    let explicit_chart = imported_two_cell_drawing_with_worksheet(
        DrawingObjectKind::Chart,
        (0, 0),
        explicit_worksheet,
    );
    assert_eq!(
        explicit_image.sheets[0].implicit_ooxml_row_height_source(),
        None
    );
    assert_eq!(
        prepared_rect(&explicit_image, DrawingObjectKind::Image, full_tile).height,
        prepared_rect(&explicit_chart, DrawingObjectKind::Chart, full_tile).height,
        "explicit row geometry must remain shared by images and charts"
    );
}

#[test]
fn prepared_print_image_continuation_uses_the_calc_row_tile_offset() {
    let workbook = imported_two_cell_drawing(DrawingObjectKind::Image, (0, 0));
    let sheet = &workbook.sheets[0];
    let geometry_range = RenderRange::new(3, 2, 7, 5);
    let geometry_options = outlined_options(geometry_range);
    let (rows, columns) = measure_sheet_axes(sheet, geometry_range, &geometry_options).unwrap();
    let continuation = build_sheet_scene_with_geometry_for_print(
        sheet,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(5, 2, 6, 4)),
            ..geometry_options
        },
        SheetGeometryOverride::new(&rows, &columns),
    )
    .unwrap();
    let image = image_placeholder_rect(&continuation.scene.nodes)
        .expect("continued image frame on the prepared print tile");
    assert_eq!(
        image.y,
        Fixed::from_raw(-CALC_OOXML_DRAWING_DEFAULT_ROW_HEIGHT.raw() * 2)
    );
    assert_eq!(
        image.height,
        Fixed::from_raw(CALC_OOXML_DRAWING_DEFAULT_ROW_HEIGHT.raw() * 4)
    );
}

#[test]
fn imported_xlsx_chart_anchors_follow_expanded_cell_rows() {
    let mut workbook = imported_two_cell_drawing(DrawingObjectKind::Chart, (0, 0));
    workbook.sheets[0].set_col_width(0, 8.0);
    workbook.sheets[0].write_styled(
        1,
        0,
        "wrapped imported chart anchor row must retain Calc automatic height",
        &CellStyle::new().wrap(),
    );
    assert!(workbook.sheets[0].has_implicit_ooxml_row_height());
    assert_eq!(
        workbook.sheets[0].charts().len(),
        1,
        "imported chart fixture"
    );

    let range = RenderRange::new(0, 0, 8, 6);
    let options = outlined_options(range);
    let (rows, _) = measure_sheet_axes(&workbook.sheets[0], range, &options).unwrap();
    let expanded_row = rows
        .iter()
        .find(|slot| slot.index == 1)
        .expect("wrapped imported cell row");
    assert!(
        expanded_row.size > CALC_OOXML_DRAWING_DEFAULT_ROW_HEIGHT,
        "wrapped imported cell must expand its automatic row"
    );
    let control = imported_two_cell_drawing(DrawingObjectKind::Chart, (0, 0));
    let (control_rows, _) = measure_sheet_axes(&control.sheets[0], range, &options).unwrap();
    let expanded_anchor_offset = rows
        .iter()
        .find(|slot| slot.index == 3)
        .expect("expanded chart anchor row")
        .offset;
    let control_anchor_offset = control_rows
        .iter()
        .find(|slot| slot.index == 3)
        .expect("control chart anchor row")
        .offset;
    let drawing_anchor_delta = calc_drawing_row_slots(&workbook.sheets[0], &rows)
        .iter()
        .find(|slot| slot.index == 3)
        .expect("expanded Calc drawing anchor row")
        .offset
        .raw()
        - calc_drawing_row_slots(&control.sheets[0], &control_rows)
            .iter()
            .find(|slot| slot.index == 3)
            .expect("control Calc drawing anchor row")
            .offset
            .raw();
    let expected_delta = expanded_anchor_offset.raw() - control_anchor_offset.raw();
    assert!(expected_delta > 0);
    assert_ne!(
        expected_delta, drawing_anchor_delta,
        "expanded cell geometry must differ from the fixed drawing track"
    );

    let chart_frame = |workbook: &Workbook| {
        let build = build_scene(workbook, 0, &options).unwrap();
        let frame = build
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Rect(RectNode { rect, stroke, .. })
                    if stroke.is_some() && rect.width > Fixed::from_pixels(100) =>
                {
                    Some(*rect)
                }
                _ => None,
            })
            .expect("imported chart frame");
        frame
    };
    let expanded_frame = chart_frame(&workbook);
    let control_frame = chart_frame(&control);
    assert_eq!(
        expanded_frame.y.raw() - control_frame.y.raw(),
        expected_delta,
        "imported chart anchor must follow the expanded worksheet row track"
    );
}

#[test]
fn ooxml_default_hidden_rows_drive_selection_sparse_origin_and_drawing_geometry() {
    let worksheet = r#"<worksheet><sheetFormatPr defaultRowHeight="15" zeroHeight="1"/><sheetData><row r="2"/><row r="4"/><row r="6"/></sheetData><drawing r:id="rIdDrawing"/></worksheet>"#;
    for kind in [DrawingObjectKind::Image, DrawingObjectKind::Chart] {
        let workbook = imported_two_cell_drawing_with_worksheet(kind, (0, 0), worksheet);
        let sheet = &workbook.sheets[0];
        assert_eq!(
            sheet
                .default_hidden_row_exceptions()
                .expect("zeroHeight provenance")
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [1, 3, 5]
        );

        let mut warnings = Warnings::default();
        let hidden_origin = sheet_grid_origin(
            sheet,
            RenderRange::new(7, 0, 7, 0),
            Fixed::from_pixels(7),
            &RenderOptions::default(),
            &mut warnings,
        )
        .unwrap()
        .1;
        assert_eq!(hidden_origin, Fixed::from_pixels(60), "{kind:?}");

        for (include_hidden, expected_last_row, expected_height, expected_origin) in [
            (false, 5, Fixed::from_pixels(40), Fixed::from_pixels(60)),
            (true, 6, Fixed::from_pixels(80), Fixed::from_pixels(140)),
        ] {
            let options = RenderOptions {
                gridlines: false,
                include_hidden,
                ..RenderOptions::default()
            };
            let build = build_scene(&workbook, 0, &options).unwrap();
            assert_eq!(
                build.report.range,
                RenderRange::new(3, 2, expected_last_row, 4)
            );
            assert_eq!(build.scene.height, expected_height, "{kind:?}");

            let mut origin_warnings = Warnings::default();
            assert_eq!(
                sheet_grid_origin(
                    sheet,
                    RenderRange::new(7, 0, 7, 0),
                    Fixed::from_pixels(7),
                    &options,
                    &mut origin_warnings,
                )
                .unwrap()
                .1,
                expected_origin,
                "{kind:?}"
            );
            assert!(
                build.scene.nodes.iter().any(|node| matches!(
                    node,
                    SceneNode::Rect(RectNode {
                        rect: Rect {
                            x: Fixed::ZERO,
                            y: Fixed::ZERO,
                            width,
                            height,
                        },
                        ..
                    }) if *width == build.scene.width && *height == build.scene.height
                )),
                "{kind:?} anchor did not follow effective row visibility"
            );
        }
    }
}

#[test]
fn worksheet_view_gridlines_use_light_gray_one_pixel_strokes() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("grid").write(0, 0, "A");
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let grid = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(node) if node.color == Rgb::GRIDLINE => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(grid.len(), 4);
    assert!(grid.iter().all(|line| line.width == Fixed::from_pixels(1)));
}

#[test]
fn single_page_print_gridlines_use_calc_hairlines_and_page_frame() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("print-grid");
    sheet.write(0, 0, "A");
    sheet.write(1, 1, "B");

    let build = build_single_page_sheet_scene_for_print(
        sheet,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 1)),
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let print_grid = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(line)
                if line.color == Rgb::BLACK && line.width == Fixed::from_raw(137) =>
            {
                Some(line)
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(print_grid.len(), 2);
    assert!(!build
        .scene
        .nodes
        .iter()
        .any(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE)));
    assert!(print_grid.iter().any(|line| {
        line.x1 == PRINT_GRIDLINE_FRAME_LEFT_INSET
            && line.x2 == PRINT_GRIDLINE_FRAME_LEFT_INSET
            && line.y1 == PRINT_GRIDLINE_FRAME_TOP_INSET
            && line.y2
                == build
                    .scene
                    .height
                    .checked_sub(PRINT_GRIDLINE_FRAME_TRAILING_INSET)
                    .unwrap()
    }));
    assert!(print_grid.iter().any(|line| {
        line.x1 == PRINT_GRIDLINE_FRAME_LEFT_INSET
            && line.x2
                == build
                    .scene
                    .width
                    .checked_sub(PRINT_GRIDLINE_FRAME_TRAILING_INSET)
                    .unwrap()
            && line.y1 == PRINT_GRIDLINE_FRAME_TOP_INSET
            && line.y2 == PRINT_GRIDLINE_FRAME_TOP_INSET
    }));
}

#[test]
fn calc_metafile_grid_remap_preserves_composed_segment_gaps() {
    fn slots<I: Copy>(indexes: &[I], size_pixels: i64) -> Vec<AxisSlot<I>> {
        indexes
            .iter()
            .copied()
            .enumerate()
            .map(|(position, index)| AxisSlot {
                index,
                offset: Fixed::from_pixels(i64::try_from(position).unwrap() * size_pixels),
                size: Fixed::from_pixels(size_pixels),
            })
            .collect()
    }

    let cell_rows = slots(&[0_u32, 1, 2], 10);
    let cell_columns = slots(&[0_u16, 1], 10);
    let grid_rows = slots(&[0_u32, 1, 2], 40);
    let grid_columns = slots(&[0_u16, 1], 40);
    let grid_claim = EdgeClaim {
        kind: EdgeClaimKind::Gridline,
        style: BorderStyle::Thin,
        color: Rgb::BLACK,
        owner: CellCoordinate { row: 0, col: 0 },
        side: CellEdge::Left,
    };
    let explicit_claim = EdgeClaim {
        kind: EdgeClaimKind::Explicit,
        ..grid_claim
    };
    let first = ComposedEdgeKey {
        orientation: ComposedEdgeOrientation::Vertical,
        axis: Fixed::from_pixels(10),
        start: Fixed::ZERO,
        end: Fixed::from_pixels(10),
    };
    let second = ComposedEdgeKey {
        start: Fixed::from_pixels(20),
        end: Fixed::from_pixels(30),
        ..first
    };
    let explicit = ComposedEdgeKey {
        orientation: ComposedEdgeOrientation::Horizontal,
        axis: Fixed::from_pixels(10),
        start: Fixed::ZERO,
        end: Fixed::from_pixels(20),
    };

    let remapped = remap_calc_metafile_grid_edges(
        &[
            (first, grid_claim),
            (second, grid_claim),
            (explicit, explicit_claim),
        ],
        &cell_rows,
        &cell_columns,
        &grid_rows,
        &grid_columns,
    )
    .unwrap();

    assert_eq!(
        remapped[0].0,
        ComposedEdgeKey {
            axis: Fixed::from_pixels(40),
            end: Fixed::from_pixels(40),
            ..first
        }
    );
    assert_eq!(
        remapped[1].0,
        ComposedEdgeKey {
            axis: Fixed::from_pixels(40),
            start: Fixed::from_pixels(80),
            end: Fixed::from_pixels(120),
            ..second
        }
    );
    assert_eq!(remapped[2].0, explicit);
}

#[test]
fn single_page_print_gridlines_precede_chart_and_image_paint() {
    fn drawing_frame(scene: &Scene) -> (usize, Rect, Option<Rgb>, Option<Rgb>) {
        scene
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| match node {
                SceneNode::Rect(node) if node.stroke.is_some() => {
                    Some((index, node.rect, node.fill, node.stroke))
                }
                _ => None,
            })
            .max_by_key(|(_, rect, _, _)| {
                i128::from(rect.width.raw()) * i128::from(rect.height.raw())
            })
            .expect("drawing frame")
    }

    fn print_gridline_overlaps_rect(line: &LineNode, rect: Rect) -> bool {
        let right = rect.x.checked_add(rect.width).unwrap();
        let bottom = rect.y.checked_add(rect.height).unwrap();
        if line.x1 == line.x2 {
            let start = std::cmp::min(line.y1, line.y2);
            let end = std::cmp::max(line.y1, line.y2);
            line.x1 >= rect.x && line.x1 <= right && start < bottom && end > rect.y
        } else if line.y1 == line.y2 {
            let start = std::cmp::min(line.x1, line.x2);
            let end = std::cmp::max(line.x1, line.x2);
            line.y1 >= rect.y && line.y1 <= bottom && start < right && end > rect.x
        } else {
            false
        }
    }

    for kind in [DrawingObjectKind::Chart, DrawingObjectKind::Image] {
        let workbook = imported_two_cell_drawing(kind, (0, 0));
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 9, 6)),
            ..RenderOptions::default()
        };
        let mut without_gridline_options = options.clone();
        without_gridline_options.gridlines = false;
        let without_gridlines = build_single_page_sheet_scene_for_print(
            &workbook.sheets[0],
            0,
            &without_gridline_options,
        )
        .unwrap();
        let with_gridlines =
            build_single_page_sheet_scene_for_print(&workbook.sheets[0], 0, &options).unwrap();
        let frame = drawing_frame(&with_gridlines.scene);
        assert_eq!(
            (frame.1, frame.2, frame.3),
            {
                let without = drawing_frame(&without_gridlines.scene);
                (without.1, without.2, without.3)
            },
            "gridline layout must not mutate drawing paint for {kind:?}"
        );
        let rect = frame.1;
        let print_gridlines = with_gridlines
            .scene
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| match node {
                SceneNode::Line(line)
                    if line.color == Rgb::BLACK && line.width == PRINT_GRIDLINE_WIDTH =>
                {
                    Some((index, line))
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(!print_gridlines.is_empty(), "{kind:?}");
        assert!(
            print_gridlines
                .iter()
                .any(|(_, line)| print_gridline_overlaps_rect(line, rect)),
            "metafile grid must continue through the {kind:?} anchor {rect:?}"
        );
        assert!(
            print_gridlines.iter().all(|(index, _)| *index < frame.0),
            "the {kind:?} paint must be emitted after worksheet hairlines"
        );
    }
}

#[test]
fn print_gridline_modes_keep_view_authored_and_calc_geometry_distinct() {
    fn print_gridlines(scene: &Scene) -> Vec<&LineNode> {
        scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Line(line)
                    if line.color == Rgb::BLACK && line.width == PRINT_GRIDLINE_WIDTH =>
                {
                    Some(line)
                }
                _ => None,
            })
            .collect()
    }

    let mut workbook = Workbook::new();
    workbook.add_sheet("print-grid");
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 17, 5)),
        ..RenderOptions::default()
    };

    let view = build_sheet_scene(&workbook.sheets[0], 0, &options).unwrap();
    let view_gridlines = view
        .scene
        .nodes
        .iter()
        .filter(|node| {
            matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE && line.width == Fixed::from_pixels(1))
        })
        .count();
    assert_eq!(view_gridlines, 26);
    assert!(print_gridlines(&view.scene).is_empty());

    let authored = build_sheet_scene_for_print(&workbook.sheets[0], 0, &options).unwrap();
    assert_eq!(print_gridlines(&authored.scene).len(), 26);
    assert!(!authored
        .scene
        .nodes
        .iter()
        .any(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE)));

    let single_page =
        build_single_page_sheet_scene_for_print(&workbook.sheets[0], 0, &options).unwrap();
    let authored_gridlines = print_gridlines(&authored.scene);
    let single_page_gridlines = print_gridlines(&single_page.scene);
    assert_eq!(single_page_gridlines.len(), 8);
    assert_ne!(
        single_page_gridlines, authored_gridlines,
        "SinglePageSheets must retain the fitted cell axis and Calc page frame"
    );

    workbook.sheets[0].set_right_to_left(true);
    let rtl = build_single_page_sheet_scene_for_print(&workbook.sheets[0], 0, &options).unwrap();
    assert_eq!(
        print_gridlines(&rtl.scene).len(),
        2,
        "Calc's RTL metafile retains only the top and leading frame"
    );
    assert_ne!(print_gridlines(&rtl.scene), authored_gridlines);
}

#[test]
fn print_gridline_precedence_retains_shared_unfilled_edge() {
    fn vertical_line_at(scene: &Scene, x: Fixed, color: Rgb, width: Fixed) -> bool {
        scene.nodes.iter().any(|node| {
            matches!(node, SceneNode::Line(line) if line.x1 == x && line.x2 == x && line.color == color && line.width == width)
        })
    }

    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("fill-boundary");
    sheet.write_with_format(
        0,
        0,
        "filled",
        &Format::new().set_background_color([0xFF, 0xCC, 0x00]),
    );
    sheet.write(0, 1, "plain");
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
        ..RenderOptions::default()
    };

    let view = build_sheet_scene(sheet, 0, &options).unwrap();
    let filled = view
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Rect(rect) if rect.fill.is_some() => Some(rect.rect),
            _ => None,
        })
        .expect("filled cell rectangle");
    let shared_x = filled.x.checked_add(filled.width).unwrap();
    assert!(!vertical_line_at(
        &view.scene,
        shared_x,
        Rgb::GRIDLINE,
        Fixed::from_pixels(1)
    ));

    let print = build_sheet_scene_for_print(sheet, 0, &options).unwrap();
    assert!(vertical_line_at(
        &print.scene,
        shared_x,
        Rgb::BLACK,
        PRINT_GRIDLINE_WIDTH
    ));
}

#[test]
fn shared_grid_edges_are_painted_once_and_coalesced_per_axis() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("grid").write(0, 0, "A");
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 1)),
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let grid = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(node) if node.color == Rgb::GRIDLINE => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        grid.len(),
        6,
        "a 2x2 selection has three coalesced lines on each axis"
    );
    assert_eq!(
        grid.iter()
            .filter(|line| line.x1 == line.x2)
            .map(|line| (line.y2.raw() - line.y1.raw()).abs())
            .collect::<BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(
        grid.iter()
            .filter(|line| line.y1 == line.y2)
            .map(|line| (line.x2.raw() - line.x1.raw()).abs())
            .collect::<BTreeSet<_>>()
            .len(),
        1
    );
}

#[test]
fn two_by_two_grid_passes_at_its_exact_coalesced_scene_node_limit() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("bounded-grid");
    let mut options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 1)),
        ..RenderOptions::default()
    };
    options.limits.max_scene_nodes = 6;
    let build = build_scene(&workbook, 0, &options).unwrap();
    assert_eq!(build.report.scene_nodes, 6);
    assert_eq!(
        build
            .scene
            .nodes
            .iter()
            .filter(|node| matches!(node, SceneNode::Line(_)))
            .count(),
        6
    );

    options.limits.max_scene_nodes = 5;
    assert_eq!(
        build_scene(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::SceneNodes,
            limit: 5,
            actual: 6,
        })
    );
}

#[test]
fn filled_and_conditionally_filled_cells_suppress_gridline_segments() {
    let fill = Color::rgb(10, 20, 30);
    let mut filled = Workbook::new();
    filled.add_sheet("filled").write_styled(
        0,
        0,
        "A",
        &CellStyle {
            fill: Some(fill),
            pattern_fill: Some(rxls::Fill::solid(fill)),
            ..CellStyle::default()
        },
    );
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        ..RenderOptions::default()
    };
    let filled_scene = build_scene(&filled, 0, &options).unwrap();
    assert!(!filled_scene
        .scene
        .nodes
        .iter()
        .any(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE)));

    let mut conditional = Workbook::new();
    let sheet = conditional.add_sheet("conditional");
    sheet.write_number(0, 0, 1);
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 0, 0),
        CfRule::cell_is(DvOp::GreaterThan, "0", None::<&str>, fill),
    ));
    let conditional_scene = build_scene(&conditional, 0, &options).unwrap();
    assert!(!conditional_scene
        .scene
        .nodes
        .iter()
        .any(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE)));
}

#[test]
fn explicit_shared_border_wins_once_over_neighbor_and_gridline() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("borders");
    sheet.write_styled(
        0,
        0,
        "left",
        &CellStyle {
            border: Some(
                Border::new()
                    .with_right(BorderStyle::Thin)
                    .with_right_color(Color::rgb(255, 0, 0)),
            ),
            ..CellStyle::default()
        },
    );
    sheet.write_styled(
        0,
        1,
        "right",
        &CellStyle {
            border: Some(
                Border::new()
                    .with_left(BorderStyle::Thick)
                    .with_left_color(Color::rgb(0, 0, 255)),
            ),
            ..CellStyle::default()
        },
    );
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let explicit = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(line) if line.color != Rgb::GRIDLINE => Some(line),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(explicit.len(), 1);
    assert_eq!(explicit[0].color, Rgb::new(0, 0, 255));
    assert_eq!(explicit[0].width, Fixed::from_pixels(3));
    assert!(!build.scene.nodes.iter().any(|node| {
        matches!(
            node,
            SceneNode::Line(line)
                if line.color == Rgb::GRIDLINE
                    && line.x1 == explicit[0].x1
                    && line.x2 == explicit[0].x2
                    && line.y1 == explicit[0].y1
                    && line.y2 == explicit[0].y2
        )
    }));
}

#[test]
fn gridlines_remain_below_text_while_explicit_borders_remain_above_it() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("edge-layers").write_styled(
        0,
        0,
        "overflowing text",
        &CellStyle {
            border: Some(
                Border::new()
                    .with_right(BorderStyle::Thin)
                    .with_right_color(Color::rgb(255, 0, 0)),
            ),
            ..CellStyle::default()
        },
    );
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
            ..RenderOptions::default()
        },
    )
    .unwrap();

    let text_index = build
        .scene
        .nodes
        .iter()
        .position(|node| matches!(node, SceneNode::Text(_)))
        .unwrap();
    let last_gridline = build
        .scene
        .nodes
        .iter()
        .rposition(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE))
        .unwrap();
    let explicit_border = build
        .scene
        .nodes
        .iter()
        .position(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::new(255, 0, 0)))
        .unwrap();
    assert!(last_gridline < text_index);
    assert!(text_index < explicit_border);
}

#[test]
fn double_border_is_retained_as_two_parallel_single_pixel_lines() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("double").write_styled(
        0,
        0,
        "A",
        &CellStyle {
            border: Some(
                Border::new()
                    .with_top(BorderStyle::Double)
                    .with_top_color(Color::rgb(1, 2, 3)),
            ),
            ..CellStyle::default()
        },
    );
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let lines = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(line) => Some(line),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|line| line.color == Rgb::new(1, 2, 3)
        && line.width == Fixed::from_pixels(1)
        && line.y1 == line.y2));
    assert_eq!(
        (lines[1].y1.raw() - lines[0].y1.raw()).abs(),
        Fixed::from_pixels(2).raw()
    );
    assert!(!build
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::DoubleBorderSimplified));
}

#[test]
fn shared_double_border_geometry_does_not_depend_on_the_authoring_neighbor() {
    fn shared_lines(author_on_left: bool) -> Vec<(i64, i64, i64, i64)> {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("shared-double");
        let (left, right) = if author_on_left {
            (
                CellStyle {
                    border: Some(Border::new().with_right(BorderStyle::Double)),
                    ..CellStyle::default()
                },
                CellStyle::default(),
            )
        } else {
            (
                CellStyle::default(),
                CellStyle {
                    border: Some(Border::new().with_left(BorderStyle::Double)),
                    ..CellStyle::default()
                },
            )
        };
        sheet.write_styled(0, 0, "A", &left);
        sheet.write_styled(0, 1, "B", &right);
        build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap()
        .scene
        .nodes
        .into_iter()
        .filter_map(|node| match node {
            SceneNode::Line(line) => {
                Some((line.x1.raw(), line.y1.raw(), line.x2.raw(), line.y2.raw()))
            }
            _ => None,
        })
        .collect()
    }

    let left_authored = shared_lines(true);
    let right_authored = shared_lines(false);
    assert_eq!(left_authored, right_authored);
    assert_eq!(left_authored.len(), 2);
}

#[test]
fn ods_physical_column_width_precedes_character_projection() {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Physical"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:default-style style:family="table-column"><style:table-column-properties style:column-width="1in"/></style:default-style></office:styles></office:document-styles>"#;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    zip.start_file("mimetype", options).unwrap();
    zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
        .unwrap();
    zip.start_file("content.xml", options).unwrap();
    zip.write_all(content.as_bytes()).unwrap();
    zip.start_file("styles.xml", options).unwrap();
    zip.write_all(styles.as_bytes()).unwrap();
    let bytes = zip.finish().unwrap().into_inner();

    let workbook = Workbook::open(&bytes).expect("ODS workbook");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.physical_column_widths().get(&0), Some(&72.0));
    assert!(sheet.column_widths()[&0] > 13.0);

    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert_eq!(build.scene.width, Fixed::from_pixels(96));
}

#[test]
fn ods_physical_width_drives_exact_automatic_cjk_row_height() {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Auto"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>한글中文</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:default-style style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap"/></style:default-style><style:default-style style:family="table-column"><style:table-column-properties style:column-width="0.1875in"/></style:default-style></office:styles></office:document-styles>"#;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let zip_options = SimpleFileOptions::default();
    zip.start_file("mimetype", zip_options).unwrap();
    zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
        .unwrap();
    zip.start_file("content.xml", zip_options).unwrap();
    zip.write_all(content.as_bytes()).unwrap();
    zip.start_file("styles.xml", zip_options).unwrap();
    zip.write_all(styles.as_bytes()).unwrap();
    let workbook = Workbook::open(&zip.finish().unwrap().into_inner()).expect("ODS workbook");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.physical_column_widths().get(&0), Some(&13.5));

    let range = RenderRange::new(0, 0, 0, 0);
    let options = outlined_options(range);
    let (rows, columns) = measure_sheet_axes(sheet, range, &options).unwrap();
    assert_eq!(columns[0].size, Fixed::from_pixels(18));
    assert_eq!(rows[0].size.raw(), 74_140);
    let build = build_scene(&workbook, 0, &options).unwrap();
    assert_eq!(build.scene.width, Fixed::from_pixels(18));
    assert_eq!(build.scene.height.raw(), 74_140);
}

#[test]
fn outlined_cell_text_is_inset_by_calc_attr_margin_not_the_generic_padding() {
    // `ATTR_MARGIN` is one attribute for all four cell edges, so the
    // horizontal inset must equal the vertical one. The generic 3 px
    // `horizontal_padding` is 2.25 pt per side against Calc's 1 pt, which
    // narrows the wrapping width by 2.5 pt and breaks lines Calc keeps.
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="2in"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Inset"><table:table-column table:style-name="co"/><table:table-row><table:table-cell office:value-type="string"><text:p>inset</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(content, styles);
    let build = build_scene(
        &workbook,
        0,
        &outlined_options(RenderRange::new(0, 0, 0, 0)),
    )
    .unwrap();
    let run = glyph_run(&build.scene, "inset");
    let start = run.cluster_metrics[0].origin_x;
    assert_eq!(
        start.raw() - run.clip_bounds.x.raw(),
        CALC_CELL_VERTICAL_MARGIN.raw(),
        "a left-aligned outlined cell must start one ATTR_MARGIN in, matching the vertical inset"
    );
    assert_ne!(
        CALC_CELL_VERTICAL_MARGIN.raw(),
        Fixed::from_pixels(3).raw(),
        "the Calc margin must not silently equal the generic padding, or this test proves nothing"
    );
}

#[test]
fn mandatory_break_only_cells_keep_legacy_valid_semantic_metadata() {
    for text in ["\n", "\r\n", "\u{0085}", "\u{2028}", "\u{2029}"] {
        let mut workbook = Workbook::new();
        workbook.add_sheet("breaks").write(0, 0, text);
        let build = build_scene(
            &workbook,
            0,
            &outlined_options(RenderRange::new(0, 0, 0, 0)),
        )
        .unwrap();
        let run = glyph_run(&build.scene, text);
        assert!(run.metadata_is_valid(), "mandatory break {text:?}");
        assert_eq!(
            run.semantic_line_records(),
            None,
            "a run without non-empty visual lines must retain the legacy encoding"
        );
    }
}

#[test]
fn draw_strings_semantics_shorten_at_a_blocker_but_not_the_output_edge() {
    let clipped_text = "clipped proportional source 0123456789";
    let terminal_text = "terminal output source 0123456789";
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("semantic-cutoff");
    sheet.write(0, 0, clipped_text);
    sheet.write(0, 1, "blocker");
    sheet.write(1, 1, terminal_text);

    let mut options = outlined_options(RenderRange::new(0, 0, 1, 1));
    options.default_column_width = Fixed::from_pixels(40);
    let build = build_scene(&workbook, 0, &options).unwrap();

    let clipped = glyph_run(&build.scene, clipped_text);
    let clipped_line = clipped.semantic_line_records().unwrap()[0];
    assert_eq!(clipped_line.source_start, 0);
    assert!(clipped_line.source_end < clipped_text.len() as u64);
    assert!(clipped_text.is_char_boundary(clipped_line.source_end as usize));
    assert!(clipped.metadata_is_valid());

    let terminal = glyph_run(&build.scene, terminal_text);
    assert_eq!(
        terminal.semantic_line_records().unwrap(),
        [GlyphSemanticGroup {
            source_start: 0,
            source_end: terminal_text.len() as u64,
        }],
        "Calc decides source shortening before the final output clip"
    );
    assert!(terminal.metadata_is_valid());
}

#[test]
fn uniform_rtl_draw_strings_shorten_from_the_visual_alignment() {
    let default_text = "אבגדהוזחטיכלמנסעפצקרשת";
    let left_text = "תשרקצפעסנמלכיטחזוהדגבא";
    let right_text = "אבגדהוזחטיכלמנסעפצקרשתאב";
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("rtl-source-cutoff");
    sheet.write(0, 1, default_text);
    sheet.write_styled(1, 1, left_text, &CellStyle::new().align(HAlign::Left));
    sheet.write_styled(2, 1, right_text, &CellStyle::new().align(HAlign::Right));
    for row in 0..3 {
        sheet.write(row, 0, "X");
        sheet.write(row, 2, "X");
    }

    let mut options = outlined_options(RenderRange::new(0, 0, 2, 2));
    options.default_column_width = Fixed::from_pixels(40);
    let build = build_scene(&workbook, 0, &options).unwrap();

    let default_line = glyph_run(&build.scene, default_text)
        .semantic_line_records()
        .unwrap()[0];
    assert!(default_line.source_start > 0);
    assert_eq!(default_line.source_end, default_text.len() as u64);

    let left_line = glyph_run(&build.scene, left_text)
        .semantic_line_records()
        .unwrap()[0];
    assert_eq!(left_line.source_start, 0);
    assert!(left_line.source_end < left_text.len() as u64);

    let right_line = glyph_run(&build.scene, right_text)
        .semantic_line_records()
        .unwrap()[0];
    assert!(right_line.source_start > 0);
    assert_eq!(right_line.source_end, right_text.len() as u64);
}

#[test]
fn mixed_rtl_edit_engine_retains_the_complete_trailing_number_group() {
    let text = "مرحبا بالعالم 0007";
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("mixed-rtl-source");
    sheet.write(0, 0, "blocker");
    sheet.write(0, 1, text);

    let mut options = outlined_options(RenderRange::new(0, 0, 0, 1));
    options.default_column_width = Fixed::from_pixels(40);
    let build = build_scene(&workbook, 0, &options).unwrap();
    let run = glyph_run(&build.scene, text);
    assert_eq!(
        run.semantic_retention_groups(),
        [GlyphSemanticGroup {
            source_start: 0,
            source_end: text.len() as u64,
        }]
    );

    let number_start = text.find("0007").unwrap() as u64;
    assert!(
        run.clusters
            .iter()
            .enumerate()
            .filter(|(_, cluster)| cluster.source_end > number_start)
            .any(|(index, _)| run
                .nominal_cluster_bounds(index)
                .is_some_and(|bounds| !rectangles_intersect(bounds, run.clip_bounds))),
        "the regression needs a Western-number cluster beyond the left blocker"
    );
    assert!(run.metadata_is_valid());

    let svg = render_sheet_svg(&workbook, 0, &options).unwrap();
    assert!(svg
        .svg
        .contains(&format!("data-rxls-visible-label=\"{text}\"")));
}

#[test]
fn mixed_rtl_edit_engine_on_an_rtl_sheet_keeps_western_scalars_clippable() {
    let text = "مرحبا بالعالم 0007";
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("mirrored-mixed-rtl-source");
    sheet.set_right_to_left(true);
    for col in 0..4 {
        sheet.write(0, col, "blocker");
    }
    sheet.write(0, 4, text);

    let mut options = outlined_options(RenderRange::new(0, 0, 0, 4));
    options.default_column_width = Fixed::from_pixels(40);
    let build = build_scene(&workbook, 0, &options).unwrap();
    let run = glyph_run(&build.scene, text);
    let groups = run.semantic_retention_groups();
    let number_start = text.find("0007").unwrap() as u64;
    assert_eq!(
        groups[0],
        GlyphSemanticGroup {
            source_start: 0,
            source_end: number_start,
        }
    );
    assert_eq!(groups.len(), 5);
    for (offset, group) in groups[1..].iter().enumerate() {
        assert_eq!(group.source_start, number_start + offset as u64);
        assert_eq!(group.source_end, number_start + offset as u64 + 1);
    }
    assert!(run.metadata_is_valid());
}

#[test]
fn mixed_rtl_multiline_cells_keep_line_scoped_retention_groups() {
    let text = "مرحبا بالعالم 0007\nsecond visual line";
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("mixed-rtl-multiline");
    sheet.write_styled(0, 0, text, &CellStyle::new().wrap());
    sheet.set_row_height(0, 12.0);

    let mut options = outlined_options(RenderRange::new(0, 0, 0, 0));
    options.default_column_width = Fixed::from_pixels(80);
    let build = build_scene(&workbook, 0, &options).unwrap();
    let run = glyph_run(&build.scene, text);
    let lines = run.semantic_line_records().unwrap();
    assert!(
        lines.len() >= 2,
        "the regression requires multiple visual lines"
    );
    assert_ne!(
        run.semantic_retention_groups(),
        [GlyphSemanticGroup {
            source_start: 0,
            source_end: text.len() as u64,
        }],
        "one RTL line must not retain every clipped line in the cell"
    );
    assert!(run.metadata_is_valid());
}

#[test]
fn rtl_sheet_draw_strings_keep_calc_full_source_semantics() {
    let default_text = "ABCDEFGHIJKLMNOPQRSTUVWXYZABCDE";
    let left_text = "BCDEFGHIJKLMNOPQRSTUVWXYZABCDEF";
    let right_text = "CDEFGHIJKLMNOPQRSTUVWXYZABCDEFG";
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("rtl-sheet-source");
    sheet.set_right_to_left(true);
    sheet.write(0, 1, default_text);
    sheet.write_styled(1, 1, left_text, &CellStyle::new().align(HAlign::Left));
    sheet.write_styled(2, 1, right_text, &CellStyle::new().align(HAlign::Right));
    for row in 0..3 {
        sheet.write(row, 0, "X");
        sheet.write(row, 2, "X");
    }

    let mut options = outlined_options(RenderRange::new(0, 0, 2, 2));
    options.default_column_width = Fixed::from_pixels(40);
    let build = build_scene(&workbook, 0, &options).unwrap();
    for text in [default_text, left_text, right_text] {
        assert_eq!(
            glyph_run(&build.scene, text)
                .semantic_line_records()
                .unwrap(),
            [GlyphSemanticGroup {
                source_start: 0,
                source_end: text.len() as u64,
            }]
        );
    }
}

#[test]
fn draw_strings_indent_is_subtracted_once_for_both_alignments() {
    let left_text = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let right_text = "ZYXWVUTSRQPONMLKJIHGFEDCBA";
    let indent = 2_u8;
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("indent-cutoff");
    sheet.write_styled(
        0,
        0,
        left_text,
        &CellStyle::new().align(HAlign::Left).indent(indent),
    );
    sheet.write(0, 1, "X");
    sheet.write(1, 0, "X");
    sheet.write_styled(
        1,
        1,
        right_text,
        &CellStyle::new().align(HAlign::Right).indent(indent),
    );

    let options = outlined_options(RenderRange::new(0, 0, 1, 1));
    let pack = options.font_pack.as_ref().unwrap();
    let family = options.default_font_family.as_str();
    let request = FontRequest {
        family,
        weight: 400,
        italic: false,
    };
    let (font_id, digit_width) = pack.max_digit_width(request).unwrap();
    let metrics = pack.metrics(font_id).unwrap();
    let indent_width = scale_font_units(
        i64::from(digit_width),
        options.default_font_size,
        metrics.units_per_em,
        1,
    )
    .unwrap();
    let aligned_margin = CALC_CELL_VERTICAL_MARGIN
        .checked_add(multiply_fixed(indent_width, i64::from(indent)).unwrap())
        .unwrap();
    let expected_len = |text: &str, run: &GlyphRunNode| {
        let shaped = shape_text_with_kerning(
            pack,
            text,
            request,
            BaseDirection::LeftToRight,
            CALC_WORKSHEET_KERNING,
            &options,
        )
        .unwrap();
        let width = shaped_width(pack, &shaped, options.default_font_size).unwrap();
        let visible = calc_draw_strings_visible_width(
            run.clip_bounds.width,
            CALC_CELL_VERTICAL_MARGIN,
            aligned_margin,
        )
        .unwrap();
        let expected = calc_draw_strings_short_utf16_len(visible, width, text.len()).unwrap();
        let old_visible = inner_width(run.clip_bounds.width, aligned_margin)
            .unwrap()
            .checked_sub(Fixed::from_pixels(1))
            .unwrap();
        let double_indent =
            calc_draw_strings_short_utf16_len(old_visible, width, text.len()).unwrap();
        assert!(double_indent < expected);
        expected
    };

    let build = build_scene(&workbook, 0, &options).unwrap();
    let left = glyph_run(&build.scene, left_text);
    let left_expected = expected_len(left_text, left);
    assert_eq!(left.semantic_line_records().unwrap()[0].source_start, 0);
    assert_eq!(
        left.semantic_line_records().unwrap()[0].source_end,
        left_expected as u64
    );

    let right = glyph_run(&build.scene, right_text);
    let right_expected = expected_len(right_text, right);
    assert_eq!(
        right.semantic_line_records().unwrap()[0].source_start,
        (right_text.len() - right_expected) as u64
    );
    assert_eq!(
        right.semantic_line_records().unwrap()[0].source_end,
        right_text.len() as u64
    );
}

#[test]
fn worksheet_draw_strings_cutoff_uses_unkerned_calc_width() {
    let text = "AVAVAVAVAVAVAVAVAVAVAVAVAVAVAVAV";
    let pack = synthetic_kerning_test_pack();
    let family = pack.default_family().to_string();
    let mut options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
        gridlines: false,
        default_font_family: family.clone(),
        font_pack: Some(pack.clone()),
        ..RenderOptions::default()
    };
    let request = FontRequest {
        family: &family,
        weight: 400,
        italic: false,
    };
    let unkerned = shape_text_with_kerning(
        &pack,
        text,
        request,
        BaseDirection::LeftToRight,
        false,
        &options,
    )
    .unwrap();
    let kerned = shape_text_with_kerning(
        &pack,
        text,
        request,
        BaseDirection::LeftToRight,
        true,
        &options,
    )
    .unwrap();
    let unkerned_width = shaped_width(&pack, &unkerned, options.default_font_size).unwrap();
    let kerned_width = shaped_width(&pack, &kerned, options.default_font_size).unwrap();
    options.default_column_width = Fixed::from_pixels(40);

    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("kerning-cutoff");
    sheet.write(0, 0, text);
    sheet.write(0, 1, "X");
    let build = build_scene(&workbook, 0, &options).unwrap();
    let run = glyph_run(&build.scene, text);
    let visible_width = calc_draw_strings_visible_width(
        run.clip_bounds.width,
        CALC_CELL_VERTICAL_MARGIN,
        CALC_CELL_VERTICAL_MARGIN,
    )
    .unwrap();
    let unkerned_len =
        calc_draw_strings_short_utf16_len(visible_width, unkerned_width, text.len()).unwrap();
    let kerned_len =
        calc_draw_strings_short_utf16_len(visible_width, kerned_width, text.len()).unwrap();
    assert_ne!(unkerned_len, kerned_len);
    assert_eq!(
        run.semantic_line_records().unwrap()[0],
        GlyphSemanticGroup {
            source_start: 0,
            source_end: unkerned_len as u64,
        }
    );
}

#[test]
fn draw_strings_utf16_cutoffs_round_outward_to_scalar_boundaries() {
    let text = "A\u{1f600}B";
    assert_eq!(utf16_prefix_byte_len(text, 1), 1);
    assert_eq!(utf16_prefix_byte_len(text, 2), 5);
    assert_eq!(utf16_prefix_byte_len(text, 3), 5);
    assert_eq!(utf16_prefix_byte_len(text, 4), text.len());
    assert_eq!(utf16_suffix_byte_len(text, 1), 1);
    assert_eq!(utf16_suffix_byte_len(text, 2), 5);
    assert_eq!(utf16_suffix_byte_len(text, 3), 5);
    assert_eq!(utf16_suffix_byte_len(text, 4), text.len());
    assert!(calc_draw_strings_edit_character('\u{200b}'));
    assert!(!calc_draw_strings_edit_character('A'));
}

#[test]
fn draw_strings_cutoffs_expand_out_of_shaped_clusters() {
    let clusters = [GlyphCluster {
        source_start: 1,
        source_end: 4,
        command_start: 0,
        command_end: 1,
    }];
    let mut prefix = GlyphSemanticGroup {
        source_start: 0,
        source_end: 2,
    };
    expand_draw_strings_cutoff_to_cluster_boundary(&mut prefix, &clusters, TextAnchor::Start);
    assert_eq!(prefix.source_end, 4);

    let mut suffix = GlyphSemanticGroup {
        source_start: 2,
        source_end: 6,
    };
    expand_draw_strings_cutoff_to_cluster_boundary(&mut suffix, &clusters, TextAnchor::End);
    assert_eq!(suffix.source_start, 1);

    let visible_cluster = [GlyphCluster {
        source_start: 9,
        source_end: 10,
        command_start: 0,
        command_end: 1,
    }];
    let metrics = [GlyphClusterMetrics {
        origin_x: Fixed::from_pixels(67),
        advance_x: Fixed::from_pixels(9),
        baseline_y: Fixed::from_pixels(10),
        ascent: Fixed::from_pixels(8),
        descent: Fixed::from_pixels(-2),
    }];
    let mut proportional_prefix = GlyphSemanticGroup {
        source_start: 0,
        source_end: 9,
    };
    expand_draw_strings_cutoff_to_visible_cluster(
        &mut proportional_prefix,
        &visible_cluster,
        &metrics,
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(70),
            height: Fixed::from_pixels(20),
        },
        Fixed::from_pixels(1),
        TextAnchor::Start,
        false,
    )
    .unwrap();
    assert_eq!(proportional_prefix.source_end, 10);

    let mut single_page_prefix = GlyphSemanticGroup {
        source_start: 0,
        source_end: 9,
    };
    expand_draw_strings_cutoff_to_visible_cluster(
        &mut single_page_prefix,
        &visible_cluster,
        &metrics,
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(70),
            height: Fixed::from_pixels(20),
        },
        Fixed::from_pixels(1),
        TextAnchor::Start,
        true,
    )
    .unwrap();
    assert_eq!(single_page_prefix.source_end, 9);
}

#[test]
fn ods_fixed_height_wrapped_text_resolves_implicit_alignment_to_calc_bottom() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="0.45in"/></style:style><style:style style:name="ro" style:family="table-row"><style:table-row-properties style:row-height="0.25in" style:use-optimal-row-height="false"/></style:style><style:style style:name="ce-default" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap"/></style:style><style:style style:name="ce-top" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:vertical-align="top"/></style:style><style:style style:name="ce-middle" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:vertical-align="middle"/></style:style><style:style style:name="ce-bottom" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:vertical-align="bottom"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Clip"><table:table-column table:style-name="co"/><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-default" office:value-type="string"><text:p>implicit one two three four five six seven</text:p></table:table-cell></table:table-row><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-top" office:value-type="string"><text:p>top one two three four five six seven</text:p></table:table-cell></table:table-row><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-middle" office:value-type="string"><text:p>middle one two three four five six seven</text:p></table:table-cell></table:table-row><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-bottom" office:value-type="string"><text:p>bottom one two three four five six seven</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(content, styles);
    let build = build_scene(
        &workbook,
        0,
        &outlined_options(RenderRange::new(0, 0, 3, 0)),
    )
    .unwrap();

    for text in [
        "implicit one two three four five six seven",
        "top one two three four five six seven",
        "middle one two three four five six seven",
        "bottom one two three four five six seven",
    ] {
        let run = glyph_run(&build.scene, text);
        let semantic_layout = run.semantic_text_layout().unwrap();
        assert!(semantic_layout.lines.len() > 1);
        let prepared_end = semantic_layout.lines.last().unwrap().source.end;
        assert!(prepared_end < text.len());
        assert_eq!(
            run.semantic_retention_groups(),
            [
                GlyphSemanticGroup {
                    source_start: 0,
                    source_end: u64::try_from(prepared_end).unwrap(),
                },
                GlyphSemanticGroup {
                    source_start: u64::try_from(prepared_end).unwrap(),
                    source_end: u64::try_from(text.len()).unwrap(),
                },
            ],
            "fixed-height ODS wrapping must stop at Calc's prepared printer prefix"
        );
        assert!(semantic_layout
            .lines
            .windows(2)
            .all(|pair| pair[0].reading_order < pair[1].reading_order));
    }

    let span = |text: &str| {
        let run = glyph_run(&build.scene, text);
        let minimum = run
            .cluster_metrics
            .iter()
            .map(|metric| metric.baseline_y)
            .min()
            .unwrap();
        let maximum = run
            .cluster_metrics
            .iter()
            .map(|metric| metric.baseline_y)
            .max()
            .unwrap();
        (run.clip_bounds, minimum, maximum)
    };
    let (implicit_clip, implicit_min, implicit_max) =
        span("implicit one two three four five six seven");
    let (top_clip, top_min, top_max) = span("top one two three four five six seven");
    let (middle_clip, middle_min, middle_max) = span("middle one two three four five six seven");
    let (bottom_clip, bottom_min, bottom_max) = span("bottom one two three four five six seven");
    let bottom = |clip: Rect| clip.y.checked_add(clip.height).unwrap();

    // Top keeps its first baseline inside, middle centers the retained
    // prefix across both row edges, and bottom shifts that prefix above
    // the row while keeping its final baseline inside.
    assert!(top_min >= top_clip.y);
    assert!(top_max > bottom(top_clip));
    assert!(middle_min < middle_clip.y);
    assert!(middle_max > bottom(middle_clip));
    assert!(bottom_min < bottom_clip.y);
    assert!(bottom_max <= bottom(bottom_clip));
    // An absent alignment resolves to Calc's bottom default, so it must
    // translate identically to the explicit bottom cell rather than
    // behaving like the top one. Verified against the pinned oracle: the
    // first word of an implicitly aligned fixed-height wrapped cell and of
    // an explicitly bottom-aligned one are both pushed above the row, while
    // the explicitly top-aligned cell retains its first word.
    assert!(implicit_min < implicit_clip.y);
    assert!(implicit_max <= bottom(implicit_clip));
    assert_eq!(
        implicit_min.raw() - implicit_clip.y.raw(),
        bottom_min.raw() - bottom_clip.y.raw(),
        "implicit alignment must translate exactly like explicit bottom"
    );
}

#[test]
fn ods_fixed_height_printer_prefix_matches_narrow_hyphenated_paragraph() {
    let text = "Wrapped project-authored text for ods-0022 seed 440022";
    let content = format!(
        r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="64pt"/></style:style><style:style style:name="ro" style:family="table-row"><style:table-row-properties style:row-height="45pt" style:use-optimal-row-height="false"/></style:style><style:style style:name="ce" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Clip"><table:table-column table:style-name="co"/><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce" office:value-type="string"><text:p>{text}</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
    );
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:default-style style:family="table-cell"><style:text-properties style:font-name="Noto Sans CJK KR" style:font-name-asian="Noto Sans CJK KR" style:font-name-complex="Noto Sans CJK KR" fo:font-family="Noto Sans CJK KR" fo:font-size="11pt" style:font-size-asian="11pt" style:font-size-complex="11pt"/></style:default-style></office:styles></office:document-styles>"#;
    let workbook = ods_workbook(&content, styles);
    let build = build_scene(
        &workbook,
        0,
        &outlined_options(RenderRange::new(0, 0, 0, 0)),
    )
    .unwrap();
    let run = glyph_run(&build.scene, text);
    let layout = run.semantic_text_layout().unwrap();
    let lines = layout
        .lines
        .iter()
        .map(|line| &text[line.source.clone()])
        .collect::<Vec<_>>();
    assert_eq!(
        lines,
        [
            "Wrapped ",
            "project-",
            "authored ",
            "text for ",
            "ods-0022 "
        ]
    );
    let prefix_end = "Wrapped project-authored text for ods-0022 ".len();
    assert_eq!(layout.lines.last().unwrap().source.end, prefix_end);
    assert_eq!(
        run.semantic_retention_groups(),
        [
            GlyphSemanticGroup {
                source_start: 0,
                source_end: prefix_end as u64,
            },
            GlyphSemanticGroup {
                source_start: prefix_end as u64,
                source_end: text.len() as u64,
            },
        ]
    );
}

#[test]
fn ods_fixed_height_multiline_text_that_fits_does_not_expand_semantics() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="0.45in"/></style:style><style:style style:name="ro" style:family="table-row"><style:table-row-properties style:row-height="0.75in" style:use-optimal-row-height="false"/></style:style><style:style style:name="ce" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:vertical-align="top"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Fits"><table:table-column table:style-name="co"/><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce" office:value-type="string"><text:p>tall one two</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(content, styles);
    let build = build_scene(
        &workbook,
        0,
        &outlined_options(RenderRange::new(0, 0, 0, 0)),
    )
    .unwrap();
    let run = glyph_run(&build.scene, "tall one two");
    assert!(
        run.cluster_metrics
            .windows(2)
            .any(|pair| pair[0].baseline_y != pair[1].baseline_y),
        "the fixed-height control must actually wrap onto multiple lines"
    );
    assert!(
        run.semantic_retention_groups().is_empty(),
        "a multiline paragraph that fits its fixed row must retain ordinary clip semantics"
    );
    assert!(run.semantic_text_layout().unwrap().lines.len() > 1);
    let clip_bottom = run
        .clip_bounds
        .y
        .checked_add(run.clip_bounds.height)
        .unwrap();
    assert!(run.cluster_metrics.iter().all(|metrics| {
        metrics.baseline_y.checked_sub(metrics.ascent).unwrap() >= run.clip_bounds.y
            && metrics.baseline_y.checked_sub(metrics.descent).unwrap() <= clip_bottom
    }));
}

#[test]
fn ods_rotated_fixed_height_wrap_retains_generic_bottom_alignment() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="0.45in"/></style:style><style:style style:name="ro" style:family="table-row"><style:table-row-properties style:row-height="0.25in" style:use-optimal-row-height="false"/></style:style><style:style style:name="ce-implicit" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:rotation-angle="45"/></style:style><style:style style:name="ce-bottom" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:rotation-angle="45" style:vertical-align="bottom"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Rotated"><table:table-column table:style-name="co"/><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-implicit" office:value-type="string"><text:p>rotated one two three four five six seven</text:p></table:table-cell></table:table-row><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-bottom" office:value-type="string"><text:p>rotated one two three four five six seven</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(content, styles);
    let build = build_scene(
        &workbook,
        0,
        &outlined_options(RenderRange::new(0, 0, 1, 0)),
    )
    .unwrap();
    let runs = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::GlyphRun(run) if run.text == "rotated one two three four five six seven" => {
                Some(run)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|run| run.rotation_degrees == 45));

    let relative_baselines = |run: &GlyphRunNode| {
        run.cluster_metrics
            .iter()
            .map(|metric| metric.baseline_y.raw() - run.clip_bounds.y.raw())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        relative_baselines(runs[0]),
        relative_baselines(runs[1]),
        "implicit rotated ODS text must retain the generic bottom-aligned path"
    );
    assert!(
        runs[0]
            .cluster_metrics
            .iter()
            .map(|metric| metric.baseline_y)
            .min()
            .unwrap()
            < runs[0].clip_bounds.y,
        "a multi-line bottom-aligned block begins above a fixed row's clip"
    );
}

#[test]
fn ods_optimal_height_wrapped_text_expands_instead_of_using_the_fixed_clip_override() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="0.45in"/></style:style><style:style style:name="ro" style:family="table-row"><style:table-row-properties style:row-height="0.25in" style:use-optimal-row-height="true"/></style:style><style:style style:name="ce" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Optimal"><table:table-column table:style-name="co"/><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce" office:value-type="string"><text:p>automatic one two three four five six seven</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(content, styles);
    let sheet = &workbook.sheets[0];
    assert!(!sheet.row_height_is_manual(0));

    let range = RenderRange::new(0, 0, 0, 0);
    let options = outlined_options(range);
    let (rows, _) = measure_sheet_axes(sheet, range, &options).unwrap();
    assert!(rows[0].size > points_to_fixed(18.0).unwrap());

    let build = build_scene(&workbook, 0, &options).unwrap();
    let run = glyph_run(&build.scene, "automatic one two three four five six seven");
    assert!(
        run.semantic_retention_groups().is_empty(),
        "an expanded ODS row must use ordinary per-cluster visibility"
    );
    assert!(run.semantic_text_layout().is_some());
    let clip_bottom = run
        .clip_bounds
        .y
        .checked_add(run.clip_bounds.height)
        .unwrap();
    for metrics in &run.cluster_metrics {
        assert!(
            metrics.baseline_y.checked_sub(metrics.ascent).unwrap() >= run.clip_bounds.y,
            "an optimal row must retain the first glyph outline inside its expanded clip"
        );
        assert!(
            metrics.baseline_y.checked_sub(metrics.descent).unwrap() <= clip_bottom,
            "an optimal row must retain the last glyph outline inside its expanded clip"
        );
    }
}

fn ods_workbook(content: &str, styles: &str) -> Workbook {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    zip.start_file("mimetype", options).unwrap();
    zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
        .unwrap();
    zip.start_file("content.xml", options).unwrap();
    zip.write_all(content.as_bytes()).unwrap();
    zip.start_file("styles.xml", options).unwrap();
    zip.write_all(styles.as_bytes()).unwrap();
    Workbook::open(&zip.finish().unwrap().into_inner()).expect("ODS workbook")
}

#[test]
fn single_page_ods_rtl_keeps_fit_slack_on_the_trailing_edge() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:automatic-styles><style:style style:name="rtl" style:family="table"><style:table-properties style:writing-mode="rl-tb"/></style:style><style:style style:name="column" style:family="table-column"><style:table-column-properties style:column-width="2.5cm"/></style:style><style:style style:name="row" style:family="table-row"><style:table-row-properties style:row-height="15pt" style:use-optimal-row-height="false"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="RTL" table:style-name="rtl"><table:table-column table:style-name="column"/><table:table-row table:style-name="row"><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(content, styles);
    let sheet = &workbook.sheets[0];
    assert!(sheet.sheet_view().right_to_left);

    let range = RenderRange::new(0, 0, 0, 0);
    let scene = build_single_page_sheet_scene(sheet, 0, &outlined_options(range)).unwrap();
    let run = glyph_run(&scene.scene, "A");
    assert_eq!(run.clip_bounds.x, Fixed::ZERO);
    assert!(run.clip_bounds.width < scene.scene.width);
}

#[test]
fn single_page_ods_undeclared_row_uses_calc_application_default_height() {
    // A row with no table:style-name, and no default-style declared for
    // table-row anywhere in styles.xml, has no explicit height anywhere
    // in the document. `src/ods.rs` used to leave
    // `imported_default_row_axis_measure` as `None` for every ODS sheet,
    // so this row's contribution to the SinglePageSheets native
    // cumulative extent silently fell back to the renderer's generic
    // 15pt Excel-style default (`RenderOptions::default_row_height`)
    // requantized through a lossy CSS-pixel round trip, instead of
    // Calc's real 0.5 cm (14.173228 pt) no-information application
    // default -- the same oracle-pinned constant OOXML already uses for
    // its own equivalent "no information" row
    // (`OOXML_APPLICATION_DEFAULT_ROW_HEIGHT`).
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(content, styles);
    let sheet = &workbook.sheets[0];
    assert_eq!(
        sheet.imported_default_row_axis_measure(),
        Some(ImportedAxisMeasure::MillimeterHundredths(500)),
        "ods.rs must expose Calc's native no-information row default, \
         mirroring the unconditional 64-point column default"
    );

    let range = RenderRange::new(0, 0, 0, 0);
    let opts = outlined_options(range);
    let single_page = build_single_page_sheet_scene(sheet, 0, &opts).unwrap();
    assert_eq!(
        single_page.scene.height,
        Fixed::from_raw(19_351),
        "an undeclared ODS row must resolve the single-page page-box \
         height through Calc's native 0.5 cm application default"
    );
}

#[test]
fn single_page_ods_undeclared_rows_accumulate_exact_native_extent() {
    // Five stacked undeclared rows exercise the SourceAxisCursor's
    // cumulative twips accounting rather than a single-row endpoint, the
    // shape of drift that showed up as page-box error scaling with sheet
    // size in the hosted pilot.
    let mut rows = String::new();
    for _ in 0..5 {
        rows.push_str(
            r#"<table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row>"#,
        );
    }
    let content = format!(
        r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/>{rows}</table:table></office:spreadsheet></office:body></office:document-content>"#
    );
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(&content, styles);
    let sheet = &workbook.sheets[0];

    let range = RenderRange::new(0, 0, 4, 0);
    let opts = outlined_options(range);
    let single_page = build_single_page_sheet_scene(sheet, 0, &opts).unwrap();
    assert_eq!(
        single_page.scene.height,
        Fixed::from_raw(96_640),
        "five undeclared ODS rows must accumulate through the same \
         twips-native cursor as a single row, not drift by a \
         per-row CSS-pixel rounding residual"
    );
}

#[test]
fn fallback_row_height_prefers_ods_native_default_over_generic_placeholder() {
    // Companion to `single_page_ods_undeclared_row_uses_calc_application_default_height`
    // above, but exercising `fallback_row_height` directly -- the shared
    // helper behind the ordinary (`AxisEndpointPolicy::PerTrackFixed`) row
    // path, not just the single-page SourceNative cursor. Before this fix
    // ODS sheets fell through every branch (BIFF, then OOXML implicit) to
    // `options.default_row_height`, the renderer's generic 15pt
    // Excel-style placeholder, ignoring the sheet's own populated
    // `imported_default_row_axis_measure`.
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(content, styles);
    let sheet = &workbook.sheets[0];
    assert_eq!(
        sheet.imported_default_row_axis_measure(),
        Some(ImportedAxisMeasure::MillimeterHundredths(500))
    );
    assert_eq!(sheet.default_row_height(), None);
    assert!(!sheet.biff_uses_application_default_row_height());
    assert_eq!(sheet.implicit_ooxml_row_height_source(), None);

    let range = RenderRange::new(0, 0, 0, 0);
    let opts = outlined_options(range);
    assert_eq!(
        fallback_row_height(sheet, &opts),
        OOXML_APPLICATION_DEFAULT_ROW_HEIGHT,
        "an ODS sheet's own native no-information row default must win \
         over the renderer's generic RenderOptions::default_row_height \
         placeholder"
    );
}

#[test]
fn ordinary_path_ods_undeclared_row_uses_calc_application_default_height() {
    // The ordinary (non-single-page) render path -- `build_sheet_scene`,
    // `AxisEndpointPolicy::PerTrackFixed` -- measures row height through
    // `fallback_row_height` directly, unlike SourceNative which only uses
    // it as a last-resort fallback behind the twips cursor. This is the
    // ordinary-path counterpart to
    // `single_page_ods_undeclared_row_uses_calc_application_default_height`.
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(content, styles);
    let sheet = &workbook.sheets[0];

    let range = RenderRange::new(0, 0, 0, 0);
    let opts = outlined_options(range);
    let ordinary = build_sheet_scene(sheet, 0, &opts).unwrap();
    assert_eq!(
        ordinary.scene.height,
        Fixed::from_raw(19_351),
        "an undeclared ODS row must resolve the ordinary-path page-box \
         height through Calc's native 0.5 cm application default, the \
         same value the single-page path already resolves to"
    );
}

#[test]
fn ordinary_and_single_page_ods_paths_agree_on_undeclared_row_height() {
    // The bug class this tranche fixes is exactly the two endpoint
    // policies disagreeing about an ODS sheet's undeclared row height:
    // SourceNative (single-page) already consumed the sheet's native
    // default after the prior tranche, while PerTrackFixed (ordinary)
    // still fell back to the unrelated generic placeholder. For a single
    // row, both paths must now resolve to the identical page-box height.
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let workbook = ods_workbook(content, styles);
    let sheet = &workbook.sheets[0];

    let range = RenderRange::new(0, 0, 0, 0);
    let opts = outlined_options(range);
    let ordinary = build_sheet_scene(sheet, 0, &opts).unwrap();
    let single_page = build_single_page_sheet_scene(sheet, 0, &opts).unwrap();
    assert_eq!(
        ordinary.scene.height, single_page.scene.height,
        "the ordinary and single-page paths must not disagree about an \
         undeclared ODS row's height"
    );
}

#[test]
fn outlined_typography_limits_are_typed_and_exact_at_the_boundary() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("limits").write(0, 0, "A");
    let range = RenderRange::new(0, 0, 0, 0);
    let baseline = build_scene(&workbook, 0, &outlined_options(range)).unwrap();
    let command_count = glyph_run(&baseline.scene, "A").commands.len() as u64;
    assert!(command_count > 1);

    let mut exact = outlined_options(range);
    exact.limits.max_glyphs = 1;
    exact.limits.max_text_runs = 3;
    exact.limits.max_text_lines = 1;
    exact.limits.max_path_commands = command_count;
    assert_eq!(build_scene(&workbook, 0, &exact).unwrap(), baseline);

    let mut limited = exact.clone();
    limited.limits.max_text_runs = 2;
    assert_eq!(
        build_scene(&workbook, 0, &limited),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TextRuns,
            limit: 2,
            actual: 3,
        })
    );

    let mut limited = exact.clone();
    limited.limits.max_text_lines = 0;
    assert_eq!(
        build_scene(&workbook, 0, &limited),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TextLines,
            limit: 0,
            actual: 1,
        })
    );

    let mut limited = exact;
    limited.limits.max_path_commands = command_count - 1;
    assert_eq!(
        build_scene(&workbook, 0, &limited),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::PathCommands,
            limit: command_count - 1,
            actual: command_count,
        })
    );
}

#[test]
fn substitution_and_missing_glyphs_are_aggregated_without_host_fallback() {
    let mut workbook = Workbook::new();
    workbook.add_sheet("warnings").write_styled(
        0,
        0,
        "A😀",
        &CellStyle::new().font_name("Host Font Must Not Be Read"),
    );
    let build = build_scene(
        &workbook,
        0,
        &outlined_options(RenderRange::new(0, 0, 0, 0)),
    )
    .unwrap();
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::FontFamilySubstituted && warning.occurrences == 1
    }));
    assert!(build
        .report
        .warnings
        .iter()
        .any(|warning| { warning.code == WarningCode::MissingGlyph && warning.occurrences == 1 }));
}

#[test]
fn numeric_overflow_uses_hashes_but_wrap_and_shrink_remain_authoritative() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("overflow");
    sheet.set_col_width(0, 1.0);
    sheet.write_number(0, 0, 123_456_789);
    sheet.write_styled(1, 0, 123_456_789, &CellStyle::new().wrap());
    sheet.write_styled(2, 0, 123_456_789, &CellStyle::new().shrink_to_fit());
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 2, 0)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let texts = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Text(node) => Some(node.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["#", "123456789", "123456789"]);
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::NumericOverflowHashed && warning.occurrences == 1
    }));
}

#[test]
fn date_overflow_uses_effective_format_for_authored_numbers_and_cached_formulas() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("date-overflow");
    sheet.set_col_width(0, 1.0);
    sheet.set_col_width(1, 20.0);
    sheet.set_col_width(2, 1.0);
    sheet.set_col_width(3, 1.0);
    let format = Format::new().set_num_format("yyyy-mm-dd");
    // These are deliberately authored as ordinary numeric cells. Their
    // effective resolved number format, not their storage variant, gives
    // them date display semantics.
    sheet.write_number_with_format(0, 0, 45_366.0, &format);
    sheet.write_number_with_format(0, 1, 45_366.0, &format);
    sheet.write_with_format(
        0,
        2,
        Cell::Formula {
            formula: "B1".to_string(),
            cached: Box::new(Cell::Number(45_366.0)),
        },
        &format,
    );
    sheet.write_number(0, 3, 123_456_789);
    let build = build_scene(
        &workbook,
        0,
        &outlined_options(RenderRange::new(0, 0, 0, 3)),
    )
    .unwrap();
    let texts = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::GlyphRun(node) => Some(node.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["###", "2024-03-15", "###", "#"]);
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::NumericOverflowHashed && warning.occurrences == 3
    }));
}

#[test]
fn date_overflow_is_stable_across_xlsx_serialize_and_reopen() {
    let mut authored = Workbook::new();
    let sheet = authored.add_sheet("roundtrip-date-overflow");
    sheet.set_col_width(0, 1.0);
    sheet.write_number_with_format(0, 0, 45_366.0, &Format::new().set_num_format("yyyy-mm-dd"));
    assert!(matches!(sheet.cell(0, 0), Some(Cell::Number(_))));

    let options = outlined_options(RenderRange::new(0, 0, 0, 0));
    let authored_build = build_scene(&authored, 0, &options).unwrap();
    assert_eq!(glyph_run(&authored_build.scene, "###").text, "###");

    let imported = Workbook::open(&authored.to_xlsx()).expect("round-tripped workbook");
    assert!(matches!(imported.sheets[0].cell(0, 0), Some(Cell::Date(_))));
    let imported_build = build_scene(&imported, 0, &options).unwrap();
    assert_eq!(glyph_run(&imported_build.scene, "###").text, "###");
}

#[test]
fn imported_formula_dates_and_ods_formula_times_use_fixed_three_hashes() {
    let xlsx = imported_xlsx(
        r#"<styleSheet><numFmts count="1"><numFmt numFmtId="165" formatCode="yyyy-mm-dd"/></numFmts><cellXfs count="1"><xf numFmtId="165" applyNumberFormat="1"/></cellXfs></styleSheet>"#,
        r#"<worksheet><cols><col min="1" max="1" width="1" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="0"><f>TODAY()</f><v>45366</v></c></row></sheetData></worksheet>"#,
    );
    match xlsx.sheets[0].cell(0, 0).expect("formula date") {
        Cell::Formula { cached, .. } => assert!(matches!(cached.as_ref(), Cell::Date(_))),
        other => panic!("expected imported formula, got {other:?}"),
    }
    let xlsx_build =
        build_scene(&xlsx, 0, &outlined_options(RenderRange::new(0, 0, 0, 0))).unwrap();
    assert_eq!(glyph_run(&xlsx_build.scene, "###").text, "###");

    let ods_content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="0.08in"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Time"><table:table-column table:style-name="co"/><table:table-row><table:table-cell table:formula="of:=TIME(12;0;0)" office:value-type="time" office:time-value="PT12H"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let ods_styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
    let ods = ods_workbook(ods_content, ods_styles);
    match ods.sheets[0].cell(0, 0).expect("formula time") {
        Cell::Formula { cached, .. } => assert!(matches!(cached.as_ref(), Cell::Date(_))),
        other => panic!("expected imported ODS formula, got {other:?}"),
    }
    let ods_build = build_scene(&ods, 0, &outlined_options(RenderRange::new(0, 0, 0, 0))).unwrap();
    assert_eq!(glyph_run(&ods_build.scene, "###").text, "###");
}

#[test]
fn typed_conditional_formats_resolve_priority_scales_ranks_and_bars() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("conditional");
    for row in 0..3 {
        for col in 0..4 {
            sheet.write_number(row, col, f64::from(row * 50));
        }
    }
    sheet.write_number(0, 2, 100);
    sheet.write_number(1, 2, 100);
    sheet.write_number(2, 2, 50);
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 2, 0),
        CfRule::color_scale2(Color::rgb(255, 0, 0), Color::rgb(0, 255, 0)),
    ));
    sheet.add_conditional_format(CondFormat::new(
        (0, 1, 2, 1),
        CfRule::cell_is(
            DvOp::GreaterThan,
            "50",
            None::<&str>,
            Color::rgb(255, 255, 0),
        ),
    ));
    sheet.add_conditional_format(CondFormat::new(
        (0, 2, 2, 2),
        CfRule::top_bottom(1, false, false, Color::rgb(255, 192, 0)),
    ));
    sheet.add_conditional_format(CondFormat::new(
        (0, 3, 2, 3),
        CfRule::data_bar(Color::rgb(68, 114, 196)),
    ));
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 2, 3),
        CfRule::expression("A1>0", Color::rgb(1, 2, 3)),
    ));
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 2, 3)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let rectangles = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Rect(node) => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();
    for color in [
        Rgb::new(255, 0, 0),
        Rgb::new(128, 128, 0),
        Rgb::new(0, 255, 0),
    ] {
        assert!(rectangles.iter().any(|node| node.fill == Some(color)));
    }
    assert_eq!(
        rectangles
            .iter()
            .filter(|node| node.fill == Some(Rgb::new(255, 192, 0)))
            .count(),
        2,
        "top-N includes all values tied at the threshold"
    );
    let bars = rectangles
        .iter()
        .filter(|node| node.fill == Some(Rgb::new(68, 114, 196)))
        .collect::<Vec<_>>();
    assert_eq!(bars.len(), 2, "the minimum-value data bar has zero width");
    assert_eq!(bars[0].rect.width, Fixed::from_pixels(31));
    assert_eq!(bars[1].rect.width, Fixed::from_pixels(62));
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::ConditionalDataBarSimplified && warning.occurrences == 1
    }));
    assert!(!build
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::ConditionalFormattingDeferred));
    assert_eq!(
        rectangles
            .iter()
            .filter(|node| node.fill == Some(Rgb::new(1, 2, 3)))
            .count(),
        4,
        "the strict numeric comparison expression uses relative A1 references"
    );
}

#[test]
fn conditional_rule_statistics_include_cells_outside_the_render_selection() {
    let render_middle = |workbook: &Workbook| {
        build_scene(
            workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(1, 0, 1, 0)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap()
    };

    let mut scale = Workbook::new();
    let sheet = scale.add_sheet("scale");
    for (row, value) in [0.0, 50.0, 100.0].into_iter().enumerate() {
        sheet.write_number(row as u32, 0, value);
    }
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 2, 0),
        CfRule::color_scale2(Color::rgb(255, 0, 0), Color::rgb(0, 255, 0)),
    ));
    let scale = render_middle(&scale);
    assert!(
        scale.scene.nodes.iter().any(|node| {
            matches!(
                node,
                SceneNode::Rect(RectNode {
                    fill: Some(Rgb {
                        red: 128,
                        green: 128,
                        blue: 0
                    }),
                    ..
                })
            )
        }),
        "the selected midpoint must be scaled against the off-selection minimum and maximum"
    );

    let mut ranked = Workbook::new();
    let sheet = ranked.add_sheet("ranked");
    for (row, value) in [100.0, 50.0, 100.0].into_iter().enumerate() {
        sheet.write_number(row as u32, 0, value);
    }
    let top_fill = Rgb::new(255, 192, 0);
    let below_average_fill = Rgb::new(0, 176, 80);
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 2, 0),
        CfRule::top_bottom(1, false, false, Color::rgb(255, 192, 0)),
    ));
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 2, 0),
        CfRule::above_average(true, Color::rgb(0, 176, 80)),
    ));
    let ranked = render_middle(&ranked);
    let fills = ranked
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Rect(node) => node.fill,
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !fills.contains(&top_fill),
        "the selected 50 is not a top-one value when off-selection 100s are included"
    );
    assert!(
        fills.contains(&below_average_fill),
        "the selected 50 is below the full rule-range average"
    );

    let mut duplicate = Workbook::new();
    let sheet = duplicate.add_sheet("duplicate");
    for (row, value) in ["Alpha", "alpha", "Beta"].into_iter().enumerate() {
        sheet.write(row as u32, 0, value);
    }
    let duplicate_fill = Rgb::new(68, 114, 196);
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 2, 0),
        CfRule::duplicate_values(false, Color::rgb(68, 114, 196)),
    ));
    let duplicate = render_middle(&duplicate);
    assert!(
        duplicate.scene.nodes.iter().any(|node| matches!(
            node,
            SceneNode::Rect(node) if node.fill == Some(duplicate_fill)
        )),
        "duplicate classification must include the matching off-selection value"
    );
    assert!(
        !duplicate
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::ConditionalFormattingDeferred),
        "a fully supported rule range must not become deferred only because it is clipped"
    );
}

#[test]
fn imported_table_region_snapshot_precedes_direct_and_conditional_layers_deterministically() {
    let workbook = imported_table_xlsx(
        r#"<styleSheet>
            <fonts count="3"><font><name val="Base"/></font><font><b/></font><font><i/></font></fonts>
            <fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="solid"><fgColor rgb="FF636363"/></patternFill></fill></fills>
            <borders count="1"><border/></borders>
            <cellXfs count="4">
                <xf numFmtId="2" fontId="0" fillId="0" borderId="0"/>
                <xf numFmtId="2" fontId="1" fillId="0" borderId="0" applyFont="1"/>
                <xf numFmtId="2" fontId="2" fillId="0" borderId="0" applyFont="1"/>
                <xf numFmtId="2" fontId="0" fillId="1" borderId="0" applyFill="1"/>
            </cellXfs>
            <dxfs count="7">
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF0A0A0A"/></patternFill></fill></dxf>
                <dxf><font><b/><color rgb="FFFFFFFF"/></font><fill><patternFill patternType="solid"><fgColor rgb="FF141414"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF1E1E1E"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF282828"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF323232"/></patternFill></fill></dxf>
                <dxf><font><color rgb="FF3C3C3C"/></font></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FFC8C8C8"/></patternFill></fill></dxf>
            </dxfs>
            <tableStyles count="1"><tableStyle name="RenderedLayers" count="6">
                <tableStyleElement type="wholeTable" dxfId="0"/>
                <tableStyleElement type="headerRow" dxfId="1"/>
                <tableStyleElement type="totalRow" dxfId="2"/>
                <tableStyleElement type="firstRowStripe" dxfId="3"/>
                <tableStyleElement type="secondRowStripe" dxfId="4"/>
                <tableStyleElement type="firstColumn" dxfId="5"/>
            </tableStyle></tableStyles>
        </styleSheet>"#,
        r#"<worksheet><cols><col min="1" max="1" style="1"/></cols><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>Left</t></is></c><c r="B1" t="inlineStr"><is><t>Right</t></is></c></row>
            <row r="2" s="2" customFormat="1"><c r="A2" s="3"><v>1</v></c><c r="B2"><v>2</v></c></row>
            <row r="3"><c r="A3"><v>3</v></c><c r="B3"><v>4</v></c></row>
            <row r="4"><c r="A4"><v>5</v></c><c r="B4"><v>6</v></c></row>
        </sheetData>
        <conditionalFormatting sqref="B2"><cfRule type="cellIs" dxfId="6" priority="1" stopIfTrue="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting>
        <tableParts count="1"><tablePart r:id="rIdTable"/></tableParts></worksheet>"#,
        r#"<table id="1" name="RenderedTable" displayName="RenderedTable" ref="A1:B4" headerRowCount="1" totalsRowCount="1"><tableColumns count="2"><tableColumn id="1" name="Left"/><tableColumn id="2" name="Right"/></tableColumns><tableStyleInfo name="RenderedLayers" showFirstColumn="1" showLastColumn="0" showRowStripes="1" showColumnStripes="0"/></table>"#,
    );
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 3, 1)),
        gridlines: false,
        ..RenderOptions::default()
    };
    let first = render_sheet_svg(&workbook, 0, &options).unwrap();
    let second = render_sheet_svg(&workbook, 0, &options).unwrap();
    assert_eq!(first.scene, second.scene);
    assert_eq!(first.svg.as_bytes(), second.svg.as_bytes());

    let fills = first
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Rect(node) => node.fill,
            _ => None,
        })
        .collect::<Vec<_>>();
    for (color, expected) in [
        (Rgb::new(0x14, 0x14, 0x14), 2),
        (Rgb::new(0x63, 0x63, 0x63), 1),
        (Rgb::new(0xC8, 0xC8, 0xC8), 1),
        (Rgb::new(0x32, 0x32, 0x32), 2),
        (Rgb::new(0x1E, 0x1E, 0x1E), 2),
    ] {
        assert_eq!(
            fills.iter().filter(|fill| **fill == color).count(),
            expected,
            "fills: {fills:?}"
        );
    }
    assert_eq!(
        fills
            .iter()
            .filter(|fill| **fill == Rgb::new(0x28, 0x28, 0x28))
            .count(),
        0,
        "direct and conditional layers cover both first-stripe cells"
    );
    let direct_text = first
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Text(node) if node.text == "1.00" => Some(node),
            _ => None,
        })
        .expect("direct cell text");
    assert!(
        !direct_text.style.bold,
        "the resolved row XF replaces the lower-precedence column XF"
    );
    assert!(direct_text.style.italic, "row style must survive");
    assert_eq!(direct_text.style.color, Rgb::new(0x3C, 0x3C, 0x3C));
}

#[test]
fn used_selection_preflights_sparse_materialized_cells_before_dense_extent_work() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("sparse-preflight");
    sheet.write(0, 0, "a");
    sheet.write(0, 1, "b");
    sheet.write(1, 0, "c");
    let mut options = RenderOptions::default();
    options.limits.max_cells = 2;
    assert_eq!(
        build_scene(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Cells,
            limit: 2,
            actual: 3,
        }),
        "the sparse preflight must fail at the third materialized cell, before the 2x2 extent"
    );
}

#[test]
fn imported_conditional_priority_stop_and_dxf_overlay_are_exact_and_deterministic() {
    let mut workbook = imported_xlsx(
        r#"<styleSheet>
            <fonts count="2"><font/><font><b/><color rgb="FF112233"/></font></fonts>
            <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
            <borders count="2"><border/><border><left style="thin"><color rgb="FF010203"/></left></border></borders>
            <cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="1"/></cellXfs>
            <dxfs count="3">
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FFFF0000"/></patternFill></fill></dxf>
                <dxf><font><color rgb="FF663399"/></font><fill><patternFill patternType="solid"><fgColor rgb="FF0000FF"/></patternFill></fill><border><bottom style="medium"><color rgb="FFAABBCC"/></bottom></border><numFmt numFmtId="2" formatCode="0.00"/><protection locked="0"/></dxf>
                <dxf><font><i/></font><fill><patternFill patternType="solid"><fgColor rgb="FF00FF00"/></patternFill></fill></dxf>
            </dxfs>
        </styleSheet>"#,
        r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>5</v></c></row></sheetData>
            <conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="10" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting>
            <conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="1" priority="1" stopIfTrue="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting>
            <conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="2" priority="2" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting>
        </worksheet>"#,
    );
    workbook.sheets[0].write_styled(
        0,
        0,
        5,
        &CellStyle {
            font: Some(
                rxls::Font::new()
                    .bold()
                    .with_color(Color::rgb(0x11, 0x22, 0x33)),
            ),
            border: Some(
                Border::new()
                    .with_left(BorderStyle::Thin)
                    .with_left_color(Color::rgb(1, 2, 3)),
            ),
            ..CellStyle::default()
        },
    );
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        gridlines: false,
        ..RenderOptions::default()
    };
    assert_eq!(
        workbook.sheets[0]
            .cell_style(0, 0)
            .and_then(|style| style.border.as_ref())
            .map(|border| (border.left, border.left_color)),
        Some((BorderStyle::Thin, Some(Color::rgb(1, 2, 3))))
    );
    let first = render_sheet_svg(&workbook, 0, &options).unwrap();
    let second = render_sheet_svg(&workbook, 0, &options).unwrap();
    assert_eq!(first.scene, second.scene);
    assert_eq!(first.svg.as_bytes(), second.svg.as_bytes());

    let fill = first.scene.nodes.iter().find_map(|node| match node {
        SceneNode::Rect(node) => node.fill,
        _ => None,
    });
    assert_eq!(fill, Some(Rgb::new(0, 0, 255)));
    let text = first
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Text(node) => Some(node),
            _ => None,
        })
        .expect("conditional text");
    assert!(
        text.style.bold,
        "base bold font must survive a color-only dxf font"
    );
    assert!(
        !text.style.italic,
        "stopIfTrue must block the lower-priority italic dxf"
    );
    assert_eq!(text.style.color, Rgb::new(0x66, 0x33, 0x99));
    let lines = first
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(line) => Some(line),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        lines.iter().any(|line| line.color == Rgb::new(1, 2, 3)),
        "line colors: {:?}",
        lines.iter().map(|line| line.color).collect::<Vec<_>>()
    );
    assert!(lines
        .iter()
        .any(|line| line.color == Rgb::new(0xAA, 0xBB, 0xCC)));
    assert!(first.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::ConditionalFormattingDeferred && warning.occurrences == 2
    }));
}

#[test]
fn authored_and_round_tripped_imported_style_snapshots_match() {
    let mut authored = Workbook::new();
    let style = CellStyle {
        font: Some(
            rxls::Font::new()
                .with_name("Liberation Sans")
                .bold()
                .with_color(Color::rgb(12, 34, 56)),
        ),
        fill: Some(Color::rgb(210, 220, 230)),
        pattern_fill: Some(rxls::Fill::solid(Color::rgb(210, 220, 230))),
        border: Some(
            Border::new()
                .with_all(BorderStyle::Thin)
                .with_color(Color::rgb(70, 80, 90)),
        ),
        ..CellStyle::default()
    };
    let sheet = authored.add_sheet("snapshot");
    // Pin geometry explicitly: a re-opened OOXML sheet with no
    // sheetFormatPr intentionally carries Calc's imported application
    // default, while a purely authored sheet retains the caller fallback.
    sheet.set_default_row_height(15.0);
    sheet.write_styled(0, 0, 5, &style);
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 0, 0),
        CfRule::cell_is(DvOp::GreaterThan, "0", None::<&str>, Color::rgb(1, 2, 3)),
    ));
    let imported = Workbook::open(&authored.to_xlsx()).expect("round-tripped workbook");
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        gridlines: false,
        ..RenderOptions::default()
    };
    let authored_scene = build_scene(&authored, 0, &options).unwrap();
    let imported_scene = build_scene(&imported, 0, &options).unwrap();
    assert_eq!(authored_scene.scene, imported_scene.scene);
}

#[test]
fn conditional_references_and_duplicate_values_are_exact_for_bounded_subset() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("O'Brien");
    for (row, (left, right)) in [(10, 5), (20, 25), (20, 15)].into_iter().enumerate() {
        sheet.write_number(row as u32, 0, left);
        sheet.write_number(row as u32, 1, right);
    }
    for (row, value) in ["Alpha", "alpha", "Beta"].into_iter().enumerate() {
        sheet.write(row as u32, 2, value);
    }
    sheet.add_conditional_format(CondFormat::new(
        (0, 1, 2, 1),
        CfRule::cell_is(
            DvOp::GreaterThan,
            "'O''Brien'!$A1",
            None::<&str>,
            Color::rgb(255, 0, 0),
        ),
    ));
    sheet.add_conditional_format(CondFormat::new(
        (0, 2, 2, 2),
        CfRule::duplicate_values(false, Color::rgb(0, 255, 0)),
    ));
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 2, 0),
        CfRule::expression("$B1>$A1", Color::rgb(0, 0, 255)),
    ));

    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 2, 2)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let fills = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Rect(node) => node.fill,
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        fills
            .iter()
            .filter(|color| **color == Rgb::new(255, 0, 0))
            .count(),
        1,
        "only B2 is greater than the row-relative absolute-column A reference"
    );
    assert_eq!(
        fills
            .iter()
            .filter(|color| **color == Rgb::new(0, 255, 0))
            .count(),
        2,
        "ASCII duplicate matching is case-insensitive"
    );
    assert_eq!(
        fills
            .iter()
            .filter(|color| **color == Rgb::new(0, 0, 255))
            .count(),
        1,
        "only A2 has B greater than A"
    );
    assert!(!build
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::ConditionalFormattingDeferred));
}

#[test]
fn duplicate_wildcards_and_sparse_ranges_remain_deferred() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("deferred");
    sheet.write(0, 0, "*");
    sheet.write(1, 0, "*");
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 1, 0),
        CfRule::duplicate_values(false, Color::rgb(1, 2, 3)),
    ));
    sheet.add_conditional_format(CondFormat::new(
        (0, 1, 1, 1),
        CfRule::duplicate_values(true, Color::rgb(4, 5, 6)),
    ));
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 1)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::ConditionalFormattingDeferred && warning.occurrences == 2
    }));
}

#[test]
fn conditional_and_media_limits_fail_before_expansion() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("limits");
    sheet.write_number(0, 0, 1);
    sheet.add_conditional_format(CondFormat::new(
        (0, 0, 0, 0),
        CfRule::cell_is(DvOp::Equal, "1", None::<&str>, Color::rgb(1, 2, 3)),
    ));
    sheet.add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (0, 0)));

    let mut options = RenderOptions::default();
    options.limits.max_conditional_rules = 0;
    assert_eq!(
        build_scene(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::ConditionalRules,
            limit: 0,
            actual: 1,
        })
    );
    options.limits.max_conditional_rules = 1;
    options.limits.max_media_bytes = 3;
    assert_eq!(
        build_scene(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::MediaBytes,
            limit: 3,
            actual: 4,
        })
    );
    options.limits.max_media_bytes = 4;
    options.limits.max_conditional_evaluations = 0;
    assert_eq!(
        build_scene(&workbook, 0, &options),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::ConditionalEvaluations,
            limit: 0,
            actual: 1,
        })
    );
}

#[test]
fn images_charts_and_sparklines_use_deterministic_geometric_placeholders() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("drawings");
    sheet.write(3, 3, "extent");
    sheet.add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (0, 0)).with_to((2, 2)));
    sheet.add_chart(Chart::new(ChartKind::Line, (1, 1), (3, 3)));
    sheet.add_sparkline(
        Sparkline::new((0, 3), "drawings!$A$1:$A$3").with_kind(SparklineKind::Column),
    );
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 3, 3)),
        gridlines: false,
        ..RenderOptions::default()
    };
    let first = build_scene(&workbook, 0, &options).unwrap();
    let second = build_scene(&workbook, 0, &options).unwrap();
    assert_eq!(first, second);
    for code in [
        WarningCode::ImagePlaceholder,
        WarningCode::ChartPlaceholder,
        WarningCode::SparklinePlaceholder,
    ] {
        assert!(first
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == code && warning.occurrences == 1));
    }
    assert!(first.scene.nodes.iter().any(|node| matches!(
        node,
        SceneNode::Rect(RectNode {
            rect: Rect {
                width,
                height,
                ..
            },
            fill: Some(Rgb {
                red: 242,
                green: 242,
                blue: 242,
            }),
            ..
        }) if *width == Fixed::from_pixels(128) && *height == Fixed::from_pixels(40)
    )));
}

#[test]
fn drawings_continue_across_explicit_range_and_print_tile_boundaries() {
    fn contains_placeholder(nodes: &[SceneNode]) -> bool {
        nodes.iter().any(|node| match node {
            SceneNode::ClipGroup(group) => contains_placeholder(&group.nodes),
            SceneNode::Rect(RectNode {
                fill:
                    Some(Rgb {
                        red: 242,
                        green: 242,
                        blue: 242,
                    }),
                ..
            }) => true,
            _ => false,
        })
    }

    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("continued-drawing");
    sheet.add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (0, 0)).with_to((4, 4)));
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(2, 2, 4, 4)),
        gridlines: false,
        ..RenderOptions::default()
    };
    let build = build_scene(&workbook, 0, &options).unwrap();
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::ImagePlaceholder && warning.occurrences == 1
    }));
    assert!(!build
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::DrawingAnchorUnavailable));
    assert!(build.scene.nodes.iter().any(|node| matches!(
        node,
        SceneNode::Rect(RectNode {
            rect: Rect {
                x,
                y,
                width,
                height,
            },
            fill: Some(Rgb {
                red: 242,
                green: 242,
                blue: 242,
            }),
            ..
        }) if *x == Fixed::ZERO
            && *y == Fixed::ZERO
            && *width == Fixed::from_pixels(128)
            && *height == Fixed::from_pixels(40)
    )));

    let mut paginated = Workbook::new();
    let sheet = paginated.add_sheet("continued-print-drawing");
    sheet.add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (0, 0)).with_to((8, 4)));
    sheet.set_page_setup(
        PageSetup::new()
            .with_print_area((0, 0, 8, 4))
            .with_paper_size(1)
            .with_scale(400),
    );
    let document = build_print_document(
        &paginated,
        0,
        &PrintOptions {
            omit_sparse_pages: false,
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert!(document.pages.len() > 1);
    assert!(document
        .pages
        .iter()
        .skip(1)
        .any(|page| contains_placeholder(&page.scene.nodes)));
}

#[test]
fn valid_png_images_decode_to_backend_neutral_rgba_nodes() {
    let rgba = [
        255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 255, 255, 255, 0,
    ];
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("decoded-image");
    sheet.add_image(Image::new(test_rgba_png(2, 2, &rgba), ImageFmt::Png, (0, 0)).with_to((1, 1)));
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 1)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    let image = build
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Image(node) => Some(node),
            _ => None,
        })
        .expect("decoded image node");
    assert_eq!((image.pixel_width, image.pixel_height), (2, 2));
    assert_eq!(image.rgba.as_ref(), rgba);
    assert_eq!(
        image.rect,
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(64),
            height: Fixed::from_pixels(20),
        }
    );
    assert!(!build
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::ImagePlaceholder));

    let mut limited = RenderOptions::default();
    limited.limits.max_image_pixels = 3;
    assert_eq!(
        build_scene(&workbook, 0, &limited),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::ImagePixels,
            limit: 3,
            actual: 4,
        })
    );
}

#[test]
fn same_sheet_a1_charts_and_sparklines_render_real_bounded_geometry() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("charts");
    for (row, (label, value, x, size)) in [
        ("Jan", 10.0, 1.0, 4.0),
        ("Feb", 20.0, 2.0, 16.0),
        ("Mar", 15.0, 3.0, 9.0),
    ]
    .into_iter()
    .enumerate()
    {
        sheet.write(row as u32, 0, label);
        sheet.write_number(row as u32, 1, value);
        sheet.write_number(row as u32, 2, x);
        sheet.write_number(row as u32, 3, size);
    }
    let categorical = || {
        Series::new("charts!$B$1:$B$3")
            .with_categories("charts!$A$1:$A$3")
            .with_name("Revenue")
    };
    sheet.add_chart(
        Chart::new(ChartKind::Line, (0, 4), (8, 10))
            .with_title("Line")
            .with_x_axis_title("Month")
            .with_y_axis_title("Value")
            .with_legend(true)
            .with_data_labels(true)
            .add_series(categorical()),
    );
    sheet.add_chart(
        Chart::new(ChartKind::Pie, (9, 4), (17, 10))
            .with_title("Pie")
            .with_legend(true)
            .with_data_labels(true)
            .add_series(categorical()),
    );
    sheet.add_chart(
        Chart::new(ChartKind::Scatter, (18, 4), (26, 10))
            .with_title("Scatter")
            .add_series(
                Series::new("charts!$B$1:$B$3")
                    .with_categories("charts!$C$1:$C$3")
                    .with_name("XY"),
            ),
    );
    sheet.add_chart(
        Chart::new(ChartKind::Bar, (27, 4), (35, 10))
            .with_title("Columns")
            .add_series(categorical()),
    );
    sheet.add_chart(
        Chart::new(ChartKind::Area, (36, 4), (44, 10))
            .with_title("Area")
            .add_series(categorical()),
    );
    sheet.add_chart(
        Chart::new(ChartKind::Doughnut, (45, 4), (53, 10))
            .with_title("Doughnut")
            .add_series(categorical()),
    );
    sheet.add_chart(
        Chart::new(ChartKind::Radar, (54, 4), (62, 10))
            .with_title("Radar")
            .add_series(categorical()),
    );
    sheet.add_chart(
        Chart::new(ChartKind::Bubble, (63, 4), (71, 10))
            .with_title("Bubble")
            .add_series(
                Series::new("charts!$B$1:$B$3")
                    .with_categories("charts!$C$1:$C$3")
                    .with_bubble_sizes("charts!$D$1:$D$3")
                    .with_name("Bubbles"),
            ),
    );
    for (row, kind) in [
        SparklineKind::Line,
        SparklineKind::Column,
        SparklineKind::WinLoss,
    ]
    .into_iter()
    .enumerate()
    {
        sheet.add_sparkline(Sparkline::new((row as u32, 11), "charts!$B$1:$B$3").with_kind(kind));
    }

    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 71, 11)),
        gridlines: false,
        ..RenderOptions::default()
    };
    let build = build_scene(&workbook, 0, &options).unwrap();
    assert!(!build.report.warnings.iter().any(|warning| matches!(
        warning.code,
        WarningCode::ChartPlaceholder | WarningCode::SparklinePlaceholder
    )));
    assert!(
        build
            .scene
            .nodes
            .iter()
            .any(|node| matches!(node, SceneNode::Path(_))),
        "pie wedges use filled paths"
    );
    for title in [
        "Line", "Pie", "Scatter", "Columns", "Area", "Doughnut", "Radar", "Bubble",
    ] {
        assert!(build.scene.nodes.iter().any(|node| match node {
            SceneNode::Text(node) => node.text == title,
            _ => false,
        }));
    }
    for (text, role) in [
        ("Line", ChartTextRole::ChartTitle),
        ("Month", ChartTextRole::AxisTitle),
        ("Value", ChartTextRole::AxisTitle),
        ("Revenue", ChartTextRole::Legend),
        ("Jan", ChartTextRole::AxisLabel),
        ("10", ChartTextRole::DataLabel),
    ] {
        assert!(
            build.scene.nodes.iter().any(|node| match node {
                SceneNode::Text(node) => {
                    node.text == text
                        && node.style.family == options.default_font_family
                        && node.style.size == role.size()
                        && node.style.bold == role.bold()
                }
                _ => false,
            }),
            "missing exact {role:?} style for {text:?}"
        );
    }

    let mut limited = options;
    limited.limits.max_chart_points = 5;
    assert_eq!(
        build_scene(&workbook, 0, &limited),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::ChartPoints,
            limit: 5,
            actual: 6,
        })
    );
}

#[test]
fn imported_sparklines_defer_paint_when_calc_omits_ooxml_extensions() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("imported_sparkline");
    for (col, value) in [10.0, 20.0, 15.0, 25.0].into_iter().enumerate() {
        sheet.write_number(0, (col + 1) as u16, value);
    }
    sheet.add_sparkline(Sparkline::new((0, 0), "imported_sparkline!$B$1:$E$1"));
    let workbook = Workbook::open(&workbook.to_xlsx()).expect("reopen imported sparkline");
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 4)),
        gridlines: false,
        ..RenderOptions::default()
    };
    let build = build_scene(&workbook, 0, &options).unwrap();
    assert!(!build
        .scene
        .nodes
        .iter()
        .any(|node| { matches!(node, SceneNode::Line(_) | SceneNode::Path(_)) }));
    assert!(!build
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::SparklinePlaceholder));
}

#[test]
fn authored_chart_text_uses_point_defaults_with_the_configured_family() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Render");
    for (column, category) in ["Q1", "Q2", "Q3", "Q4"].into_iter().enumerate() {
        sheet.write(5, column as u16 + 1, category);
    }
    sheet.write(6, 0, "Series 0000");
    for (column, value) in [28.0, 41.0, 54.0, 67.0].into_iter().enumerate() {
        sheet.write_number(6, column as u16 + 1, value);
    }
    sheet.add_chart(
        Chart::new(ChartKind::Line, (7, 0), (18, 6)).add_series(
            Series::new("Render!$B$7:$E$7")
                .with_categories("Render!$B$6:$E$6")
                .with_name("Series 0000"),
        ),
    );
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 18, 6)),
        gridlines: false,
        default_font_family: "Noto Sans CJK KR".to_string(),
        ..RenderOptions::default()
    };
    let build = build_scene(&workbook, 0, &options).unwrap();
    assert!(!build
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::ChartPlaceholder));

    let chart_size = points_to_fixed(10.0).unwrap();
    for expected in [
        "Q1", "Q2", "Q3", "Q4", "0", "10", "20", "30", "40", "50", "60", "70", "80",
    ] {
        assert!(
            build.scene.nodes.iter().any(|node| match node {
                SceneNode::Text(node) => {
                    node.text == expected
                        && node.style.family == options.default_font_family
                        && node.style.size == chart_size
                        && !node.style.bold
                }
                _ => false,
            }),
            "missing xlsx-0000-like chart label {expected:?}"
        );
    }
    assert!(build.scene.nodes.iter().any(|node| match node {
        SceneNode::Text(node) => {
            node.text == "Q1"
                && node.style.family == "Noto Sans CJK KR"
                && node.style.size == options.default_font_size
        }
        _ => false,
    }));

    let horizontal_axis = build
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Line(line)
                if line.color == Rgb::BLACK && line.y1 == line.y2 && line.x1 < line.x2 =>
            {
                Some(line)
            }
            _ => None,
        })
        .expect("chart horizontal axis");
    let vertical_axis = build
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Line(line)
                if line.color == Rgb::BLACK && line.x1 == line.x2 && line.y1 < line.y2 =>
            {
                Some(line)
            }
            _ => None,
        })
        .expect("chart vertical axis");
    assert_eq!(vertical_axis.x1, horizontal_axis.x1);
    assert_eq!(vertical_axis.y2, horizontal_axis.y1);
    assert!(build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Line(line) if line.color == Rgb::new(0x44, 0x72, 0xC4) => Some(line),
            _ => None,
        })
        .all(|line| {
            line.x1 >= horizontal_axis.x1
                && line.x1 <= horizontal_axis.x2
                && line.x2 >= horizontal_axis.x1
                && line.x2 <= horizontal_axis.x2
                && line.y1 >= vertical_axis.y1
                && line.y1 <= vertical_axis.y2
                && line.y2 >= vertical_axis.y1
                && line.y2 <= vertical_axis.y2
        }));
}

#[test]
fn chart_text_roles_are_source_exact_and_do_not_change_non_chart_text() {
    for (role, points, bold) in [
        (ChartTextRole::ChartTitle, 18.0, true),
        (ChartTextRole::AxisTitle, 10.0, true),
        (ChartTextRole::AxisLabel, 10.0, false),
        (ChartTextRole::Legend, 10.0, false),
        (ChartTextRole::DataLabel, 10.0, false),
    ] {
        let style = role.style("Theme Sans", TextAnchor::Start, 0);
        assert_eq!(style.family, "Theme Sans");
        assert_eq!(style.size, points_to_fixed(points).unwrap());
        assert_eq!(style.bold, bold);
    }

    let mut workbook = Workbook::new();
    workbook.add_sheet("control").write(0, 0, "worksheet-only");
    let options = RenderOptions {
        selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
        gridlines: false,
        default_font_family: "Noto Sans CJK KR".to_string(),
        default_font_size: points_to_fixed(11.0).unwrap(),
        ..RenderOptions::default()
    };
    let build = build_scene(&workbook, 0, &options).unwrap();
    let text = build
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Text(node) if node.text == "worksheet-only" => Some(node),
            _ => None,
        })
        .expect("non-chart control text");
    assert_eq!(text.style.family, options.default_font_family);
    assert_eq!(text.style.size, options.default_font_size);
    assert!(!text.style.bold);
}

#[test]
fn chart_gutters_use_verified_shaped_advances_when_a_pack_is_present() {
    let pack = synthetic_test_pack();
    let options = RenderOptions {
        default_font_family: "worksheet-family-must-not-leak".to_string(),
        font_pack: Some(pack.clone()),
        ..RenderOptions::default()
    };
    let role = ChartTextRole::AxisLabel;
    let style = role.resolved_style(CALC_MISSING_THEME_CHART_LATIN_FAMILY);
    let shaped = shape_text_with_kerning(
        &pack,
        "WWW",
        style.request(),
        BaseDirection::LeftToRight,
        true,
        &options,
    )
    .unwrap();
    let expected_width = shaped_width(&pack, &shaped, style.size)
        .unwrap()
        .checked_add(Fixed::from_pixels(4))
        .unwrap();
    let expected_height = line_height_from_metrics(
        styled_line_metrics(
            &pack,
            &shaped,
            std::slice::from_ref(&style),
            CellLineLayoutPolicy::Native,
            CalcLinePlacementPolicy::Native,
            "WWW",
            1,
            1,
            &options,
        )
        .unwrap(),
        CalcLinePlacementPolicy::Native,
    )
    .unwrap()
    .checked_add(Fixed::from_pixels(4))
    .unwrap();
    let measured = measure_chart_text(
        "WWW",
        role,
        CALC_MISSING_THEME_CHART_LATIN_FAMILY,
        &options,
        None,
    )
    .unwrap();
    assert_eq!(
        measured,
        ChartTextMetrics {
            width: expected_width,
            height: expected_height,
        }
    );

    let mut nodes = Vec::new();
    let mut text_bytes = 0;
    let mut glyphs = 0;
    let mut typography_stats = TypographyStats::default();
    push_chart_text(
        &mut nodes,
        "WWW".to_string(),
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: measured.width,
            height: measured.height,
        },
        TextAnchor::Start,
        0,
        role,
        CALC_MISSING_THEME_CHART_LATIN_FAMILY,
        &mut text_bytes,
        &mut glyphs,
        &mut typography_stats,
        &options,
    )
    .unwrap();
    let SceneNode::GlyphRun(run) = &nodes[0] else {
        panic!("verified chart text must be outlined");
    };
    assert_eq!(run.clip_bounds.width, measured.width);
    assert_eq!(run.clip_bounds.height, measured.height);
    assert!(run.glyphs.iter().all(|glyph| glyph.size == role.size()));
}

#[test]
fn packless_chart_gutters_use_the_same_proportional_helvetica_advances_as_pdf() {
    let options = RenderOptions::default();
    let role = ChartTextRole::AxisLabel;
    let wide = measure_chart_text("WWW", role, "Chart Sans", &options, None).unwrap();
    let narrow = measure_chart_text("iii", role, "Chart Sans", &options, None).unwrap();
    let padding = Fixed::from_pixels(4);
    let expected_wide = Fixed::from_raw(role.size().raw() * (944 * 3) / 1_000)
        .checked_add(padding)
        .unwrap();
    let expected_narrow = Fixed::from_raw(role.size().raw() * (222 * 3) / 1_000)
        .checked_add(padding)
        .unwrap();

    assert_eq!(wide.width, expected_wide);
    assert_eq!(narrow.width, expected_narrow);
    assert!(wide.width > narrow.width);
}

#[test]
fn auxiliary_text_does_not_use_worksheet_draw_strings_shortening() {
    let options = outlined_options(RenderRange::new(0, 0, 0, 0));
    let text = "auxiliary chart text remains complete";
    let node = build_auxiliary_text_node_with_clip_and_kerning(
        text.to_string(),
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(240),
            height: Fixed::from_pixels(24),
        },
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(24),
            height: Fixed::from_pixels(24),
        },
        Fixed::from_pixels(2),
        ChartTextRole::AxisLabel.style(options.default_font_family.as_str(), TextAnchor::Start, 0),
        true,
        &options,
    )
    .unwrap();
    let SceneNode::GlyphRun(run) = node else {
        panic!("verified auxiliary text must be outlined");
    };
    assert_eq!(
        run.semantic_line_records(),
        Some(
            [GlyphSemanticGroup {
                source_start: 0,
                source_end: u64::try_from(text.len()).unwrap(),
            }]
            .as_slice()
        )
    );
    assert!(run.metadata_is_valid());
}

#[test]
fn packless_chart_kerning_falls_back_with_explicit_approximation_provenance() {
    let options = RenderOptions::default();
    let mut style = ResolvedChartTextStyle::for_role(ChartTextRole::AxisLabel, "Chart Sans");
    style.kerning_minimum_hundredths_of_point =
        Some(style.size_hundredths_of_point.saturating_add(1));
    assert!(!style.kerning());
    assert!(measure_chart_text_with_style("AV", &style, &options, None).is_ok());
    let node = build_auxiliary_text_node_with_kerning(
        "AV".to_string(),
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(80),
            height: Fixed::from_pixels(24),
        },
        Fixed::from_pixels(2),
        style.text_style(TextAnchor::Start, 0),
        style.kerning(),
        &options,
    )
    .unwrap();
    assert!(matches!(node, SceneNode::Text(_)));

    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("chart");
    for (row, (category, value)) in [("A", 1.0), ("B", 2.0)].into_iter().enumerate() {
        sheet.write(row as u32, 0, category);
        sheet.write_number(row as u32, 1, value);
    }
    let chart = Chart::new(ChartKind::Line, (0, 0), (10, 8))
        .add_series(Series::new("chart!$B$1:$B$2").with_categories("chart!$A$1:$A$2"));
    let warning_cell = CellCoordinate { row: 0, col: 0 };
    let mut nodes = Vec::new();
    let mut warnings = Warnings::default();
    assert!(try_push_chart(
        &mut nodes,
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(500),
            height: Fixed::from_pixels(300),
        },
        &chart,
        None,
        sheet,
        &mut 0,
        &mut 0,
        &mut 0,
        &mut TypographyStats::default(),
        &options,
        &mut warnings,
        warning_cell,
    )
    .unwrap());
    assert!(nodes.iter().any(|node| matches!(node, SceneNode::Text(_))));
    assert_eq!(
        warnings.finish(),
        [RenderWarning {
            code: WarningCode::ApproximateTextMetrics,
            occurrences: 1,
            first_cell: Some(warning_cell),
        }]
    );

    let pie = Chart::new(ChartKind::Pie, (0, 0), (10, 8))
        .add_series(Series::new("chart!$B$1:$B$2").with_categories("chart!$A$1:$A$2"));
    let mut pie_warnings = Warnings::default();
    assert!(try_push_chart(
        &mut Vec::new(),
        Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(500),
            height: Fixed::from_pixels(300),
        },
        &pie,
        None,
        sheet,
        &mut 0,
        &mut 0,
        &mut 0,
        &mut TypographyStats::default(),
        &options,
        &mut pie_warnings,
        warning_cell,
    )
    .unwrap());
    assert!(pie_warnings.finish().is_empty());
}

#[test]
fn scatter_and_bubble_points_share_the_nice_x_axis_used_by_labels() {
    let mut style = ChartSeriesStyle::default();
    style.marker = ChartMarkerSymbol::None;
    style.line_color = Some(Color::rgb(0x12, 0x34, 0x56));
    let series = [ResolvedChartSeries {
        name: "Series".to_string(),
        values: vec![1.0, 2.0],
        x_values: Some(vec![11.0, 19.0]),
        labels: Vec::new(),
        bubble_sizes: Some(vec![1.0, 1.0]),
        style,
    }];
    let axis = chart_nice_x_axis(&series).unwrap();
    assert_eq!((axis.minimum, axis.maximum), (10.0, 20.0));
    assert_eq!(axis.ticks.first(), Some(&axis.minimum));
    assert_eq!(axis.ticks.last(), Some(&axis.maximum));
    let data_bounds = chart_x_data_bounds(&series).unwrap();
    let retained_ticks = axis
        .ticks
        .iter()
        .copied()
        .filter(|value| *value >= data_bounds.0 && *value <= data_bounds.1)
        .collect::<Vec<_>>();
    assert_eq!(retained_ticks.first(), Some(&11.0));
    assert_eq!(retained_ticks.last(), Some(&19.0));
    let plot = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: Fixed::from_pixels(100),
        height: Fixed::from_pixels(100),
    };
    let options = RenderOptions::default();

    let mut scatter_nodes = Vec::new();
    push_scatter_chart(
        &mut scatter_nodes,
        plot,
        &series,
        (0.0, 2.0),
        &axis,
        &[],
        false,
        &mut Vec::new(),
        &mut TypographyStats::default(),
        &options,
    )
    .unwrap();
    let scatter_line = scatter_nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Line(line) if line.color == Rgb::new(0x12, 0x34, 0x56) => Some(line),
            _ => None,
        })
        .expect("retained scatter line");
    assert_eq!(scatter_line.x1, Fixed::from_pixels(10));
    assert_eq!(scatter_line.x2, Fixed::from_pixels(90));

    let mut bubble_nodes = Vec::new();
    push_bubble_chart(
        &mut bubble_nodes,
        plot,
        &series,
        (0.0, 2.0),
        &axis,
        &[],
        false,
        &mut Vec::new(),
        &mut TypographyStats::default(),
        &options,
    )
    .unwrap();
    let bubble_centers = bubble_nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Path(path) => {
                let x_values = path.commands.iter().filter_map(|command| match command {
                    PathCommand::MoveTo { x, .. }
                    | PathCommand::LineTo { x, .. }
                    | PathCommand::QuadraticTo { x, .. }
                    | PathCommand::CubicTo { x, .. } => Some(x.raw()),
                    PathCommand::Close => None,
                });
                let (minimum, maximum) = x_values
                    .fold((i64::MAX, i64::MIN), |(minimum, maximum), value| {
                        (minimum.min(value), maximum.max(value))
                    });
                Some(Fixed::from_raw(minimum + (maximum - minimum) / 2))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        bubble_centers,
        [Fixed::from_pixels(10), Fixed::from_pixels(90)]
    );
}

#[test]
fn chart_category_sampling_preserves_endpoints_without_exceeding_its_bound() {
    for count in 0..=4_096_usize {
        let stride = chart_category_label_stride(count);
        let retained = (0..count)
            .filter(|index| chart_category_label_is_retained(*index, count, stride))
            .collect::<Vec<_>>();
        assert!(retained.len() <= MAX_CHART_CATEGORY_LABELS, "count={count}");
        if count == 0 {
            assert!(retained.is_empty());
        } else {
            assert_eq!(retained.first(), Some(&0), "count={count}");
            assert_eq!(retained.last(), Some(&(count - 1)), "count={count}");
            if count <= MAX_CHART_CATEGORY_LABELS {
                assert_eq!(retained.len(), count, "count={count}");
            }
        }
    }
}

#[test]
fn imported_line_chart_value_axis_keeps_calc_zero_baseline() {
    let mut style = ChartSeriesStyle::default();
    style.marker = ChartMarkerSymbol::None;
    let mut series = [ResolvedChartSeries {
        name: "Imported line".to_string(),
        values: vec![51.0, 64.0, 77.0, 90.0],
        x_values: None,
        labels: vec![
            "Q1".to_string(),
            "Q2".to_string(),
            "Q3".to_string(),
            "Q4".to_string(),
        ],
        bubble_sizes: None,
        style,
    }];
    let authored = chart_nice_value_axis(&series, false).unwrap();
    let imported = chart_calc_imported_line_value_axis(&series).unwrap();
    assert_eq!((authored.minimum, authored.maximum), (45.0, 95.0));
    assert_eq!((imported.minimum, imported.maximum), (0.0, 100.0));
    assert_eq!(imported.ticks, vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);

    for (maximum, expected_major, expected_maximum) in
        [(81.0, 10.0, 90.0), (85.0, 10.0, 90.0), (86.0, 20.0, 100.0)]
    {
        series[0].values = vec![42.0, 55.0, 68.0, maximum];
        let axis = chart_calc_imported_line_value_axis(&series).unwrap();
        assert_eq!(axis.minimum, 0.0, "maximum={maximum}");
        assert_eq!(axis.major, expected_major, "maximum={maximum}");
        assert_eq!(axis.maximum, expected_maximum, "maximum={maximum}");
    }
}

#[test]
fn finite_extreme_chart_axes_fail_closed_before_nonfinite_geometry() {
    let mut style = ChartSeriesStyle::default();
    style.marker = ChartMarkerSymbol::None;
    let constant_extreme_value = [ResolvedChartSeries {
        name: "Constant extreme value".to_string(),
        values: vec![f64::MAX],
        x_values: None,
        labels: vec!["A".to_string()],
        bubble_sizes: None,
        style: style.clone(),
    }];
    assert!(chart_nice_value_axis(&constant_extreme_value, false).is_none());
    let derived_extreme_value_span = [ResolvedChartSeries {
        name: "Derived extreme value span".to_string(),
        values: vec![-f64::MAX, f64::MAX],
        x_values: None,
        labels: vec!["A".to_string(), "B".to_string()],
        bubble_sizes: None,
        style: style.clone(),
    }];
    assert!(chart_nice_value_axis(&derived_extreme_value_span, false).is_none());
    let subnormal_value_span = [ResolvedChartSeries {
        name: "Subnormal value span".to_string(),
        values: vec![0.0, f64::from_bits(1)],
        x_values: None,
        labels: vec!["A".to_string(), "B".to_string()],
        bubble_sizes: None,
        style: style.clone(),
    }];
    assert!(chart_nice_value_axis(&subnormal_value_span, false).is_none());
    let extreme_x = [ResolvedChartSeries {
        name: "Extreme x".to_string(),
        values: vec![1.0, 2.0],
        x_values: Some(vec![-f64::MAX, f64::MAX]),
        labels: Vec::new(),
        bubble_sizes: None,
        style,
    }];
    assert!(chart_nice_x_axis(&extreme_x).is_none());

    let options = RenderOptions::default();
    let rect = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: Fixed::from_pixels(500),
        height: Fixed::from_pixels(300),
    };
    let mut value_workbook = Workbook::new();
    let value_sheet = value_workbook.add_sheet("extreme_value");
    for (row, (category, value)) in [("A", -f64::MAX), ("B", f64::MAX)].into_iter().enumerate() {
        value_sheet.write(row as u32, 0, category);
        value_sheet.write_number(row as u32, 1, value);
    }
    let value_chart = Chart::new(ChartKind::Line, (0, 0), (10, 8)).add_series(
        Series::new("extreme_value!$B$1:$B$2").with_categories("extreme_value!$A$1:$A$2"),
    );
    let mut chart_points = 0;
    assert!(!try_push_chart(
        &mut Vec::new(),
        rect,
        &value_chart,
        None,
        value_sheet,
        &mut chart_points,
        &mut 0,
        &mut 0,
        &mut TypographyStats::default(),
        &options,
        &mut Warnings::default(),
        CellCoordinate { row: 0, col: 0 },
    )
    .unwrap());
    assert_eq!(chart_points, 0);

    let mut x_workbook = Workbook::new();
    let x_sheet = x_workbook.add_sheet("extreme_x");
    for (row, (x, y)) in [(-f64::MAX, 1.0), (f64::MAX, 2.0)].into_iter().enumerate() {
        x_sheet.write_number(row as u32, 0, x);
        x_sheet.write_number(row as u32, 1, y);
    }
    let x_chart = Chart::new(ChartKind::Scatter, (0, 0), (10, 8))
        .add_series(Series::new("extreme_x!$B$1:$B$2").with_categories("extreme_x!$A$1:$A$2"));
    assert!(!try_push_chart(
        &mut Vec::new(),
        rect,
        &x_chart,
        None,
        x_sheet,
        &mut chart_points,
        &mut 0,
        &mut 0,
        &mut TypographyStats::default(),
        &options,
        &mut Warnings::default(),
        CellCoordinate { row: 0, col: 0 },
    )
    .unwrap());
    assert_eq!(chart_points, 0);
}

#[test]
fn pie_and_doughnut_nonfinite_totals_fail_closed_to_placeholders() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("extreme_totals");
    for (row, category) in ["A", "B"].into_iter().enumerate() {
        sheet.write(row as u32, 0, category);
        sheet.write_number(row as u32, 1, f64::MAX);
    }
    let series =
        || Series::new("extreme_totals!$B$1:$B$2").with_categories("extreme_totals!$A$1:$A$2");
    sheet.add_chart(Chart::new(ChartKind::Pie, (0, 2), (10, 8)).add_series(series()));
    sheet.add_chart(Chart::new(ChartKind::Doughnut, (11, 2), (21, 8)).add_series(series()));

    let rect = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: Fixed::from_pixels(500),
        height: Fixed::from_pixels(300),
    };
    let options = RenderOptions::default();
    for chart in sheet.charts() {
        let mut nodes = Vec::new();
        let mut chart_points = 0;
        assert!(!try_push_chart(
            &mut nodes,
            rect,
            chart,
            None,
            sheet,
            &mut chart_points,
            &mut 0,
            &mut 0,
            &mut TypographyStats::default(),
            &options,
            &mut Warnings::default(),
            CellCoordinate { row: 0, col: 0 },
        )
        .unwrap());
        assert_eq!(chart_points, 0);
        assert!(nodes.is_empty());
    }

    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 21, 8)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::ChartPlaceholder && warning.occurrences == 2
    }));
}

#[test]
fn pie_and_doughnut_finite_extreme_totals_normalize_before_multiplication() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("finite_extreme_total");
    sheet.write(0, 0, "A");
    sheet.write_number(0, 1, f64::MAX);
    let series =
        || Series::new("finite_extreme_total!$B$1").with_categories("finite_extreme_total!$A$1");
    let charts = [
        Chart::new(ChartKind::Pie, (0, 0), (10, 8)).add_series(series()),
        Chart::new(ChartKind::Doughnut, (0, 0), (10, 8)).add_series(series()),
    ];
    let rect = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: Fixed::from_pixels(500),
        height: Fixed::from_pixels(300),
    };
    let options = RenderOptions::default();
    for chart in &charts {
        let mut nodes = Vec::new();
        assert!(try_push_chart(
            &mut nodes,
            rect,
            chart,
            None,
            sheet,
            &mut 0,
            &mut 0,
            &mut 0,
            &mut TypographyStats::default(),
            &options,
            &mut Warnings::default(),
            CellCoordinate { row: 0, col: 0 },
        )
        .unwrap());
        assert!(nodes.iter().any(|node| matches!(node, SceneNode::Path(_))));
    }
}

#[test]
fn radar_axes_gridlines_and_labels_obey_visibility_and_bounds() {
    let mut style = ChartSeriesStyle::default();
    style.marker = ChartMarkerSymbol::None;
    let category_count = 128_usize;
    let series = [ResolvedChartSeries {
        name: "Radar".to_string(),
        values: (1..=category_count).map(|value| value as f64).collect(),
        x_values: None,
        labels: (0..category_count)
            .map(|index| format!("C{index:03}"))
            .collect(),
        bubble_sizes: None,
        style,
    }];
    let axis = chart_nice_value_axis(&series, true).unwrap();
    let plot = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: Fixed::from_pixels(400),
        height: Fixed::from_pixels(300),
    };
    let options = RenderOptions::default();
    let warning_cell = CellCoordinate { row: 4, col: 2 };
    let mut nodes = Vec::new();
    let mut category_labels = Vec::new();
    let mut value_labels = Vec::new();
    let mut warnings = Warnings::default();
    push_radar_chart(
        &mut nodes,
        plot,
        &series,
        &axis,
        &[],
        true,
        true,
        false,
        false,
        &mut category_labels,
        &mut value_labels,
        false,
        &mut Vec::new(),
        &mut TypographyStats::default(),
        &options,
        &mut warnings,
        warning_cell,
    )
    .unwrap();
    assert!(category_labels.len() <= MAX_CHART_CATEGORY_LABELS);
    assert_eq!(
        category_labels.first().map(|label| label.text.as_str()),
        Some("C000")
    );
    assert_eq!(
        category_labels.last().map(|label| label.text.as_str()),
        Some("C127")
    );
    assert_eq!(value_labels.len(), axis.ticks.len());
    assert!(!nodes.iter().any(|node| match node {
        SceneNode::Line(line) => line.color == Rgb::new(205, 205, 205),
        SceneNode::Path(path) => path.stroke == Some(Rgb::new(205, 205, 205)),
        _ => false,
    }));
    let retained = category_labels.len();
    assert_eq!(
        warnings.finish(),
        [RenderWarning {
            code: WarningCode::ChartMetadataSimplified,
            occurrences: category_count.saturating_sub(retained) as u64,
            first_cell: Some(warning_cell),
        }]
    );

    let mut hidden_nodes = Vec::new();
    let mut hidden_category_labels = Vec::new();
    let mut hidden_value_labels = Vec::new();
    push_radar_chart(
        &mut hidden_nodes,
        plot,
        &series,
        &axis,
        &[],
        false,
        false,
        true,
        true,
        &mut hidden_category_labels,
        &mut hidden_value_labels,
        false,
        &mut Vec::new(),
        &mut TypographyStats::default(),
        &options,
        &mut Warnings::default(),
        warning_cell,
    )
    .unwrap();
    assert!(hidden_category_labels.is_empty());
    assert!(hidden_value_labels.is_empty());
    assert!(!hidden_nodes.iter().any(|node| match node {
        SceneNode::Line(line) => line.color == Rgb::new(205, 205, 205),
        SceneNode::Path(path) => path.stroke == Some(Rgb::new(205, 205, 205)),
        _ => false,
    }));

    let mut grid_nodes = Vec::new();
    push_radar_chart(
        &mut grid_nodes,
        plot,
        &series,
        &axis,
        &[],
        true,
        true,
        true,
        true,
        &mut Vec::new(),
        &mut Vec::new(),
        false,
        &mut Vec::new(),
        &mut TypographyStats::default(),
        &options,
        &mut Warnings::default(),
        warning_cell,
    )
    .unwrap();
    assert_eq!(
        grid_nodes
            .iter()
            .filter(|node| {
                matches!(
                    node,
                    SceneNode::Line(line) if line.color == Rgb::new(205, 205, 205)
                )
            })
            .count(),
        category_count
    );
    assert!(grid_nodes.iter().any(|node| {
        matches!(
            node,
            SceneNode::Path(path) if path.stroke == Some(Rgb::new(205, 205, 205))
        )
    }));

    let mut limited = options;
    limited.limits.max_path_commands = 16;
    assert_eq!(
        push_radar_chart(
            &mut Vec::new(),
            plot,
            &series,
            &axis,
            &[],
            true,
            true,
            false,
            true,
            &mut Vec::new(),
            &mut Vec::new(),
            false,
            &mut Vec::new(),
            &mut TypographyStats::default(),
            &limited,
            &mut Warnings::default(),
            warning_cell,
        ),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::PathCommands,
            limit: 16,
            actual: 129,
        })
    );
}

#[test]
fn chart_measurement_aggregates_limits_faces_and_warnings() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("chart_accounting");
    for (row, (label, value)) in [("A", 1.0), ("B", 2.0)].into_iter().enumerate() {
        sheet.write(row as u32, 0, label);
        sheet.write_number(row as u32, 1, value);
    }
    let chart = Chart::new(ChartKind::Line, (0, 0), (10, 8))
        .with_title("📈")
        .add_series(
            Series::new("chart_accounting!$B$1:$B$2")
                .with_categories("chart_accounting!$A$1:$A$2")
                .with_name("Legacy series"),
        );
    let mut metadata = DrawingMetadata::default();
    metadata.chart_default_latin_font_family = Some("Legacy Sans".to_string());
    let pack = synthetic_test_pack();
    let options = RenderOptions {
        default_font_family: pack.default_family().to_string(),
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    let rect = Rect {
        x: Fixed::from_pixels(10),
        y: Fixed::from_pixels(20),
        width: Fixed::from_pixels(500),
        height: Fixed::from_pixels(300),
    };
    let warning_cell = CellCoordinate { row: 0, col: 0 };
    let mut nodes = Vec::new();
    let mut chart_points = 0;
    let mut text_bytes = 0;
    let mut glyphs = 0;
    let mut typography = TypographyStats::default();
    let mut warnings = Warnings::default();
    assert!(try_push_chart(
        &mut nodes,
        rect,
        &chart,
        Some(&metadata),
        sheet,
        &mut chart_points,
        &mut text_bytes,
        &mut glyphs,
        &mut typography,
        &options,
        &mut warnings,
        warning_cell,
    )
    .unwrap());

    assert!(typography
        .clone()
        .finish_font_faces()
        .iter()
        .any(|face| face.family == "Wide Sans" && face.substituted));
    let warnings = warnings.finish();
    assert!(warnings
        .iter()
        .any(|warning| warning.code == WarningCode::FontFamilySubstituted));
    assert!(warnings
        .iter()
        .any(|warning| warning.code == WarningCode::MissingGlyph));

    let mut limited = options;
    limited.limits.max_text_runs = 7;
    let style = ResolvedChartTextStyle::for_role(ChartTextRole::AxisLabel, "Legacy Sans");
    let mut limited_typography = TypographyStats::default();
    let mut limited_warnings = Warnings::default();
    assert_eq!(
        max_chart_text_metrics_with_style(
            ["A", "BC"],
            &style,
            &limited,
            &mut limited_typography,
            &mut limited_warnings,
            warning_cell,
        ),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::TextRuns,
            limit: 7,
            actual: 8,
        })
    );
    assert_eq!(limited_typography.text_work, 8);
    assert_eq!(limited_typography.text_lines, 1);
}

#[test]
fn short_chart_legend_columnizes_without_leaving_the_frame() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("legend");
    let mut chart = Chart::new(ChartKind::Line, (0, 0), (4, 14)).with_legend(true);
    for index in 0_u16..12 {
        sheet.write_number(0, index, f64::from(index + 1));
        let column = char::from(b'A' + u8::try_from(index).unwrap());
        chart = chart.add_series(
            Series::new(format!("legend!${column}$1")).with_name(format!("S{index:02}")),
        );
    }
    let rect = Rect {
        x: Fixed::from_pixels(7),
        y: Fixed::from_pixels(11),
        width: Fixed::from_pixels(900),
        height: Fixed::from_pixels(100),
    };
    let options = RenderOptions::default();
    let mut nodes = Vec::new();
    assert!(try_push_chart(
        &mut nodes,
        rect,
        &chart,
        None,
        sheet,
        &mut 0,
        &mut 0,
        &mut 0,
        &mut TypographyStats::default(),
        &options,
        &mut Warnings::default(),
        CellCoordinate { row: 0, col: 0 },
    )
    .unwrap());

    let legend_text = nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Text(text) if text.text.len() == 3 && text.text.starts_with('S') => {
                Some(text)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(legend_text.len(), 12);
    assert!(
        legend_text
            .iter()
            .map(|text| text.bounds.x.raw())
            .collect::<BTreeSet<_>>()
            .len()
            > 1,
        "a short chart must use more than one legend column"
    );
    assert!(legend_text
        .iter()
        .all(|text| rect_contains(rect, text.bounds)));

    let swatches = nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Rect(node)
                if node.rect.width == Fixed::from_pixels(10)
                    && node.rect.height == Fixed::from_pixels(10) =>
            {
                Some(node.rect)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(swatches.len(), 12);
    assert!(swatches
        .iter()
        .copied()
        .all(|swatch| rect_contains(rect, swatch)));
}

#[test]
fn vertical_axis_endpoint_labels_stay_inside_a_short_chart() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("axis_bounds");
    for (row, (label, value)) in [("A", 0.0), ("B", 100.0)].into_iter().enumerate() {
        sheet.write(row as u32, 0, label);
        sheet.write_number(row as u32, 1, value);
    }
    let chart = Chart::new(ChartKind::Line, (0, 0), (4, 8))
        .add_series(Series::new("axis_bounds!$B$1:$B$2").with_categories("axis_bounds!$A$1:$A$2"));
    let rect = Rect {
        x: Fixed::from_pixels(13),
        y: Fixed::from_pixels(17),
        width: Fixed::from_pixels(500),
        height: Fixed::from_pixels(80),
    };
    let options = RenderOptions::default();
    let mut nodes = Vec::new();
    assert!(try_push_chart(
        &mut nodes,
        rect,
        &chart,
        None,
        sheet,
        &mut 0,
        &mut 0,
        &mut 0,
        &mut TypographyStats::default(),
        &options,
        &mut Warnings::default(),
        CellCoordinate { row: 0, col: 0 },
    )
    .unwrap());

    let vertical_axis = nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::Line(line)
                if line.color == Rgb::BLACK && line.x1 == line.x2 && line.y1 < line.y2 =>
            {
                Some(line)
            }
            _ => None,
        })
        .expect("vertical chart axis");
    let mut endpoint_centers = nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Text(text) if text.style.anchor == TextAnchor::End => {
                assert!(rect_contains(rect, text.bounds), "{:?}", text.bounds);
                Some(
                    text.bounds
                        .y
                        .checked_add(Fixed::from_raw(text.bounds.height.raw() / 2))
                        .unwrap(),
                )
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    endpoint_centers.sort_unstable_by_key(|center| center.raw());
    assert_eq!(endpoint_centers.first(), Some(&vertical_axis.y1));
    assert_eq!(endpoint_centers.last(), Some(&vertical_axis.y2));
}

#[test]
fn imported_calc_single_page_chart_padding_tracks_the_chart_width() {
    assert_eq!(
        chart_frame_padding(Fixed::from_pixels(346), true)
            .unwrap()
            .raw(),
        5_038
    );
    assert_eq!(
        chart_frame_padding(Fixed::from_pixels(671), true)
            .unwrap()
            .raw(),
        11_694
    );
    assert_eq!(
        chart_frame_padding(Fixed::from_pixels(671), false).unwrap(),
        Fixed::from_pixels(8)
    );
}

#[test]
fn pinned_missing_theme_chart_roles_resolve_to_the_verified_arimo_alias() {
    let Some(manifest) = std::env::var_os("RXLS_TEST_FONT_PACK_MANIFEST") else {
        return;
    };
    let pack = FontPack::load_manifest(manifest).expect("load pinned render font pack");
    for role in [
        ChartTextRole::ChartTitle,
        ChartTextRole::AxisTitle,
        ChartTextRole::AxisLabel,
        ChartTextRole::Legend,
        ChartTextRole::DataLabel,
    ] {
        let request = role.resolved_style(CALC_MISSING_THEME_CHART_LATIN_FAMILY);
        let resolution = pack.resolve(request.request());
        assert!(!resolution.exact_family, "{role:?}");
        assert!(resolution.declared_alias, "{role:?}");
        assert!(resolution.exact_style, "{role:?}");
        let identity = pack.selected_face_identity(resolution.id).unwrap();
        assert_eq!(identity.family, "Arimo", "{role:?}");
        assert_eq!(identity.weight, if role.bold() { 700 } else { 400 });
    }
}

#[test]
fn chart_text_rotation_bounds_are_exact_for_representative_angles() {
    let metrics = ChartTextMetrics {
        width: Fixed::from_raw(100),
        height: Fixed::from_raw(40),
    };
    for (angle, width, height) in [
        (0, 100, 40),
        (30, 107, 85),
        (45, 99, 99),
        (90, 40, 100),
        (135, 99, 99),
    ] {
        assert_eq!(
            metrics.rotated(angle).unwrap(),
            ChartTextMetrics {
                width: Fixed::from_raw(width),
                height: Fixed::from_raw(height),
            },
            "angle {angle}"
        );
    }
}

#[test]
fn chart_text_rotation_bounds_normalize_turns_and_negative_angles() {
    let metrics = ChartTextMetrics {
        width: Fixed::from_raw(137),
        height: Fixed::from_raw(53),
    };
    for equivalent in [390, -330] {
        assert_eq!(metrics.rotated(equivalent), metrics.rotated(30));
    }
    for equivalent in [-30, 330, -390] {
        assert_eq!(metrics.rotated(equivalent), metrics.rotated(30));
    }
    for equivalent in [495, -225] {
        assert_eq!(metrics.rotated(equivalent), metrics.rotated(135));
    }
}

#[test]
fn chart_text_rotation_coefficients_saturate_and_extents_fail_closed() {
    assert_eq!(CHART_TEXT_SINE_Q62_CEIL[0], 0);
    assert_eq!(
        u128::from(CHART_TEXT_SINE_Q62_CEIL[90]),
        CHART_TEXT_TRIG_SCALE
    );
    assert!(CHART_TEXT_SINE_Q62_CEIL
        .windows(2)
        .all(|pair| pair[0] < pair[1]));
    assert!(CHART_TEXT_SINE_Q62_CEIL
        .iter()
        .all(|coefficient| u128::from(*coefficient) <= CHART_TEXT_TRIG_SCALE));

    let maximum_width = ChartTextMetrics {
        width: Fixed::from_raw(i64::MAX),
        height: Fixed::ZERO,
    };
    assert_eq!(maximum_width.rotated(0).unwrap(), maximum_width);
    assert_eq!(
        maximum_width.rotated(90).unwrap(),
        ChartTextMetrics {
            width: Fixed::ZERO,
            height: Fixed::from_raw(i64::MAX),
        }
    );
    assert!(maximum_width.rotated(1).is_ok());

    let maximum_square = ChartTextMetrics {
        width: Fixed::from_raw(i64::MAX),
        height: Fixed::from_raw(i64::MAX),
    };
    assert_eq!(
        maximum_square.rotated(45),
        Err(RenderError::CoordinateOverflow)
    );
    assert_eq!(
        ChartTextMetrics {
            width: Fixed::from_raw(-1),
            height: Fixed::ZERO,
        }
        .rotated(30),
        Err(RenderError::CoordinateOverflow)
    );
}

fn imported_rtl_circle_chart() -> Workbook {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (name, body) in [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Render" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetViews><sheetView rightToLeft="1"/></sheetViews><sheetData>
              <row r="1"><c r="A1" t="inlineStr"><is><t>Q1</t></is></c><c r="B1"><v>10</v></c></row>
              <row r="2"><c r="A2" t="inlineStr"><is><t>Q2</t></is></c><c r="B2"><v>23</v></c></row>
              <row r="3"><c r="A3" t="inlineStr"><is><t>Q3</t></is></c><c r="B3"><v>36</v></c></row>
              <row r="4"><c r="A4" t="inlineStr"><is><t>Q4</t></is></c><c r="B4"><v>49</v></c></row>
            </sheetData><drawing r:id="rIdDraw"/></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
        ),
        (
            "xl/drawings/drawing1.xml",
            r#"<wsDr><twoCellAnchor><from><col>0</col><row>7</row></from><to><col>6</col><row>18</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#,
        ),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
        ),
        (
            "xl/charts/chart1.xml",
            r#"<chartSpace><chart><plotArea><lineChart><ser><idx val="0"/><order val="0"/>
              <marker><symbol val="circle"/><size val="3"/></marker>
              <cat><strRef><f>Render!$A$1:$A$4</f></strRef></cat>
              <val><numRef><f>Render!$B$1:$B$4</f></numRef></val>
            </ser><axId val="1"/><axId val="2"/></lineChart>
            <catAx><axId val="1"/><crossAx val="2"/></catAx>
            <valAx><axId val="2"/><crossAx val="1"/></valAx>
            </plotArea></chart></chartSpace>"#,
        ),
    ] {
        writer.start_file(name, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    Workbook::open(&writer.finish().unwrap().into_inner()).expect("imported RTL circle chart")
}

#[test]
fn rtl_chart_continuation_keeps_signed_tile_coordinates() {
    let workbook = imported_rtl_circle_chart();
    assert_eq!(workbook.sheets[0].charts().len(), 1);

    let rows = (0_u32..=18)
        .map(|index| MeasuredAxisSlot {
            index,
            offset: Fixed::from_pixels(i64::from(index) * 20),
            size: Fixed::from_pixels(20),
        })
        .collect::<Vec<_>>();
    let columns = (0_u16..=6)
        .map(|index| MeasuredAxisSlot {
            index,
            offset: Fixed::from_pixels(i64::from(index) * 64),
            size: Fixed::from_pixels(64),
        })
        .collect::<Vec<_>>();
    let options = outlined_options(RenderRange::new(1, 0, 8, 2));
    let build = build_sheet_scene_with_geometry(
        &workbook.sheets[0],
        0,
        &options,
        SheetGeometryOverride::new(&rows, &columns),
    )
    .expect("a continued chart may retain negative pre-clip coordinates");

    assert!(build
        .report
        .warnings
        .iter()
        .all(|warning| warning.code != WarningCode::ChartPlaceholder));
    let chart_group = build
        .scene
        .nodes
        .iter()
        .find_map(|node| match node {
            SceneNode::ClipGroup(group)
                if group.nodes.iter().any(|node| {
                    matches!(
                        node,
                        SceneNode::Path(path)
                            if path.commands.iter().filter(|command| {
                                matches!(command, PathCommand::CubicTo { .. })
                            }).count() == 4
                    )
                }) =>
            {
                Some(group)
            }
            _ => None,
        })
        .expect("the continued imported chart must retain its clipped circle markers");
    assert!(chart_group.clip.x.raw() >= 0);
    let circle_markers = chart_group
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Path(path)
                if path
                    .commands
                    .iter()
                    .filter(|command| matches!(command, PathCommand::CubicTo { .. }))
                    .count()
                    == 4 =>
            {
                Some(path)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(circle_markers.len(), 4);
    assert!(
        circle_markers
            .iter()
            .flat_map(|path| &path.commands)
            .any(|command| match command {
                PathCommand::MoveTo { x, .. }
                | PathCommand::LineTo { x, .. }
                | PathCommand::QuadraticTo { x, .. }
                | PathCommand::CubicTo { x, .. } => x.raw() < 0,
                PathCommand::Close => false,
            }),
        "a real circle marker must retain a negative pre-clip x coordinate"
    );
}

#[test]
fn chart_coordinate_conversion_is_signed_and_fail_closed() {
    assert_eq!(pixels_as_fixed(0.0).unwrap(), Fixed::ZERO);
    assert_eq!(
        pixels_as_fixed(-0.25).unwrap(),
        Fixed::from_raw(-FIXED_UNITS_PER_PIXEL / 4)
    );
    assert_eq!(
        pixels_as_fixed(-9_007_199_254_740_992.0).unwrap(),
        Fixed::from_raw(i64::MIN)
    );
    assert_eq!(
        pixels_as_fixed(9_007_199_254_740_991.0).unwrap(),
        Fixed::from_raw(i64::MAX - (FIXED_UNITS_PER_PIXEL - 1))
    );

    for invalid in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        9_007_199_254_740_992.0,
        -9_007_199_254_740_994.0,
    ] {
        assert_eq!(
            pixels_as_fixed(invalid),
            Err(RenderError::CoordinateOverflow)
        );
    }
    for boundary in [i64::MIN, i64::MAX] {
        assert_eq!(
            interpolate_fixed(Fixed::from_raw(boundary), Fixed::ZERO, 0.0).unwrap(),
            Fixed::from_raw(boundary)
        );
    }
    assert_eq!(
        interpolate_fixed(Fixed::from_raw(i64::MIN), Fixed::from_raw(i64::MAX), 1.0).unwrap(),
        Fixed::from_raw(-1)
    );
    assert_eq!(
        interpolate_fixed(
            Fixed::from_raw((1_i64 << 62) - 1),
            Fixed::from_raw((1_i64 << 62) + 1),
            1.0
        ),
        Err(RenderError::CoordinateOverflow)
    );
    for (start, extent, ratio) in [
        (i64::MAX, 1, 1.0),
        (i64::MIN, -1, 1.0),
        (i64::MAX, 1, 0.5),
        (i64::MIN, -1, 0.5),
        (0, 1_i64 << 62, 2.0),
    ] {
        assert_eq!(
            interpolate_fixed(Fixed::from_raw(start), Fixed::from_raw(extent), ratio),
            Err(RenderError::CoordinateOverflow)
        );
    }
    for ratio in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            interpolate_fixed(Fixed::ZERO, Fixed::ZERO, ratio),
            Err(RenderError::CoordinateOverflow)
        );
    }
}

#[test]
fn clipped_circle_chart_marker_keeps_negative_coordinates() {
    let mut nodes = Vec::new();
    let mut typography_stats = TypographyStats::default();
    let mut style = ChartSeriesStyle::default();
    style.marker = ChartMarkerSymbol::Circle;
    style.marker_size = Some(3);

    push_chart_marker(
        &mut nodes,
        pixels_as_fixed(-188.25).unwrap(),
        Fixed::from_pixels(12),
        Rgb::new(0x12, 0x34, 0x56),
        &style,
        &mut typography_stats,
        &RenderOptions::default(),
    )
    .expect("a clipped circular marker may retain negative pre-clip coordinates");

    let SceneNode::Path(path) = &nodes[0] else {
        panic!("a circular marker must render as a path");
    };
    assert!(path.commands.iter().any(|command| match command {
        PathCommand::MoveTo { x, .. }
        | PathCommand::LineTo { x, .. }
        | PathCommand::QuadraticTo { x, .. }
        | PathCommand::CubicTo { x, .. } => x.raw() < 0,
        PathCommand::Close => false,
    }));
}

#[test]
fn retained_emu_width_drives_line_scatter_and_area_scene_strokes() {
    let mut style = ChartSeriesStyle::default();
    style.marker = ChartMarkerSymbol::None;
    style.line_color = Some(Color::rgb(0x12, 0x34, 0x56));
    style.line_width_emu = Some(19_050);
    let series = [ResolvedChartSeries {
        name: "Series".to_string(),
        values: vec![1.0, 2.0],
        x_values: Some(vec![1.0, 2.0]),
        labels: vec!["A".to_string(), "B".to_string()],
        bubble_sizes: None,
        style,
    }];
    let plot = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: Fixed::from_pixels(100),
        height: Fixed::from_pixels(100),
    };
    let options = RenderOptions::default();

    let mut line_nodes = Vec::new();
    push_line_chart(
        &mut line_nodes,
        plot,
        &series,
        (0.0, 2.0),
        false,
        &[],
        false,
        &mut Vec::new(),
        &mut TypographyStats::default(),
        &options,
    )
    .unwrap();
    assert!(line_nodes.iter().any(|node| {
        matches!(
            node,
            SceneNode::Line(line)
                if line.color == Rgb::new(0x12, 0x34, 0x56)
                    && line.width == Fixed::from_pixels(2)
        )
    }));

    let mut scatter_nodes = Vec::new();
    let x_axis = chart_nice_x_axis(&series).unwrap();
    push_scatter_chart(
        &mut scatter_nodes,
        plot,
        &series,
        (0.0, 2.0),
        &x_axis,
        &[],
        false,
        &mut Vec::new(),
        &mut TypographyStats::default(),
        &options,
    )
    .unwrap();
    assert!(scatter_nodes.iter().any(|node| {
        matches!(
            node,
            SceneNode::Line(line)
                if line.color == Rgb::new(0x12, 0x34, 0x56)
                    && line.width == Fixed::from_pixels(2)
        )
    }));

    let mut area_nodes = Vec::new();
    push_area_chart(
        &mut area_nodes,
        plot,
        &series,
        (0.0, 2.0),
        false,
        &[],
        false,
        &mut Vec::new(),
        &mut TypographyStats::default(),
        &options,
    )
    .unwrap();
    assert!(area_nodes.iter().any(|node| {
        matches!(
            node,
            SceneNode::Path(path)
                if path.stroke == Some(Rgb::new(0x12, 0x34, 0x56))
                    && path.stroke_width == Fixed::from_pixels(2)
        )
    }));
}

#[test]
fn shifted_category_positions_use_band_centers_and_legacy_positions_keep_endpoints() {
    let shifted = (0..4)
        .map(|index| chart_category_ratio(index, 4, true))
        .collect::<Vec<_>>();
    assert_eq!(shifted, [0.125, 0.375, 0.625, 0.875]);

    let legacy = (0..4)
        .map(|index| chart_category_ratio(index, 4, false))
        .collect::<Vec<_>>();
    assert_eq!(legacy, [0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]);
    assert_eq!(chart_category_ratio(0, 1, true), 0.5);
    assert_eq!(chart_category_ratio(0, 1, false), 0.5);
}

#[test]
fn shifted_category_data_plot_keeps_calc_band_inset() {
    let plot = Rect {
        x: Fixed::from_pixels(10),
        y: Fixed::from_pixels(20),
        width: Fixed::from_pixels(100),
        height: Fixed::from_pixels(40),
    };
    let shifted = chart_category_data_plot(plot, true, false).unwrap();
    assert_eq!(shifted.x, Fixed::from_pixels(13));
    assert_eq!(shifted.y, plot.y);
    assert_eq!(shifted.width, Fixed::from_pixels(94));
    assert_eq!(shifted.height, plot.height);
    assert_eq!(chart_category_data_plot(plot, false, false).unwrap(), plot);
    assert_eq!(chart_category_data_plot(plot, true, true).unwrap(), plot);
}

#[test]
fn imported_default_line_chart_keeps_calc_vertical_plot_insets() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("line_plot_insets");
    for (row, (category, value)) in [("Low", 0.0), ("High", 100.0)].into_iter().enumerate() {
        sheet.write(row as u32, 0, category);
        sheet.write_number(row as u32, 1, value);
    }
    let series =
        || Series::new("line_plot_insets!$B$1:$B$2").with_categories("line_plot_insets!$A$1:$A$2");
    let default_chart = Chart::new(ChartKind::Line, (0, 0), (10, 8)).add_series(series());
    let labeled_chart = Chart::new(ChartKind::Line, (0, 0), (10, 8))
        .with_data_labels(true)
        .add_series(series());
    let mut metadata = DrawingMetadata::default();
    metadata.chart_category_axis_shifted = Some(true);
    metadata.chart_category_axis_visible = Some(true);
    metadata.chart_value_axis_visible = Some(true);
    metadata.chart_value_major_gridlines = Some(true);
    let mut series_style = ChartSeriesStyle::default();
    series_style.marker = ChartMarkerSymbol::Circle;
    series_style.marker_size = Some(5);
    metadata.chart_series_styles.push(series_style);
    let rect = Rect {
        x: Fixed::from_pixels(10),
        y: Fixed::from_pixels(20),
        width: Fixed::from_pixels(500),
        height: Fixed::from_pixels(300),
    };
    let render = |chart: &Chart, rect: Rect, calc_single_page_layout: bool| {
        let mut nodes = Vec::new();
        assert!(try_push_chart_with_layout(
            &mut nodes,
            rect,
            chart,
            Some(&metadata),
            calc_single_page_layout,
            sheet,
            &mut 0,
            &mut 0,
            &mut 0,
            &mut TypographyStats::default(),
            &RenderOptions::default(),
            &mut Warnings::default(),
            CellCoordinate { row: 0, col: 0 },
        )
        .unwrap());
        nodes
    };
    let plot_bounds = |nodes: &[SceneNode]| {
        let mut gridlines = nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Line(line)
                    if line.color == Rgb::new(217, 217, 217) && line.y1 == line.y2 =>
                {
                    Some(line.y1)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        gridlines.sort_unstable_by_key(|y| y.raw());
        (
            *gridlines.first().expect("top value gridline"),
            *gridlines.last().expect("bottom value gridline"),
        )
    };

    let (default_top, default_bottom) = plot_bounds(&render(&default_chart, rect, false));
    let (labeled_top, labeled_bottom) = plot_bounds(&render(&labeled_chart, rect, false));
    assert_eq!(
        default_top,
        labeled_top.checked_add(Fixed::from_pixels(7)).unwrap()
    );
    assert_eq!(
        default_bottom,
        labeled_bottom.checked_sub(Fixed::from_pixels(4)).unwrap()
    );

    let (default_top, default_bottom) = plot_bounds(&render(&default_chart, rect, true));
    let (labeled_top, labeled_bottom) = plot_bounds(&render(&labeled_chart, rect, true));
    assert_eq!(
        default_top,
        labeled_top.checked_add(Fixed::from_pixels(4)).unwrap()
    );
    assert_eq!(
        default_bottom,
        labeled_bottom.checked_sub(Fixed::from_pixels(1)).unwrap()
    );

    let compact_rect = Rect {
        width: Fixed::from_pixels(400),
        ..rect
    };
    let (default_top, default_bottom) = plot_bounds(&render(&default_chart, compact_rect, true));
    let (labeled_top, labeled_bottom) = plot_bounds(&render(&labeled_chart, compact_rect, true));
    assert_eq!(
        default_top,
        labeled_top.checked_add(Fixed::from_pixels(7)).unwrap()
    );
    assert_eq!(
        default_bottom,
        labeled_bottom.checked_sub(Fixed::from_pixels(4)).unwrap()
    );
}

#[test]
fn imported_bar_chart_renders_real_geometry() {
    let mut authored = Workbook::new();
    let sheet = authored.add_sheet("imported_bar");
    for (row, (label, value)) in [("A", 2.0), ("B", 5.0), ("C", 3.0)].into_iter().enumerate() {
        sheet.write(row as u32, 0, label);
        sheet.write_number(row as u32, 1, value);
    }
    sheet.add_chart(
        Chart::new(ChartKind::Bar, (0, 3), (10, 9))
            .with_title("Imported columns")
            .add_series(
                Series::new("imported_bar!$B$1:$B$3").with_categories("imported_bar!$A$1:$A$3"),
            ),
    );
    let imported = Workbook::open(&authored.to_xlsx()).expect("reopen authored chart");
    assert_ne!(imported.sheets[0].style_fidelity(), StyleFidelity::Authored);
    let mut resolved_points = 0;
    assert_eq!(
        resolve_numeric_a1_range(
            &imported.sheets[0],
            "imported_bar!$B$1:$B$3",
            &mut resolved_points,
            &RenderOptions::default(),
            true,
        )
        .unwrap(),
        Some(vec![2.0, 5.0, 3.0])
    );
    assert_eq!(
        resolve_label_a1_range(
            &imported.sheets[0],
            "imported_bar!$A$1:$A$3",
            &mut resolved_points,
            &RenderOptions::default(),
        )
        .unwrap(),
        Some(vec!["A".into(), "B".into(), "C".into()])
    );
    let build = build_scene(
        &imported,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 10, 9)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert!(
        !build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::ChartPlaceholder),
        "charts={:?} report={:?}",
        imported.sheets[0].charts(),
        build.report
    );
    assert!(build.scene.nodes.iter().any(|node| matches!(
        node,
        SceneNode::Rect(RectNode {
            fill: Some(Rgb {
                red: 68,
                green: 114,
                blue: 196,
            }),
            ..
        })
    )));
}

#[test]
fn imported_cross_sheet_chart_uses_complete_cache_and_theme_palette() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Host" r:id="rId1"/><sheet name="Data" r:id="rId2"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="worksheets/sheet2.xml"/><Relationship Id="rIdTheme" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/></Relationships>"#,
        ),
        (
            "xl/theme/theme1.xml",
            r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:accent1><a:srgbClr val="123456"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme><a:majorFont><a:latin typeface="Liberation Sans"/></a:majorFont><a:minorFont><a:latin typeface="Liberation Sans"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData/><drawing r:id="rIdDraw"/></worksheet>"#,
        ),
        (
            "xl/worksheets/sheet2.xml",
            r#"<worksheet><sheetData/></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
        ),
        (
            "xl/drawings/drawing1.xml",
            r#"<wsDr><twoCellAnchor><from><col>0</col><row>0</row></from><to><col>8</col><row>12</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#,
        ),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
        ),
        (
            "xl/charts/chart1.xml",
            r#"<chartSpace><chart><plotArea><barChart><barDir val="bar"/><ser><idx val="0"/><order val="0"/><tx><strRef><f>Data!$C$1</f><strCache><pt idx="0"><v>Cached revenue</v></pt></strCache></strRef></tx><cat><strRef><f>Data!$A$1:$A$3</f><strCache><pt idx="0"><v>A</v></pt><pt idx="1"><v>B</v></pt><pt idx="2"><v>C</v></pt></strCache></strRef></cat><val><numRef><f>Data!$B$1:$B$3</f><numCache><pt idx="0"><v>2</v></pt><pt idx="1"><v>5</v></pt><pt idx="2"><v>3</v></pt></numCache></numRef></val></ser><axId val="1"/><axId val="2"/></barChart><catAx><axId val="1"/><crossAx val="2"/></catAx><valAx><axId val="2"/><crossAx val="1"/></valAx></plotArea><legend/></chart></chartSpace>"#,
        ),
    ];
    for (name, body) in parts {
        writer.start_file(name, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();
    let workbook = Workbook::open(&bytes).expect("cached chart workbook");
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("chart sidecar");
    assert_eq!(metadata.chart_bar_direction, ChartBarDirection::Horizontal);
    assert_eq!(
        metadata.chart_default_latin_font_family.as_deref(),
        Some(CALC_MISSING_THEME_CHART_LATIN_FAMILY)
    );
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 12, 8)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert!(
        !build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::ChartPlaceholder),
        "chart reasons: {:?}",
        metadata.chart_unsupported_reasons
    );
    assert!(build.scene.nodes.iter().any(|node| match node {
        SceneNode::Text(text) => {
            text.text == "Cached revenue"
                && text.style.family == CALC_MISSING_THEME_CHART_LATIN_FAMILY
        }
        _ => false,
    }));
    let horizontal_bars = build
        .scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Rect(RectNode {
                rect,
                fill:
                    Some(Rgb {
                        red: 0x12,
                        green: 0x34,
                        blue: 0x56,
                    }),
                ..
            }) if rect.width > rect.height => Some(*rect),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(horizontal_bars.len(), 3);
}

#[test]
fn unsupported_imported_chart_constructs_are_explicit_placeholders() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Host" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="2"><c r="A2"><v>2</v></c></row></sheetData><drawing r:id="rIdDraw"/></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
        ),
        (
            "xl/drawings/drawing1.xml",
            r#"<wsDr><twoCellAnchor><from><col>0</col><row>0</row></from><to><col>8</col><row>12</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#,
        ),
        (
            "xl/drawings/_rels/drawing1.xml.rels",
            r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
        ),
        (
            "xl/charts/chart1.xml",
            r#"<chartSpace><pivotSource/><externalData/><chart><view3D/><plotArea><barChart><ser><val><numRef><f>Host!$A$1:$A$2</f></numRef></val></ser></barChart><lineChart><ser><val><numRef><f>Host!$A$1:$A$2</f></numRef></val></ser></lineChart></plotArea></chart></chartSpace>"#,
        ),
    ];
    for (name, body) in parts {
        writer.start_file(name, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    let workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
    let metadata = workbook.sheets[0]
        .drawing_metadata()
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("retained unsupported chart");
    let expected_reasons = [
        rxls::ChartUnsupportedReason::Combo,
        rxls::ChartUnsupportedReason::ThreeDimensional,
        rxls::ChartUnsupportedReason::Pivot,
        rxls::ChartUnsupportedReason::ExternalData,
        rxls::ChartUnsupportedReason::UnsupportedAxisTopology,
        rxls::ChartUnsupportedReason::UnsupportedPlotSemantics,
    ];
    assert_eq!(
        metadata.chart_unsupported_reasons.len(),
        expected_reasons.len(),
        "unexpected reasons: {:?}",
        metadata.chart_unsupported_reasons
    );
    for reason in expected_reasons {
        assert!(
            metadata.chart_unsupported_reasons.contains(&reason),
            "missing {reason:?} in {:?}",
            metadata.chart_unsupported_reasons
        );
    }
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 12, 8)),
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::ChartPlaceholder && warning.occurrences == 1
    }));
}

#[test]
fn used_and_single_page_bounds_expand_to_visible_drawing_anchors() {
    let mut workbook = Workbook::new();
    workbook
        .add_sheet("drawing-only")
        .add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (5, 3)).with_to((15, 7)));
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();
    assert_eq!(build.report.range, RenderRange::new(5, 3, 15, 7));
    assert_eq!(build.scene.width, Fixed::from_pixels(320));
    assert_eq!(build.scene.height, Fixed::from_pixels(220));
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::ImagePlaceholder && warning.occurrences == 1
    }));
    assert!(!build
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::DrawingAnchorUnavailable));

    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            single_page_sheets: true,
            render: RenderOptions {
                gridlines: false,
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(document.pages[0].scene.width, Fixed::from_raw(327_732));
    assert_eq!(document.pages[0].scene.height, Fixed::from_raw(225_325));
    assert_eq!(document.report.source.range, RenderRange::new(5, 3, 15, 7));
}

#[test]
fn imported_two_cell_drawing_end_markers_use_the_last_visibly_occupied_cell() {
    for kind in [DrawingObjectKind::Image, DrawingObjectKind::Chart] {
        for (offset, expected) in [
            ((0, 0), RenderRange::new(3, 2, 6, 4)),
            ((1, 0), RenderRange::new(3, 2, 6, 5)),
            ((0, 1), RenderRange::new(3, 2, 7, 4)),
            ((1, 1), RenderRange::new(3, 2, 7, 5)),
        ] {
            let workbook = imported_two_cell_drawing(kind, offset);
            let build = build_scene(
                &workbook,
                0,
                &RenderOptions {
                    gridlines: false,
                    ..RenderOptions::default()
                },
            )
            .unwrap();
            assert_eq!(build.report.range, expected, "{kind:?} at {offset:?}");
        }
    }
}

#[test]
fn calc_ooxml_closed_terminal_boundaries_round_trip_through_mm100_exactly() {
    for (twips, mm100, closed_twips) in [
        (5_185, 9_146, 5_185),
        (6_222, 10_975, 6_221),
        (9_028, 15_924, 9_027),
        (10_065, 17_754, 10_065),
    ] {
        assert_eq!(round_unsigned_ratio(twips, 127, 72), Some(mm100));
        assert_eq!(round_unsigned_ratio(mm100 - 1, 72, 127), Some(closed_twips));
    }
}

#[test]
fn single_page_ooxml_zero_offset_terminal_columns_follow_calc_physical_bounds_only() {
    let pack = synthetic_test_pack();
    let render_options = RenderOptions {
        gridlines: false,
        default_font_family: pack.default_family().to_string(),
        // The synthetic face has a 600/1000 digit advance, yielding the
        // hosted Noto-equivalent 8_336 raw / 122-twip digit width.
        default_font_size: Fixed::from_raw(13_893),
        font_pack: Some(pack),
        ..RenderOptions::default()
    };
    for (
        hidden_terminal_column,
        explicit_prefix_widths,
        ordinary_last_column,
        single_page_last_column,
        label,
    ) in [
        (true, false, 4, 6, "hidden implicit F"),
        (false, false, 5, 5, "visible implicit F"),
        (true, true, 4, 4, "hidden explicit-prefix F"),
        (false, true, 5, 6, "visible explicit-prefix F"),
    ] {
        let workbook = imported_single_page_terminal_column_drawing(
            hidden_terminal_column,
            explicit_prefix_widths,
            0,
            6,
        );
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.implicit_ooxml_column_width(), Some(None), "{label}");
        assert_eq!(sheet.xlsb_default_column_width(), None, "{label}");
        assert!(sheet.xlsb_column_widths_256().is_empty(), "{label}");
        let metadata = &sheet.drawing_metadata()[0];
        assert_eq!(metadata.from_cell, Some((0, 0)), "{label}");
        assert_eq!(metadata.to_cell, Some((1, 6)), "{label}");
        assert_eq!(metadata.to_offset_emu, Some((0, 0)), "{label}");

        let ordinary = build_scene(&workbook, 0, &render_options).unwrap();
        assert_eq!(
            ordinary.report.range,
            RenderRange::new(0, 0, 0, ordinary_last_column),
            "ordinary Used bounds changed for {label}"
        );
        let single_page = build_print_document(
            &workbook,
            0,
            &PrintOptions {
                single_page_sheets: true,
                render: render_options.clone(),
                ..PrintOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            single_page.report.source.range,
            RenderRange::new(0, 0, 0, single_page_last_column),
            "{label}"
        );
        assert_eq!(
            single_page.report.pages[0].body_range, single_page.report.source.range,
            "{label}"
        );
        if !hidden_terminal_column && explicit_prefix_widths {
            assert_eq!(
                ordinary.report.range,
                RenderRange::new(0, 0, 0, 5),
                "visible explicit-prefix ordinary bounds remain A:F"
            );
            assert_eq!(
                single_page.report.source.range,
                RenderRange::new(0, 0, 0, 6),
                "visible explicit-prefix SinglePageSheets bounds expand to A:G"
            );
            assert_eq!(
                single_page.pages[0].scene.width,
                Fixed::from_raw(757_947),
                "visible explicit-prefix SinglePageSheets canvas is 555.136962890625 pt"
            );
        }
        let single_rect =
            shape_placeholder_rect(&single_page.pages[0].scene.nodes).expect("single-page shape");
        let ordinary_rect = shape_placeholder_rect(&ordinary.scene.nodes).expect("ordinary shape");
        assert_eq!(single_rect.x, ordinary_rect.x, "{label}");
        assert_eq!(single_rect.y, ordinary_rect.y, "{label}");
        assert_eq!(single_rect.height, ordinary_rect.height, "{label}");
        if explicit_prefix_widths {
            let mut visible_twips = 2_196_i128 + 4 * 1_708;
            if !hidden_terminal_column {
                visible_twips += 1_037;
            }
            let native_width = calc_twips_position_to_fixed(visible_twips).unwrap();
            let native_width = if hidden_terminal_column {
                calc_inclusive_rectangle_extent(native_width).unwrap()
            } else {
                native_width
            };
            assert_eq!(
                single_rect.width, native_width,
                "OOXML explicit prefixes must use rounded default-font digit twips for {label}"
            );
        } else {
            let visible_columns = if hidden_terminal_column { 5 } else { 6 };
            assert_eq!(
                ordinary_rect.width,
                Fixed::from_raw(70_793 * visible_columns),
                "ordinary layout must retain per-track rounding for {label}"
            );
            let native_width = calc_twips_position_to_fixed(
                i128::from(visible_columns).checked_mul(1_037).unwrap(),
            )
            .unwrap();
            let native_width = if hidden_terminal_column {
                native_width
            } else {
                calc_inclusive_rectangle_extent(native_width).unwrap()
            };
            assert_eq!(
                single_rect.width, native_width,
                "single-page layout must round the cumulative Calc source width once for {label}"
            );
        }
    }
}

#[test]
fn single_page_ooxml_physical_bounds_keep_sparse_high_columns_within_span_limits() {
    let pack = synthetic_test_pack();
    let workbook = imported_single_page_terminal_column_drawing(false, false, 1_024, 1_030);
    let document = build_print_document(
        &workbook,
        0,
        &PrintOptions {
            single_page_sheets: true,
            render: RenderOptions {
                gridlines: false,
                default_font_family: pack.default_family().to_string(),
                default_font_size: Fixed::from_raw(13_893),
                font_pack: Some(pack),
                limits: RenderLimits {
                    max_columns: 6,
                    ..RenderLimits::default()
                },
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        document.report.source.range,
        RenderRange::new(0, 1_024, 0, 1_029)
    );
}

#[test]
fn hidden_axes_keep_move_only_size_but_shrink_move_and_size_used_bounds() {
    for kind in [
        DrawingObjectKind::Image,
        DrawingObjectKind::Chart,
        DrawingObjectKind::Shape,
    ] {
        for right_to_left in [false, true] {
            let move_and_size = imported_hidden_two_cell_drawing(kind, false, right_to_left);
            let metadata = &move_and_size.sheets[0].drawing_metadata()[0];
            assert_eq!(metadata.behavior, DrawingAnchorBehavior::MoveAndSize);
            assert_eq!(metadata.to_offset_emu, Some((0, 0)));
            assert_eq!(metadata.absolute_size_emu, None);
            let resized = build_scene(
                &move_and_size,
                0,
                &RenderOptions {
                    gridlines: false,
                    ..RenderOptions::default()
                },
            )
            .unwrap();
            assert_eq!(
                resized.report.range,
                RenderRange::new(3, 2, 4, 2),
                "{kind:?} MoveAndSize rtl={right_to_left}"
            );
            assert_eq!(resized.scene.width, Fixed::from_pixels(64));
            assert_eq!(resized.scene.height, Fixed::from_pixels(40));

            let move_only = imported_hidden_two_cell_drawing(kind, true, right_to_left);
            let metadata = &move_only.sheets[0].drawing_metadata()[0];
            assert_eq!(metadata.behavior, DrawingAnchorBehavior::MoveOnly);
            assert_eq!(metadata.to_offset_emu, Some((0, 0)));
            assert_eq!(metadata.absolute_size_emu, Some((1_619_250, 666_750)));
            let fixed = build_scene(
                &move_only,
                0,
                &RenderOptions {
                    gridlines: false,
                    ..RenderOptions::default()
                },
            )
            .unwrap();
            assert_eq!(
                fixed.report.range,
                RenderRange::new(3, 2, 9, 7),
                "{kind:?} MoveOnly rtl={right_to_left}"
            );
            assert_eq!(fixed.scene.width, Fixed::from_pixels(192));
            assert_eq!(fixed.scene.height, Fixed::from_pixels(80));
            assert_eq!(
                fixed_drawing_outer_rect(&fixed.scene.nodes),
                Rect {
                    x: Fixed::from_pixels(if right_to_left { 22 } else { 0 }),
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(170),
                    height: Fixed::from_pixels(70),
                }
            );

            let (drawing_extents, has_absolute) = prepared_drawing_geometry_extent(
                &move_only.sheets[0],
                &[RenderRange::new(8, 6, 9, 7)],
                &RenderOptions {
                    gridlines: false,
                    ..RenderOptions::default()
                },
            )
            .unwrap();
            assert!(!has_absolute);
            assert_eq!(
                drawing_extents,
                vec![RenderRange::new(3, 2, 9, 7)],
                "the print continuation after the raw F8 marker must retain full geometry"
            );

            let document = build_print_document(
                &move_only,
                0,
                &PrintOptions {
                    single_page_sheets: true,
                    render: RenderOptions {
                        gridlines: false,
                        ..RenderOptions::default()
                    },
                    ..PrintOptions::default()
                },
            )
            .unwrap();
            assert_eq!(document.report.source.range, RenderRange::new(3, 2, 9, 7));
            assert_eq!(document.pages[0].scene.width, Fixed::from_raw(190_493));
            assert_eq!(document.pages[0].scene.height, Fixed::from_raw(81_933));
            assert_eq!(
                fixed_drawing_outer_rect(&document.pages[0].scene.nodes),
                Rect {
                    x: if right_to_left {
                        Fixed::from_raw(16_413)
                    } else {
                        Fixed::ZERO
                    },
                    y: Fixed::ZERO,
                    width: Fixed::from_pixels(170),
                    height: Fixed::from_pixels(70),
                },
                "SinglePageSheets must reflect the fixed-size object inside Calc's cumulative imported width"
            );
        }
    }
}

#[test]
fn used_bounds_render_cell_anchored_shapes_as_explicit_placeholders() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let parts = [
        (
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="Shapes" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData/><drawing r:id="rIdDraw"/></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
        ),
        (
            "xl/drawings/drawing1.xml",
            r#"<wsDr><twoCellAnchor><from><col>1</col><row>2</row></from><to><col>4</col><row>5</row></to><sp><nvSpPr><cNvPr id="1" name="Callout"/></nvSpPr></sp></twoCellAnchor></wsDr>"#,
        ),
    ];
    for (name, body) in parts {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    let workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
    let build = build_scene(
        &workbook,
        0,
        &RenderOptions {
            gridlines: false,
            ..RenderOptions::default()
        },
    )
    .unwrap();

    assert_eq!(build.report.range, RenderRange::new(2, 1, 5, 4));
    assert!(build.report.warnings.iter().any(|warning| {
        warning.code == WarningCode::ShapePlaceholder && warning.occurrences == 1
    }));
    assert!(!build
        .report
        .warnings
        .iter()
        .any(|warning| warning.code == WarningCode::ShapeAnchorUnavailable));
    assert!(build.scene.nodes.iter().any(|node| matches!(
        node,
        SceneNode::Rect(RectNode {
            fill: Some(Rgb {
                red: 221,
                green: 235,
                blue: 247,
            }),
            ..
        })
    )));
}

#[test]
fn single_page_native_rows_use_global_hidden_prefix_and_final_dimension() {
    let workbook = imported_xlsx(
        "<styleSheet/>",
        r#"<worksheet><sheetFormatPr defaultRowHeight="12.85"/><sheetData><row r="2" hidden="1"/><row r="3"><c r="A3" t="inlineStr"><is><t>visible</t></is></c></row></sheetData></worksheet>"#,
    );
    let sheet = &workbook.sheets[0];
    let selected = RenderRange::new(2, 0, 2, 0);
    let base = RenderOptions {
        selection: RenderSelection::Range(selected),
        gridlines: false,
        default_column_width: Fixed::from_pixels(1),
        ..RenderOptions::default()
    };

    let ordinary = build_sheet_scene(sheet, 0, &base).unwrap();
    let single_page = build_single_page_sheet_scene(sheet, 0, &base).unwrap();
    assert_eq!(ordinary.scene.height, Fixed::from_raw(17_545));
    assert_eq!(
        single_page.scene.height,
        Fixed::from_raw(17_610),
        "row 3 must inherit the cumulative endpoint phase of visible row 1, skip hidden row 2, and retain the inclusive rectangle unit"
    );

    let tight = RenderOptions {
        limits: RenderLimits {
            max_dimension_raw: 17_571,
            ..RenderLimits::default()
        },
        ..base.clone()
    };
    assert_eq!(
        build_single_page_sheet_scene(sheet, 0, &tight),
        Err(RenderError::LimitExceeded {
            kind: LimitKind::Dimension,
            limit: 17_571,
            actual: 17_610,
        })
    );
    assert_eq!(
        build_sheet_scene(sheet, 0, &tight).unwrap().scene.height,
        Fixed::from_raw(17_545)
    );

    let full_range = RenderRange::new(0, 0, 2, 0);
    let full = RenderOptions {
        selection: RenderSelection::Range(full_range),
        include_hidden: true,
        ..base
    };
    let ordinary = build_sheet_scene(sheet, 0, &full).unwrap();
    let single_page = build_single_page_sheet_scene(sheet, 0, &full).unwrap();
    assert_eq!(ordinary.scene.height, Fixed::from_raw(52_635));
    assert_eq!(single_page.scene.height, Fixed::from_raw(52_674));

    let measured = measure_sheet_axes_for_ranges(sheet, &[full_range], &full).unwrap();
    assert_eq!(
        measured[0]
            .0
            .iter()
            .map(|slot| slot.size)
            .collect::<Vec<_>>(),
        vec![Fixed::from_raw(17_545); 3],
        "prepared paginated geometry must remain per-track Fixed"
    );
    let replay = build_sheet_scene_with_geometry(
        sheet,
        0,
        &full,
        SheetGeometryOverride::new(&measured[0].0, &measured[0].1),
    )
    .unwrap();
    assert_eq!(replay.scene.height, Fixed::from_raw(52_635));
}

#[test]
fn single_page_native_row_baselines_precede_merged_auto_height() {
    let pack = synthetic_test_pack();
    let family = pack.default_family();
    let styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let text = "한글中文한글中文한글中文한글中文한글中文";
    let unmerged = imported_xlsx(
        &styles,
        &format!(
            r#"<worksheet><sheetFormatPr defaultRowHeight="12.85" defaultColWidth="2"/><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{text}</t></is></c></row></sheetData></worksheet>"#
        ),
    );
    let merged = imported_xlsx(
        &styles,
        &format!(
            r#"<worksheet><sheetFormatPr defaultRowHeight="12.85" defaultColWidth="2"/><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{text}</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:A3"/></mergeCells></worksheet>"#
        ),
    );
    let render = |workbook: &Workbook, range| {
        build_single_page_sheet_scene(
            &workbook.sheets[0],
            0,
            &RenderOptions {
                selection: RenderSelection::Range(range),
                gridlines: false,
                default_font_family: family.to_string(),
                font_pack: Some(pack.clone()),
                ..RenderOptions::default()
            },
        )
        .unwrap()
        .scene
        .height
    };
    let required = render(&unmerged, RenderRange::new(0, 0, 0, 0));
    let merged_height = render(&merged, RenderRange::new(0, 0, 2, 0));
    assert!(required > Fixed::from_raw(52_634));
    assert_eq!(
        merged_height, required,
        "native baseline residuals must be resolved before the merged-row deficit"
    );
}

#[test]
fn imported_row_manuality_controls_single_page_auto_height_expansion() {
    let pack = synthetic_test_pack();
    let family = pack.default_family();
    let styles = format!(
        r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
    );
    let text = "한글中文한글中文한글中文한글中文한글中文";
    let height = |default_custom: bool, row_height: Option<&str>, row_custom: Option<&str>| {
        let row_height = row_height
            .map(|height| format!(r#" ht="{height}""#))
            .unwrap_or_default();
        let row_custom = row_custom
            .map(|value| format!(r#" customHeight="{value}""#))
            .unwrap_or_default();
        let workbook = imported_xlsx(
            &styles,
            &format!(
                r#"<worksheet><sheetFormatPr defaultRowHeight="12.85" customHeight="{}" defaultColWidth="2"/><sheetData><row r="1"{row_height}{row_custom}><c r="A1" s="1" t="inlineStr"><is><t>{text}</t></is></c></row></sheetData></worksheet>"#,
                u8::from(default_custom)
            ),
        );
        build_single_page_sheet_scene(
            &workbook.sheets[0],
            0,
            &RenderOptions {
                gridlines: false,
                default_font_family: family.to_string(),
                font_pack: Some(pack.clone()),
                ..RenderOptions::default()
            },
        )
        .unwrap()
        .scene
        .height
    };

    let manual_row = height(false, Some("12.85"), Some("1"));
    let automatic_row = height(false, Some("12.85"), Some("0"));
    assert!(automatic_row > manual_row);
    assert_eq!(
        height(false, Some("12.85"), None),
        automatic_row,
        "an absent customHeight flag retains a cached automatic baseline"
    );
    assert_eq!(
        height(true, None, None),
        manual_row,
        "a manual default fixes rows without an explicit cached height"
    );
    assert_eq!(
        height(true, Some("12.85"), Some("0")),
        automatic_row,
        "an explicit automatic row overrides an inherited manual default"
    );
}

#[test]
fn single_page_fixed_extent_and_absolute_origin_share_native_boundaries() {
    let workbook = imported_xlsx(
        "<styleSheet/>",
        r#"<worksheet><sheetFormatPr defaultRowHeight="12.85"/><sheetData/></worksheet>"#,
    );
    let sheet = &workbook.sheets[0];
    let options = RenderOptions::default();
    let maximum_digit_width = Fixed::from_pixels(7);

    assert_eq!(
        fixed_size_used_row(
            sheet,
            0,
            0,
            489_598,
            maximum_digit_width,
            &options,
            AxisEndpointPolicy::PerTrackFixed,
        )
        .unwrap(),
        2
    );
    assert_eq!(
        fixed_size_used_row(
            sheet,
            0,
            0,
            489_598,
            maximum_digit_width,
            &options,
            AxisEndpointPolicy::SourceNative,
        )
        .unwrap(),
        2,
        "drawing anchors and SinglePageSheets share the same cumulative twip endpoints before the page rectangle's terminal inclusive unit"
    );

    let range = RenderRange::new(3, 0, 3, 0);
    let mut warnings = Warnings::default();
    assert_eq!(
        sheet_grid_origin_with_policy(
            sheet,
            range,
            maximum_digit_width,
            &options,
            AxisEndpointPolicy::PerTrackFixed,
            &mut warnings,
        )
        .unwrap()
        .1,
        Fixed::from_raw(52_635)
    );
    let mut warnings = Warnings::default();
    assert_eq!(
        sheet_grid_origin_with_policy(
            sheet,
            range,
            maximum_digit_width,
            &options,
            AxisEndpointPolicy::SourceNative,
            &mut warnings,
        )
        .unwrap()
        .1,
        Fixed::from_raw(52_635)
    );
}

#[test]
fn source_axis_ratio_variants_quantize_to_calc_twips() {
    assert_eq!(
        imported_axis_measure_twips(
            ImportedAxisMeasure::PointRatio(15, 1),
            Fixed::from_pixels(7),
        ),
        Some(300)
    );
    assert_eq!(
        imported_axis_measure_twips(
            ImportedAxisMeasure::CharacterWidthRatio(843, 100),
            Fixed::from_pixels(7),
        ),
        Some(885)
    );
}

#[test]
fn source_character_widths_follow_format_specific_calc_importers() {
    const NOTO_11_MDW: Fixed = Fixed::from_raw(8_336);
    const CARLITO_11_MDW: Fixed = Fixed::from_raw(7_612);

    assert_eq!(
        imported_axis_measure_twips(
            ImportedAxisMeasure::CharacterWidth256(18 * 256),
            NOTO_11_MDW,
        ),
        Some(2_195),
        "BIFF truncates width times digit twips after subtracting one half"
    );
    assert_eq!(
        imported_axis_measure_twips(ImportedAxisMeasure::CharacterWidthRatio(18, 1), NOTO_11_MDW,),
        Some(2_196),
        "OOXML rounds width times digit twips without BIFF's half-twip bias"
    );
    assert_eq!(
        imported_axis_measure_twips(ImportedAxisMeasure::DigitWidth256(18 * 256), CARLITO_11_MDW,),
        Some(1_998),
        "XLSB uses the rounded standard-digit width"
    );
    assert_eq!(
        imported_axis_measure_twips(
            ImportedAxisMeasure::CharacterBaseWidth256(8 * 256),
            NOTO_11_MDW,
        ),
        Some(1_051),
        "OOXML base widths add five 96-DPI screen pixels"
    );
}

#[test]
fn exact_character_widths_match_legacy_digit_pixel_quantization() {
    for maximum_digit_width in [Fixed::from_raw(7_612), Fixed::from_raw(8_336)] {
        for (numerator, denominator) in [(2_160_u64, 256_u64), (843, 100)] {
            let characters = numerator as f32 / denominator as f32;
            let expected = column_chars_to_fixed(
                characters,
                maximum_digit_width,
                IMPORTED_COLUMN_PADDING_PIXELS,
            )
            .unwrap();
            let pixels = character_width_ratio_to_pixels(
                numerator,
                denominator,
                maximum_digit_width,
                IMPORTED_COLUMN_PADDING_PIXELS,
            )
            .unwrap();
            assert_eq!(
                pixels * i128::from(FIXED_UNITS_PER_PIXEL),
                i128::from(expected.raw()),
                "{numerator}/{denominator} characters at {maximum_digit_width:?}"
            );
        }
    }
}

#[test]
fn source_native_endpoints_follow_calc_cumulative_twip_boundaries() {
    fn sizes(measure: ImportedAxisMeasure, count: u64, prefix_count: u64) -> Vec<(i64, i64)> {
        let contribution = imported_axis_measure_twips(measure, Fixed::from_pixels(7)).unwrap();
        let prefix = contribution.checked_mul(i128::from(prefix_count)).unwrap();
        let mut cursor = SourceAxisCursor::new(prefix).unwrap();
        (0..count)
            .map(|_| {
                let (offset, size, _) = cursor.advance(contribution).unwrap();
                (offset.raw(), size.raw())
            })
            .collect()
    }

    assert_eq!(
        sizes(ImportedAxisMeasure::Twips(280), 3, 0),
        [(0, 19_119), (19_119, 19_119), (38_238, 19_119)]
    );
    assert_eq!(
        sizes(ImportedAxisMeasure::MillimeterHundredths(2_000), 3, 0),
        [(0, 77_405), (77_405, 77_443), (154_848, 77_405)]
    );
    assert_eq!(
        sizes(ImportedAxisMeasure::DigitWidth256(2_432), 4, 0),
        [
            (0, 68_116),
            (68_116, 68_155),
            (136_271, 68_116),
            (204_387, 68_116),
        ]
    );
    assert_eq!(
        sizes(ImportedAxisMeasure::Twips(280), 2, 1),
        [(0, 19_119), (19_119, 19_119)],
        "a nonzero selection must retain the global rounding phase"
    );

    for measure in [
        ImportedAxisMeasure::Twips(280),
        ImportedAxisMeasure::MillimeterHundredths(2_000),
        ImportedAxisMeasure::PointRatio(14, 1),
        ImportedAxisMeasure::CharacterWidth256(2_048),
        ImportedAxisMeasure::CharacterWidthRatio(843, 100),
        ImportedAxisMeasure::CharacterBaseWidth256(2_048),
        ImportedAxisMeasure::DigitWidth256(2_432),
        ImportedAxisMeasure::DigitBaseWidth256(2_048),
    ] {
        assert!(
            imported_axis_measure_twips(measure, Fixed::from_pixels(7))
                .is_some_and(|twips| twips > 0),
            "missing exact conversion for {measure:?}"
        );
    }
}

#[test]
fn calc_single_page_fit_adds_twip_padding_and_truncates_each_track() {
    let source_twips = [964_i128, 1_757, 1_247, 2_296, 765, 1_644];
    let slots = source_twips
        .iter()
        .enumerate()
        .map(|(index, _)| MeasuredAxisSlot {
            index: u16::try_from(index).unwrap(),
            offset: Fixed::ZERO,
            size: Fixed::from_pixels(1),
        })
        .collect::<Vec<_>>();
    let page_extent = calc_hmm_to_fixed(15_299).unwrap();
    let mut cell_slots = slots.clone();
    let mut grid_slots = slots;

    apply_calc_single_page_axis_fit(
        &mut cell_slots,
        &source_twips,
        page_extent,
        &RenderOptions::default(),
    )
    .unwrap();
    apply_calc_single_page_metafile_grid_axis_fit(&mut grid_slots, &source_twips).unwrap();

    let to_hmm = |value: Fixed| {
        round_positive_mul_div(
            i128::from(value.raw()),
            635,
            24_i128 * i128::from(FIXED_UNITS_PER_PIXEL),
        )
        .unwrap()
    };
    let boundaries = cell_slots
        .iter()
        .map(|slot| to_hmm(slot.offset.checked_add(slot.size).unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(boundaries, [1_696, 4_788, 6_982, 11_022, 12_368, 15_261]);
    let grid_boundaries = grid_slots
        .iter()
        .map(|slot| slot.offset.checked_add(slot.size).unwrap().raw())
        .collect::<Vec<_>>();
    assert_eq!(
        grid_boundaries,
        [230_279, 650_064, 947_937, 1_496_407, 1_679_141, 2_071_834],
        "DrawToDev must truncate each native-twip track before 84/635 projection"
    );
    assert_eq!(to_hmm(page_extent), 15_299);
    assert_eq!(15_299 - boundaries.last().copied().unwrap(), 38);
}

#[test]
fn calc_metafile_grid_truncates_tracks_before_accumulating_hmm() {
    let source_twips = [343_i128, 343, 276, 276];
    let mut slots = source_twips
        .iter()
        .enumerate()
        .map(|(index, _)| MeasuredAxisSlot {
            index: u32::try_from(index).unwrap(),
            offset: Fixed::ZERO,
            size: Fixed::from_pixels(1),
        })
        .collect::<Vec<_>>();

    apply_calc_single_page_metafile_grid_axis_fit(&mut slots, &source_twips).unwrap();

    let projected = slots
        .iter()
        .map(|slot| slot.offset.checked_add(slot.size).unwrap().raw())
        .collect::<Vec<_>>();
    assert_eq!(projected, [81_952, 163_905, 229_737, 295_570]);

    let mut cumulative_twips = 0_i128;
    let cumulative_endpoint_projection = source_twips
        .iter()
        .map(|twips| {
            cumulative_twips += twips;
            calc_metafile_grid_hmm_to_fixed(calc_twips_position_to_hmm(cumulative_twips).unwrap())
                .unwrap()
                .raw()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        cumulative_endpoint_projection,
        [81_952, 163_905, 229_873, 295_841]
    );
    assert_ne!(projected, cumulative_endpoint_projection);
}
