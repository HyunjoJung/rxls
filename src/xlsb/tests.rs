use super::{
    apply_col_outline, apply_row_outline, built_in_xlsb_num_fmt, decode_cell,
    drawing_relationship_target, parse_brt_extern_sheets, parse_dval, parse_external_defined_names,
    parse_header_footer, parse_sheet, parse_styles, parse_supporting_link_rel_ids, parse_workbook,
    parse_xlsb_drawing_refs, parse_xlsb_page_break, parse_xlsb_theme, read_sheet_drawings,
    verified_xlsb_collection_count, BrtFormulaDefinitions, RecReader, SheetReadMetadata, Styles,
    XlsbDrawingKind, XlsbPageBreakAxis, XlsbTheme, BRT_AC_BEGIN, BRT_AC_END, BRT_ARR_FMLA,
    BRT_BEGIN_CELL_STYLE_XFS, BRT_BEGIN_CELL_XFS, BRT_BEGIN_COL_BRK, BRT_BEGIN_FONTS,
    BRT_BEGIN_HEADER_FOOTER, BRT_BEGIN_RW_BRK, BRT_BEGIN_STYLES, BRT_BEGIN_STYLE_SHEET,
    BRT_BEGIN_SUP_BOOK, BRT_BEGIN_WS_VIEW, BRT_BORDER, BRT_BRK, BRT_BUNDLE_SH, BRT_CELL_BLANK,
    BRT_CELL_ISST, BRT_CELL_REAL, BRT_CELL_ST, BRT_COL_INFO, BRT_END_CELL_STYLE_XFS,
    BRT_END_CELL_XFS, BRT_END_COL_BRK, BRT_END_FONTS, BRT_END_RW_BRK, BRT_END_STYLES,
    BRT_END_STYLE_SHEET, BRT_END_SUP_BOOK, BRT_END_WS_VIEW, BRT_EXTERN_SHEET, BRT_FILL,
    BRT_FMLA_NUM, BRT_FMLA_STRING, BRT_FMT, BRT_FONT, BRT_MARGINS, BRT_NAME, BRT_PAGE_SETUP,
    BRT_PRINT_OPTIONS, BRT_ROW_HDR, BRT_SHEET_PROTECTION, BRT_SHR_FMLA, BRT_SST_ITEM, BRT_STYLE,
    BRT_SUP_ADDIN, BRT_SUP_BOOK_SRC, BRT_SUP_NAME_START, BRT_SUP_SAME, BRT_SUP_SELF, BRT_WB_PROP,
    BRT_WS_FMT_INFO, BRT_WS_PROP, BRT_XF, MAX_DVAL_RANGES, MAX_VERIFIED_XLSB_STYLE_RECORDS,
    MAX_XLSB_COL_INDEX, MAX_XLSB_FONT_RECORDS, MAX_XLSB_ROW_HEIGHT_TWIPS, MAX_XLSB_ROW_INDEX,
    MAX_XLSB_STYLE_RECORDS, XLSB_APPLICATION_DEFAULT_COL_WIDTH_256,
};
use crate::{
    format, Alignment, BorderStyle, Cell, CellProtection, CellStyle, Color, DrawingAnchorBehavior,
    DrawingCrop, DrawingObjectKind, FormatPattern, FormatScript, HAlign, ImportedAxisMeasure,
    OoxmlImplicitRowHeight, PrintLossKind, PrintPageOrder, SheetMetadata, SheetType, SheetVisible,
    StyleLossKind, VAlign, Workbook, XlsbDefaultColumnWidth,
};
use std::collections::BTreeMap;
use std::io::Write;
use zip::write::SimpleFileOptions;

fn complete_theme_xml(major_latin: &str, minor_latin: &str) -> String {
    format!(
        r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme><a:majorFont><a:latin typeface="{major_latin}"/></a:majorFont><a:minorFont><a:latin typeface="{minor_latin}"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#
    )
}

/// `XLWideString`: `cch:u32` + UTF-16LE chars.
fn wstr(s: &str) -> Vec<u8> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut v = (units.len() as u32).to_le_bytes().to_vec();
    for u in units {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v
}

fn null_wstr() -> Vec<u8> {
    0xFFFF_FFFFu32.to_le_bytes().to_vec()
}

/// Frame a BIFF12 record: var-uint type, var-uint size, payload.
fn rec(rt: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    if rt < 0x80 {
        v.push(rt as u8);
    } else {
        v.push((rt & 0x7F) as u8 | 0x80);
        v.push((rt >> 7) as u8 & 0x7F);
    }
    let mut sz = payload.len();
    loop {
        let mut b = (sz & 0x7F) as u8;
        sz >>= 7;
        if sz > 0 {
            b |= 0x80;
        }
        v.push(b);
        if sz == 0 {
            break;
        }
    }
    v.extend_from_slice(payload);
    v
}

fn xf(numfmt: u16) -> Vec<u8> {
    let mut v = vec![0u8; 16];
    v[2..4].copy_from_slice(&numfmt.to_le_bytes());
    v
}

fn provenance_font(name: &str, height_twips: u16) -> Vec<u8> {
    let mut font = vec![0_u8; 21];
    font[0..2].copy_from_slice(&height_twips.to_le_bytes());
    font[4..6].copy_from_slice(&400_u16.to_le_bytes());
    font[20] = 2;
    font.extend_from_slice(&wstr(name));
    font
}

fn provenance_xf(parent: u16, font: u16, changed_groups: u16) -> Vec<u8> {
    let mut xf = vec![0_u8; 16];
    xf[0..2].copy_from_slice(&parent.to_le_bytes());
    xf[4..6].copy_from_slice(&font.to_le_bytes());
    xf[14..16].copy_from_slice(&changed_groups.to_le_bytes());
    xf
}

fn provenance_style(xf_index: u32, flags: u16, builtin_id: u8, name: Option<&str>) -> Vec<u8> {
    let mut style = xf_index.to_le_bytes().to_vec();
    style.extend_from_slice(&flags.to_le_bytes());
    style.push(builtin_id);
    style.push(u8::MAX);
    style.extend_from_slice(&name.map_or_else(null_wstr, wstr));
    style
}

fn complete_provenance_styles_with_normal_name(
    second_font_twips: u16,
    normal_name: Option<&str>,
) -> Vec<u8> {
    let mut styles = rec(BRT_BEGIN_STYLE_SHEET, &[]);
    styles.extend_from_slice(&rec(BRT_BEGIN_FONTS, &2_u32.to_le_bytes()));
    styles.extend_from_slice(&rec(BRT_FONT, &provenance_font("Calibri", 220)));
    styles.extend_from_slice(&rec(
        BRT_FONT,
        &provenance_font("Exact direct", second_font_twips),
    ));
    styles.extend_from_slice(&rec(BRT_END_FONTS, &[]));
    styles.extend_from_slice(&rec(BRT_BEGIN_CELL_STYLE_XFS, &1_u32.to_le_bytes()));
    styles.extend_from_slice(&rec(BRT_XF, &provenance_xf(u16::MAX, 0, 0)));
    styles.extend_from_slice(&rec(BRT_END_CELL_STYLE_XFS, &[]));
    styles.extend_from_slice(&rec(BRT_BEGIN_CELL_XFS, &2_u32.to_le_bytes()));
    styles.extend_from_slice(&rec(BRT_XF, &provenance_xf(0, 0, 0)));
    styles.extend_from_slice(&rec(BRT_XF, &provenance_xf(0, 1, 1 << 1)));
    styles.extend_from_slice(&rec(BRT_END_CELL_XFS, &[]));
    styles.extend_from_slice(&rec(BRT_BEGIN_STYLES, &1_u32.to_le_bytes()));
    styles.extend_from_slice(&rec(BRT_STYLE, &provenance_style(0, 1, 0, normal_name)));
    styles.extend_from_slice(&rec(BRT_END_STYLES, &[]));
    styles.extend_from_slice(&rec(BRT_END_STYLE_SHEET, &[]));
    styles
}

fn complete_provenance_styles(second_font_twips: u16) -> Vec<u8> {
    complete_provenance_styles_with_normal_name(second_font_twips, Some("Normal"))
}

#[test]
fn record_framing_var_uints() {
    // A 2-byte record type (156 = BrtBundleSh) and small size round-trip.
    let r = rec(BRT_BUNDLE_SH, &[1, 2, 3]);
    let mut rr = RecReader::new(&r);
    let (rt, p) = rr.next().unwrap();
    assert_eq!(rt, BRT_BUNDLE_SH);
    assert_eq!(p, &[1, 2, 3]);
    assert!(rr.next().is_none());
}

#[test]
fn xlsb_supporting_link_relationships_preserve_non_external_slots() {
    let mut workbook = rec(BRT_SUP_SELF, &[]);
    workbook.extend_from_slice(&rec(BRT_SUP_BOOK_SRC, &wstr("rIdExternal")));
    workbook.extend_from_slice(&rec(BRT_SUP_SAME, &[]));
    workbook.extend_from_slice(&rec(BRT_SUP_ADDIN, &[]));

    assert_eq!(
        parse_supporting_link_rel_ids(&workbook),
        vec![None, Some("rIdExternal".to_string()), None, None]
    );
}

#[test]
fn xlsb_external_link_names_follow_sup_name_start_order() {
    let mut external_link = rec(BRT_SUP_NAME_START, &wstr("OutsideBook"));
    let mut begin = 0u16.to_le_bytes().to_vec(); // sbt: external workbook
    begin.extend_from_slice(&wstr("rIdPath"));
    begin.extend_from_slice(&null_wstr());
    external_link.extend_from_slice(&rec(BRT_BEGIN_SUP_BOOK, &begin));
    external_link.extend_from_slice(&rec(BRT_SUP_NAME_START, &wstr("External.Rate_β")));
    external_link.extend_from_slice(&rec(BRT_SUP_NAME_START, &[1]));
    external_link.extend_from_slice(&rec(BRT_SUP_NAME_START, &wstr("Second_Name")));
    external_link.extend_from_slice(&rec(BRT_END_SUP_BOOK, &[]));
    external_link.extend_from_slice(&rec(BRT_SUP_NAME_START, &wstr("AfterBook")));

    assert_eq!(
        parse_external_defined_names(&external_link),
        vec![
            "External.Rate_β".to_string(),
            String::new(),
            "Second_Name".to_string()
        ]
    );
}

#[test]
fn xlsb_namex_resolves_names_from_external_link_parts() {
    let external_name = "External.Rate_β";

    // The XTI points at supporting-link slot 1. Slot 0 is deliberately a
    // self-link to prove the loader retains non-external index positions.
    let mut workbook = rec(BRT_SUP_SELF, &[]);
    workbook.extend_from_slice(&rec(BRT_SUP_BOOK_SRC, &wstr("rIdExternal")));
    let mut extern_sheet = 1u32.to_le_bytes().to_vec();
    extern_sheet.extend_from_slice(&1u32.to_le_bytes());
    extern_sheet.extend_from_slice(&(-2i32).to_le_bytes());
    extern_sheet.extend_from_slice(&(-2i32).to_le_bytes());
    workbook.extend_from_slice(&rec(BRT_EXTERN_SHEET, &extern_sheet));

    let namex = [0x39, 0, 0, 1, 0, 0, 0];
    let mut defined_name = 0u32.to_le_bytes().to_vec();
    defined_name.push(0); // chKey
    defined_name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // workbook scope
    defined_name.extend_from_slice(&wstr("Imported"));
    defined_name.extend_from_slice(&(namex.len() as u32).to_le_bytes());
    defined_name.extend_from_slice(&namex);
    defined_name.extend_from_slice(&0u32.to_le_bytes()); // formula cb
    defined_name.extend_from_slice(&null_wstr()); // comment
    workbook.extend_from_slice(&rec(BRT_NAME, &defined_name));

    let mut bundle = vec![0u8; 8];
    bundle.extend_from_slice(&wstr("rIdSheet"));
    bundle.extend_from_slice(&wstr("Data"));
    workbook.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));

    let mut external_begin = 0u16.to_le_bytes().to_vec();
    external_begin.extend_from_slice(&wstr("rIdPath"));
    external_begin.extend_from_slice(&null_wstr());
    let mut external_link = rec(BRT_BEGIN_SUP_BOOK, &external_begin);
    external_link.extend_from_slice(&rec(BRT_SUP_NAME_START, &wstr(external_name)));
    external_link.extend_from_slice(&rec(587, &[])); // BrtSupNameEnd
    external_link.extend_from_slice(&rec(BRT_END_SUP_BOOK, &[]));

    let mut sheet = rec(BRT_ROW_HDR, &0u32.to_le_bytes());
    sheet.extend_from_slice(&rec(
        BRT_FMLA_NUM,
        &brt_numeric_formula(0, 12.5, &namex, &[]),
    ));
    let imported = [0x23, 1, 0, 0, 0]; // PtgName: workbook BrtName 1
    sheet.extend_from_slice(&rec(
        BRT_FMLA_NUM,
        &brt_numeric_formula(1, 12.5, &imported, &[]),
    ));

    let workbook_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdSheet" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/><Relationship Id="rIdExternal" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLink" Target="externalLinks/externalLink1.bin"/></Relationships>"#;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", workbook.as_slice()),
        ("xl/_rels/workbook.bin.rels", workbook_rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        (
            "xl/externalLinks/externalLink1.bin",
            external_link.as_slice(),
        ),
    ] {
        writer.start_file(path, options).unwrap();
        writer.write_all(body).unwrap();
    }

    let workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
    let external_formula = format!("[ixti:0]!{external_name}");
    assert_eq!(
        workbook.defined_names(),
        &[("Imported".to_string(), external_formula.clone())]
    );
    assert_eq!(
        workbook.sheets[0].cell(0, 0),
        Some(&Cell::Formula {
            formula: external_formula,
            cached: Box::new(Cell::Number(12.5)),
        })
    );
    assert_eq!(
        workbook.evaluate_cell("Data", 0, 0),
        crate::FormulaEvaluation::Fallback {
            cached: Cell::Number(12.5),
            reason: crate::FormulaUnsupportedReason::ExternalRef,
        }
    );
    assert_eq!(
        workbook.evaluate_cell("Data", 0, 1),
        crate::FormulaEvaluation::Fallback {
            cached: Cell::Number(12.5),
            reason: crate::FormulaUnsupportedReason::ExternalRef,
        }
    );
}

#[test]
fn reads_a_synthetic_xlsb() {
    // workbook.bin: one BrtBundleSh(rId1, "시트1").
    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1")); // strRelID (non-null)
    wb_bin.extend_from_slice(&wstr("시트1")); // strName
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    // sharedStrings.bin: BrtSSTItem("품목").
    let mut item = vec![1u8]; // flags: rich string
    item.extend_from_slice(&wstr("품목"));
    item.extend_from_slice(&2u32.to_le_bytes()); // two StrRun boundaries
    item.extend_from_slice(&0u32.to_le_bytes());
    item.extend_from_slice(&1u16.to_le_bytes()); // font index (not exposed safely)
    item.extend_from_slice(&1u32.to_le_bytes());
    item.extend_from_slice(&2u16.to_le_bytes());
    let sst = rec(BRT_SST_ITEM, &item);

    // sheet1.bin: RowHdr(0), CellIsst(0,0 → isst 0), CellReal(0,1 → 42.0).
    let mut sh = rec(BRT_ROW_HDR, &[0, 0, 0, 0]);
    let mut isst = vec![0u8; 8]; // col=0, styleRef/flags
    isst.extend_from_slice(&0u32.to_le_bytes()); // isst = 0
    sh.extend_from_slice(&rec(BRT_CELL_ISST, &isst));
    let mut real = vec![1, 0, 0, 0, 0, 0, 0, 0]; // col=1, styleRef
    real.extend_from_slice(&42.0f64.to_le_bytes());
    sh.extend_from_slice(&rec(BRT_CELL_REAL, &real));

    // The workbook base is `/xl/workbook.bin`; RFC 3986 removes both
    // parent segments, clamps at `/`, and then resolves the sheet below
    // `/xl/worksheets`.
    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="../../xl/worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (name, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/sharedStrings.bin", sst.as_slice()),
        ("xl/worksheets/sheet1.bin", sh.as_slice()),
    ] {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 1);
    assert_eq!(wb.sheets[0].name, "시트1");
    assert_eq!(
        wb.sheets[0].cell(0, 0),
        Some(&Cell::Text("품목".to_string()))
    );
    assert_eq!(wb.sheets[0].cell(0, 1), Some(&Cell::Number(42.0)));
    assert_eq!(wb.sheets[0].dimensions(), Some((0, 0, 0, 1)));
    assert_eq!(wb.sheets[0].default_column_width(), None);
    assert_eq!(wb.sheets[0].implicit_ooxml_column_width(), Some(None));
    assert_eq!(
        wb.sheets[0].xlsb_default_column_width(),
        Some(XlsbDefaultColumnWidth::ApplicationDefault)
    );
    assert_eq!(
        wb.sheets[0].imported_default_column_axis_measure(),
        Some(ImportedAxisMeasure::DigitWidth256(
            XLSB_APPLICATION_DEFAULT_COL_WIDTH_256
        ))
    );
    assert_eq!(wb.sheets[0].default_row_height(), None);
    assert!(wb.sheets[0].has_implicit_ooxml_row_height());
    assert_eq!(
        wb.sheets[0].implicit_ooxml_row_height_source(),
        Some(OoxmlImplicitRowHeight::XlsbApplicationDefault)
    );
    assert_eq!(
        wb.sheets[0]
            .rich_text_runs(0, 0)
            .expect("rich boundaries")
            .iter()
            .map(|run| run.text.as_str())
            .collect::<Vec<_>>(),
        ["품", "목"]
    );
}

#[test]
fn external_xlsb_worksheet_relationship_does_not_load_a_local_part() {
    let mut bundle = vec![0u8; 8];
    bundle.extend_from_slice(&wstr("rId1"));
    bundle.extend_from_slice(&wstr("External"));
    let workbook = rec(BRT_BUNDLE_SH, &bundle);

    let mut sheet = rec(BRT_ROW_HDR, &[0, 0, 0, 0]);
    let mut number = vec![0u8; 8];
    number.extend_from_slice(&42.0f64.to_le_bytes());
    sheet.extend_from_slice(&rec(BRT_CELL_REAL, &number));
    let relationships = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin" TargetMode="External"/></Relationships>"#;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", workbook.as_slice()),
        ("xl/_rels/workbook.bin.rels", relationships.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        writer.start_file(path, options).unwrap();
        writer.write_all(body).unwrap();
    }

    let workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
    assert_eq!(workbook.sheets[0].sheet_type(), SheetType::Vba);
    assert!(workbook.sheets[0].cells.is_empty());
}

#[test]
fn xlsb_digit_column_widths_survive_parser_model_and_authored_overrides() {
    let mut bundle = vec![0u8; 8];
    bundle.extend_from_slice(&wstr("rId1"));
    bundle.extend_from_slice(&wstr("Widths"));
    let workbook = rec(BRT_BUNDLE_SH, &bundle);

    let mut format = Vec::new();
    format.extend_from_slice(&(14_u32 * 256).to_le_bytes());
    format.extend_from_slice(&42_u16.to_le_bytes());
    format.extend_from_slice(&300_u16.to_le_bytes());
    format.extend_from_slice(&0_u16.to_le_bytes());
    format.extend_from_slice(&[0, 0]);
    let mut column = Vec::new();
    column.extend_from_slice(&0_u32.to_le_bytes());
    column.extend_from_slice(&0_u32.to_le_bytes());
    column.extend_from_slice(&(18_u32 * 256).to_le_bytes());
    column.extend_from_slice(&0_u32.to_le_bytes());
    column.extend_from_slice(&1_u16.to_le_bytes());
    let mut sheet = rec(BRT_WS_FMT_INFO, &format);
    sheet.extend_from_slice(&rec(BRT_COL_INFO, &column));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", workbook.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        writer.start_file(path, options).unwrap();
        writer.write_all(body).unwrap();
    }

    let mut workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
    let sheet = &mut workbook.sheets[0];
    assert_eq!(sheet.column_widths().get(&0), Some(&18.0));
    assert_eq!(sheet.xlsb_column_widths_256().get(&0), Some(&(18 * 256)));
    assert_eq!(sheet.default_column_width(), Some(14.0));
    assert_eq!(
        sheet.xlsb_default_column_width(),
        Some(XlsbDefaultColumnWidth::Digits256(14 * 256))
    );
    assert_eq!(
        sheet.imported_column_axis_measures().get(&0),
        Some(&ImportedAxisMeasure::DigitWidth256(18 * 256))
    );
    assert_eq!(
        sheet.imported_default_column_axis_measure(),
        Some(ImportedAxisMeasure::DigitWidth256(14 * 256))
    );
    assert!(sheet.hidden_columns().contains(&0));

    sheet.set_col_width(0, 20.0);
    assert!(sheet.xlsb_column_widths_256().get(&0).is_none());
    assert!(sheet.imported_column_axis_measures().get(&0).is_none());
    assert_eq!(sheet.column_widths().get(&0), Some(&20.0));
    sheet.set_default_col_width(12.0);
    assert_eq!(sheet.xlsb_default_column_width(), None);
    assert_eq!(sheet.imported_default_column_axis_measure(), None);

    let mut oversized = Vec::new();
    oversized.extend_from_slice(&0_u32.to_le_bytes());
    oversized.extend_from_slice(&0_u32.to_le_bytes());
    oversized.extend_from_slice(&65_536_u32.to_le_bytes());
    oversized.extend_from_slice(&0_u32.to_le_bytes());
    oversized.extend_from_slice(&0_u16.to_le_bytes());
    let mut metadata = SheetReadMetadata::default();
    apply_col_outline(&oversized, &mut metadata, &Styles::default());
    assert!(metadata.col_widths.is_empty());
    assert!(metadata.col_widths_256.is_empty());
}

#[test]
fn xlsb_book_view_active_tab_surfaces_workbook_metadata() {
    let mut wb_bin = Vec::new();
    for (rid, name) in [("rId1", "Data"), ("rId2", "Summary")] {
        let mut bundle = vec![0u8; 8]; // hsState + iTabID
        bundle.extend_from_slice(&wstr(rid));
        bundle.extend_from_slice(&wstr(name));
        wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));
    }
    let mut book_view = Vec::new();
    book_view.extend_from_slice(&0i32.to_le_bytes()); // xWn
    book_view.extend_from_slice(&0i32.to_le_bytes()); // yWn
    book_view.extend_from_slice(&0u32.to_le_bytes()); // dxWn
    book_view.extend_from_slice(&0u32.to_le_bytes()); // dyWn
    book_view.extend_from_slice(&600u32.to_le_bytes()); // iTabRatio
    book_view.extend_from_slice(&0u32.to_le_bytes()); // itabFirst
    book_view.extend_from_slice(&1u32.to_le_bytes()); // itabCur
    wb_bin.extend_from_slice(&rec(158, &book_view)); // BrtBookView

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/><Relationship Id="rId2" Target="worksheets/sheet2.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", [].as_slice()),
        ("xl/worksheets/sheet2.bin", [].as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    let metadata = wb.metadata();

    assert_eq!(wb.active_sheet_index(), Some(1));
    assert_eq!(wb.active_sheet_name(), Some("Summary"));
    assert_eq!(metadata.active_sheet, Some(1));
    assert_eq!(metadata.active_sheet_name, Some("Summary"));
    assert_eq!(
        <Workbook as crate::Reader>::metadata(&wb).active_sheet_name,
        Some("Summary")
    );
}

#[test]
fn xlsb_selected_sheet_view_falls_back_to_active_sheet_metadata() {
    const BRT_BEGIN_WS_VIEWS: u32 = 133;
    const BRT_END_WS_VIEWS: u32 = 134;

    let mut wb_bin = Vec::new();
    for (rid, name) in [("rId1", "Data"), ("rId2", "Summary")] {
        let mut bundle = vec![0u8; 8]; // hsState + iTabID
        bundle.extend_from_slice(&wstr(rid));
        bundle.extend_from_slice(&wstr(name));
        wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));
    }

    let mut ws_view = Vec::new();
    ws_view.extend_from_slice(&(1u16 << 6).to_le_bytes()); // fSelected
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // xlView normal
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // rwTop
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // colLeft
    ws_view.push(0x40); // icvHdr
    ws_view.push(0); // reserved2
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // reserved3
    ws_view.extend_from_slice(&100u16.to_le_bytes()); // wScale
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleNormal
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleSLV
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScalePLV
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // iWbkView

    let mut selected_sheet = rec(BRT_BEGIN_WS_VIEWS, &[]);
    selected_sheet.extend_from_slice(&rec(BRT_BEGIN_WS_VIEW, &ws_view));
    selected_sheet.extend_from_slice(&rec(BRT_END_WS_VIEW, &[]));
    selected_sheet.extend_from_slice(&rec(BRT_END_WS_VIEWS, &[]));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/><Relationship Id="rId2" Target="worksheets/sheet2.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", [].as_slice()),
        ("xl/worksheets/sheet2.bin", selected_sheet.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    let metadata = wb.metadata();

    assert_eq!(wb.active_sheet_index(), Some(1));
    assert_eq!(wb.active_sheet_name(), Some("Summary"));
    assert_eq!(metadata.active_sheet, Some(1));
    assert_eq!(metadata.active_sheet_name, Some("Summary"));
    assert_eq!(
        <Workbook as crate::Reader>::metadata(&wb).active_sheet_name,
        Some("Summary")
    );
}

#[test]
fn xlsb_shared_strings_part_lookup_is_case_insensitive() {
    // calamine/tests/issue_419.xlsb stores the shared-string part as
    // `xl/SharedStrings.bin`; the cell stream still references it through
    // BrtCellIsst.
    let mut wb_bin = vec![0u8; 8];
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("Sheet1"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let mut item = vec![0u8];
    item.extend_from_slice(&wstr("Hello"));
    let sst = rec(BRT_SST_ITEM, &item);

    let mut sh = rec(BRT_ROW_HDR, &[0, 0, 0, 0]);
    let mut isst = vec![0u8; 8];
    isst.extend_from_slice(&0u32.to_le_bytes());
    sh.extend_from_slice(&rec(BRT_CELL_ISST, &isst));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (name, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/SharedStrings.bin", sst.as_slice()),
        ("xl/worksheets/sheet1.bin", sh.as_slice()),
    ] {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(
        wb.sheets[0].cell(0, 0),
        Some(&Cell::Text("Hello".to_string()))
    );
}

#[test]
fn xlsb_styles_use_only_cell_xfs_for_cell_style_indexes() {
    // Real styles.bin parts can contain a non-cell XF group before
    // BrtBeginCellXFs. Cell records index only the XF records inside
    // BrtBeginCellXFs; collecting earlier BrtXF records shifts style
    // indexes and can turn plain numeric cells into dates.
    let mut wb_bin = vec![0u8; 8];
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("Sheet1"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let mut fmt = 164u16.to_le_bytes().to_vec();
    fmt.extend_from_slice(&wstr("yyyy\"년\" m\"월\" d\"일\""));
    let mut styles = rec(BRT_FMT, &fmt);
    styles.extend_from_slice(&rec(0x0272, &1u32.to_le_bytes()));
    styles.extend_from_slice(&rec(BRT_XF, &xf(0)));
    styles.extend_from_slice(&rec(0x0273, &[]));
    styles.extend_from_slice(&rec(BRT_BEGIN_CELL_XFS, &2u32.to_le_bytes()));
    styles.extend_from_slice(&rec(BRT_XF, &xf(164)));
    styles.extend_from_slice(&rec(BRT_XF, &xf(0)));
    styles.extend_from_slice(&rec(BRT_END_CELL_XFS, &[]));

    let mut sh = rec(BRT_ROW_HDR, &[0, 0, 0, 0]);
    let mut date = vec![0u8; 8];
    date.extend_from_slice(&44_197.0f64.to_le_bytes());
    sh.extend_from_slice(&rec(BRT_CELL_REAL, &date));
    let mut number = vec![1, 0, 0, 0, 1, 0, 0, 0];
    number.extend_from_slice(&15.0f64.to_le_bytes());
    sh.extend_from_slice(&rec(BRT_CELL_REAL, &number));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (name, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/styles.bin", styles.as_slice()),
        ("xl/worksheets/sheet1.bin", sh.as_slice()),
    ] {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Date(44_197.0)));
    assert_eq!(wb.sheets[0].formatted(0, 0), Some("2021년 1월 1일"));
    assert_eq!(wb.sheets[0].cell(0, 1), Some(&Cell::Number(15.0)));
}

#[test]
fn xlsb_compact_xf_prefix_retains_date_numfmt_mapping() {
    let compact_xf = |num_fmt: u16| {
        let mut payload = 0u16.to_le_bytes().to_vec();
        payload.extend_from_slice(&num_fmt.to_le_bytes());
        rec(BRT_XF, &payload)
    };
    let mut style_stream = rec(BRT_BEGIN_CELL_XFS, &2u32.to_le_bytes());
    style_stream.extend_from_slice(&compact_xf(0));
    style_stream.extend_from_slice(&compact_xf(14));
    style_stream.extend_from_slice(&rec(BRT_END_CELL_XFS, &[]));

    let styles = parse_styles(&style_stream, &XlsbTheme::default());

    assert_eq!(styles.format_id(1), 14);
    assert_eq!(styles.kind(1), format::Kind::Date);
    assert_eq!(
        styles
            .cell_style(1)
            .and_then(|style| style.num_fmt.as_deref()),
        Some("mm-dd-yy")
    );
    assert_eq!(
        format::render_indexed(45_366.0, styles.format_id(1), false),
        "2024-03-15"
    );
}

#[test]
fn xlsb_standard_builtin_number_formats_are_retained_as_style_codes() {
    for (id, expected) in [
        (12, "# ?/?"),
        (13, "# ??/??"),
        (15, "d-mmm-yy"),
        (18, "h:mm AM/PM"),
        (45, "mm:ss"),
        (46, "[h]:mm:ss"),
        (48, "##0.0E+0"),
    ] {
        assert_eq!(built_in_xlsb_num_fmt(id), Some(expected), "format {id}");
    }
}

#[test]
fn xlsb_font_provenance_requires_exact_complete_binary_sources() {
    let exact = parse_styles(&complete_provenance_styles(280), &XlsbTheme::default());
    assert_eq!(exact.xlsb_normal_font_size_pt, Some(11));
    assert_eq!(exact.xlsb_cell_xf_font_sizes_pt, [Some(11), Some(14)]);

    let nullable_normal = parse_styles(
        &complete_provenance_styles_with_normal_name(280, None),
        &XlsbTheme::default(),
    );
    assert_eq!(nullable_normal.xlsb_normal_font_size_pt, Some(11));
    assert_eq!(
        nullable_normal.xlsb_cell_xf_font_sizes_pt,
        [Some(11), Some(14)]
    );

    let fractional = parse_styles(&complete_provenance_styles(270), &XlsbTheme::default());
    assert_eq!(
        fractional
            .cell_styles
            .get(1)
            .and_then(|style| style.font.as_ref())
            .and_then(|font| font.size_pt),
        Some(14),
        "the public style rounds 13.5pt and cannot prove the source"
    );
    assert_eq!(fractional.xlsb_normal_font_size_pt, Some(11));
    assert_eq!(fractional.xlsb_cell_xf_font_sizes_pt, [Some(11), None]);

    let mut missing_normal = complete_provenance_styles(280);
    let normal = rec(BRT_STYLE, &provenance_style(0, 1, 0, Some("Normal")));
    let offset = missing_normal
        .windows(normal.len())
        .position(|window| window == normal)
        .expect("Normal style");
    let custom = rec(BRT_STYLE, &provenance_style(0, 0, 0, Some("Custom")));
    missing_normal[offset..offset + normal.len()].copy_from_slice(&custom);
    let missing_normal = parse_styles(&missing_normal, &XlsbTheme::default());
    assert_eq!(missing_normal.xlsb_normal_font_size_pt, None);
    assert_eq!(
        missing_normal.xlsb_cell_xf_font_sizes_pt,
        [Some(11), Some(14)],
        "cell-XF provenance is independent of the named Normal oracle"
    );

    let mut different_default_source = complete_provenance_styles(220);
    let inherited = rec(BRT_XF, &provenance_xf(0, 0, 0));
    let offset = different_default_source
        .windows(inherited.len())
        .position(|window| window == inherited)
        .expect("first CellXF");
    let direct = rec(BRT_XF, &provenance_xf(0, 1, 1 << 1));
    different_default_source[offset..offset + inherited.len()].copy_from_slice(&direct);
    let different_default_source = parse_styles(&different_default_source, &XlsbTheme::default());
    assert_eq!(
        different_default_source.xlsb_normal_font_size_pt, None,
        "equal point sizes from different source font records do not prove Normal"
    );
    assert_eq!(
        different_default_source.xlsb_cell_xf_font_sizes_pt,
        [Some(11), Some(11)]
    );

    for invalid_parent in [u16::MAX, 1] {
        let mut invalid_cell_parent = complete_provenance_styles(280);
        let direct = rec(BRT_XF, &provenance_xf(0, 1, 1 << 1));
        let offset = invalid_cell_parent
            .windows(direct.len())
            .position(|window| window == direct)
            .expect("direct CellXF");
        let invalid = rec(BRT_XF, &provenance_xf(invalid_parent, 1, 1 << 1));
        invalid_cell_parent[offset..offset + direct.len()].copy_from_slice(&invalid);
        let invalid_cell_parent = parse_styles(&invalid_cell_parent, &XlsbTheme::default());
        assert_eq!(invalid_cell_parent.xlsb_normal_font_size_pt, Some(11));
        assert_eq!(
            invalid_cell_parent.xlsb_cell_xf_font_sizes_pt,
            [Some(11), None]
        );
    }

    let mut count_mismatch = complete_provenance_styles(280);
    let begin_fonts = rec(BRT_BEGIN_FONTS, &2_u32.to_le_bytes());
    let offset = count_mismatch
        .windows(begin_fonts.len())
        .position(|window| window == begin_fonts)
        .expect("font collection");
    let replacement = rec(BRT_BEGIN_FONTS, &1_u32.to_le_bytes());
    count_mismatch[offset..offset + begin_fonts.len()].copy_from_slice(&replacement);
    let malformed = parse_styles(&count_mismatch, &XlsbTheme::default());
    assert_eq!(malformed.xlsb_normal_font_size_pt, None);
    assert!(malformed.xlsb_cell_xf_font_sizes_pt.is_empty());

    let mut truncated = complete_provenance_styles(280);
    truncated.pop();
    let truncated = parse_styles(&truncated, &XlsbTheme::default());
    assert_eq!(truncated.xlsb_normal_font_size_pt, None);
    assert!(truncated.xlsb_cell_xf_font_sizes_pt.is_empty());
}

#[test]
fn xlsb_font_provenance_collection_counts_enforce_bounds() {
    let count = |value: usize, max: usize| {
        verified_xlsb_collection_count(&(value as u32).to_le_bytes(), max)
    };

    assert_eq!(count(1, MAX_XLSB_FONT_RECORDS), Some(1));
    assert_eq!(
        count(MAX_XLSB_FONT_RECORDS, MAX_XLSB_FONT_RECORDS),
        Some(MAX_XLSB_FONT_RECORDS)
    );
    assert_eq!(count(0, MAX_XLSB_FONT_RECORDS), None);
    assert_eq!(
        count(MAX_XLSB_FONT_RECORDS + 1, MAX_XLSB_FONT_RECORDS),
        None
    );

    assert_eq!(count(1, MAX_VERIFIED_XLSB_STYLE_RECORDS), Some(1));
    assert_eq!(
        count(
            MAX_VERIFIED_XLSB_STYLE_RECORDS,
            MAX_VERIFIED_XLSB_STYLE_RECORDS,
        ),
        Some(MAX_VERIFIED_XLSB_STYLE_RECORDS)
    );
    assert_eq!(count(0, MAX_VERIFIED_XLSB_STYLE_RECORDS), None);
    assert_eq!(
        count(
            MAX_VERIFIED_XLSB_STYLE_RECORDS + 1,
            MAX_VERIFIED_XLSB_STYLE_RECORDS,
        ),
        None
    );
    assert_eq!(
        verified_xlsb_collection_count(&1_u16.to_le_bytes(), usize::MAX),
        None
    );
}

#[test]
fn xlsb_font_provenance_alternate_content_is_bounded_and_fails_closed() {
    let ac_begin = rec(BRT_AC_BEGIN, &[1, 0, 0, 0, 0, 0]);
    let ac_end = rec(BRT_AC_END, &[]);
    let end_style_sheet = rec(BRT_END_STYLE_SHEET, &[]);
    let insert_before_end = |mut styles: Vec<u8>, records: &[u8]| {
        let offset = styles
            .windows(end_style_sheet.len())
            .rposition(|window| window == end_style_sheet)
            .expect("style sheet end");
        styles.splice(offset..offset, records.iter().copied());
        styles
    };
    let provenance = |styles: &[u8]| {
        let parsed = parse_styles(styles, &XlsbTheme::default());
        (
            parsed.xlsb_normal_font_size_pt,
            parsed.xlsb_cell_xf_font_sizes_pt,
        )
    };

    let mut harmless_block = ac_begin.clone();
    harmless_block.extend_from_slice(&rec(0x0200, &[1, 2, 3]));
    harmless_block.extend_from_slice(&ac_end);
    assert_eq!(
        provenance(&insert_before_end(
            complete_provenance_styles(280),
            &harmless_block,
        )),
        (Some(11), vec![Some(11), Some(14)]),
        "unknown alternate content cannot mutate verified style tables"
    );

    let mut table_record_block = ac_begin.clone();
    table_record_block.extend_from_slice(&rec(BRT_FONT, &provenance_font("alternate font", 400)));
    table_record_block.extend_from_slice(&ac_end);
    assert_eq!(
        provenance(&insert_before_end(
            complete_provenance_styles(280),
            &table_record_block,
        )),
        (None, Vec::new()),
        "the permissive parser cannot supply provenance through alternate content"
    );

    let mut nested = ac_begin.clone();
    nested.extend_from_slice(&ac_begin);
    nested.extend_from_slice(&ac_end);
    nested.extend_from_slice(&ac_end);
    assert_eq!(
        provenance(&insert_before_end(complete_provenance_styles(280), &nested)),
        (None, Vec::new()),
        "alternate-content blocks are not recursive"
    );

    assert_eq!(
        provenance(&insert_before_end(
            complete_provenance_styles(280),
            &rec(BRT_AC_BEGIN, &[0, 0]),
        )),
        (None, Vec::new()),
        "a zero-version or unterminated alternate-content block is malformed"
    );
    assert_eq!(
        provenance(&insert_before_end(
            complete_provenance_styles(280),
            &rec(BRT_AC_END, &[]),
        )),
        (None, Vec::new()),
        "a stray alternate-content terminator is malformed"
    );
}

#[test]
fn xlsb_font_provenance_surfaces_per_cell_and_fails_closed_after_authoring() {
    let mut workbook_record = vec![0_u8; 8];
    workbook_record.extend_from_slice(&wstr("rId1"));
    workbook_record.extend_from_slice(&wstr("Font provenance"));
    let workbook_bin = rec(BRT_BUNDLE_SH, &workbook_record);

    let mut column = Vec::new();
    column.extend_from_slice(&0_u32.to_le_bytes());
    column.extend_from_slice(&1_u32.to_le_bytes());
    column.extend_from_slice(&(8_u32 * 256).to_le_bytes());
    column.extend_from_slice(&0_u32.to_le_bytes());
    column.extend_from_slice(&0_u16.to_le_bytes());
    let mut sheet_bin = rec(BRT_COL_INFO, &column);
    sheet_bin.extend_from_slice(&rec(BRT_ROW_HDR, &0_u32.to_le_bytes()));
    for (col, style, text) in [(0_u32, 0_u32, "normal"), (1, 1, "direct")] {
        let mut cell = col.to_le_bytes().to_vec();
        cell.extend_from_slice(&style.to_le_bytes());
        cell.extend_from_slice(&wstr(text));
        sheet_bin.extend_from_slice(&rec(BRT_CELL_ST, &cell));
    }
    let relationships = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (name, body) in [
        ("xl/workbook.bin", workbook_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", relationships.as_bytes()),
        ("xl/styles.bin", complete_provenance_styles(280).as_slice()),
        ("xl/worksheets/sheet1.bin", sheet_bin.as_slice()),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body).unwrap();
    }

    let mut workbook = Workbook::open(&zip.finish().unwrap().into_inner()).unwrap();
    let sheet = &mut workbook.sheets[0];
    assert_eq!(sheet.verified_xlsb_normal_font_size_pt(), Some(11));
    assert_eq!(sheet.verified_xlsb_cell_font_size_pt(0, 0), Some(11));
    assert_eq!(sheet.verified_xlsb_cell_font_size_pt(0, 1), Some(14));
    assert_eq!(sheet.verified_xlsx_normal_font_size_pt(), None);
    assert_eq!(sheet.verified_xlsx_cell_font_size_pt(0, 0), None);

    sheet.write_blank_styled(0, 0, &crate::CellStyle::new());
    assert!(sheet.cell(0, 0).is_none());
    assert_eq!(sheet.xlsb_cell_font_sizes_pt, [Some(14)]);
    assert_eq!(
        sheet.verified_xlsb_cell_font_size_pt(0, 1),
        Some(14),
        "blank authoring preserves aligned provenance for unrelated cells"
    );

    sheet.set_col_format(0, &crate::Format::new().set_font_size(20));
    assert_eq!(
        sheet.verified_xlsb_cell_font_size_pt(0, 0),
        None,
        "an authored column font invalidates its rounded source layer"
    );
    sheet.write(0, 0, "authored replacement");
    assert_eq!(sheet.verified_xlsb_cell_font_size_pt(0, 0), None);
    assert_eq!(
        sheet.verified_xlsb_cell_font_size_pt(0, 1),
        Some(14),
        "unrelated retained cells keep their source provenance"
    );
    sheet.set_default_format(&crate::Format::new().set_font_size(20));
    assert_eq!(sheet.verified_xlsb_normal_font_size_pt(), None);
    assert_eq!(sheet.verified_xlsb_cell_font_size_pt(0, 1), None);
}

#[test]
fn xlsb_sheet_style_references_report_missing_xfs_once_per_record() {
    let styles = Styles {
        cell_styles: vec![CellStyle::default()],
        has_source_styles: true,
        ..Default::default()
    };

    let mut col_info = Vec::new();
    col_info.extend_from_slice(&0u32.to_le_bytes());
    col_info.extend_from_slice(&1u32.to_le_bytes());
    col_info.extend_from_slice(&256u32.to_le_bytes());
    col_info.extend_from_slice(&9u32.to_le_bytes());
    col_info.extend_from_slice(&0u16.to_le_bytes());

    let mut row_header = Vec::new();
    row_header.extend_from_slice(&0u32.to_le_bytes());
    row_header.extend_from_slice(&8u32.to_le_bytes());
    row_header.extend_from_slice(&0u16.to_le_bytes());
    row_header.extend_from_slice(&(1u16 << 14).to_le_bytes());

    let mut blank = vec![0u8; 8];
    blank[4..7].copy_from_slice(&7u32.to_le_bytes()[..3]);
    let mut number = vec![0u8; 8];
    number[0..4].copy_from_slice(&1u32.to_le_bytes());
    number[4..7].copy_from_slice(&6u32.to_le_bytes()[..3]);
    number.extend_from_slice(&1.0f64.to_le_bytes());

    let mut stream = rec(BRT_COL_INFO, &col_info);
    stream.extend_from_slice(&rec(BRT_ROW_HDR, &row_header));
    stream.extend_from_slice(&rec(BRT_CELL_BLANK, &blank));
    stream.extend_from_slice(&rec(BRT_CELL_REAL, &number));
    let mut budget = crate::MAX_TEXT_BYTES;
    let (_, _, _, metadata) = parse_sheet(
        &stream,
        &[],
        &styles,
        false,
        &[],
        &mut budget,
        &[],
        &[],
        &[],
        &[],
    );

    assert_eq!(
        metadata
            .style_losses
            .iter()
            .find(|loss| loss.kind == StyleLossKind::MissingReference)
            .map(|loss| loss.occurrences),
        Some(4)
    );
}

#[test]
fn xlsb_ws_format_info_distinguishes_column_and_row_default_provenance() {
    let parse = |default_width_256: u32,
                 base_characters: u16,
                 default_row_height_twips: u16,
                 flags: u16| {
        let mut payload = Vec::new();
        payload.extend_from_slice(&default_width_256.to_le_bytes());
        payload.extend_from_slice(&base_characters.to_le_bytes());
        payload.extend_from_slice(&default_row_height_twips.to_le_bytes());
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.extend_from_slice(&[0, 0]); // row/column outline levels
        let mut budget = crate::MAX_TEXT_BYTES;
        parse_sheet(
            &rec(BRT_WS_FMT_INFO, &payload),
            &[],
            &Styles::default(),
            false,
            &[],
            &mut budget,
            &[],
            &[],
            &[],
            &[],
        )
        .3
    };

    let implicit = parse(2_158, 42, 300, 0);
    assert_eq!(implicit.default_col_width, Some(2_158.0 / 256.0));
    assert_eq!(implicit.default_col_width_256, Some(2_158));
    assert_eq!(implicit.ooxml_base_col_width, None);
    assert_eq!(implicit.default_row_height, None);
    assert_eq!(implicit.imported_default_row_axis_measure, None);

    let base = parse(u32::MAX, 8, 300, 0);
    assert_eq!(base.default_col_width, None);
    assert_eq!(base.default_col_width_256, None);
    assert_eq!(base.ooxml_base_col_width, Some(8));

    let invalid_base = parse(u32::MAX, 256, 300, 0);
    assert_eq!(invalid_base.default_col_width, None);
    assert_eq!(invalid_base.ooxml_base_col_width, None);

    let custom_row = parse(u32::MAX, 8, 300, 0x0001);
    assert_eq!(custom_row.default_row_height, Some(15.0));
    assert_eq!(
        custom_row.imported_default_row_axis_measure,
        Some(ImportedAxisMeasure::Twips(300))
    );

    let implicit_zero = parse(u32::MAX, 8, 0, 0);
    assert_eq!(implicit_zero.default_row_height, None);
    assert_eq!(implicit_zero.imported_default_row_axis_measure, None);

    let manual_zero = parse(u32::MAX, 8, 0, 0x0001);
    assert_eq!(manual_zero.default_row_height, Some(0.0));
    assert_eq!(
        manual_zero.imported_default_row_axis_measure,
        Some(ImportedAxisMeasure::Twips(0))
    );

    let hidden_zero = parse(u32::MAX, 8, 0, 0x0002);
    assert_eq!(hidden_zero.default_row_height, None);
    assert_eq!(hidden_zero.imported_default_row_axis_measure, None);
    assert!(hidden_zero.default_rows_hidden);

    for valid_height in [MAX_XLSB_ROW_HEIGHT_TWIPS + 1, u16::MAX] {
        let valid = parse(u32::MAX, 8, valid_height, 0x0001);
        assert_eq!(
            valid.default_row_height,
            Some(f32::from(valid_height) / 20.0)
        );
        assert_eq!(
            valid.imported_default_row_axis_measure,
            Some(ImportedAxisMeasure::Twips(u32::from(valid_height)))
        );
    }

    let default_hidden = parse(u32::MAX, 8, 300, 0x0002);
    assert!(default_hidden.default_rows_hidden);

    let mut ws_format = Vec::new();
    ws_format.extend_from_slice(&u32::MAX.to_le_bytes());
    ws_format.extend_from_slice(&8u16.to_le_bytes());
    ws_format.extend_from_slice(&300u16.to_le_bytes());
    ws_format.extend_from_slice(&0x0002u16.to_le_bytes());
    ws_format.extend_from_slice(&[0, 0]);
    let row_header = |row: u32, hidden: bool| {
        let mut payload = Vec::new();
        payload.extend_from_slice(&row.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&300u16.to_le_bytes());
        payload.extend_from_slice(&(if hidden { 1u16 << 12 } else { 0 }).to_le_bytes());
        payload
    };
    let mut stream = rec(BRT_WS_FMT_INFO, &ws_format);
    stream.extend_from_slice(&rec(BRT_ROW_HDR, &row_header(1, false)));
    stream.extend_from_slice(&rec(BRT_ROW_HDR, &row_header(2, true)));
    let mut budget = crate::MAX_TEXT_BYTES;
    let metadata = parse_sheet(
        &stream,
        &[],
        &Styles::default(),
        false,
        &[],
        &mut budget,
        &[],
        &[],
        &[],
        &[],
    )
    .3;
    assert!(metadata.default_rows_hidden);
    assert_eq!(
        metadata
            .explicit_visible_rows
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(
        metadata.hidden_rows.iter().copied().collect::<Vec<_>>(),
        [2]
    );
}

#[test]
fn xlsb_row_geometry_enforces_schema_row_and_height_bounds() {
    let row_header = |row: u32, height_twips: u16, flags: u16| {
        let mut payload = Vec::new();
        payload.extend_from_slice(&row.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&height_twips.to_le_bytes());
        payload.extend_from_slice(&flags.to_le_bytes());
        payload
    };
    let mut metadata = SheetReadMetadata::default();
    let styles = Styles::default();

    apply_row_outline(
        &row_header(MAX_XLSB_ROW_INDEX, MAX_XLSB_ROW_HEIGHT_TWIPS, 1 << 13),
        &mut metadata,
        &styles,
    );
    assert_eq!(
        metadata.row_heights.get(&MAX_XLSB_ROW_INDEX),
        Some(&(f32::from(MAX_XLSB_ROW_HEIGHT_TWIPS) / 20.0))
    );
    assert_eq!(
        metadata.imported_row_axis_measures.get(&MAX_XLSB_ROW_INDEX),
        Some(&ImportedAxisMeasure::Twips(u32::from(
            MAX_XLSB_ROW_HEIGHT_TWIPS
        )))
    );

    apply_row_outline(&row_header(6, 300, 0), &mut metadata, &styles);
    assert!(!metadata.row_heights.contains_key(&6));
    assert!(!metadata.imported_row_axis_measures.contains_key(&6));

    let out_of_range_row = MAX_XLSB_ROW_INDEX + 1;
    apply_row_outline(
        &row_header(out_of_range_row, 300, (1 << 13) | (1 << 12)),
        &mut metadata,
        &styles,
    );
    assert!(!metadata.row_heights.contains_key(&out_of_range_row));
    assert!(!metadata
        .imported_row_axis_measures
        .contains_key(&out_of_range_row));
    assert!(!metadata.hidden_rows.contains(&out_of_range_row));
    assert!(!metadata.explicit_visible_rows.contains(&out_of_range_row));

    let oversized_height = MAX_XLSB_ROW_HEIGHT_TWIPS + 1;
    apply_row_outline(
        &row_header(7, oversized_height, 1 << 13),
        &mut metadata,
        &styles,
    );
    assert!(!metadata.row_heights.contains_key(&7));
    assert!(!metadata.imported_row_axis_measures.contains_key(&7));
    assert!(metadata.explicit_visible_rows.contains(&7));

    apply_row_outline(&row_header(8, 320, 0), &mut metadata, &styles);
    apply_row_outline(&row_header(8, 340, 1 << 13), &mut metadata, &styles);
    let mut truncated = row_header(8, 360, 0);
    truncated.truncate(11);
    apply_row_outline(&truncated, &mut metadata, &styles);
    assert_eq!(metadata.row_heights.get(&8), Some(&17.0));
    assert_eq!(
        metadata.imported_row_axis_measures.get(&8),
        Some(&ImportedAxisMeasure::Twips(340))
    );
}

#[test]
fn xlsb_unrepresentable_xf_and_border_variants_are_typed_losses() {
    let mut dashed_border = vec![0u8; 51];
    dashed_border[1] = 3;
    let mut raw = xf(0);
    raw[0..2].copy_from_slice(&u16::MAX.to_le_bytes());
    raw[8..10].copy_from_slice(&0u16.to_le_bytes());
    raw[12..14].copy_from_slice(&(7u16 | (1 << 7) | (1 << 10)).to_le_bytes());

    let mut stream = rec(BRT_BORDER, &dashed_border);
    stream.extend_from_slice(&rec(BRT_BEGIN_CELL_XFS, &1u32.to_le_bytes()));
    stream.extend_from_slice(&rec(BRT_XF, &raw));
    stream.extend_from_slice(&rec(BRT_END_CELL_XFS, &[]));
    let styles = parse_styles(&stream, &XlsbTheme::default());

    assert!(styles
        .losses
        .iter()
        .any(|loss| loss.kind == StyleLossKind::UnsupportedProperty && loss.occurrences >= 3));
}

#[test]
fn xlsb_style_tables_retain_inherited_components() {
    fn rgb(red: u8, green: u8, blue: u8) -> [u8; 8] {
        [5, 0, 0, 0, red, green, blue, 0xFF]
    }

    fn font(
        name: &str,
        height_twips: u16,
        flags: u16,
        weight: u16,
        script: u16,
        underline: u8,
        color: [u8; 8],
    ) -> Vec<u8> {
        let mut out = vec![0u8; 21];
        out[0..2].copy_from_slice(&height_twips.to_le_bytes());
        out[2..4].copy_from_slice(&flags.to_le_bytes());
        out[4..6].copy_from_slice(&weight.to_le_bytes());
        out[6..8].copy_from_slice(&script.to_le_bytes());
        out[8] = underline;
        out[12..20].copy_from_slice(&color);
        out.extend_from_slice(&wstr(name));
        out
    }

    fn raw_xf(
        parent: u16,
        num_fmt: u16,
        font: u16,
        fill: u16,
        border: u16,
        changed_groups: u16,
    ) -> Vec<u8> {
        let mut out = vec![0u8; 16];
        out[0..2].copy_from_slice(&parent.to_le_bytes());
        out[2..4].copy_from_slice(&num_fmt.to_le_bytes());
        out[4..6].copy_from_slice(&font.to_le_bytes());
        out[6..8].copy_from_slice(&fill.to_le_bytes());
        out[8..10].copy_from_slice(&border.to_le_bytes());
        out[14..16].copy_from_slice(&changed_groups.to_le_bytes());
        out
    }

    let default_font = font("Aptos", 220, 0, 400, 0, 0, [0; 8]);
    let custom_font = font(
        "Noto Sans KR",
        240,
        0x000A,
        700,
        2,
        1,
        rgb(0x11, 0x22, 0x33),
    );

    let default_fill = vec![0u8; 20];
    let mut custom_fill = vec![0u8; 20];
    custom_fill[0..4].copy_from_slice(&1u32.to_le_bytes());
    custom_fill[4..12].copy_from_slice(&rgb(0xFF, 0xEE, 0xCC));

    let default_border = vec![0u8; 41];
    let mut custom_border = vec![0u8; 41];
    custom_border[1] = 6;
    custom_border[3..11].copy_from_slice(&rgb(0x44, 0x55, 0x66));

    let mut format = 164u16.to_le_bytes().to_vec();
    format.extend_from_slice(&wstr("₩#,##0.00"));

    let mut styles = rec(BRT_FONT, &default_font);
    styles.extend_from_slice(&rec(BRT_FONT, &custom_font));
    styles.extend_from_slice(&rec(BRT_FILL, &default_fill));
    styles.extend_from_slice(&rec(BRT_FILL, &custom_fill));
    styles.extend_from_slice(&rec(BRT_BORDER, &default_border));
    styles.extend_from_slice(&rec(BRT_BORDER, &custom_border));
    styles.extend_from_slice(&rec(BRT_FMT, &format));
    styles.extend_from_slice(&rec(BRT_BEGIN_CELL_STYLE_XFS, &1u32.to_le_bytes()));
    styles.extend_from_slice(&rec(BRT_XF, &raw_xf(u16::MAX, 0, 0, 0, 0, 0)));
    styles.extend_from_slice(&rec(BRT_END_CELL_STYLE_XFS, &[]));
    styles.extend_from_slice(&rec(BRT_BEGIN_CELL_XFS, &1u32.to_le_bytes()));
    let mut cell_xf = raw_xf(0, 164, 1, 1, 1, 0x3F);
    cell_xf[10] = 135; // -45 degrees.
    cell_xf[11] = 3;
    let alignment = 2u16 | (2 << 3) | (1 << 6) | (1 << 8) | (1 << 12) | (1 << 13);
    cell_xf[12..14].copy_from_slice(&alignment.to_le_bytes());
    styles.extend_from_slice(&rec(BRT_XF, &cell_xf));
    styles.extend_from_slice(&rec(BRT_END_CELL_XFS, &[]));

    let parsed = parse_styles(&styles, &XlsbTheme::default());
    assert!(
        parsed.losses.is_empty(),
        "unexpected losses: {:?}",
        parsed.losses
    );
    let style = &parsed.cell_styles[0];

    assert_eq!(style.num_fmt.as_deref(), Some("₩#,##0.00"));
    assert_eq!(style.fill, Some(Color::rgb(0xFF, 0xEE, 0xCC)));
    assert_eq!(
        style.pattern_fill.as_ref().unwrap().pattern,
        FormatPattern::Solid
    );

    let font = style.font.as_ref().unwrap();
    assert_eq!(font.name.as_deref(), Some("Noto Sans KR"));
    assert_eq!(font.size_pt, Some(12));
    assert_eq!(font.color, Some(Color::rgb(0x11, 0x22, 0x33)));
    assert!(font.bold && font.italic && font.underline && font.strikethrough);
    assert_eq!(font.script, FormatScript::Subscript);

    let border = style.border.as_ref().unwrap();
    assert_eq!(border.top, BorderStyle::Double);
    assert_eq!(border.top_color, Some(Color::rgb(0x44, 0x55, 0x66)));

    assert_eq!(
        style.align,
        Some(Alignment {
            horizontal: Some(HAlign::Center),
            vertical: Some(VAlign::Bottom),
            wrap: true,
            rotation: -45,
            indent: 3,
            shrink_to_fit: true,
        })
    );
    assert_eq!(
        style.protection,
        Some(CellProtection {
            locked: Some(true),
            hidden: true,
        })
    );
}

#[test]
fn xlsb_style_table_limit_is_bounded_and_typed() {
    let mut no_parent = xf(0);
    no_parent[0..2].copy_from_slice(&u16::MAX.to_le_bytes());
    let record = rec(BRT_XF, &no_parent);
    let mut styles = rec(
        BRT_BEGIN_CELL_XFS,
        &(MAX_XLSB_STYLE_RECORDS as u32 + 1).to_le_bytes(),
    );
    styles.reserve(record.len() * (MAX_XLSB_STYLE_RECORDS + 1));
    for _ in 0..=MAX_XLSB_STYLE_RECORDS {
        styles.extend_from_slice(&record);
    }
    styles.extend_from_slice(&rec(BRT_END_CELL_XFS, &[]));

    let parsed = parse_styles(&styles, &XlsbTheme::default());
    assert_eq!(parsed.cell_styles.len(), MAX_XLSB_STYLE_RECORDS);
    assert_eq!(
        parsed
            .losses
            .iter()
            .find(|loss| loss.kind == StyleLossKind::LimitExceeded)
            .map(|loss| loss.occurrences),
        Some(1)
    );
}

#[test]
fn xlsb_drawing_anchor_retains_offsets_crop_rotation_and_accessibility() {
    let xml = r#"
            <xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"
                      xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                      xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
              <xdr:twoCellAnchor editAs="oneCell">
                <xdr:from><xdr:col>1</xdr:col><xdr:colOff>123</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>456</xdr:rowOff></xdr:from>
                <xdr:to><xdr:col>4</xdr:col><xdr:colOff>789</xdr:colOff><xdr:row>5</xdr:row><xdr:rowOff>1011</xdr:rowOff></xdr:to>
                <xdr:pic>
                  <xdr:nvPicPr><xdr:cNvPr id="2" name="Logo" descr="Accessible logo"/></xdr:nvPicPr>
                  <xdr:blipFill><a:blip r:embed="rId5"/><a:srcRect l="1000" t="2000" r="3000" b="4000"/></xdr:blipFill>
                  <xdr:spPr><a:xfrm rot="60000"><a:ext cx="914400" cy="457200"/></a:xfrm></xdr:spPr>
                </xdr:pic>
              </xdr:twoCellAnchor>
            </xdr:wsDr>
        "#;

    let refs = parse_xlsb_drawing_refs(xml);
    assert_eq!(refs.len(), 1);
    let drawing = &refs[0];
    assert!(matches!(drawing.kind, XlsbDrawingKind::Image));
    assert_eq!(drawing.rid.as_deref(), Some("rId5"));
    assert_eq!(drawing.from, (2, 1));
    assert_eq!(drawing.to, Some((5, 4)));
    assert_eq!(drawing.metadata.from_cell, Some((2, 1)));
    assert_eq!(drawing.metadata.to_cell, Some((5, 4)));
    assert_eq!(drawing.metadata.from_offset_emu, Some((123, 456)));
    assert_eq!(drawing.metadata.to_offset_emu, Some((789, 1011)));
    assert_eq!(drawing.metadata.absolute_size_emu, Some((914_400, 457_200)));
    assert_eq!(drawing.metadata.rotation_mdeg, Some(1_000));
    assert_eq!(drawing.metadata.name.as_deref(), Some("Logo"));
    assert_eq!(
        drawing.metadata.alt_text.as_deref(),
        Some("Accessible logo")
    );
    assert_eq!(drawing.metadata.behavior, DrawingAnchorBehavior::MoveOnly);
    assert_eq!(
        drawing.metadata.crop,
        Some(DrawingCrop {
            left_ppm: 10_000,
            top_ppm: 20_000,
            right_ppm: 30_000,
            bottom_ppm: 40_000,
        })
    );
}

#[test]
fn xlsb_drawing_anchor_preserves_explicit_zero_offsets() {
    let xml = r#"
            <xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing">
              <xdr:twoCellAnchor>
                <xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                <xdr:to><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>5</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="2" name="Zero offsets"/></xdr:nvSpPr></xdr:sp>
              </xdr:twoCellAnchor>
            </xdr:wsDr>
        "#;

    let refs = parse_xlsb_drawing_refs(xml);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].metadata.from_offset_emu, Some((0, 0)));
    assert_eq!(refs[0].metadata.to_offset_emu, Some((0, 0)));
    assert_eq!(refs[0].metadata.from_cell, Some((2, 1)));
    assert_eq!(refs[0].metadata.to_cell, Some((5, 4)));
}

#[test]
fn xlsb_drawing_relationship_selection_rejects_ambiguity_and_external_targets() {
    let duplicate = r#"<Relationships><Relationship Id="first" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="a.xml"/><Relationship Id="second" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="b.xml"/></Relationships>"#;
    assert_eq!(
        drawing_relationship_target(duplicate),
        crate::xlsx::RelationshipTarget::Invalid
    );

    let duplicate_id = r#"<Relationships><Relationship Id="same" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="a.xml"/><Relationship Id="same" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="b.xml"/></Relationships>"#;
    assert_eq!(
        drawing_relationship_target(duplicate_id),
        crate::xlsx::RelationshipTarget::Invalid
    );

    let external = r#"<Relationships><Relationship Id="draw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="https://example.invalid/drawing.xml" TargetMode="External"/></Relationships>"#;
    assert_eq!(
        drawing_relationship_target(external),
        crate::xlsx::RelationshipTarget::Invalid
    );

    let suffix_attacker = r#"<Relationships><Relationship Id="draw" Type="https://example.invalid/relationships/drawing" Target="evil.xml"/></Relationships>"#;
    assert_eq!(
        drawing_relationship_target(suffix_attacker),
        crate::xlsx::RelationshipTarget::Missing
    );
}

#[test]
fn xlsb_chart_import_count_budget_is_shared_across_sheets() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    const CHART_XML: &str = r#"<chartSpace xmlns="http://schemas.openxmlformats.org/drawingml/2006/chart"><chart><plotArea><lineChart><grouping val="standard"/><varyColors val="0"/><axId val="1"/><axId val="2"/></lineChart><catAx><axId val="1"/><crossAx val="2"/></catAx><valAx><axId val="2"/><crossAx val="1"/></valAx></plotArea></chart></chartSpace>"#;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for index in 1..=2 {
        writer
            .start_file(format!("xl/drawings/drawing{index}.xml"), options)
            .unwrap();
        writer
                .write_all(
                    format!(r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:twoCellAnchor><xdr:from><xdr:col>0</xdr:col><xdr:row>0</xdr:row></xdr:from><xdr:to><xdr:col>4</xdr:col><xdr:row>8</xdr:row></xdr:to><xdr:graphicFrame><c:chart r:id="rIdChart{index}"/></xdr:graphicFrame></xdr:twoCellAnchor></xdr:wsDr>"#).as_bytes(),
                )
                .unwrap();
        writer
            .start_file(
                format!("xl/drawings/_rels/drawing{index}.xml.rels"),
                options,
            )
            .unwrap();
        writer
                .write_all(
                    format!(r#"<Relationships><Relationship Id="rIdChart{index}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart{index}.xml"/></Relationships>"#).as_bytes(),
                )
                .unwrap();
        writer
            .start_file(format!("xl/charts/chart{index}.xml"), options)
            .unwrap();
        writer.write_all(CHART_XML.as_bytes()).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
    let sheet_rels = |index| {
        format!(
            r#"<Relationships><Relationship Id="rIdDraw{index}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing{index}.xml"/></Relationships>"#
        )
    };
    let mut chart_budget = crate::xlsx::ChartImportBudget {
        charts_remaining: 1,
        ..crate::xlsx::ChartImportBudget::default()
    };

    let first = read_sheet_drawings(
        &mut zip,
        "xl/worksheets/sheet1.bin",
        &sheet_rels(1),
        &XlsbTheme::default(),
        &mut chart_budget,
    );
    let second = read_sheet_drawings(
        &mut zip,
        "xl/worksheets/sheet2.bin",
        &sheet_rels(2),
        &XlsbTheme::default(),
        &mut chart_budget,
    );
    assert_eq!(first.1.len(), 1);
    assert!(second.1.is_empty());
    assert!(second
        .3
        .iter()
        .any(|loss| loss.kind == StyleLossKind::LimitExceeded));
}

#[test]
fn xlsb_chart_sidecar_retains_horizontal_bar_direction() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    writer
        .start_file("xl/drawings/drawing1.xml", options)
        .unwrap();
    writer
            .write_all(br#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:row>2</xdr:row></xdr:from><xdr:to><xdr:col>7</xdr:col><xdr:row>14</xdr:row></xdr:to><xdr:graphicFrame><c:chart r:id="rIdChart"/></xdr:graphicFrame></xdr:twoCellAnchor></xdr:wsDr>"#)
            .unwrap();
    writer
        .start_file("xl/drawings/_rels/drawing1.xml.rels", options)
        .unwrap();
    writer
            .write_all(br#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#)
            .unwrap();
    writer.start_file("xl/charts/chart1.xml", options).unwrap();
    writer
            .write_all(br#"<chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><chart><plotArea><barChart><barDir val="bar"/><ser><idx val="0"/><order val="0"/><spPr><a:ln w="19050"><a:solidFill><a:srgbClr val="123456"/></a:solidFill></a:ln></spPr><val><numRef><f>Sheet1!$A$1:$A$2</f></numRef></val></ser><axId val="1"/><axId val="2"/></barChart><catAx><axId val="1"/><majorGridlines/><crossAx val="2"/></catAx><valAx><axId val="2"/><crossAx val="1"/></valAx></plotArea></chart><spPr><a:noFill/></spPr></chartSpace>"#)
            .unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
    let sheet_rels = r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#;
    let mut chart_budget = crate::xlsx::ChartImportBudget::default();
    let (_, charts, metadata, losses) = read_sheet_drawings(
        &mut zip,
        "xl/worksheets/sheet1.bin",
        sheet_rels,
        &XlsbTheme::default(),
        &mut chart_budget,
    );
    assert_eq!(charts.len(), 1);
    assert!(losses.is_empty(), "unexpected losses: {losses:?}");
    let sidecar = metadata
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("chart sidecar");
    assert_eq!(
        sidecar.chart_default_latin_font_family.as_deref(),
        Some(crate::xlsx::CALC_IMPORTED_CHART_LATIN_FONT_FAMILY)
    );
    assert_eq!(
        sidecar.chart_bar_direction,
        crate::ChartBarDirection::Horizontal
    );
    assert_eq!(sidecar.chart_series_styles.len(), 1);
    assert_eq!(sidecar.chart_series_styles[0].line_width_emu, Some(19_050));
    assert_eq!(
        sidecar.chart_series_styles[0].line_color,
        Some(Color::rgb(0x12, 0x34, 0x56))
    );
    assert_eq!(sidecar.chart_frame_fill, crate::ChartFrameFill::NoFill);
    assert_eq!(sidecar.chart_category_major_gridlines, Some(true));
    assert_eq!(sidecar.chart_value_major_gridlines, Some(false));
    assert_eq!(sidecar.from_cell, Some((2, 1)));
    assert_eq!(sidecar.to_cell, Some((14, 7)));
}

#[test]
fn xlsb_chart_sidecar_uses_explicit_minor_theme_latin_font() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    writer
        .start_file("xl/drawings/drawing1.xml", options)
        .unwrap();
    writer
            .write_all(br#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:row>2</xdr:row></xdr:from><xdr:to><xdr:col>7</xdr:col><xdr:row>14</xdr:row></xdr:to><xdr:graphicFrame><c:chart r:id="rIdChart"/></xdr:graphicFrame></xdr:twoCellAnchor></xdr:wsDr>"#)
            .unwrap();
    writer
        .start_file("xl/drawings/_rels/drawing1.xml.rels", options)
        .unwrap();
    writer
            .write_all(br#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#)
            .unwrap();
    writer.start_file("xl/charts/chart1.xml", options).unwrap();
    writer
            .write_all(
                br#"<chartSpace><chart><plotArea><lineChart><axId val="1"/><axId val="2"/></lineChart><catAx><axId val="1"/><crossAx val="2"/></catAx><valAx><axId val="2"/><crossAx val="1"/></valAx></plotArea></chart></chartSpace>"#,
            )
            .unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
    let sheet_rels = r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#;
    let theme = parse_xlsb_theme(&complete_theme_xml("Ignored Major", "Source Sans 3"));

    let mut chart_budget = crate::xlsx::ChartImportBudget::default();
    let (_, charts, metadata, losses) = read_sheet_drawings(
        &mut zip,
        "xl/worksheets/sheet1.bin",
        sheet_rels,
        &theme,
        &mut chart_budget,
    );

    assert_eq!(charts.len(), 1);
    assert!(losses.is_empty(), "unexpected losses: {losses:?}");
    let sidecar = metadata
        .iter()
        .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
        .expect("chart sidecar");
    assert_eq!(
        sidecar.chart_default_latin_font_family.as_deref(),
        Some("Source Sans 3")
    );
}

#[test]
fn xlsb_minor_theme_latin_font_family_rejects_empty_and_oversized_values() {
    for invalid in [String::new(), " ".to_string(), "x".repeat(256)] {
        let xml = complete_theme_xml("Major", &invalid);
        let theme = parse_xlsb_theme(&xml);
        assert!(!theme.source_valid);
        assert!(theme.minor_latin_font_family.is_none());
        assert_eq!(
            theme.chart_default_latin_font_family(),
            crate::xlsx::CALC_IMPORTED_CHART_LATIN_FONT_FAMILY
        );
    }
}

#[test]
fn xlsb_missing_drawing_target_retains_anchor_sidecar() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file("xl/drawings/drawing1.xml", SimpleFileOptions::default())
        .unwrap();
    writer
            .write_all(
                br#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:row>2</xdr:row></xdr:from><xdr:to><xdr:col>4</xdr:col><xdr:row>5</xdr:row></xdr:to><xdr:pic><xdr:blipFill><a:blip r:embed="rIdMissing"/></xdr:blipFill></xdr:pic></xdr:twoCellAnchor></xdr:wsDr>"#,
            )
            .unwrap();
    writer
        .start_file(
            "xl/drawings/_rels/drawing1.xml.rels",
            SimpleFileOptions::default(),
        )
        .unwrap();
    writer.write_all(b"<Relationships/>").unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
    let sheet_rels = r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#;
    let mut chart_budget = crate::xlsx::ChartImportBudget::default();
    let (images, charts, metadata, losses) = read_sheet_drawings(
        &mut zip,
        "xl/worksheets/sheet1.bin",
        sheet_rels,
        &XlsbTheme::default(),
        &mut chart_budget,
    );
    assert!(images.is_empty());
    assert!(charts.is_empty());
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].kind, DrawingObjectKind::Shape);
    assert_eq!(metadata[0].from_offset_emu, None);
    assert!(losses
        .iter()
        .any(|loss| loss.kind == StyleLossKind::DrawingMetadataPartial));
}

#[test]
fn xlsb_hyperlinks_surface_public_metadata() {
    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("Links"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let url = "https://example.com/xlsb";
    let mut hlink = Vec::new();
    for value in [1u32, 2, 1, 1] {
        hlink.extend_from_slice(&value.to_le_bytes());
    }
    hlink.extend_from_slice(&wstr("rId2"));
    hlink.extend_from_slice(&wstr(""));
    hlink.extend_from_slice(&wstr("Open report"));
    hlink.extend_from_slice(&wstr(""));
    let sheet = rec(0x01EE, &hlink);

    let wb_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/></Relationships>"#;
    let sheet_rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="{url}" TargetMode="External"/></Relationships>"#
    );

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", wb_rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ("xl/worksheets/_rels/sheet1.bin.rels", sheet_rels.as_bytes()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

    assert_eq!(
        wb.sheets[0].hyperlinks(),
        &[(1, 1, url.to_string()), (2, 1, url.to_string())]
    );
}

#[test]
fn xlsb_comments_surface_public_metadata() {
    const BRT_BEGIN_COMMENTS: u32 = 628;
    const BRT_END_COMMENTS: u32 = 629;
    const BRT_BEGIN_COMMENT_AUTHORS: u32 = 630;
    const BRT_END_COMMENT_AUTHORS: u32 = 631;
    const BRT_COMMENT_AUTHOR: u32 = 632;
    const BRT_BEGIN_COMMENT_LIST: u32 = 633;
    const BRT_END_COMMENT_LIST: u32 = 634;
    const BRT_BEGIN_COMMENT: u32 = 635;
    const BRT_END_COMMENT: u32 = 636;
    const BRT_COMMENT_TEXT: u32 = 637;

    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("Notes"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let mut comment = Vec::new();
    comment.extend_from_slice(&0u32.to_le_bytes()); // iauthor
    for value in [2u32, 2, 3, 3] {
        comment.extend_from_slice(&value.to_le_bytes()); // UncheckedRfX: D3
    }
    comment.extend_from_slice(&[0u8; 16]); // guid

    let mut rich_text = vec![0x01]; // RichStr flags: fRichStr=1, fExtStr=0
    rich_text.extend_from_slice(&wstr("검토 필요"));
    rich_text.extend_from_slice(&0u32.to_le_bytes()); // zero StrRun entries

    let mut comments = rec(BRT_BEGIN_COMMENTS, &[]);
    comments.extend_from_slice(&rec(BRT_BEGIN_COMMENT_AUTHORS, &[]));
    comments.extend_from_slice(&rec(BRT_COMMENT_AUTHOR, &wstr("auditor")));
    comments.extend_from_slice(&rec(BRT_END_COMMENT_AUTHORS, &[]));
    comments.extend_from_slice(&rec(BRT_BEGIN_COMMENT_LIST, &[]));
    comments.extend_from_slice(&rec(BRT_BEGIN_COMMENT, &comment));
    comments.extend_from_slice(&rec(BRT_COMMENT_TEXT, &rich_text));
    comments.extend_from_slice(&rec(BRT_END_COMMENT, &[]));
    comments.extend_from_slice(&rec(BRT_END_COMMENT_LIST, &[]));
    comments.extend_from_slice(&rec(BRT_END_COMMENTS, &[]));

    let wb_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/></Relationships>"#;
    let sheet_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", wb_rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", [].as_slice()),
        ("xl/worksheets/_rels/sheet1.bin.rels", sheet_rels.as_bytes()),
        ("xl/comments1.bin", comments.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    let comments = wb.sheets[0].comments();

    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].row, 2);
    assert_eq!(comments[0].col, 3);
    assert_eq!(comments[0].text, "검토 필요");
    assert_eq!(comments[0].author.as_deref(), Some("auditor"));
}

#[test]
fn xlsb_tables_surface_public_metadata() {
    const BRT_BEGIN_LIST: u32 = 288;
    const BRT_BEGIN_LIST_COL: u32 = 291;
    const BRT_BEGIN_LIST_COLS: u32 = 293;
    const BRT_LIST_PART: u32 = 550;
    const BRT_TABLE_STYLE_CLIENT: u32 = 649;

    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("Tables"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let sheet = rec(BRT_LIST_PART, &wstr("rId2"));

    let mut begin_list = Vec::new();
    for value in [0u32, 2, 0, 2] {
        begin_list.extend_from_slice(&value.to_le_bytes()); // A1:C3
    }
    begin_list.extend_from_slice(&0u32.to_le_bytes()); // lt = LTRANGE
    begin_list.extend_from_slice(&1u32.to_le_bytes()); // idList
    begin_list.extend_from_slice(&1u32.to_le_bytes()); // crwHeader
    begin_list.extend_from_slice(&0u32.to_le_bytes()); // crwTotals
    begin_list.extend_from_slice(&0u32.to_le_bytes()); // table flags
    for _ in 0..6 {
        begin_list.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // DXF ids
    }
    begin_list.extend_from_slice(&0u32.to_le_bytes()); // dwConnID
    begin_list.extend_from_slice(&null_wstr()); // stName
    begin_list.extend_from_slice(&wstr("SalesTable")); // stDisplayName
    begin_list.extend_from_slice(&wstr("")); // stComment
    begin_list.extend_from_slice(&null_wstr()); // stStyleHeader
    begin_list.extend_from_slice(&null_wstr()); // stStyleData
    begin_list.extend_from_slice(&null_wstr()); // stStyleAgg

    let mut table = rec(BRT_BEGIN_LIST, &begin_list);
    table.extend_from_slice(&rec(BRT_BEGIN_LIST_COLS, &3u32.to_le_bytes()));
    for (idx, caption) in ["Item", "Qty", "Total"].iter().enumerate() {
        let mut column = Vec::new();
        column.extend_from_slice(&((idx + 1) as u32).to_le_bytes()); // idField
        column.extend_from_slice(&0u32.to_le_bytes()); // ilta
        column.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // nDxfHdr
        column.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // nDxfInsertRow
        column.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // nDxfAgg
        column.extend_from_slice(&0u32.to_le_bytes()); // idqsif
        column.extend_from_slice(&null_wstr()); // stName
        column.extend_from_slice(&wstr(caption)); // stCaption
        column.extend_from_slice(&null_wstr()); // stTotal
        column.extend_from_slice(&null_wstr()); // stStyleHeader
        column.extend_from_slice(&null_wstr()); // stStyleInsertRow
        column.extend_from_slice(&null_wstr()); // stStyleAgg
        table.extend_from_slice(&rec(BRT_BEGIN_LIST_COL, &column));
    }
    let mut style = 0b100u16.to_le_bytes().to_vec(); // fRowStripes
    style.extend_from_slice(&wstr("TableStyleMedium9"));
    table.extend_from_slice(&rec(BRT_TABLE_STYLE_CLIENT, &style));

    let wb_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/></Relationships>"#;
    let sheet_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", wb_rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ("xl/worksheets/_rels/sheet1.bin.rels", sheet_rels.as_bytes()),
        ("xl/tables/table1.bin", table.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    let tables = wb.sheets[0].tables();

    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "SalesTable");
    assert_eq!(tables[0].range, (0, 0, 2, 2));
    assert_eq!(tables[0].columns, vec!["Item", "Qty", "Total"]);
    assert_eq!(tables[0].style.as_deref(), Some("TableStyleMedium9"));
}

#[test]
fn xlsb_sheet_view_and_autofilter_surface_public_metadata() {
    const BRT_BEGIN_WS_VIEWS: u32 = 133;
    const BRT_END_WS_VIEWS: u32 = 134;
    const BRT_BEGIN_WS_VIEW: u32 = 137;
    const BRT_END_WS_VIEW: u32 = 138;
    const BRT_BEGIN_AFILTER: u32 = 161;
    const BRT_END_AFILTER: u32 = 162;
    const BRT_PANE: u32 = 151;

    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("View"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let mut ws_view = Vec::new();
    let flags = (1u16 << 4) // fDspZeros
            | (1u16 << 5) // fRightToLeft
            | (1u16 << 6) // fSelected
            | (1u16 << 7) // fDspRuler
            | (1u16 << 8) // fDspGuts
            | (1u16 << 9); // fDefaultHdr
    ws_view.extend_from_slice(&flags.to_le_bytes());
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // xlView normal
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // rwTop
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // colLeft
    ws_view.push(0x40); // icvHdr
    ws_view.push(0); // reserved2
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // reserved3
    ws_view.extend_from_slice(&125u16.to_le_bytes()); // wScale
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleNormal
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleSLV
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScalePLV
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // iWbkView

    let mut pane = Vec::new();
    pane.extend_from_slice(&1.0f64.to_le_bytes()); // frozen rows
    pane.extend_from_slice(&2.0f64.to_le_bytes()); // frozen columns
    pane.extend_from_slice(&1u32.to_le_bytes()); // rwTop
    pane.extend_from_slice(&2u32.to_le_bytes()); // colLeft
    pane.extend_from_slice(&0u32.to_le_bytes()); // pnnAct
    pane.push(0x01); // fFrozen

    let mut autofilter = Vec::new();
    for value in [0u32, 9, 0, 3] {
        autofilter.extend_from_slice(&value.to_le_bytes());
    }

    let mut sheet = rec(BRT_BEGIN_WS_VIEWS, &[]);
    sheet.extend_from_slice(&rec(BRT_BEGIN_WS_VIEW, &ws_view));
    sheet.extend_from_slice(&rec(BRT_PANE, &pane));
    sheet.extend_from_slice(&rec(BRT_END_WS_VIEW, &[]));
    sheet.extend_from_slice(&rec(BRT_END_WS_VIEWS, &[]));
    sheet.extend_from_slice(&rec(BRT_BEGIN_AFILTER, &autofilter));
    sheet.extend_from_slice(&rec(BRT_END_AFILTER, &[]));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    let sheet = &wb.sheets[0];

    assert_eq!(
        sheet.sheet_view(),
        crate::SheetView {
            freeze: Some((1, 2)),
            hide_gridlines: true,
            zoom: Some(125),
            show_headers: Some(false),
            right_to_left: true,
        }
    );
    assert_eq!(sheet.autofilter_range(), Some((0, 0, 9, 3)));
}

#[test]
fn xlsb_sheet_view_explicit_visible_headers_are_preserved() {
    const BRT_BEGIN_WS_VIEWS: u32 = 133;
    const BRT_END_WS_VIEWS: u32 = 134;
    const BRT_BEGIN_WS_VIEW: u32 = 137;
    const BRT_END_WS_VIEW: u32 = 138;

    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("View"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let mut ws_view = Vec::new();
    let flags = (1u16 << 2) // display gridlines
            | (1u16 << 3); // display row/column headings
    ws_view.extend_from_slice(&flags.to_le_bytes());
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // xlView normal
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // rwTop
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // colLeft
    ws_view.push(0x40); // icvHdr
    ws_view.push(0); // reserved2
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // reserved3
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScale
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleNormal
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleSLV
    ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScalePLV
    ws_view.extend_from_slice(&0u32.to_le_bytes()); // iWbkView

    let mut sheet = rec(BRT_BEGIN_WS_VIEWS, &[]);
    sheet.extend_from_slice(&rec(BRT_BEGIN_WS_VIEW, &ws_view));
    sheet.extend_from_slice(&rec(BRT_END_WS_VIEW, &[]));
    sheet.extend_from_slice(&rec(BRT_END_WS_VIEWS, &[]));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

    assert_eq!(wb.sheets[0].sheet_view().show_headers, Some(true));
}

#[test]
fn xlsb_ws_prop_tab_color_surfaces_public_metadata() {
    const BRT_WS_PROP: u32 = 147;

    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("Color"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let mut color = Vec::new();
    color.push(1 | (2 << 1)); // fValidRGB + xColorType=ARGB
    color.push(0); // index, ignored for ARGB
    color.extend_from_slice(&0i16.to_le_bytes()); // nTintAndShade
    color.extend_from_slice(&[0x12, 0x34, 0x56, 0xFF]); // RGB + alpha

    let mut ws_prop = Vec::new();
    ws_prop.extend_from_slice(&0u16.to_le_bytes()); // worksheet property flags
    ws_prop.push(0); // filter/conditional-format flags
    ws_prop.extend_from_slice(&color);
    ws_prop.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // rwSync ignored
    ws_prop.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // colSync ignored
    ws_prop.extend_from_slice(&wstr("")); // code name

    let sheet = rec(BRT_WS_PROP, &ws_prop);

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

    assert_eq!(wb.sheets[0].tab_color(), Some(Color::rgb(0x12, 0x34, 0x56)));
}

#[test]
fn xlsb_sheet_protection_surfaces_public_metadata() {
    fn sheet_protection() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0u16.to_le_bytes()); // protpwd
        for allowed in [
            1u32, // fLocked
            0,    // fObjects (not modeled)
            0,    // fScenarios (not modeled)
            1,    // fFormatCells
            0,    // fFormatColumns
            1,    // fFormatRows
            0,    // fInsertColumns
            1,    // fInsertRows
            1,    // fInsertHyperlinks
            0,    // fDeleteColumns
            1,    // fDeleteRows
            1,    // fSelLockedCells (not modeled)
            1,    // fSort
            1,    // fAutoFilter
            0,    // fPivotTables
            1,    // fSelUnlockedCells (not modeled)
        ] {
            out.extend_from_slice(&allowed.to_le_bytes());
        }
        out
    }

    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("Protected"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let sheet = rec(BRT_SHEET_PROTECTION, &sheet_protection());

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    let sheet = &wb.sheets[0];

    assert!(sheet.is_protected());
    assert_eq!(
        sheet.protection_options(),
        Some(crate::ProtectionOptions {
            sort: true,
            auto_filter: true,
            format_cells: true,
            format_rows: true,
            insert_rows: true,
            insert_hyperlinks: true,
            delete_rows: true,
            ..Default::default()
        })
    );

    let metadata = sheet.metadata();
    assert!(metadata.protected);
    assert_eq!(metadata.protection_options, sheet.protection_options());
}

#[test]
fn xlsb_book_protection_surfaces_workbook_metadata() {
    const BRT_BOOK_PROTECTION: u32 = 534;

    fn book_protection(flags: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0u16.to_le_bytes()); // protpwdBook
        out.extend_from_slice(&0u16.to_le_bytes()); // protpwdRev
        out.extend_from_slice(&flags.to_le_bytes()); // wFlags
        out
    }

    let mut wb_bin = rec(BRT_BOOK_PROTECTION, &book_protection(0x0001));
    let mut sheet_ref = vec![0u8; 8]; // hsState + iTabID
    sheet_ref.extend_from_slice(&wstr("rId1"));
    sheet_ref.extend_from_slice(&wstr("LockedBook"));
    wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &sheet_ref));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", [].as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

    assert!(wb.is_structure_protected());
    assert!(wb.metadata().structure_protected);
}

#[test]
fn xlsb_outline_records_surface_public_metadata() {
    fn row_hdr(row: u32, level: u8, collapsed: bool, manual: bool) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&row.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // ixfe
        out.extend_from_slice(&400u16.to_le_bytes()); // miyRw: 20 pt
        let mut flags = u16::from(level) << 8;
        if manual {
            flags |= 1 << 13; // fUnsynced
        }
        if collapsed {
            flags |= 1 << 11;
            flags |= 1 << 12; // hidden
        }
        out.extend_from_slice(&flags.to_le_bytes());
        out
    }

    fn col_info(first: u32, last: u32, level: u8) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&first.to_le_bytes());
        out.extend_from_slice(&last.to_le_bytes());
        out.extend_from_slice(&0x08FFu32.to_le_bytes()); // default width
        out.extend_from_slice(&0u32.to_le_bytes()); // ixfe
        out.extend_from_slice(&((u16::from(level) << 8) | 1).to_le_bytes());
        out
    }

    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("Outline"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let mut ws_prop = Vec::new();
    ws_prop.extend_from_slice(&0u16.to_le_bytes()); // summaries above/left
    ws_prop.push(0); // filter/conditional-format flags
    ws_prop.extend_from_slice(&[0u8; 8]); // no tab color
    ws_prop.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // rwSync ignored
    ws_prop.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // colSync ignored
    ws_prop.extend_from_slice(&wstr("")); // code name

    let mut sheet = rec(BRT_WS_PROP, &ws_prop);
    let mut ws_format = Vec::new();
    ws_format.extend_from_slice(&u32::MAX.to_le_bytes());
    ws_format.extend_from_slice(&8u16.to_le_bytes());
    ws_format.extend_from_slice(&300u16.to_le_bytes());
    ws_format.extend_from_slice(&0x0002u16.to_le_bytes()); // fDyZero
    ws_format.extend_from_slice(&[0, 0]);
    sheet.extend_from_slice(&rec(BRT_WS_FMT_INFO, &ws_format));
    sheet.extend_from_slice(&rec(BRT_COL_INFO, &col_info(1, 3, 3)));
    sheet.extend_from_slice(&rec(BRT_ROW_HDR, &row_hdr(2, 2, true, true)));
    sheet.extend_from_slice(&rec(BRT_ROW_HDR, &row_hdr(3, 2, false, false)));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    let sheet = &wb.sheets[0];

    assert_eq!(sheet.row_outline_levels().get(&2), Some(&2));
    assert_eq!(sheet.row_outline_levels().get(&3), Some(&2));
    assert!(sheet.collapsed_rows().contains(&2));
    assert_eq!(sheet.col_outline_levels().get(&1), Some(&3));
    assert_eq!(sheet.col_outline_levels().get(&3), Some(&3));
    assert_eq!(sheet.row_heights().get(&2), Some(&20.0));
    assert!(!sheet.row_heights().contains_key(&3));
    assert!(sheet.row_height_is_manual(2));
    assert_eq!(sheet.default_row_height(), None);
    assert!(!sheet.default_row_height_is_manual());
    assert!(sheet.hidden_rows().contains(&2));
    assert_eq!(
        sheet
            .default_hidden_row_exceptions()
            .expect("fDyZero provenance")
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [3]
    );
    assert_eq!(
        sheet.column_widths().get(&1),
        Some(&(0x08FF as f32 / 256.0))
    );
    assert_eq!(sheet.xlsb_column_widths_256().get(&1), Some(&0x08FF));
    assert_eq!(
        sheet.xlsb_default_column_width(),
        Some(XlsbDefaultColumnWidth::BaseCharacters(8))
    );
    assert!(sheet.hidden_columns().contains(&1));
    assert!(!sheet.outline_summary_below());
    assert!(!sheet.outline_summary_right());

    let metadata = sheet.metadata();
    assert_eq!(metadata.row_outline_levels.get(&2), Some(&2));
    assert_eq!(metadata.col_outline_levels.get(&1), Some(&3));
    assert!(metadata.collapsed_rows.contains(&2));
    assert!(!metadata.outline_summary_below);
    assert!(!metadata.outline_summary_right);
}

#[test]
fn xlsb_page_setup_records_surface_public_metadata() {
    // [MS-XLSB] 2.3 record numbering; these exact ids also occur in
    // Excel-authored Apache POI fixtures.  A former off-by-one synthetic
    // sequence (475..479) must not define the reader contract.
    const BRT_END_HEADER_FOOTER: u32 = 480;

    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("Print"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let mut margins = Vec::new();
    for margin in [0.7, 0.8, 0.9, 1.0, 0.3, 0.4] {
        margins.extend_from_slice(&f64::to_le_bytes(margin));
    }

    let mut page_setup = Vec::new();
    page_setup.extend_from_slice(&9u32.to_le_bytes()); // iPaperSize: A4
    page_setup.extend_from_slice(&80u32.to_le_bytes()); // iScale
    page_setup.extend_from_slice(&600u32.to_le_bytes()); // iRes
    page_setup.extend_from_slice(&600u32.to_le_bytes()); // iVRes
    page_setup.extend_from_slice(&1u32.to_le_bytes()); // iCopies
    page_setup.extend_from_slice(&3i32.to_le_bytes()); // iPageStart
    page_setup.extend_from_slice(&2u32.to_le_bytes()); // iFitWidth
    page_setup.extend_from_slice(&1u32.to_le_bytes()); // iFitHeight
    page_setup.extend_from_slice(&((1u16 << 0) | (1u16 << 1) | (1u16 << 7)).to_le_bytes());
    page_setup.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // szRelID null

    let mut header_footer = Vec::new();
    header_footer.extend_from_slice(&0x000Fu16.to_le_bytes());
    for text in [
        "&CQuarterly",
        "&RPage &P",
        "&LEven",
        "&REvenF",
        "&LFirst",
        "&RFirstF",
    ] {
        header_footer.extend_from_slice(&wstr(text));
    }

    fn page_break(index: u32, manual: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&index.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&u32::MAX.to_le_bytes());
        out.extend_from_slice(&manual.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    let mut sheet = rec(BRT_WS_PROP, &0x0100u16.to_le_bytes());
    sheet.extend_from_slice(&rec(BRT_MARGINS, &margins));
    sheet.extend_from_slice(&rec(BRT_PRINT_OPTIONS, &0b1111u16.to_le_bytes()));
    sheet.extend_from_slice(&rec(BRT_PAGE_SETUP, &page_setup));
    sheet.extend_from_slice(&rec(BRT_BEGIN_HEADER_FOOTER, &header_footer));
    sheet.extend_from_slice(&rec(BRT_END_HEADER_FOOTER, &[]));
    sheet.extend_from_slice(&rec(BRT_BEGIN_RW_BRK, &[]));
    sheet.extend_from_slice(&rec(BRT_BRK, &page_break(20, 1)));
    sheet.extend_from_slice(&rec(BRT_BRK, &page_break(5, 1)));
    sheet.extend_from_slice(&rec(BRT_BRK, &page_break(8, 0)));
    sheet.extend_from_slice(&rec(BRT_END_RW_BRK, &[]));
    sheet.extend_from_slice(&rec(BRT_BEGIN_COL_BRK, &[]));
    sheet.extend_from_slice(&rec(BRT_BRK, &page_break(7, 1)));
    sheet.extend_from_slice(&rec(BRT_BRK, &page_break(3, 1)));
    sheet.extend_from_slice(&rec(BRT_END_COL_BRK, &[]));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    let ps = wb.sheets[0].page_setup().expect("page setup");

    assert!(ps.landscape);
    assert_eq!(ps.paper_size, Some(9));
    assert_eq!(ps.scale, Some(80));
    assert_eq!(ps.fit_to_width, Some(2));
    assert_eq!(ps.fit_to_height, Some(1));
    assert_eq!(ps.first_page_number, Some(3));
    assert_eq!(ps.margins, Some((0.7, 0.8, 0.9, 1.0, 0.3, 0.4)));
    assert!(ps.center_horizontally);
    assert!(ps.center_vertically);
    assert!(wb.sheets[0].print_headings());
    assert!(wb.sheets[0].print_gridlines());
    assert_eq!(ps.header.as_deref(), Some("&CQuarterly"));
    assert_eq!(ps.footer.as_deref(), Some("&RPage &P"));
    let metadata = wb.sheets[0].print_metadata();
    assert_eq!(metadata.fidelity(), crate::PrintFidelity::Retained);
    assert_eq!(metadata.manual_row_breaks(), &[5, 20]);
    assert_eq!(metadata.manual_col_breaks(), &[3, 7]);
    assert_eq!(metadata.page_order(), Some(PrintPageOrder::OverThenDown));
    assert_eq!(metadata.fit_to_page(), Some(true));
    assert_eq!(metadata.print_headings(), Some(true));
    assert_eq!(metadata.print_gridlines(), Some(true));
    assert_eq!(metadata.center_horizontally(), Some(true));
    assert_eq!(metadata.center_vertically(), Some(true));
    assert_eq!(metadata.header_footer().odd_header(), Some("&CQuarterly"));
    assert_eq!(metadata.header_footer().odd_footer(), Some("&RPage &P"));
    assert_eq!(metadata.header_footer().even_header(), Some("&LEven"));
    assert_eq!(metadata.header_footer().even_footer(), Some("&REvenF"));
    assert_eq!(metadata.header_footer().first_header(), Some("&LFirst"));
    assert_eq!(metadata.header_footer().first_footer(), Some("&RFirstF"));
    assert_eq!(metadata.header_footer().different_odd_even(), Some(true));
    assert_eq!(metadata.header_footer().different_first(), Some(true));
}

#[test]
fn xlsb_fit_to_page_mode_controls_zero_fit_count_retention_in_any_record_order() {
    fn open_page_setup(
        ws_flags: Option<u16>,
        ws_prop_first: bool,
        fit_width: u32,
        fit_height: u32,
    ) -> Workbook {
        let mut wb_bin = vec![0u8; 8];
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Print"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let mut page_setup = Vec::new();
        page_setup.extend_from_slice(&9u32.to_le_bytes());
        page_setup.extend_from_slice(&80u32.to_le_bytes());
        page_setup.extend_from_slice(&600u32.to_le_bytes());
        page_setup.extend_from_slice(&600u32.to_le_bytes());
        page_setup.extend_from_slice(&1u32.to_le_bytes());
        page_setup.extend_from_slice(&1i32.to_le_bytes());
        page_setup.extend_from_slice(&fit_width.to_le_bytes());
        page_setup.extend_from_slice(&fit_height.to_le_bytes());
        page_setup.extend_from_slice(&0u16.to_le_bytes());
        page_setup.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        let ws_prop = ws_flags.map(|flags| rec(BRT_WS_PROP, &flags.to_le_bytes()));
        let mut sheet = Vec::new();
        if ws_prop_first {
            if let Some(record) = ws_prop.as_ref() {
                sheet.extend_from_slice(record);
            }
        }
        sheet.extend_from_slice(&rec(BRT_PAGE_SETUP, &page_setup));
        if !ws_prop_first {
            if let Some(record) = ws_prop.as_ref() {
                sheet.extend_from_slice(record);
            }
        }

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ] {
            writer.start_file(path, options).unwrap();
            writer.write_all(body).unwrap();
        }
        Workbook::open(&writer.finish().unwrap().into_inner()).unwrap()
    }

    for (
        label,
        ws_flags,
        ws_prop_first,
        fit_width,
        fit_height,
        expected_mode,
        expected_width,
        expected_height,
    ) in [
        (
            "fit flag before conflicting scale and fit",
            Some(0x0100),
            true,
            2,
            1,
            Some(true),
            Some(2),
            Some(1),
        ),
        (
            "scale flag after conflicting scale and fit",
            Some(0),
            false,
            2,
            1,
            Some(false),
            Some(2),
            Some(1),
        ),
        (
            "fit one by unconstrained after page setup",
            Some(0x0100),
            false,
            1,
            0,
            Some(true),
            Some(1),
            Some(0),
        ),
        (
            "legacy fallback without worksheet properties",
            None,
            true,
            1,
            0,
            None,
            Some(1),
            None,
        ),
    ] {
        let workbook = open_page_setup(ws_flags, ws_prop_first, fit_width, fit_height);
        let sheet = &workbook.sheets[0];
        let setup = sheet.page_setup().expect(label);
        assert_eq!(setup.scale, Some(80), "{label}");
        assert_eq!(setup.fit_to_width, expected_width, "{label}");
        assert_eq!(setup.fit_to_height, expected_height, "{label}");
        assert_eq!(
            sheet.print_metadata().fit_to_page(),
            expected_mode,
            "{label}"
        );

        if let Some(expected_mode) = expected_mode {
            let output = workbook.to_xlsx();
            let reopened = Workbook::open(&output).expect(label);
            let sheet = &reopened.sheets[0];
            assert_eq!(
                sheet.print_metadata().fit_to_page(),
                Some(expected_mode),
                "{label} after XLSX write/reopen"
            );
            let setup = sheet.page_setup().expect(label);
            assert_eq!(setup.scale, Some(80), "{label} after XLSX write/reopen");
            assert_eq!(
                setup.fit_to_width, expected_width,
                "{label} after XLSX write/reopen"
            );
            assert_eq!(
                setup.fit_to_height, expected_height,
                "{label} after XLSX write/reopen"
            );
        }
    }
}

#[test]
fn malformed_xlsb_print_records_report_typed_losses() {
    let mut metadata = SheetReadMetadata::default();
    parse_header_footer(&[0x01], &mut metadata);
    parse_xlsb_page_break(
        &[0; 8],
        Some(XlsbPageBreakAxis::Row),
        &mut metadata.print_metadata,
    );
    parse_xlsb_page_break(
        &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0],
        Some(XlsbPageBreakAxis::Row),
        &mut metadata.print_metadata,
    );

    assert_eq!(
        metadata.print_metadata.fidelity(),
        crate::PrintFidelity::Partial
    );
    assert!(metadata
        .print_metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::MalformedHeaderFooter));
    assert!(metadata
        .print_metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::InvalidPageBreak));
}

#[test]
fn xlsb_data_validations_surface_public_metadata() {
    const BRT_DVAL: u32 = 64;
    const BRT_BEGIN_DVALS: u32 = 573;
    const BRT_END_DVALS: u32 = 574;
    const BRT_DVAL_LIST: u32 = 681;

    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("Validation"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let mut dvals = Vec::new();
    dvals.extend_from_slice(&0u16.to_le_bytes()); // DVals flags
    dvals.extend_from_slice(&0u32.to_le_bytes()); // xLeft
    dvals.extend_from_slice(&0u32.to_le_bytes()); // yTop
    dvals.extend_from_slice(&0u32.to_le_bytes()); // unused
    dvals.extend_from_slice(&1u32.to_le_bytes()); // idvMac

    let mut dval = Vec::new();
    let flags = 3u32 // valType=list
            | (1u32 << 8) // fAllowBlank
            | (1u32 << 18); // fShowInputMsg
    dval.extend_from_slice(&flags.to_le_bytes());
    dval.extend_from_slice(&2i32.to_le_bytes()); // two UncheckedRfX ranges
    for value in [0u32, 0, 0, 0, 2, 3, 0, 0] {
        dval.extend_from_slice(&value.to_le_bytes()); // A1, A3:A4
    }
    dval.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // strErrorTitle null
    dval.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // strError null
    dval.extend_from_slice(&wstr("Pick")); // strPromptTitle
    dval.extend_from_slice(&wstr("Choose one")); // strPrompt
    dval.extend_from_slice(&0u32.to_le_bytes()); // formula1.cce
    dval.extend_from_slice(&0u32.to_le_bytes()); // formula1.cb
    dval.extend_from_slice(&0u32.to_le_bytes()); // formula2.cce
    dval.extend_from_slice(&0u32.to_le_bytes()); // formula2.cb

    let mut sheet = rec(BRT_BEGIN_DVALS, &dvals);
    sheet.extend_from_slice(&rec(BRT_DVAL_LIST, &wstr("\"Yes,No\"")));
    sheet.extend_from_slice(&rec(BRT_DVAL, &dval));
    sheet.extend_from_slice(&rec(BRT_END_DVALS, &[]));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    let validations = wb.sheets[0].data_validations();

    assert_eq!(validations.len(), 2);
    assert_eq!(validations[0].sqref, (0, 0, 0, 0));
    assert_eq!(validations[1].sqref, (2, 0, 3, 0));
    assert_eq!(validations[0].kind, crate::DvKind::List);
    assert_eq!(validations[0].operator, crate::DvOp::Between);
    assert_eq!(validations[0].formula1, "\"Yes,No\"");
    assert!(validations[0].allow_blank);
    assert!(validations[0].show_input_message);
    assert!(!validations[0].show_error_message);
    assert_eq!(
        validations[0].prompt.as_ref(),
        Some(&("Pick".to_string(), "Choose one".to_string()))
    );
}

#[test]
fn xlsb_data_validations_consume_ranges_beyond_retained_cap() {
    let mut dval = Vec::new();
    dval.extend_from_slice(&3u32.to_le_bytes()); // valType=list
    dval.extend_from_slice(&((MAX_DVAL_RANGES + 1) as i32).to_le_bytes());
    for _ in 0..=MAX_DVAL_RANGES {
        for value in [0u32, 0, 0, 0] {
            dval.extend_from_slice(&value.to_le_bytes());
        }
    }
    for _ in 0..4 {
        dval.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    }
    dval.extend_from_slice(&0u32.to_le_bytes()); // formula1.cce
    dval.extend_from_slice(&0u32.to_le_bytes()); // formula1.cb
    dval.extend_from_slice(&0u32.to_le_bytes()); // formula2.cce
    dval.extend_from_slice(&0u32.to_le_bytes()); // formula2.cb

    let validations = parse_dval(&dval, Some("\"A\"".to_string()));

    assert_eq!(validations.len(), MAX_DVAL_RANGES);
    assert_eq!(validations[0].kind, crate::DvKind::List);
    assert_eq!(validations[0].formula1, "\"A\"");
}

#[test]
fn xlsb_defined_name_is_read_from_brt_name_record() {
    let mut name = Vec::new();
    name.extend_from_slice(&0u32.to_le_bytes()); // flags: visible, non-built-in
    name.push(0); // chKey
    name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // workbook scope
    name.extend_from_slice(&wstr("Answer"));
    name.extend_from_slice(&3u32.to_le_bytes()); // cce
    name.extend_from_slice(&[0x1E, 42, 0]); // PtgInt(42)
    name.extend_from_slice(&0u32.to_le_bytes()); // formula cb
    name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // null comment

    let mut wb_bin = rec(39, &name);
    let mut local_name = Vec::new();
    local_name.extend_from_slice(&0u32.to_le_bytes());
    local_name.push(0);
    local_name.extend_from_slice(&0u32.to_le_bytes()); // zero-based sheet scope
    local_name.extend_from_slice(&wstr("Rate"));
    local_name.extend_from_slice(&3u32.to_le_bytes());
    local_name.extend_from_slice(&[0x1E, 7, 0]);
    local_name.extend_from_slice(&0u32.to_le_bytes());
    local_name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    wb_bin.extend_from_slice(&rec(39, &local_name));
    let mut bundle = vec![0u8; 8]; // hsState + iTabID
    bundle.extend_from_slice(&wstr("rId1"));
    bundle.extend_from_slice(&wstr("S1"));
    wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
    let mut formula = vec![0u8; 8];
    formula.extend_from_slice(&42.0f64.to_le_bytes());
    formula.extend_from_slice(&[0, 0]);
    let rgce = [0x23, 1, 0, 0, 0]; // PtgName, one-based BrtName index 1
    formula.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
    formula.extend_from_slice(&rgce);
    formula.extend_from_slice(&0u32.to_le_bytes());
    let sheet = rec(BRT_FMLA_NUM, &formula);
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", sheet.as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    assert_eq!(
        wb.defined_names(),
        &[("Answer".to_string(), "42".to_string())]
    );
    assert_eq!(
        wb.local_defined_names(),
        &[crate::LocalDefinedName {
            sheet: "S1".into(),
            name: "Rate".into(),
            refers_to: "7".into(),
        }]
    );
    assert_eq!(
        wb.sheets[0].cell(0, 0),
        Some(&Cell::Formula {
            formula: "Answer".to_string(),
            cached: Box::new(Cell::Number(42.0))
        })
    );
}

#[test]
fn xlsb_sheet_local_builtin_names_surface_page_setup() {
    fn name_builtin(name_text: &str, sheet_index: u32, rgce: &[u8]) -> Vec<u8> {
        let mut name = Vec::new();
        name.extend_from_slice(&0x20u32.to_le_bytes()); // flags: built-in
        name.push(0); // chKey: no macro shortcut
        name.extend_from_slice(&sheet_index.to_le_bytes());
        name.extend_from_slice(&wstr(name_text));
        name.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
        name.extend_from_slice(rgce);
        name.extend_from_slice(&0u32.to_le_bytes()); // formula cb
        name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // null comment
        name
    }

    fn ptg_area(r0: u32, c0: u16, r1: u32, c1: u16) -> Vec<u8> {
        let mut rgce = vec![0x25]; // PtgArea with BIFF12 row widths
        rgce.extend_from_slice(&r0.to_le_bytes());
        rgce.extend_from_slice(&r1.to_le_bytes());
        rgce.extend_from_slice(&c0.to_le_bytes());
        rgce.extend_from_slice(&c1.to_le_bytes());
        rgce
    }

    let mut print_area = ptg_area(1, 1, 5, 3);
    print_area.extend_from_slice(&ptg_area(9, 4, 11, 6));
    print_area.push(0x10); // PtgUnion
    let mut print_titles = ptg_area(0, 0, 1, MAX_XLSB_COL_INDEX as u16);
    print_titles.extend_from_slice(&ptg_area(0, 0, 1_048_575, 2));
    print_titles.push(0x10); // PtgUnion

    let mut wb_bin = rec(BRT_NAME, &name_builtin("_xlnm.Print_Area", 0, &print_area));
    wb_bin.extend_from_slice(&rec(
        BRT_NAME,
        &name_builtin("_xlnm.Print_Titles", 0, &print_titles),
    ));
    let mut bundle = vec![0u8; 8]; // hsState + iTabID
    bundle.extend_from_slice(&wstr("rId1"));
    bundle.extend_from_slice(&wstr("S1"));
    wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", [].as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

    assert!(wb.defined_names().is_empty());
    let ps = wb.sheets[0].page_setup().expect("page setup");
    assert_eq!(ps.print_area, Some((1, 1, 5, 3)));
    assert_eq!(ps.repeat_rows, Some((0, 1)));
    assert_eq!(ps.repeat_cols, Some((0, 2)));
    assert_eq!(
        wb.sheets[0].print_metadata().print_areas(),
        &[(1, 1, 5, 3), (9, 4, 11, 6)]
    );
}

#[test]
fn xlsb_sheet_local_filter_database_name_surfaces_autofilter() {
    fn name_builtin(name_text: &str, sheet_index: u32, rgce: &[u8]) -> Vec<u8> {
        let mut name = Vec::new();
        name.extend_from_slice(&0x20u32.to_le_bytes()); // flags: built-in
        name.push(0); // chKey: no macro shortcut
        name.extend_from_slice(&sheet_index.to_le_bytes());
        name.extend_from_slice(&wstr(name_text));
        name.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
        name.extend_from_slice(rgce);
        name.extend_from_slice(&0u32.to_le_bytes()); // formula cb
        name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // null comment
        name
    }

    let mut filter_area = vec![0x25]; // PtgArea with BIFF12 row widths
    filter_area.extend_from_slice(&2u32.to_le_bytes());
    filter_area.extend_from_slice(&9u32.to_le_bytes());
    filter_area.extend_from_slice(&1u16.to_le_bytes());
    filter_area.extend_from_slice(&4u16.to_le_bytes());

    let mut wb_bin = rec(
        BRT_NAME,
        &name_builtin("_xlnm._FilterDatabase", 0, &filter_area),
    );
    let mut bundle = vec![0u8; 8]; // hsState + iTabID
    bundle.extend_from_slice(&wstr("rId1"));
    bundle.extend_from_slice(&wstr("S1"));
    wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", [].as_slice()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

    assert!(wb.defined_names().is_empty());
    assert_eq!(wb.sheets[0].autofilter_range(), Some((2, 1, 9, 4)));
    assert_eq!(wb.sheets[0].page_setup(), None);
}

#[test]
fn xlsb_doc_properties_surface_through_workbook_metadata() {
    let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
    wb_bin.extend_from_slice(&wstr("rId1"));
    wb_bin.extend_from_slice(&wstr("S1"));
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
    let core = r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"><dc:title>Binary Report</dc:title><dc:subject>Operations</dc:subject><dc:creator>rxls xlsb</dc:creator><cp:keywords>ops,binary</cp:keywords><dc:description>XLSB public metadata</dc:description><cp:lastModifiedBy>reviewer</cp:lastModifiedBy><dcterms:created>2026-06-24T01:02:03Z</dcterms:created></cp:coreProperties>"#;
    let app = r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>Excel</Application><Company>ACME XLSB</Company></Properties>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (path, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", [].as_slice()),
        ("docProps/core.xml", core.as_bytes()),
        ("docProps/app.xml", app.as_bytes()),
    ] {
        zw.start_file(path, opt).unwrap();
        zw.write_all(body).unwrap();
    }

    let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
    let metadata = wb.metadata();
    assert_eq!(metadata.properties.title.as_deref(), Some("Binary Report"));
    assert_eq!(metadata.properties.subject.as_deref(), Some("Operations"));
    assert_eq!(metadata.properties.creator.as_deref(), Some("rxls xlsb"));
    assert_eq!(metadata.properties.keywords.as_deref(), Some("ops,binary"));
    assert_eq!(
        metadata.properties.description.as_deref(),
        Some("XLSB public metadata")
    );
    assert_eq!(
        metadata.properties.last_modified_by.as_deref(),
        Some("reviewer")
    );
    assert_eq!(
        metadata.properties.created.as_deref(),
        Some("2026-06-24T01:02:03Z")
    );
    assert_eq!(metadata.properties.company.as_deref(), Some("ACME XLSB"));
}

#[test]
fn xlsb_formula_is_decoded() {
    // BrtFmlaNum: cell(8) + xnum 30.0 + grbitFlags(2) + cce(4) + rgce. The rgce
    // is BIFF12 SUM(A1:A2): PtgArea (u32 rows) + PtgFuncVar(SUM, 1 arg).
    let mut p = vec![0u8; 8]; // col + style
    p.extend_from_slice(&30.0f64.to_le_bytes()); // xnum
    p.extend_from_slice(&[0, 0]); // grbitFlags
    let rgce: Vec<u8> = vec![
        0x25, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, // PtgArea A1:A2 (r0=0,r1=1,c0=0,c1=0)
        0x22, 0x01, 0x04, 0x00, // PtgFuncVar SUM(1 arg)
    ];
    p.extend_from_slice(&(rgce.len() as u32).to_le_bytes()); // cce
    p.extend_from_slice(&rgce);
    p.extend_from_slice(&0u32.to_le_bytes()); // cb
    let mut cells = Vec::new();
    let mut font_sizes = Vec::new();
    let mut rich = BTreeMap::new();
    let mut budget = crate::MAX_TEXT_BYTES;
    decode_cell(
        BRT_FMLA_NUM,
        &p,
        0,
        0,
        0,
        &[],
        &Styles::default(),
        false,
        &mut cells,
        &mut font_sizes,
        &mut rich,
        &mut budget,
        &[],
        &[],
        &[],
        &[],
        &BrtFormulaDefinitions::new(),
    );
    assert_eq!(cells.len(), 1);
    match &cells[0].value {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "SUM($A$1:$A$2)");
            assert_eq!(**cached, Cell::Number(30.0));
        }
        o => panic!("expected a formula cell, got {o:?}"),
    }
}

fn brt_numeric_formula(col: u32, cached: f64, rgce: &[u8], rgb_extra: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&col.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&cached.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
    body.extend_from_slice(rgce);
    body.extend_from_slice(&(rgb_extra.len() as u32).to_le_bytes());
    body.extend_from_slice(rgb_extra);
    body
}

fn brt_formula_definition(
    range: (u32, u32, u32, u32),
    is_array: bool,
    rgce: &[u8],
    rgb_extra: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&range.0.to_le_bytes());
    body.extend_from_slice(&range.1.to_le_bytes());
    body.extend_from_slice(&range.2.to_le_bytes());
    body.extend_from_slice(&range.3.to_le_bytes());
    if is_array {
        body.push(0);
    }
    body.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
    body.extend_from_slice(rgce);
    body.extend_from_slice(&(rgb_extra.len() as u32).to_le_bytes());
    body.extend_from_slice(rgb_extra);
    body
}

#[test]
fn xlsb_shared_formula_is_reconstructed_for_each_cell() {
    let exp = [0x01, 0, 0, 0, 0]; // PtgExp row 0
    let exp_col = 1u32.to_le_bytes(); // PtgExtraCol B
    let shared_rgce = [0x2C, 0, 0, 0, 0, 0xFF, 0xFF]; // one column left
    let mut sheet = rec(BRT_ROW_HDR, &0u32.to_le_bytes());
    sheet.extend_from_slice(&rec(
        BRT_FMLA_NUM,
        &brt_numeric_formula(1, 10.0, &exp, &exp_col),
    ));
    sheet.extend_from_slice(&rec(
        BRT_SHR_FMLA,
        &brt_formula_definition((0, 1, 1, 1), false, &shared_rgce, &[]),
    ));
    sheet.extend_from_slice(&rec(BRT_ROW_HDR, &1u32.to_le_bytes()));
    sheet.extend_from_slice(&rec(
        BRT_FMLA_NUM,
        &brt_numeric_formula(1, 20.0, &exp, &exp_col),
    ));

    let mut budget = crate::MAX_TEXT_BYTES;
    let (cells, _, _, _) = parse_sheet(
        &sheet,
        &[],
        &Styles::default(),
        false,
        &[],
        &mut budget,
        &[],
        &[],
        &[],
        &[],
    );
    for (row, expected) in [(0, "A1"), (1, "A2")] {
        let cell = cells
            .iter()
            .find(|cell| (cell.row, cell.col) == (row, 1))
            .unwrap();
        match &cell.value {
            Cell::Formula { formula, .. } => assert_eq!(formula, expected),
            other => panic!("expected shared formula at row {row}, got {other:?}"),
        }
    }
}

#[test]
fn xlsb_array_formula_and_array_constant_are_reconstructed() {
    let exp = [0x01, 0, 0, 0, 0];
    let exp_col = 0u32.to_le_bytes();
    let array_rgce = [0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut array_extra = 1u32.to_le_bytes().to_vec();
    array_extra.extend_from_slice(&2u32.to_le_bytes());
    array_extra.push(0);
    array_extra.extend_from_slice(&1.0f64.to_le_bytes());
    array_extra.push(0);
    array_extra.extend_from_slice(&2.0f64.to_le_bytes());

    let mut sheet = rec(BRT_ROW_HDR, &0u32.to_le_bytes());
    sheet.extend_from_slice(&rec(
        BRT_FMLA_NUM,
        &brt_numeric_formula(0, 1.0, &exp, &exp_col),
    ));
    sheet.extend_from_slice(&rec(
        BRT_ARR_FMLA,
        &brt_formula_definition((0, 0, 0, 1), true, &array_rgce, &array_extra),
    ));
    sheet.extend_from_slice(&rec(
        BRT_FMLA_NUM,
        &brt_numeric_formula(1, 2.0, &exp, &exp_col),
    ));

    let mut budget = crate::MAX_TEXT_BYTES;
    let (cells, _, _, _) = parse_sheet(
        &sheet,
        &[],
        &Styles::default(),
        false,
        &[],
        &mut budget,
        &[],
        &[],
        &[],
        &[],
    );
    for col in 0..=1 {
        let cell = cells
            .iter()
            .find(|cell| (cell.row, cell.col) == (0, col))
            .unwrap();
        match &cell.value {
            Cell::Formula { formula, .. } => assert_eq!(formula, "{1,2}"),
            other => panic!("expected array formula at col {col}, got {other:?}"),
        }
    }
}

#[test]
fn xlsb_formula_string_is_decoded() {
    // BrtFmlaString stores its cached string inside the formula cell record
    // (`XLWideString`), unlike BIFF8 `.xls` which uses a following STRING
    // record for string-result formulas.
    let mut p = vec![0u8; 8]; // col + style
    p.extend_from_slice(&wstr("cached"));
    p.extend_from_slice(&[0, 0]); // grbitFlags
    let rgce = vec![0x17, 1, 0, b'x']; // PtgStr("x")
    p.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
    p.extend_from_slice(&rgce);
    p.extend_from_slice(&0u32.to_le_bytes());

    let mut cells = Vec::new();
    let mut font_sizes = Vec::new();
    let mut rich = BTreeMap::new();
    let mut budget = crate::MAX_TEXT_BYTES;
    decode_cell(
        BRT_FMLA_STRING,
        &p,
        0,
        0,
        0,
        &[],
        &Styles::default(),
        false,
        &mut cells,
        &mut font_sizes,
        &mut rich,
        &mut budget,
        &[],
        &[],
        &[],
        &[],
        &BrtFormulaDefinitions::new(),
    );

    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].text, "cached");
    match &cells[0].value {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "\"x\"");
            assert_eq!(**cached, Cell::Text("cached".to_string()));
        }
        other => panic!("expected a string-result formula cell, got {other:?}"),
    }
}

#[test]
fn xlsb_formula_resolves_3d_sheet_range() {
    let mut p = vec![0u8; 8];
    p.extend_from_slice(&9.0f64.to_le_bytes());
    p.extend_from_slice(&[0, 0]);
    let mut rgce = vec![0x3A, 0, 0]; // PtgRef3d, ixti 0
    rgce.extend_from_slice(&4u32.to_le_bytes());
    rgce.extend_from_slice(&2u16.to_le_bytes());
    p.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
    p.extend_from_slice(&rgce);
    p.extend_from_slice(&0u32.to_le_bytes());

    let sheet_names = vec!["Start".to_string(), "End Sheet".to_string()];
    let extern_sheets = vec![crate::ptg::ExternSheet {
        supbook_index: 0,
        first_sheet: 0,
        last_sheet: 1,
    }];
    let mut cells = Vec::new();
    let mut font_sizes = Vec::new();
    let mut rich = BTreeMap::new();
    let mut budget = crate::MAX_TEXT_BYTES;
    decode_cell(
        BRT_FMLA_NUM,
        &p,
        0,
        0,
        0,
        &[],
        &Styles::default(),
        false,
        &mut cells,
        &mut font_sizes,
        &mut rich,
        &mut budget,
        &sheet_names,
        &extern_sheets,
        &[],
        &[],
        &BrtFormulaDefinitions::new(),
    );

    match &cells[0].value {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "Start:'End Sheet'!$C$5");
            assert_eq!(cached.as_ref(), &Cell::Number(9.0));
        }
        other => panic!("expected 3D formula, got {other:?}"),
    }
}

#[test]
fn xlsb_extern_sheet_record_preserves_first_and_last_sheet() {
    let mut p = 1u32.to_le_bytes().to_vec();
    p.extend_from_slice(&0u32.to_le_bytes()); // supporting link index
    p.extend_from_slice(&2i32.to_le_bytes());
    p.extend_from_slice(&4i32.to_le_bytes());
    assert_eq!(
        parse_brt_extern_sheets(&p),
        vec![crate::ptg::ExternSheet {
            supbook_index: 0,
            first_sheet: 2,
            last_sheet: 4
        }]
    );
}

#[test]
fn xlsb_formula_string_budget_exhaustion_sets_partial_signal() {
    // BrtFmlaString: cell(8) + XLWideString cached value + grbitFlags(2) +
    // CellParsedFormula. If the cached display text cannot fit in the shared
    // workbook text budget, parsing must leave a partial-extraction signal.
    let mut p = vec![0u8; 8]; // col + style
    p.extend_from_slice(&wstr("toolong"));
    p.extend_from_slice(&[0, 0]); // grbitFlags
    let rgce = vec![0x17, 1, 0, b'x']; // PtgStr("x")
    p.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
    p.extend_from_slice(&rgce);
    p.extend_from_slice(&0u32.to_le_bytes());

    let sh = rec(BRT_FMLA_STRING, &p);
    let mut budget = "toolong".len() - 1;
    let (cells, _merges, _hyperlinks, _metadata) = parse_sheet(
        &sh,
        &[],
        &Styles::default(),
        false,
        &[],
        &mut budget,
        &[],
        &[],
        &[],
        &[],
    );

    assert!(cells.is_empty());
    assert_eq!(budget, 0);
}

#[test]
fn bundle_sh_hsstate_visibility() {
    // BrtBundleSh: hsState:u32 (0 visible / 1 hidden / 2 veryHidden), iTabID:u32,
    // strRelID, strName. Build one with hsState=1 and assert it parses as hidden.
    let bundle = |hs_state: u32| {
        let mut p = hs_state.to_le_bytes().to_vec(); // hsState
        p.extend_from_slice(&0u32.to_le_bytes()); // iTabID
        p.extend_from_slice(&wstr("rId1")); // strRelID
        p.extend_from_slice(&wstr("S1")); // strName
        let (names, _, _, _, _, _, _, _, _) = parse_workbook(&rec(BRT_BUNDLE_SH, &p), &[]);
        names
    };
    assert_eq!(bundle(0), vec![("S1".to_string(), "rId1".to_string(), 0)]);
    assert_eq!(bundle(1), vec![("S1".to_string(), "rId1".to_string(), 1)]);
    assert_eq!(bundle(2), vec![("S1".to_string(), "rId1".to_string(), 2)]);
}

#[test]
fn xlsb_hidden_sheet_end_to_end() {
    // workbook.bin: one BrtBundleSh with hsState=1 (hidden) for "Secret".
    let mut wb_bin = 1u32.to_le_bytes().to_vec(); // hsState = 1 (hidden)
    wb_bin.extend_from_slice(&0u32.to_le_bytes()); // iTabID
    wb_bin.extend_from_slice(&wstr("rId1")); // strRelID
    wb_bin.extend_from_slice(&wstr("Secret")); // strName
    let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (name, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", [].as_slice()),
    ] {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(wb.sheets.len(), 1);
    assert_eq!(wb.sheets[0].name, "Secret");
    assert!(wb.sheets[0].is_hidden(), "hsState=1 => hidden");
    assert!(!wb.sheets[0].is_very_hidden());
}

#[test]
fn xlsb_bundle_sheet_preserves_sheet_types_end_to_end() {
    let bundle = |name: &str, rid: Option<&str>, hs_state: u32, tab_id: u32| {
        let mut p = hs_state.to_le_bytes().to_vec();
        p.extend_from_slice(&tab_id.to_le_bytes());
        if let Some(rid) = rid {
            p.extend_from_slice(&wstr(rid));
        } else {
            p.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        p.extend_from_slice(&wstr(name));
        rec(BRT_BUNDLE_SH, &p)
    };

    let mut wb_bin = bundle("Data", Some("rId1"), 0, 1);
    wb_bin.extend_from_slice(&bundle("Chart", Some("rId2"), 0, 2));
    wb_bin.extend_from_slice(&bundle("Macro", Some("rId3"), 1, 3));
    wb_bin.extend_from_slice(&bundle("Dialog", Some("rId4"), 2, 4));
    wb_bin.extend_from_slice(&bundle("Module", None, 2, 5));

    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/>
            <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.bin"/>
            <Relationship Id="rId3" Type="http://schemas.microsoft.com/office/2006/relationships/xlMacrosheet" Target="macrosheets/sheet1.bin"/>
            <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/dialogsheet" Target="dialogsheets/sheet1.bin"/>
        </Relationships>"#;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    for (name, body) in [
        ("xl/workbook.bin", wb_bin.as_slice()),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.bin", [].as_slice()),
    ] {
        zw.start_file(name, opt).unwrap();
        zw.write_all(body).unwrap();
    }
    let bytes = zw.finish().unwrap().into_inner();

    let wb = Workbook::open(&bytes).unwrap();
    assert_eq!(
        wb.sheets_metadata(),
        vec![
            SheetMetadata {
                name: "Data".to_string(),
                typ: SheetType::WorkSheet,
                visible: SheetVisible::Visible,
            },
            SheetMetadata {
                name: "Chart".to_string(),
                typ: SheetType::ChartSheet,
                visible: SheetVisible::Visible,
            },
            SheetMetadata {
                name: "Macro".to_string(),
                typ: SheetType::MacroSheet,
                visible: SheetVisible::Hidden,
            },
            SheetMetadata {
                name: "Dialog".to_string(),
                typ: SheetType::DialogSheet,
                visible: SheetVisible::VeryHidden,
            },
            SheetMetadata {
                name: "Module".to_string(),
                typ: SheetType::Vba,
                visible: SheetVisible::VeryHidden,
            },
        ]
    );
    assert_eq!(
        wb.worksheets()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["Data".to_string()]
    );
}

#[test]
fn parses_date1904_flag() {
    // BrtWbProp with bit 0 set => 1904 date system (matches calamine/MS-XLSB).
    let on = rec(BRT_WB_PROP, &[0x01, 0, 0, 0]);
    assert!(parse_workbook(&on, &[]).1, "bit 0 set => 1904");
    let off = rec(BRT_WB_PROP, &[0x00, 0, 0, 0]);
    assert!(!parse_workbook(&off, &[]).1, "bit 0 clear => 1900");
}
