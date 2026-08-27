use super::style::parse_ods_axis_measure;
use super::{
    jpeg_physical_size_points, parse_settings, read_image_parts_with_limits, MAX_ODS_STYLE_NAME,
    ODS_MIME,
};
use crate::{
    BorderStyle, Cell, Color, DrawingAnchorBehavior, DrawingCrop, DrawingObjectKind,
    ImportedAxisMeasure, PrintLossKind, PrintPageOrder, StyleFidelity, StyleLossKind, Workbook,
};
use std::io::Write;
use zip::write::SimpleFileOptions;

const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0A, 0x49, 0x44, 0x41, 0x54,
    0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn ods_bytes(content: &str) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    zw.start_file("mimetype", opt).unwrap();
    zw.write_all(ODS_MIME.as_bytes()).unwrap();
    zw.start_file("content.xml", opt).unwrap();
    zw.write_all(content.as_bytes()).unwrap();
    zw.finish().unwrap().into_inner()
}

fn ods_bytes_without_mimetype(content: &str) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    zw.start_file("content.xml", opt).unwrap();
    zw.write_all(content.as_bytes()).unwrap();
    zw.finish().unwrap().into_inner()
}

fn ods_bytes_with_styles(content: &str, styles: &str) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    zw.start_file("mimetype", opt).unwrap();
    zw.write_all(ODS_MIME.as_bytes()).unwrap();
    zw.start_file("content.xml", opt).unwrap();
    zw.write_all(content.as_bytes()).unwrap();
    zw.start_file("styles.xml", opt).unwrap();
    zw.write_all(styles.as_bytes()).unwrap();
    zw.finish().unwrap().into_inner()
}

fn ods_bytes_with_meta(content: &str, meta: &str) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    zw.start_file("mimetype", opt).unwrap();
    zw.write_all(ODS_MIME.as_bytes()).unwrap();
    zw.start_file("content.xml", opt).unwrap();
    zw.write_all(content.as_bytes()).unwrap();
    zw.start_file("meta.xml", opt).unwrap();
    zw.write_all(meta.as_bytes()).unwrap();
    zw.finish().unwrap().into_inner()
}

fn ods_bytes_with_part(content: &str, path: &str, data: &[u8]) -> Vec<u8> {
    ods_bytes_with_parts(content, &[(path, data)])
}

fn png_1x1_with_physical_density() -> Vec<u8> {
    // 1,000 px/m horizontally and 2,000 px/m vertically. The resulting
    // intrinsic dimensions are exactly 0.1 cm by 0.05 cm.
    const PHYS_CHUNK: &[u8] = &[
        0x00, 0x00, 0x00, 0x09, b'p', b'H', b'Y', b's', 0x00, 0x00, 0x03, 0xe8, 0x00, 0x00, 0x07,
        0xd0, 0x01, 0xa5, 0xed, 0x46, 0x4c,
    ];
    let mut png = Vec::with_capacity(PNG_1X1.len() + PHYS_CHUNK.len());
    png.extend_from_slice(&PNG_1X1[..33]);
    png.extend_from_slice(PHYS_CHUNK);
    png.extend_from_slice(&PNG_1X1[33..]);
    png
}

fn ods_bytes_with_parts(content: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    zw.start_file("mimetype", opt).unwrap();
    zw.write_all(ODS_MIME.as_bytes()).unwrap();
    zw.start_file("content.xml", opt).unwrap();
    zw.write_all(content.as_bytes()).unwrap();
    for (path, data) in parts {
        zw.start_file(*path, opt).unwrap();
        zw.write_all(data).unwrap();
    }
    zw.finish().unwrap().into_inner()
}

fn encrypted_ods_bytes() -> Vec<u8> {
    let manifest = r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
  <manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet">
    <manifest:encryption-data manifest:checksum-type="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0#sha256-1k" manifest:checksum="abc">
      <manifest:algorithm manifest:algorithm-name="http://www.w3.org/2001/04/xmlenc#aes256-cbc" manifest:initialisation-vector="abc"/>
    </manifest:encryption-data>
  </manifest:file-entry>
  <manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml">
    <manifest:encryption-data manifest:checksum-type="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0#sha256-1k" manifest:checksum="abc"/>
  </manifest:file-entry>
</manifest:manifest>"#;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default();
    zw.start_file("mimetype", opt).unwrap();
    zw.write_all(ODS_MIME.as_bytes()).unwrap();
    zw.start_file("content.xml", opt).unwrap();
    zw.write_all(&[0xff, 0xfe, 0xfd, 0xfc]).unwrap();
    zw.start_file("META-INF/manifest.xml", opt).unwrap();
    zw.write_all(manifest.as_bytes()).unwrap();
    zw.finish().unwrap().into_inner()
}

#[test]
fn reads_a_synthetic_ods() {
    let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="시트"><table:table-row><table:table-cell office:value-type="string"><text:p>품목</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="42"><text:p>42</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="boolean" office:boolean-value="true"><text:p>TRUE</text:p></table:table-cell><table:table-cell office:value-type="date" office:date-value="2024-03-15"><text:p>2024-03-15</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    assert_eq!(wb.sheets.len(), 1);
    let s = &wb.sheets[0];
    assert_eq!(s.name, "시트");
    assert_eq!(s.cell(0, 0), Some(&Cell::Text("품목".to_string())));
    assert_eq!(s.cell(0, 1), Some(&Cell::Number(42.0)));
    assert_eq!(s.cell(1, 0), Some(&Cell::Bool(true)));
    assert_eq!(s.cell(1, 1), Some(&Cell::Date(45366.0))); // 2024-03-15
}

#[test]
fn ods_rich_spans_preserve_text_boundaries_and_unicode() {
    let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="RTL"><table:table-row><table:table-cell office:value-type="string"><text:p>한글 <text:span text:style-name="em">مرحباً 👩‍💻</text:span> e&#x301;</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = &wb.sheets[0];
    let expected = "한글 مرحباً 👩‍💻 e\u{301}";
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text(expected.to_string())));
    let runs = sheet.rich_text_runs(0, 0).expect("ODF span boundaries");
    assert_eq!(
        runs.iter().map(|run| run.text.as_str()).collect::<Vec<_>>(),
        ["한글 ", "مرحباً 👩‍💻", " e\u{301}"]
    );
    assert_eq!(
        runs.iter().map(|run| run.text.as_str()).collect::<String>(),
        expected
    );
}

#[test]
fn ods_rich_span_base_font_uses_whole_cell_style_precedence() {
    let content = r##"
<office:document-content
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <style:style style:name="column_font" style:family="table-cell">
      <style:text-properties fo:font-family="ColumnFont"/>
    </style:style>
    <style:style style:name="fill_only" style:family="table-cell">
      <style:table-cell-properties fo:background-color="#ffeecc"/>
    </style:style>
    <style:style style:name="italic" style:family="text">
      <style:text-properties fo:font-style="italic"/>
    </style:style>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Font precedence">
        <table:table-column table:default-cell-style-name="column_font"
            table:number-columns-repeated="2"/>
        <table:table-row>
          <table:table-cell office:value-type="string">
            <text:p><text:span text:style-name="italic">control</text:span></text:p>
          </table:table-cell>
          <table:table-cell table:style-name="fill_only" office:value-type="string">
            <text:p><text:span text:style-name="italic">explicit</text:span></text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row table:default-cell-style-name="fill_only">
          <table:table-cell office:value-type="string">
            <text:p><text:span text:style-name="italic">row</text:span></text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"##;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    let control = sheet.rich_text_runs(0, 0).expect("column font control");
    let explicit = sheet.rich_text_runs(0, 1).expect("explicit cell style");
    let row = sheet.rich_text_runs(1, 0).expect("row default cell style");

    assert_eq!(control[0].font.name.as_deref(), Some("ColumnFont"));
    assert_eq!(explicit[0].font.name, None);
    assert_eq!(row[0].font.name, None);
    assert!(control[0].font.italic);
    assert!(explicit[0].font.italic);
    assert!(row[0].font.italic);
    assert_eq!(
        sheet.resolved_cell_style(0, 1).and_then(|style| style.font),
        None
    );
    assert_eq!(
        sheet.resolved_cell_style(1, 0).and_then(|style| style.font),
        None
    );
}

#[test]
fn ods_table_protection_surfaces_public_metadata() {
    let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Locked" table:protected="true"></table:table><table:table table:name="LockedEmpty" table:protected="true"/><table:table table:name="Plain" table:protected="false"/></office:spreadsheet></office:body></office:document-content>"#;
    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let locked = wb.sheet_by_name("Locked").unwrap();
    let locked_empty = wb.sheet_by_name("LockedEmpty").unwrap();
    let plain = wb.sheet_by_name("Plain").unwrap();

    assert!(locked.is_protected());
    assert_eq!(locked.protection_options(), None);
    assert!(locked_empty.is_protected());
    assert_eq!(locked_empty.protection_options(), None);
    assert!(!plain.is_protected());

    let metadata = wb.worksheet_metadata("Locked").unwrap();
    assert!(metadata.protected);
    assert_eq!(metadata.protection_options, None);

    let generic_metadata = <Workbook as crate::Reader>::worksheet_metadata(&wb, "Locked").unwrap();
    assert!(generic_metadata.protected);
    assert_eq!(generic_metadata.protection_options, None);
}

#[test]
fn ods_encrypted_package_is_reported_before_missing_workbook() {
    let err = Workbook::open(&encrypted_ods_bytes()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "unsupported encrypted OpenDocument package"
    );
}

#[test]
fn ods_percentage_and_time_fallback_display_text_is_formatted() {
    let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="formats"><table:table-row><table:table-cell office:value-type="percentage" office:value="0.5"/><table:table-cell office:value-type="time" office:time-value="PT12H"/><table:table-cell office:value-type="percentage" office:value="0.25"><text:p>quarter</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = &wb.sheets[0];
    let range = wb.worksheet_range("formats").expect("range");

    assert_eq!(sheet.cell(0, 0), Some(&Cell::Number(0.5)));
    assert_eq!(sheet.formatted(0, 0), Some("50%"));
    assert_eq!(range.formatted_abs(0, 0), Some("50%"));
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Date(0.5)));
    assert_eq!(sheet.formatted(0, 1), Some("12:00:00"));
    assert_eq!(range.formatted_abs(0, 1), Some("12:00:00"));
    assert_eq!(sheet.formatted(0, 2), Some("quarter"));
}

#[test]
fn ods_typed_general_and_explicit_number_styles_drive_display_text() {
    let content = r##"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:number-style style:name="FourDecimals">
      <number:number number:decimal-places="4" number:min-decimal-places="4"/>
    </number:number-style>
    <number:percentage-style style:name="OneDecimalPercent">
      <number:number number:decimal-places="1" number:min-decimal-places="1"/>
      <number:text>%</number:text>
    </number:percentage-style>
    <number:currency-style style:name="TwoDecimalCurrency">
      <number:currency-symbol>$</number:currency-symbol>
      <number:number number:decimal-places="2" number:min-decimal-places="2"/>
    </number:currency-style>
    <number:date-style style:name="IsoDate">
      <number:year number:style="long"/>
      <number:text>-</number:text>
      <number:month number:style="long"/>
      <number:text>-</number:text>
      <number:day number:style="long"/>
    </number:date-style>
    <style:style style:name="ce_four" style:family="table-cell"
        style:data-style-name="FourDecimals"/>
    <style:style style:name="ce_percent" style:family="table-cell"
        style:data-style-name="OneDecimalPercent"/>
    <style:style style:name="ce_currency" style:family="table-cell"
        style:data-style-name="TwoDecimalCurrency"/>
    <style:style style:name="ce_date" style:family="table-cell"
        style:data-style-name="IsoDate"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Typed display">
        <table:table-row>
          <table:table-cell office:value-type="float" office:value="0.2900">
            <text:p>0.2900</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_four"
              office:value-type="float" office:value="0.2900">
            <text:p>0.29</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_percent"
              office:value-type="percentage" office:value="0.29">
            <text:p>29%</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_currency"
              office:value-type="currency" office:value="1.25"
              office:currency="USD">
            <text:p>USD 1.25</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_date"
              office:value-type="date" office:date-value="2024-03-15">
            <text:p>15/03/2024</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_four"
              table:formula="of:=SUM([.A1])"
              office:value-type="float" office:value="0.2900">
            <text:p>0.29</text:p>
          </table:table-cell>
          <table:table-cell table:formula="of:=SUM([.A1])"
              office:value-type="float" office:value="0.2900">
            <text:p>0.2900</text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row table:default-cell-style-name="ce_four">
          <table:table-cell office:value-type="float" office:value="0.2900">
            <text:p>0.29</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"##;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];

    assert_eq!(sheet.formatted(0, 0), Some("0.29"));
    assert_eq!(sheet.formatted(0, 1), Some("0.2900"));
    assert_eq!(sheet.formatted(0, 2), Some("29.0%"));
    assert_eq!(sheet.formatted(0, 3), Some("$1.25"));
    assert_eq!(sheet.formatted(0, 4), Some("2024-03-15"));
    assert_eq!(sheet.formatted(0, 5), Some("0.2900"));
    assert_eq!(sheet.formatted(0, 6), Some("0.29"));
    assert_eq!(sheet.formatted(1, 0), Some("0.2900"));

    for col in [5, 6] {
        match sheet.cell(0, col).expect("formula cell") {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM([.A1])");
                assert_eq!(cached.as_ref(), &Cell::Number(0.29));
            }
            other => panic!("expected formula cell, got {other:?}"),
        }
    }
}

#[test]
fn ods_unresolved_decimal_precision_preserves_bounded_producer_display() {
    let content = r##"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:number-style style:name="InheritedDecimals">
      <number:number number:min-integer-digits="1" number:grouping="true"/>
      <number:text> kg</number:text>
    </number:number-style>
    <number:number-style style:name="ExplicitInteger">
      <number:number number:decimal-places="0" number:min-integer-digits="1"/>
    </number:number-style>
    <number:number-style style:name="MalformedDecimals">
      <number:number number:decimal-places="many" number:min-integer-digits="1"/>
    </number:number-style>
    <number:number-style style:name="InheritedScientific">
      <number:scientific-number number:min-integer-digits="1"
          number:min-exponent-digits="2"/>
    </number:number-style>
    <number:currency-style style:name="InheritedCurrency">
      <number:currency-symbol>$</number:currency-symbol>
      <number:number number:min-integer-digits="1" number:grouping="true"/>
    </number:currency-style>
    <number:percentage-style style:name="InheritedPercentage">
      <number:number number:min-integer-digits="1"/>
      <number:text>%</number:text>
    </number:percentage-style>
    <style:style style:name="ce_inherited" style:family="table-cell"
        style:data-style-name="InheritedDecimals"/>
    <style:style style:name="ce_integer" style:family="table-cell"
        style:data-style-name="ExplicitInteger"/>
    <style:style style:name="ce_malformed" style:family="table-cell"
        style:data-style-name="MalformedDecimals"/>
    <style:style style:name="ce_scientific" style:family="table-cell"
        style:data-style-name="InheritedScientific"/>
    <style:style style:name="ce_currency" style:family="table-cell"
        style:data-style-name="InheritedCurrency"/>
    <style:style style:name="ce_percentage" style:family="table-cell"
        style:data-style-name="InheritedPercentage"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Decimal inheritance">
        <table:table-row>
          <table:table-cell table:style-name="ce_inherited"
              office:value-type="float" office:value="1234.5">
            <text:p>1,234.50 kg</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_integer"
              office:value-type="float" office:value="1.5">
            <text:p>1.5</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_malformed"
              office:value-type="float" office:value="1.5">
            <text:p>malformed 1.50</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_inherited"
              office:value-type="float" office:value="1234.5"/>
          <table:table-cell table:style-name="ce_scientific"
              office:value-type="float" office:value="12345">
            <text:p>1.23E+04</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_currency"
              office:value-type="currency" office:value="1.5" office:currency="USD">
            <text:p>$1.50 cached</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_currency"
              office:value-type="currency" office:value="1.5" office:currency="USD"/>
          <table:table-cell table:style-name="ce_percentage"
              office:value-type="percentage" office:value="0.125">
            <text:p>12.50% cached</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_percentage"
              office:value-type="percentage" office:value="0.125"/>
        </table:table-row>
        <table:table-row table:default-cell-style-name="ce_inherited">
          <table:table-cell office:value-type="float" office:value="9876.5">
            <text:p>9,876.50 kg</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"##;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];

    assert_eq!(sheet.cell(0, 0), Some(&Cell::Number(1234.5)));
    assert_eq!(sheet.formatted(0, 0), Some("1,234.50 kg"));
    assert_eq!(
        sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.num_fmt),
        None
    );
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Number(1.5)));
    assert_eq!(sheet.formatted(0, 1), Some("2"));
    assert_eq!(
        sheet
            .resolved_cell_style(0, 1)
            .and_then(|style| style.num_fmt)
            .as_deref(),
        Some("0")
    );
    assert_eq!(sheet.formatted(0, 2), Some("malformed 1.50"));
    assert_eq!(sheet.formatted(0, 3), Some("1234.5"));
    assert_eq!(sheet.formatted(0, 4), Some("1.23E+04"));
    assert_eq!(sheet.formatted(0, 5), Some("$1.50 cached"));
    assert_eq!(sheet.formatted(0, 6), Some("1.5"));
    assert_eq!(sheet.formatted(0, 7), Some("12.50% cached"));
    assert_eq!(sheet.formatted(0, 8), Some("12.5%"));
    assert_eq!(sheet.formatted(1, 0), Some("9,876.50 kg"));
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
    assert_eq!(
        sheet
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::UnsupportedProperty)
            .map(|loss| loss.occurrences),
        Some(5)
    );
    assert!(!sheet
        .style_losses()
        .iter()
        .any(|loss| loss.kind == StyleLossKind::MissingReference));
}

#[test]
fn ods_omitted_decimal_places_inherit_default_cell_precision() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Inherited precision">
        <table:table-row>
          <table:table-cell table:style-name="ce_inherited"
              office:value-type="float" office:value="1234.5">
            <text:p>stale producer cache</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="ce_inherited"
              office:value-type="float" office:value="1234.567">
            <text:p>stale producer cache</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;
    let styles = r#"
<office:document-styles
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0">
  <office:styles>
    <style:default-style style:family="table-cell">
      <style:table-cell-properties style:decimal-places=" 2 "/>
    </style:default-style>
    <number:number-style style:name="InheritedDecimals">
      <number:number number:min-integer-digits="1" number:grouping=" true "/>
      <number:text> kg</number:text>
    </number:number-style>
    <style:style style:name="ce_inherited" style:family="table-cell"
        style:data-style-name="InheritedDecimals"/>
  </office:styles>
</office:document-styles>
"#;

    let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.formatted(0, 0), Some("1,234.5 kg"));
    assert_eq!(sheet.formatted(0, 1), Some("1,234.57 kg"));
    assert_eq!(
        sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.num_fmt)
            .as_deref(),
        Some("#,##0.##\\ \\k\\g")
    );
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
    assert!(sheet.style_losses().is_empty());
}

#[test]
fn ods_zero_minimum_integer_digits_remains_optional() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0">
  <office:automatic-styles>
    <number:number-style style:name="OptionalInteger">
      <number:number number:decimal-places="2" number:min-decimal-places="2"
          number:min-integer-digits="0"/>
    </number:number-style>
    <number:number-style style:name="GroupedOptionalInteger">
      <number:number number:decimal-places="2" number:min-decimal-places="2"
          number:min-integer-digits="0" number:grouping="true"/>
    </number:number-style>
    <style:style style:name="c0" style:family="table-cell"
        style:data-style-name="OptionalInteger"/>
    <style:style style:name="c1" style:family="table-cell"
        style:data-style-name="GroupedOptionalInteger"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Optional integer">
        <table:table-row>
          <table:table-cell table:style-name="c0"
              office:value-type="float" office:value="0.5"/>
          <table:table-cell table:style-name="c1"
              office:value-type="float" office:value="1234.5"/>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.formatted(0, 0), Some(".50"));
    assert_eq!(sheet.formatted(0, 1), Some("1,234.50"));
    assert_eq!(
        sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.num_fmt)
            .as_deref(),
        Some("#.00")
    );
    assert_eq!(
        sheet
            .resolved_cell_style(0, 1)
            .and_then(|style| style.num_fmt)
            .as_deref(),
        Some("#,###.00")
    );
}

#[test]
fn ods_seconds_omission_keeps_its_explicit_zero_default() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0">
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Seconds">
        <table:table-row>
          <table:table-cell table:style-name="ce_time"
              office:value-type="time" office:time-value="PT12H34M56S"/>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;
    let styles = r#"
<office:document-styles
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0">
  <office:styles>
    <style:default-style style:family="table-cell">
      <style:table-cell-properties style:decimal-places="2"/>
    </style:default-style>
    <number:time-style style:name="WholeSeconds">
      <number:hours number:style="long"/>
      <number:text>:</number:text>
      <number:minutes number:style="long"/>
      <number:text>:</number:text>
      <number:seconds number:style="long"/>
    </number:time-style>
    <style:style style:name="ce_time" style:family="table-cell"
        style:data-style-name="WholeSeconds"/>
  </office:styles>
</office:document-styles>
"#;

    let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.formatted(0, 0), Some("12:34:56"));
    let format = sheet
        .resolved_cell_style(0, 0)
        .and_then(|style| style.num_fmt)
        .expect("resolved time format");
    assert!(!format.contains('.'));
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
}

#[test]
fn ods_repeated_cells_resolve_each_output_columns_default_number_style() {
    let content = r##"
<office:document-content
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:number-style style:name="TwoDecimals">
      <number:number number:decimal-places="2" number:min-decimal-places="2"/>
    </number:number-style>
    <number:percentage-style style:name="OneDecimalPercent">
      <number:number number:decimal-places="1" number:min-decimal-places="1"/>
    </number:percentage-style>
    <number:number-style style:name="Integer">
      <number:number number:decimal-places="0"/>
    </number:number-style>
    <number:number-style style:name="FourDecimals">
      <number:number number:decimal-places="4" number:min-decimal-places="4"/>
    </number:number-style>
    <style:style style:name="ce_two" style:family="table-cell"
        style:data-style-name="TwoDecimals"/>
    <style:style style:name="ce_percent" style:family="table-cell"
        style:data-style-name="OneDecimalPercent"/>
    <style:style style:name="ce_integer" style:family="table-cell"
        style:data-style-name="Integer"/>
    <style:style style:name="ce_four" style:family="table-cell"
        style:data-style-name="FourDecimals"/>
    <style:style style:name="ce_fill" style:family="table-cell">
      <style:table-cell-properties fo:background-color="#ffeecc"/>
    </style:style>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Repeated formats">
        <table:table-column table:default-cell-style-name="ce_two"/>
        <table:table-column table:default-cell-style-name="ce_percent"/>
        <table:table-column table:default-cell-style-name="ce_integer"/>
        <table:table-row>
          <table:table-cell table:number-columns-repeated="3"
              office:value-type="float" office:value="0.125"/>
        </table:table-row>
        <table:table-row>
          <table:table-cell table:number-columns-repeated="3"
              office:value-type="float" office:value="0.125">
            <text:p>stale serialized display</text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell table:style-name="ce_four"
              table:number-columns-repeated="3"
              office:value-type="float" office:value="0.125"/>
        </table:table-row>
        <table:table-row>
          <table:table-cell table:style-name="ce_fill"
              table:number-columns-repeated="3"
              office:value-type="float" office:value="0.125"/>
        </table:table-row>
        <table:table-row table:default-cell-style-name="ce_fill">
          <table:table-cell table:number-columns-repeated="3"
              office:value-type="float" office:value="0.125"/>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"##;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];

    for row in [0, 1] {
        assert_eq!(sheet.formatted(row, 0), Some("0.13"));
        assert_eq!(sheet.formatted(row, 1), Some("12.5%"));
        assert_eq!(sheet.formatted(row, 2), Some("0"));
        for col in 0..3 {
            assert_eq!(sheet.cell(row, col), Some(&Cell::Number(0.125)));
        }
    }
    for col in 0..3 {
        assert_eq!(sheet.formatted(2, col), Some("0.1250"));
        assert_eq!(sheet.formatted(3, col), Some("0.125"));
        assert_eq!(sheet.formatted(4, col), Some("0.125"));
        for row in [3, 4] {
            assert_eq!(
                sheet
                    .resolved_cell_style(row, col)
                    .and_then(|style| style.num_fmt),
                None
            );
        }
        assert_eq!(
            sheet
                .resolved_cell_style(0, col)
                .and_then(|style| style.num_fmt),
            Some(["0.00", "0.0%", "0"][usize::from(col)].to_string())
        );
    }
}

#[test]
fn ods_number_format_precedence_retains_general_and_unresolved_markers() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:number-style style:name="TwoDecimals">
      <number:number number:decimal-places="2" number:min-decimal-places="2"/>
    </number:number-style>
    <number:number-style style:name="UnresolvedDecimals">
      <number:number number:min-integer-digits="1"/>
    </number:number-style>
    <style:style style:name="ce_two" style:family="table-cell"
        style:data-style-name="TwoDecimals"/>
    <style:style style:name="ce_unresolved" style:family="table-cell"
        style:data-style-name="UnresolvedDecimals"/>
    <style:style style:name="ce_general" style:family="table-cell"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Precedence">
        <table:table-column table:default-cell-style-name="ce_two"/>
        <table:table-row table:default-cell-style-name="ce_general">
          <table:table-cell office:value-type="float" office:value="1.5">
            <text:p>stale general cache</text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row table:default-cell-style-name="ce_unresolved">
          <table:table-cell office:value-type="float" office:value="1.5">
            <text:p>row cached</text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row table:default-cell-style-name="ce_two">
          <table:table-cell table:style-name="ce_unresolved"
              office:value-type="float" office:value="1.5">
            <text:p>explicit cached</text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell office:value-type="float" office:value="1.5"/>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.formatted(0, 0), Some("1.5"));
    assert_eq!(sheet.formatted(1, 0), Some("row cached"));
    assert_eq!(sheet.formatted(2, 0), Some("explicit cached"));
    assert_eq!(sheet.formatted(3, 0), Some("1.50"));
    for row in 0..3 {
        assert_eq!(
            sheet
                .resolved_cell_style(row, 0)
                .and_then(|style| style.num_fmt),
            None
        );
    }
    assert_eq!(
        sheet
            .resolved_cell_style(3, 0)
            .and_then(|style| style.num_fmt)
            .as_deref(),
        Some("0.00")
    );
}

#[test]
fn ods_missing_data_style_preserves_cache_without_parent_format_leakage() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:number-style style:name="TwoDecimals">
      <number:number number:decimal-places="2" number:min-decimal-places="2"/>
    </number:number-style>
    <style:style style:name="Base" style:family="table-cell"
        style:data-style-name="TwoDecimals"/>
    <style:style style:name="Child" style:family="table-cell"
        style:parent-style-name="Base" style:data-style-name="MissingFormat"/>
    <style:style style:name="Grandchild" style:family="table-cell"
        style:parent-style-name="Child"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Missing format">
        <table:table-row>
          <table:table-cell table:style-name="Grandchild"
              office:value-type="float" office:value="1.5">
            <text:p>missing-format cache</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.formatted(0, 0), Some("missing-format cache"));
    assert_eq!(
        sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.num_fmt),
        None
    );
    assert_eq!(
        sheet
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::MissingReference)
            .map(|loss| loss.occurrences),
        Some(1)
    );
}

#[test]
fn ods_missing_parent_style_preserves_cache_without_lower_format_leakage() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:number-style style:name="TwoDecimals">
      <number:number number:decimal-places="2" number:min-decimal-places="2"/>
    </number:number-style>
    <style:style style:name="ce_two" style:family="table-cell"
        style:data-style-name="TwoDecimals"/>
    <style:style style:name="Child" style:family="table-cell"
        style:parent-style-name="MissingParent"/>
    <style:style style:name="BrokenRow" style:family="table-row"
        style:parent-style-name="MissingRowParent"/>
    <style:style style:name="WrongFamilyData" style:family="table-row"
        style:data-style-name="TwoDecimals"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Missing parent">
        <table:table-column table:default-cell-style-name="ce_two"/>
        <table:table-row>
          <table:table-cell table:style-name="Child"
              office:value-type="float" office:value="1.5">
            <text:p>missing-parent cache</text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row table:style-name="BrokenRow">
          <table:table-cell office:value-type="float" office:value="1.5"/>
        </table:table-row>
        <table:table-row table:style-name="WrongFamilyData">
          <table:table-cell office:value-type="float" office:value="1.5"/>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.formatted(0, 0), Some("missing-parent cache"));
    assert_eq!(sheet.formatted(1, 0), Some("1.50"));
    assert_eq!(sheet.formatted(2, 0), Some("1.50"));
    for row in [1, 2] {
        assert_eq!(
            sheet
                .resolved_cell_style(row, 0)
                .and_then(|style| style.num_fmt)
                .as_deref(),
            Some("0.00")
        );
    }
    assert_eq!(
        sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.num_fmt),
        None
    );
    assert_eq!(
        sheet
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::MissingReference)
            .map(|loss| loss.occurrences),
        Some(2)
    );
    assert_eq!(
        sheet
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::UnsupportedProperty)
            .map(|loss| loss.occurrences),
        Some(1)
    );
}

#[test]
fn ods_missing_cell_style_references_block_lower_format_precedence() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:number-style style:name="TwoDecimals">
      <number:number number:decimal-places="2" number:min-decimal-places="2"/>
    </number:number-style>
    <style:style style:name="ce_two" style:family="table-cell"
        style:data-style-name="TwoDecimals"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Axis missing">
        <table:table-column table:default-cell-style-name="ce_two"/>
        <table:table-row table:default-cell-style-name="MissingRowDefault">
          <table:table-cell office:value-type="float" office:value="1.5">
            <text:p>missing-row cache</text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row table:default-cell-style-name="ce_two">
          <table:table-cell table:style-name="MissingExplicit"
              office:value-type="float" office:value="1.5">
            <text:p>missing-explicit cache</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
      <table:table table:name="Table missing"
          table:default-cell-style-name="MissingTableDefault">
        <table:table-row>
          <table:table-cell office:value-type="float" office:value="1.5">
            <text:p>missing-table cache</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let axis = &workbook.sheets[0];
    assert_eq!(axis.formatted(0, 0), Some("missing-row cache"));
    assert_eq!(axis.formatted(1, 0), Some("missing-explicit cache"));
    for row in 0..2 {
        assert_eq!(
            axis.resolved_cell_style(row, 0)
                .and_then(|style| style.num_fmt),
            None
        );
    }
    assert_eq!(
        axis.style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::MissingReference)
            .map(|loss| loss.occurrences),
        Some(2)
    );

    let table = &workbook.sheets[1];
    assert_eq!(table.formatted(0, 0), Some("missing-table cache"));
    assert_eq!(
        table
            .resolved_cell_style(0, 0)
            .and_then(|style| style.num_fmt),
        None
    );
    assert_eq!(
        table
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::MissingReference)
            .map(|loss| loss.occurrences),
        Some(1)
    );
}

#[test]
fn ods_over_limit_data_styles_preserve_cache_and_clear_stale_formats() {
    let long_name = "N".repeat(MAX_ODS_STYLE_NAME + 1);
    let long_literal = "x".repeat(2_050);
    let content = format!(
        r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:number-style style:name="TwoDecimals">
      <number:number number:decimal-places="2" number:min-decimal-places="2"/>
    </number:number-style>
    <number:number-style style:name="Duplicate">
      <number:number number:decimal-places="2"/>
    </number:number-style>
    <number:number-style style:name="Duplicate">
      <number:number number:decimal-places="2" number:grouping="sometimes"/>
      <number:text>{long_literal}</number:text>
    </number:number-style>
    <style:style style:name="LongReference" style:family="table-cell"
        style:data-style-name="{long_name}"/>
    <style:style style:name="TwoDecimalCell" style:family="table-cell"
        style:data-style-name="TwoDecimals"/>
    <style:style style:name="LongCode" style:family="table-cell"
        style:data-style-name="Duplicate"/>
    <style:style style:name="LongParent" style:family="table-cell"
        style:parent-style-name="{long_name}"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Limits">
        <table:table-column table:default-cell-style-name="TwoDecimalCell"
            table:number-columns-repeated="4"/>
        <table:table-row>
          <table:table-cell table:style-name="LongReference"
              office:value-type="float" office:value="1.5">
            <text:p>long-reference cache</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="LongCode"
              office:value-type="float" office:value="1.5">
            <text:p>long-code cache</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="LongParent"
              office:value-type="float" office:value="1.5">
            <text:p>long-parent cache</text:p>
          </table:table-cell>
          <table:table-cell table:style-name="{long_name}"
              office:value-type="float" office:value="1.5">
            <text:p>long-cell-reference cache</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#
    );

    let workbook = Workbook::open(&ods_bytes(&content)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.formatted(0, 0), Some("long-reference cache"));
    assert_eq!(sheet.formatted(0, 1), Some("long-code cache"));
    assert_eq!(sheet.formatted(0, 2), Some("long-parent cache"));
    assert_eq!(sheet.formatted(0, 3), Some("long-cell-reference cache"));
    assert_eq!(
        sheet
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::LimitExceeded)
            .map(|loss| loss.occurrences),
        Some(4)
    );
    assert!(!sheet
        .style_losses()
        .iter()
        .any(|loss| loss.kind == StyleLossKind::MissingReference));
    assert!(!sheet
        .style_losses()
        .iter()
        .any(|loss| loss.kind == StyleLossKind::UnsupportedProperty));
}

#[test]
fn ods_malformed_number_components_do_not_synthesize_formats() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:number-style style:name="BadMinDecimals">
      <number:number number:decimal-places="2" number:min-decimal-places="many"/>
    </number:number-style>
    <number:number-style style:name="BadMinInteger">
      <number:number number:decimal-places="2" number:min-integer-digits="-1"/>
    </number:number-style>
    <number:number-style style:name="BadGrouping">
      <number:number number:decimal-places="2" number:grouping="sometimes"/>
    </number:number-style>
    <number:number-style style:name="BadGrouping">
      <number:number number:decimal-places="2" number:grouping="still-not-a-boolean"/>
    </number:number-style>
    <number:number-style style:name="BadDecimalOrder">
      <number:number number:decimal-places="2" number:min-decimal-places="3"/>
    </number:number-style>
    <number:number-style style:name="HugeDecimals">
      <number:number number:decimal-places="999999999999999999999999999999999999"/>
    </number:number-style>
    <number:number-style style:name="HugeExponent">
      <number:scientific-number number:decimal-places="2"
          number:min-exponent-digits="31"/>
    </number:number-style>
    <style:style style:name="c0" style:family="table-cell"
        style:data-style-name="BadMinDecimals"/>
    <style:style style:name="c1" style:family="table-cell"
        style:data-style-name="BadMinInteger"/>
    <style:style style:name="c2" style:family="table-cell"
        style:data-style-name="BadGrouping"/>
    <style:style style:name="c3" style:family="table-cell"
        style:data-style-name="BadDecimalOrder"/>
    <style:style style:name="c4" style:family="table-cell"
        style:data-style-name="HugeDecimals"/>
    <style:style style:name="c5" style:family="table-cell"
        style:data-style-name="HugeExponent"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Malformed formats">
        <table:table-row>
          <table:table-cell table:style-name="c0" office:value-type="float"
              office:value="1.5"><text:p>cache 0</text:p></table:table-cell>
          <table:table-cell table:style-name="c1" office:value-type="float"
              office:value="1.5"><text:p>cache 1</text:p></table:table-cell>
          <table:table-cell table:style-name="c2" office:value-type="float"
              office:value="1.5"><text:p>cache 2</text:p></table:table-cell>
          <table:table-cell table:style-name="c3" office:value-type="float"
              office:value="1.5"><text:p>cache 3</text:p></table:table-cell>
          <table:table-cell table:style-name="c4" office:value-type="float"
              office:value="1.5"><text:p>cache 4</text:p></table:table-cell>
          <table:table-cell table:style-name="c5" office:value-type="float"
              office:value="1.5"><text:p>cache 5</text:p></table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    for col in 0..6 {
        assert_eq!(
            sheet.formatted(0, col),
            Some(format!("cache {col}").as_str())
        );
    }
    assert_eq!(
        sheet
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::UnsupportedProperty)
            .map(|loss| loss.occurrences),
        Some(5)
    );
    assert_eq!(
        sheet
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::LimitExceeded)
            .map(|loss| loss.occurrences),
        Some(2)
    );
}

#[test]
fn ods_display_changing_unsupported_formats_preserve_producer_cache() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:number-style style:name="DisplayFactor">
      <number:number number:decimal-places="2" number:display-factor="1000"/>
    </number:number-style>
    <number:number-style style:name="DecimalReplacement">
      <number:number number:decimal-places="2" number:decimal-replacement="-"/>
    </number:number-style>
    <number:number-style style:name="ExponentInterval">
      <number:scientific-number number:decimal-places="2"
          number:exponent-interval="3"/>
    </number:number-style>
    <number:number-style style:name="Mapped">
      <number:number number:decimal-places="2"/>
      <style:map style:condition="value()&gt;0" style:apply-style-name="Other"/>
    </number:number-style>
    <number:time-style style:name="MalformedSeconds">
      <number:seconds number:decimal-places="many"/>
    </number:time-style>
    <number:time-style style:name="HugeSeconds">
      <number:seconds number:decimal-places="10"/>
    </number:time-style>
    <style:style style:name="c0" style:family="table-cell"
        style:data-style-name="DisplayFactor"/>
    <style:style style:name="c1" style:family="table-cell"
        style:data-style-name="DecimalReplacement"/>
    <style:style style:name="c2" style:family="table-cell"
        style:data-style-name="ExponentInterval"/>
    <style:style style:name="c3" style:family="table-cell"
        style:data-style-name="Mapped"/>
    <style:style style:name="c4" style:family="table-cell"
        style:data-style-name="MalformedSeconds"/>
    <style:style style:name="c5" style:family="table-cell"
        style:data-style-name="HugeSeconds"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Unsupported formats">
        <table:table-row>
          <table:table-cell table:style-name="c0" office:value-type="float"
              office:value="1.5"><text:p>cache 0</text:p></table:table-cell>
          <table:table-cell table:style-name="c1" office:value-type="float"
              office:value="1.5"><text:p>cache 1</text:p></table:table-cell>
          <table:table-cell table:style-name="c2" office:value-type="float"
              office:value="1.5"><text:p>cache 2</text:p></table:table-cell>
          <table:table-cell table:style-name="c3" office:value-type="float"
              office:value="1.5"><text:p>cache 3</text:p></table:table-cell>
          <table:table-cell table:style-name="c4" office:value-type="time"
              office:time-value="PT1.5S"><text:p>cache 4</text:p></table:table-cell>
          <table:table-cell table:style-name="c5" office:value-type="time"
              office:time-value="PT1.5S"><text:p>cache 5</text:p></table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    for col in 0..6 {
        let expected = format!("cache {col}");
        assert_eq!(sheet.formatted(0, col), Some(expected.as_str()));
    }
    assert_eq!(
        sheet
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::UnsupportedProperty)
            .map(|loss| loss.occurrences),
        Some(5)
    );
    assert_eq!(
        sheet
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::LimitExceeded)
            .map(|loss| loss.occurrences),
        Some(1)
    );
}

#[test]
fn ods_invalid_default_decimal_precision_is_reported_once() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Invalid default">
        <table:table-row>
          <table:table-cell table:style-name="c0" office:value-type="float"
              office:value="1.5"><text:p>cache 0</text:p></table:table-cell>
          <table:table-cell table:style-name="c1" office:value-type="float"
              office:value="2.5"><text:p>cache 1</text:p></table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;
    let styles_template = r#"
<office:document-styles
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0">
  <office:styles>
    <style:default-style style:family="table-cell">
      <style:table-cell-properties style:decimal-places="DEFAULT_PRECISION"/>
    </style:default-style>
    <number:number-style style:name="N0"><number:number/></number:number-style>
    <number:number-style style:name="N1"><number:number/></number:number-style>
    <style:style style:name="c0" style:family="table-cell" style:data-style-name="N0"/>
    <style:style style:name="c1" style:family="table-cell" style:data-style-name="N1"/>
  </office:styles>
</office:document-styles>
"#;

    for (precision, expected_kind) in [
        ("many", StyleLossKind::UnsupportedProperty),
        ("31", StyleLossKind::LimitExceeded),
    ] {
        let styles = styles_template.replace("DEFAULT_PRECISION", precision);
        let workbook = Workbook::open(&ods_bytes_with_styles(content, &styles)).expect("ods");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.formatted(0, 0), Some("cache 0"));
        assert_eq!(sheet.formatted(0, 1), Some("cache 1"));
        assert_eq!(sheet.style_losses().len(), 1);
        assert_eq!(sheet.style_losses()[0].kind, expected_kind);
        assert_eq!(sheet.style_losses()[0].occurrences, 1);
    }
}

#[test]
fn ods_number_style_text_and_currency_symbols_remain_literals() {
    let content = r#"
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:automatic-styles>
    <number:currency-style style:name="Usd">
      <number:currency-symbol>USD </number:currency-symbol>
      <number:number number:decimal-places="2" number:min-decimal-places="2"/>
    </number:currency-style>
    <number:number-style style:name="Mass">
      <number:number number:decimal-places="2" number:min-decimal-places="2"/>
      <number:text> m/d;s\y"h</number:text>
    </number:number-style>
    <style:style style:name="usd" style:family="table-cell" style:data-style-name="Usd"/>
    <style:style style:name="mass" style:family="table-cell" style:data-style-name="Mass"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Literals">
        <table:table-row>
          <table:table-cell table:style-name="usd"
              office:value-type="currency" office:value="1.25" office:currency="USD"/>
          <table:table-cell table:style-name="mass"
              office:value-type="float" office:value="1.25"/>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.formatted(0, 0), Some("USD 1.25"));
    assert_eq!(sheet.formatted(0, 1), Some(r#"1.25 m/d;s\y"h"#));
}

#[test]
fn ods_sheet_visibility_follows_table_style_display() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:automatic-styles><style:style style:name="ta_hidden" style:family="table"><style:table-properties table:display="false"/></style:style><style:style style:name="ta_visible" style:family="table"><style:table-properties table:display="true"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Visible" table:style-name="ta_visible"></table:table><table:table table:name="Hidden" table:style-name="ta_hidden"></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();

    assert_eq!(wb.sheets.len(), 2);
    assert!(!wb.sheets[0].is_hidden(), "table:display=true is visible");
    assert!(wb.sheets[1].is_hidden(), "table:display=false is hidden");
}

#[test]
fn ods_table_writing_mode_surfaces_inherited_right_to_left_view() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:automatic-styles><style:style style:name="rtl-parent" style:family="table"><style:table-properties style:writing-mode="rl-tb"/></style:style><style:style style:name="rtl-child" style:family="table" style:parent-style-name="rtl-parent"/><style:style style:name="ltr-child" style:family="table" style:parent-style-name="rtl-parent"><style:table-properties style:writing-mode="lr-tb"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="RTL" table:style-name="rtl-child"><table:table-row/></table:table><table:table table:name="LTR" table:style-name="ltr-child"/></office:spreadsheet></office:body></office:document-content>"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");

    assert!(
        workbook
            .sheet_by_name("RTL")
            .expect("RTL sheet")
            .sheet_view()
            .right_to_left
    );
    assert!(
        !workbook
            .sheet_by_name("LTR")
            .expect("LTR sheet")
            .sheet_view()
            .right_to_left
    );
}

#[test]
fn ods_direct_row_and_column_visibility_is_retained() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Hidden axes"><table:table-column/><table:table-column table:visibility="collapse" table:number-columns-repeated="2"/><table:table-column table:visibility="filter"/><table:table-row><table:table-cell office:value-type="string"><text:p>visible</text:p></table:table-cell></table:table-row><table:table-row table:visibility="collapse"><table:table-cell office:value-type="string"><text:p>collapsed</text:p></table:table-cell></table:table-row><table:table-row table:visibility="filter"><table:table-cell office:value-type="string"><text:p>filtered</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = &wb.sheets[0];

    assert_eq!(
        sheet.hidden_columns().iter().copied().collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        sheet.hidden_rows().iter().copied().collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn ods_without_mimetype_dispatches_from_content_xml() {
    let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Fallback"><table:table-row><table:table-cell office:value-type="string"><text:p>ok</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes_without_mimetype(content)).unwrap();

    assert_eq!(wb.sheets[0].name, "Fallback");
    assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Text("ok".into())));
}

#[test]
fn ods_sheet_visibility_follows_styles_xml_table_style() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="FromStyles" table:style-name="ta_hidden"></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:styles><style:style style:name="ta_hidden" style:family="table"><style:table-properties table:display="false"/></style:style></office:styles></office:document-styles>"#;

    let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();

    assert_eq!(wb.sheets.len(), 1);
    assert!(wb.sheets[0].is_hidden());
}

#[test]
fn ods_page_layout_print_options_surface_public_metadata() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Printable" table:style-name="ta_print"></table:table><table:table table:name="Plain" table:style-name="ta_plain"/></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:styles><style:style style:name="ta_print" style:family="table" style:master-page-name="mp_print"><style:table-properties table:display="true"/></style:style><style:style style:name="ta_plain" style:family="table"><style:table-properties table:display="true"/></style:style></office:styles><office:automatic-styles><style:page-layout style:name="pm_print"><style:page-layout-properties style:print="headers grid"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="mp_print" style:page-layout-name="pm_print"/></office:master-styles></office:document-styles>"#;

    let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();
    let printable = wb.sheet_by_name("Printable").unwrap();
    let plain = wb.sheet_by_name("Plain").unwrap();

    assert!(printable.print_gridlines());
    assert!(printable.print_headings());
    assert!(!plain.print_gridlines());
    assert!(!plain.print_headings());

    let metadata = wb.worksheet_metadata("Printable").unwrap();
    assert!(metadata.print_gridlines);
    assert!(metadata.print_headings);

    let generic_metadata =
        <Workbook as crate::Reader>::worksheet_metadata(&wb, "Printable").unwrap();
    assert!(generic_metadata.print_gridlines);
    assert!(generic_metadata.print_headings);
}

#[test]
fn ods_print_sidecar_retains_ranges_breaks_order_and_header_variants() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Print" table:style-name="ta" table:print-ranges="$Print.$A$1:$Print.$B$2 $Print.$D$4:$Print.$F$8"><table:table-column table:style-name="cb"/><table:table-row/><table:table-row table:style-name="rb"/></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:style style:name="ta" style:family="table" style:master-page-name="mp"/><style:style style:name="rb" style:family="table-row"><style:table-row-properties fo:break-before="page"/></style:style><style:style style:name="cb" style:family="table-column"><style:table-column-properties fo:break-after="page"/></style:style></office:styles><office:automatic-styles><style:page-layout style:name="pm"><style:page-layout-properties style:print="headers" style:table-centering="horizontal" style:print-page-order="ltr"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="mp" style:page-layout-name="pm"><style:header><text:p>odd-h</text:p></style:header><style:footer><text:p>odd-f</text:p></style:footer><style:header-left><text:p>even-h</text:p></style:header-left><style:footer-left><text:p>even-f</text:p></style:footer-left><style:header-first><text:p>first-h</text:p></style:header-first><style:footer-first><text:p>first-f</text:p></style:footer-first></style:master-page></office:master-styles></office:document-styles>"#;

    let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();
    let metadata = wb.sheets[0].print_metadata();

    assert_eq!(metadata.fidelity(), crate::PrintFidelity::Retained);
    assert_eq!(metadata.print_areas(), &[(0, 0, 1, 1), (3, 3, 7, 5)]);
    assert_eq!(metadata.manual_row_breaks(), &[1]);
    assert_eq!(metadata.manual_col_breaks(), &[1]);
    assert_eq!(metadata.page_order(), Some(PrintPageOrder::OverThenDown));
    assert_eq!(metadata.print_gridlines(), Some(false));
    assert_eq!(metadata.print_headings(), Some(true));
    assert_eq!(metadata.center_horizontally(), Some(true));
    assert_eq!(metadata.center_vertically(), Some(false));
    assert_eq!(metadata.header_footer().odd_header(), Some("odd-h"));
    assert_eq!(metadata.header_footer().odd_footer(), Some("odd-f"));
    assert_eq!(metadata.header_footer().even_header(), Some("even-h"));
    assert_eq!(metadata.header_footer().even_footer(), Some("even-f"));
    assert_eq!(metadata.header_footer().first_header(), Some("first-h"));
    assert_eq!(metadata.header_footer().first_footer(), Some("first-f"));
    assert_eq!(metadata.header_footer().different_odd_even(), Some(true));
    assert_eq!(metadata.header_footer().different_first(), Some(true));
    assert_eq!(
        wb.sheets[0].page_setup().and_then(|setup| setup.print_area),
        Some((0, 0, 1, 1))
    );
}

#[test]
fn malformed_ods_print_state_reports_typed_losses() {
    let content = r##"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Bad" table:style-name="ta" table:print-ranges="#REF! bad"/></office:spreadsheet></office:body></office:document-content>"##;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:style style:name="ta" style:family="table" style:master-page-name="mp"/></office:styles><office:automatic-styles><style:page-layout style:name="pm"><style:page-layout-properties style:print-page-order="diagonal"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="mp" style:page-layout-name="pm"/></office:master-styles></office:document-styles>"#;

    let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();
    let metadata = wb.sheets[0].print_metadata();
    assert_eq!(metadata.fidelity(), crate::PrintFidelity::Partial);
    assert!(metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::MissingReference));
    assert!(metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::InvalidPrintArea));
    assert!(metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::UnsupportedProperty));
}

#[test]
fn ods_table_groups_surface_outline_metadata() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Grouped"><table:table-column/><table:table-column-group table:display="false"><table:table-column table:number-columns-repeated="2"/><table:table-column-group><table:table-column/></table:table-column-group></table:table-column-group><table:table-row><table:table-cell office:value-type="string"><text:p>top</text:p></table:table-cell></table:table-row><table:table-row-group table:display="false"><table:table-row table:number-rows-repeated="2"><table:table-cell office:value-type="string"><text:p>detail</text:p></table:table-cell></table:table-row><table:table-row-group><table:table-row><table:table-cell office:value-type="string"><text:p>nested</text:p></table:table-cell></table:table-row></table:table-row-group></table:table-row-group></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = wb.sheet_by_name("Grouped").unwrap();

    assert_eq!(sheet.col_outline_levels().get(&1), Some(&1));
    assert_eq!(sheet.col_outline_levels().get(&2), Some(&1));
    assert_eq!(sheet.col_outline_levels().get(&3), Some(&2));
    assert_eq!(sheet.row_outline_levels().get(&1), Some(&1));
    assert_eq!(sheet.row_outline_levels().get(&2), Some(&1));
    assert_eq!(sheet.row_outline_levels().get(&3), Some(&2));

    let metadata = sheet.metadata();
    assert_eq!(metadata.col_outline_levels.get(&1), Some(&1));
    assert_eq!(metadata.row_outline_levels.get(&3), Some(&2));
}

#[test]
fn ods_page_layout_orientation_surfaces_page_setup() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Landscape" table:style-name="ta_landscape"/><table:table table:name="Plain"/></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:style style:name="ta_landscape" style:family="table" style:master-page-name="mp_landscape"/></office:styles><office:automatic-styles><style:page-layout style:name="pm_landscape"><style:page-layout-properties style:print-orientation="landscape"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="mp_landscape" style:page-layout-name="pm_landscape"/></office:master-styles></office:document-styles>"#;

    let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();
    let landscape = wb.sheet_by_name("Landscape").unwrap();
    let plain = wb.sheet_by_name("Plain").unwrap();

    assert!(landscape.page_setup().expect("page setup").landscape);
    assert_eq!(plain.page_setup(), None);

    let metadata = wb.worksheet_metadata("Landscape").unwrap();
    assert!(metadata.page_setup.expect("metadata page setup").landscape);

    let generic_metadata =
        <Workbook as crate::Reader>::worksheet_metadata(&wb, "Landscape").unwrap();
    assert!(
        generic_metadata
            .page_setup
            .expect("generic metadata page setup")
            .landscape
    );
}

#[test]
fn ods_page_layout_numbering_surfaces_page_setup() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Layout" table:style-name="ta_layout"/><table:table table:name="Plain"/></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:style style:name="ta_layout" style:family="table" style:master-page-name="mp_layout"/></office:styles><office:automatic-styles><style:page-layout style:name="pm_layout"><style:page-layout-properties style:scale-to="85%" style:first-page-number="3" style:table-centering="both"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="mp_layout" style:page-layout-name="pm_layout"/></office:master-styles></office:document-styles>"#;

    let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();
    let layout = wb.sheet_by_name("Layout").unwrap();
    let plain = wb.sheet_by_name("Plain").unwrap();

    let setup = layout.page_setup().expect("page setup");
    assert_eq!(setup.scale, Some(85));
    assert_eq!(setup.first_page_number, Some(3));
    assert!(setup.center_horizontally);
    assert!(setup.center_vertically);
    assert_eq!(plain.page_setup(), None);

    let metadata = wb.worksheet_metadata("Layout").unwrap();
    let metadata_setup = metadata.page_setup.expect("metadata page setup");
    assert_eq!(metadata_setup.scale, Some(85));
    assert_eq!(metadata_setup.first_page_number, Some(3));
    assert!(metadata_setup.center_horizontally);
    assert!(metadata_setup.center_vertically);
}

#[test]
fn ods_doc_properties_surface_through_workbook_metadata() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Meta"/></office:spreadsheet></office:body></office:document-content>"#;
    let meta = r#"<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0"><office:meta><dc:title>ODS Report</dc:title><dc:subject>Operations</dc:subject><meta:initial-creator>rxls ods</meta:initial-creator><dc:creator>reviewer</dc:creator><meta:keyword>ops</meta:keyword><meta:keyword>ods</meta:keyword><dc:description>ODS public metadata</dc:description><meta:creation-date>2026-06-24T02:03:04Z</meta:creation-date><meta:user-defined meta:name="Company">ACME ODS</meta:user-defined></office:meta></office:document-meta>"#;

    let wb = Workbook::open(&ods_bytes_with_meta(content, meta)).unwrap();
    let metadata = wb.metadata();

    assert_eq!(metadata.properties.title.as_deref(), Some("ODS Report"));
    assert_eq!(metadata.properties.subject.as_deref(), Some("Operations"));
    assert_eq!(metadata.properties.creator.as_deref(), Some("rxls ods"));
    assert_eq!(
        metadata.properties.last_modified_by.as_deref(),
        Some("reviewer")
    );
    assert_eq!(metadata.properties.keywords.as_deref(), Some("ops,ods"));
    assert_eq!(
        metadata.properties.description.as_deref(),
        Some("ODS public metadata")
    );
    assert_eq!(
        metadata.properties.created.as_deref(),
        Some("2026-06-24T02:03:04Z")
    );
    assert_eq!(metadata.properties.company.as_deref(), Some("ACME ODS"));
}

#[test]
fn ods_settings_active_table_surfaces_workbook_metadata() {
    let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data"/><table:table table:name="Summary"/></office:spreadsheet></office:body></office:document-content>"#;
    let settings = r#"<?xml version="1.0" encoding="UTF-8"?><office:document-settings xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:config="urn:oasis:names:tc:opendocument:xmlns:config:1.0"><office:settings><config:config-item-set config:name="ooo:view-settings"><config:config-item-map-indexed config:name="Views"><config:config-item-map-entry><config:config-item-map-named config:name="Tables"><config:config-item-map-entry config:name="Data"/><config:config-item-map-entry config:name="Summary"/></config:config-item-map-named><config:config-item config:name="ActiveTable" config:type="string">Summary</config:config-item></config:config-item-map-entry></config:config-item-map-indexed></config:config-item-set></office:settings></office:document-settings>"#;

    let wb = Workbook::open(&ods_bytes_with_parts(
        content,
        &[("settings.xml", settings.as_bytes())],
    ))
    .unwrap();
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
fn ods_settings_table_view_state_surfaces_sheet_view_metadata() {
    let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data"/><table:table table:name="Summary"/></office:spreadsheet></office:body></office:document-content>"#;
    let settings = r#"<?xml version="1.0" encoding="UTF-8"?><office:document-settings xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:config="urn:oasis:names:tc:opendocument:xmlns:config:1.0"><office:settings><config:config-item-set config:name="ooo:view-settings"><config:config-item-map-indexed config:name="Views"><config:config-item-map-entry><config:config-item-map-named config:name="Tables"><config:config-item-map-entry config:name="Data"><config:config-item config:name="HorizontalSplitMode" config:type="short">2</config:config-item><config:config-item config:name="VerticalSplitMode" config:type="short">2</config:config-item><config:config-item config:name="HorizontalSplitPosition" config:type="int">2</config:config-item><config:config-item config:name="VerticalSplitPosition" config:type="int">1</config:config-item><config:config-item config:name="PositionRight" config:type="int">2</config:config-item><config:config-item config:name="PositionBottom" config:type="int">1</config:config-item><config:config-item config:name="ZoomValue" config:type="short">125</config:config-item><config:config-item config:name="ShowGrid" config:type="boolean">false</config:config-item></config:config-item-map-entry><config:config-item-map-entry config:name="Summary"/></config:config-item-map-named><config:config-item config:name="HasColumnRowHeaders" config:type="boolean">false</config:config-item></config:config-item-map-entry></config:config-item-map-indexed></config:config-item-set></office:settings></office:document-settings>"#;

    let wb = Workbook::open(&ods_bytes_with_parts(
        content,
        &[("settings.xml", settings.as_bytes())],
    ))
    .unwrap();
    let data = wb.sheet_by_name("Data").unwrap();
    let summary = wb.sheet_by_name("Summary").unwrap();

    assert_eq!(
        data.sheet_view(),
        crate::SheetView {
            freeze: Some((1, 2)),
            hide_gridlines: true,
            zoom: Some(125),
            show_headers: Some(false),
            right_to_left: false,
        }
    );
    let metadata = wb.worksheet_metadata("Data").unwrap();
    assert_eq!(metadata.sheet_view.freeze, Some((1, 2)));
    assert!(metadata.sheet_view.hide_gridlines);
    assert_eq!(metadata.sheet_view.zoom, Some(125));
    assert_eq!(metadata.sheet_view.show_headers, Some(false));

    assert_eq!(summary.sheet_view().show_headers, Some(false));
    assert_eq!(summary.sheet_view().freeze, None);
    assert_eq!(summary.sheet_view().zoom, None);
    assert!(!summary.sheet_view().hide_gridlines);
}

#[test]
fn ods_draw_image_surfaces_public_metadata() {
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0A, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0,
        0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="Images"><table:table-row><table:table-cell office:value-type="string"><text:p>Logo</text:p></table:table-cell><table:table-cell><draw:frame draw:name="LogoFrame"><draw:image xlink:href="Pictures/logo.png" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/></draw:frame></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb =
        Workbook::open(&ods_bytes_with_part(content, "Pictures/logo.png", PNG_1X1)).expect("ods");
    let images = wb.sheets[0].images();

    assert_eq!(images.len(), 1);
    assert_eq!(images[0].format, crate::ImageFmt::Png);
    assert_eq!(images[0].data, PNG_1X1);
    assert_eq!(images[0].from, (0, 1));
    assert_eq!(images[0].to, None);
    let pictures = wb.pictures().expect("pictures");
    assert_eq!(pictures, vec![("png".to_string(), PNG_1X1.to_vec())]);
}

#[test]
fn ods_image_part_reader_obeys_aggregate_budget() {
    let first = vec![1_u8; 8];
    let second = vec![2_u8; 8];
    let bytes = ods_bytes_with_parts(
        "",
        &[
            ("Pictures/first.png", &first),
            ("Pictures/second.png", &second),
        ],
    );
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();

    let images = read_image_parts_with_limits(&mut zip, 16, 12);

    assert_eq!(images.len(), 1);
    assert_eq!(images["Pictures/first.png"].1, first);
    assert!(!images.contains_key("Pictures/second.png"));
}

#[test]
fn ods_named_ranges_surface_through_workbook_defined_names() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data"/><table:named-expressions><table:named-range table:name="TaxRate" table:cell-range-address="$Data.$B$2"/><table:named-range table:name="DataBlock" table:cell-range-address="$Data.$A$1:$Data.$B$2"/></table:named-expressions></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();

    assert_eq!(
        wb.defined_names(),
        &[
            ("TaxRate".to_string(), "Data!$B$2".to_string()),
            ("DataBlock".to_string(), "Data!$A$1:Data!$B$2".to_string()),
        ]
    );
    assert_eq!(wb.metadata().defined_names, wb.defined_names());
}

#[test]
fn ods_database_range_surfaces_as_autofilter_metadata() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data"/><table:database-ranges><table:database-range table:name="Filter" table:target-range-address="$Data.$A$1:$Data.$C$10" table:display-filter-buttons="true"/></table:database-ranges></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();

    assert_eq!(wb.sheets[0].autofilter_range(), Some((0, 0, 9, 2)));
}

#[test]
fn ods_unnamed_database_range_still_surfaces_as_autofilter_metadata() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data"/><table:database-ranges><table:database-range table:target-range-address="$Data.$A$1:$Data.$C$10"/></table:database-ranges></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();

    assert_eq!(wb.sheets[0].autofilter_range(), Some((0, 0, 9, 2)));
    assert!(wb.sheets[0].tables().is_empty());
}

#[test]
fn ods_table_print_ranges_surface_as_page_setup_print_area() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data" table:print-ranges="$Data.$B$2:$Data.$D$9"/></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();

    assert_eq!(
        wb.sheets[0].page_setup().and_then(|setup| setup.print_area),
        Some((1, 1, 8, 3))
    );
}

#[test]
fn ods_table_print_ranges_allow_quoted_sheet_names_with_spaces() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data Sheet" table:print-ranges="'Data Sheet'.$B$2:'Data Sheet'.$D$9"/></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();

    assert_eq!(
        wb.sheets[0].page_setup().and_then(|setup| setup.print_area),
        Some((1, 1, 8, 3))
    );
}

#[test]
fn ods_table_header_rows_surface_as_page_setup_repeat_rows() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-header-rows><table:table-row><table:table-cell office:value-type="string"><text:p>Region</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>Amount</text:p></table:table-cell></table:table-row></table:table-header-rows><table:table-row><table:table-cell office:value-type="float" office:value="42"><text:p>42</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();

    assert_eq!(
        wb.sheets[0]
            .page_setup()
            .and_then(|setup| setup.repeat_rows),
        Some((0, 1))
    );
    assert_eq!(wb.sheets[0].cells[2].row, 2);
}

#[test]
fn ods_table_header_columns_surface_as_page_setup_repeat_cols() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-header-columns><table:table-column/><table:table-column table:number-columns-repeated="2"/></table:table-header-columns><table:table-row><table:table-cell office:value-type="string"><text:p>Region</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Amount</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Owner</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Notes</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();

    assert_eq!(
        wb.sheets[0]
            .page_setup()
            .and_then(|setup| setup.repeat_cols),
        Some((0, 2))
    );
}

#[test]
fn ods_database_range_surfaces_as_table_metadata() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-row><table:table-cell office:value-type="string"><text:p>Item</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Amount</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>Paper</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="42"><text:p>42</text:p></table:table-cell></table:table-row></table:table><table:database-ranges><table:database-range table:name="DataBlock" table:target-range-address="$Data.$A$1:$Data.$B$2"/></table:database-ranges></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let tables = wb.sheets[0].tables();

    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "DataBlock");
    assert_eq!(tables[0].range, (0, 0, 1, 1));
    assert_eq!(tables[0].columns, ["Item", "Amount"]);
}

#[test]
fn ods_database_range_without_filter_buttons_keeps_table_without_autofilter() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-row><table:table-cell office:value-type="string"><text:p>Item</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Amount</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>Paper</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="42"><text:p>42</text:p></table:table-cell></table:table-row></table:table><table:database-ranges><table:database-range table:name="DataBlock" table:target-range-address="$Data.$A$1:$Data.$B$2" table:display-filter-buttons="false"/></table:database-ranges></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = &wb.sheets[0];
    let tables = sheet.tables();

    assert_eq!(sheet.autofilter_range(), None);
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "DataBlock");
    assert_eq!(tables[0].range, (0, 0, 1, 1));
}

#[test]
fn ods_content_validation_surfaces_as_data_validation_metadata() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:content-validations><table:content-validation table:name="PositiveAmount" table:condition="cell-content() &gt;= 0" table:allow-empty-cell="false"/></table:content-validations><table:table table:name="Data"><table:table-row><table:table-cell table:content-validation-name="PositiveAmount" office:value-type="float" office:value="5"><text:p>5</text:p></table:table-cell><table:table-cell table:content-validation-name="PositiveAmount" table:number-columns-repeated="2" office:value-type="float" office:value="7"><text:p>7</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let validations = wb.sheets[0].data_validations();

    assert_eq!(validations.len(), 2);
    assert_eq!(validations[0].sqref, (0, 0, 0, 0));
    assert_eq!(validations[1].sqref, (0, 1, 0, 2));
    for validation in validations {
        assert_eq!(validation.kind, crate::DvKind::Custom);
        assert_eq!(validation.operator, crate::DvOp::Between);
        assert_eq!(validation.formula1, "cell-content() >= 0");
        assert!(validation.formula2.is_none());
        assert!(!validation.allow_blank);
        assert!(!validation.show_input_message);
        assert!(!validation.show_error_message);
    }
}

#[test]
fn ods_cell_annotation_surfaces_as_comment_metadata() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Notes"><table:table-row><table:table-cell office:value-type="string"><text:p>Reviewed</text:p><office:annotation><dc:creator>auditor</dc:creator><text:p>Check source total</text:p><text:p>before award.</text:p></office:annotation></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let comments = wb.sheets[0].comments();

    assert_eq!(
        wb.sheets[0].cell(0, 0),
        Some(&Cell::Text("Reviewed".to_string()))
    );
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].row, 0);
    assert_eq!(comments[0].col, 0);
    assert_eq!(comments[0].text, "Check source total\nbefore award.");
    assert_eq!(comments[0].author.as_deref(), Some("auditor"));
}

#[test]
fn general_refs_are_reassembled_across_ods_text_surfaces() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="A&amp;B"><table:table-row><table:table-cell office:value-type="string"><text:p>A&amp;B&#33;</text:p><office:annotation><dc:creator>R&amp;D</dc:creator><text:p>Check &lt;now&gt;</text:p></office:annotation></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let meta = r#"<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><office:meta><dc:title>Budget &amp; Plan&#33;</dc:title></office:meta></office:document-meta>"#;

    let wb = Workbook::open(&ods_bytes_with_meta(content, meta)).unwrap();
    assert_eq!(wb.sheets[0].name, "A&B");
    assert_eq!(
        wb.sheets[0].cell(0, 0),
        Some(&Cell::Text("A&B!".to_string()))
    );
    assert_eq!(wb.sheets[0].comments()[0].author.as_deref(), Some("R&D"));
    assert_eq!(wb.sheets[0].comments()[0].text, "Check <now>");
    assert_eq!(
        wb.metadata().properties.title.as_deref(),
        Some("Budget & Plan!")
    );

    let settings = parse_settings(
        r#"<config><config-item config:name="ActiveTable">A&amp;B</config-item></config>"#,
    );
    assert_eq!(settings.active_table.as_deref(), Some("A&B"));
}

#[test]
fn unknown_and_illegal_general_refs_are_preserved_lexically_on_read() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-row><table:table-cell office:value-type="string"><text:p>A&bogus;&#x1;</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let workbook = Workbook::open(&ods_bytes(content)).unwrap();

    assert_eq!(
        workbook.sheets[0].cell(0, 0),
        Some(&Cell::Text("A&bogus;&#x1;".to_string()))
    );
}

#[test]
fn ods_repeated_annotation_only_rows_replicate_comment_metadata() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Notes"><table:table-row table:number-rows-repeated="2"><table:table-cell><office:annotation><text:p>row note</text:p></office:annotation></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let comments = wb.sheets[0].comments();

    assert_eq!(comments.len(), 2);
    assert_eq!((comments[0].row, comments[0].col), (0, 0));
    assert_eq!((comments[1].row, comments[1].col), (1, 0));
    assert!(wb.sheets[0].cells().next().is_none());
}

#[test]
fn ods_dc_creator_fills_creator_when_initial_creator_is_absent() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Meta"/></office:spreadsheet></office:body></office:document-content>"#;
    let meta = r#"<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><office:meta><dc:creator>standalone author</dc:creator></office:meta></office:document-meta>"#;

    let wb = Workbook::open(&ods_bytes_with_meta(content, meta)).unwrap();
    let metadata = wb.metadata();

    assert_eq!(
        metadata.properties.creator.as_deref(),
        Some("standalone author")
    );
    assert_eq!(
        metadata.properties.last_modified_by.as_deref(),
        Some("standalone author")
    );
}

#[test]
fn ods_self_closing_empty_table_is_preserved() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Empty"/></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();

    assert_eq!(wb.sheets.len(), 1);
    assert_eq!(wb.sheets[0].name, "Empty");
    assert_eq!(wb.sheets[0].cells().count(), 0);
}

#[test]
fn ods_structural_empty_repeats_do_not_declare_a_source_used_range() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Structural"><table:table-column table:number-columns-repeated="4"/><table:table-row table:number-rows-repeated="6"><table:table-cell table:number-columns-repeated="4"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let workbook = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = &workbook.sheets[0];

    assert_eq!(sheet.dimensions(), None);
    assert_eq!(sheet.cells().count(), 0);
}

#[test]
fn repeated_valued_rows_are_replicated() {
    // A `number-rows-repeated` row carrying a value must be replicated, not
    // collapsed to one row + a skip.
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="r"><table:table-row table:number-rows-repeated="3"><table:table-cell office:value-type="float" office:value="5"><text:p>5</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let s = &wb.sheets[0];
    assert_eq!(s.cell(0, 0), Some(&Cell::Number(5.0)));
    assert_eq!(s.cell(1, 0), Some(&Cell::Number(5.0)));
    assert_eq!(s.cell(2, 0), Some(&Cell::Number(5.0)));
}

#[test]
fn large_row_repeat_is_not_truncated_at_64k() {
    // A legitimate `number-rows-repeated` above the old 64k cap must replicate
    // up to the row grid (bounded by the text budget), not silently truncate.
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="big"><table:table-row table:number-rows-repeated="70000"><table:table-cell office:value-type="float" office:value="5"><text:p>5</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let s = &wb.sheets[0];
    assert_eq!(
        s.cell(69_999, 0),
        Some(&Cell::Number(5.0)),
        "row 69999 survives"
    );
    assert_eq!(s.cell(70_000, 0), None, "exactly 70000 rows (0..=69999)");
}

#[test]
fn hostile_row_and_column_repeats_exhaust_budget_without_hanging() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="dos"><table:table-row table:number-rows-repeated="999999999"><table:table-cell table:number-columns-repeated="999999999" office:value-type="string"><text:p>X</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();

    assert!(wb.text_truncated);
    assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Text("X".into())));
}

#[test]
fn merged_range_from_span() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="m"><table:table-row><table:table-cell table:number-columns-spanned="3" table:number-rows-spanned="1" office:value-type="string"><text:p>title</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    assert_eq!(wb.sheets[0].merged_ranges(), &[(0, 0, 0, 2)]);
}

#[test]
fn ods_text_space_elements_preserve_significant_whitespace() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="spaces"><table:table-row><table:table-cell office:value-type="string"><text:p>Value <text:s/>With spaces</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p><text:s text:c="2"/>Value <text:s text:c="2"/>With after <text:s/></text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>A<text:tab/>B<text:line-break/>C</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = &wb.sheets[0];

    assert_eq!(
        sheet.cell(0, 0),
        Some(&Cell::Text("Value  With spaces".into()))
    );
    assert_eq!(
        sheet.cell(1, 0),
        Some(&Cell::Text("  Value   With after  ".into()))
    );
    assert_eq!(sheet.cell(2, 0), Some(&Cell::Text("A\tB\nC".into())));
}

#[test]
fn ods_text_a_hyperlink_preserves_label_and_target() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="links"><table:table-row><table:table-cell office:value-type="string"><text:p><text:a xlink:href="https://example.com/path?q=1">Example</text:a></text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = &wb.sheets[0];

    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("Example".to_string())));
    assert_eq!(
        sheet.hyperlinks(),
        &[(0u32, 0u16, "https://example.com/path?q=1".to_string())]
    );
}

#[test]
fn ods_repeated_hyperlink_cells_replicate_targets() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="links"><table:table-row table:number-rows-repeated="2"><table:table-cell table:number-columns-repeated="2" office:value-type="string"><text:p><text:a xlink:href="https://example.com/repeat">Repeat</text:a></text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = &wb.sheets[0];

    for row in 0..=1 {
        for col in 0..=1 {
            assert_eq!(
                sheet.cell(row, col),
                Some(&Cell::Text("Repeat".to_string()))
            );
        }
    }
    assert_eq!(
        sheet.hyperlinks(),
        &[
            (0u32, 0u16, "https://example.com/repeat".to_string()),
            (0u32, 1u16, "https://example.com/repeat".to_string()),
            (1u32, 0u16, "https://example.com/repeat".to_string()),
            (1u32, 1u16, "https://example.com/repeat".to_string()),
        ]
    );
}

#[test]
fn ods_formula_source_and_cached_value_are_preserved() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="formulas"><table:table-row><table:table-cell office:value-type="float" office:value="2"><text:p>2</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="5"><text:p>5</text:p></table:table-cell><table:table-cell table:formula="of:=SUM([.A1:.B1])" office:value-type="float" office:value="7"><text:p>7</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = &wb.sheets[0];

    match sheet.cell(0, 2).expect("formula cell") {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "SUM([.A1:.B1])");
            assert_eq!(cached.as_ref(), &Cell::Number(7.0));
        }
        other => panic!("expected formula cell, got {other:?}"),
    }
    assert_eq!(sheet.formatted(0, 2), Some("7"));
    let formulas = wb.worksheet_formula("formulas").expect("formula range");
    assert_eq!(formulas.get_abs(0, 2), Some("SUM([.A1:.B1])"));
}

#[test]
fn ods_formula_without_cached_value_is_preserved() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="formulas"><table:table-row><table:table-cell table:formula="of:=SUM([.A1:.A2])"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let wb = Workbook::open(&ods_bytes(content)).unwrap();
    let sheet = &wb.sheets[0];

    match sheet.cell(0, 0).expect("formula cell") {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "SUM([.A1:.A2])");
            assert_eq!(cached.as_ref(), &Cell::Text(String::new()));
        }
        other => panic!("expected formula cell, got {other:?}"),
    }
    let formulas = wb.worksheet_formula("formulas").expect("formula range");
    assert_eq!(formulas.get_abs(0, 0), Some("SUM([.A1:.A2])"));
}

#[test]
fn ods_style_cascade_retains_cell_row_column_text_and_number_formats() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Styled"><table:table-column table:style-name="Column" table:default-cell-style-name="Base" table:number-columns-repeated="2"/><table:table-row table:style-name="Row" table:default-cell-style-name="Base"><table:table-cell office:value-type="string"><text:p>row default</text:p></table:table-cell><table:table-cell table:style-name="Child" office:value-type="float" office:value="1234.5"><text:p><text:span text:style-name="Em">₩1,234.50</text:span></text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r##"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"><office:styles><style:default-style style:family="table-cell"><style:text-properties fo:font-family="Noto Sans" fo:font-size="10pt"/></style:default-style><number:currency-style style:name="Money"><number:currency-symbol>₩</number:currency-symbol><number:number number:decimal-places="2" number:min-integer-digits="1" number:grouping="true"/></number:currency-style><style:style style:name="Base" style:family="table-cell" style:data-style-name="Money"><style:table-cell-properties fo:background-color="#ffeecc" fo:border="0.75pt solid #112233"/><style:text-properties fo:font-weight="bold"/></style:style><style:style style:name="Child" style:family="table-cell" style:parent-style-name="Base"><style:text-properties fo:color="#2244aa" fo:font-style="italic"/></style:style><style:style style:name="Row" style:family="table-row"><style:table-row-properties style:row-height="0.5in"/></style:style><style:style style:name="Column" style:family="table-column"><style:table-column-properties style:column-width="2cm"/></style:style><style:style style:name="Em" style:family="text"><style:text-properties fo:font-style="italic" fo:color="#008800"/></style:style></office:styles></office:document-styles>"##;

    let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
    assert!(sheet.style_losses().is_empty());
    assert_eq!(sheet.row_heights().get(&0), Some(&36.0));
    assert!((sheet.column_widths()[&0] - (2.0 * 72.0 / 2.54 / 5.25) as f32).abs() < 0.01);
    assert!((sheet.physical_column_widths()[&0] - (2.0 * 72.0 / 2.54) as f32).abs() < 0.01);
    assert_eq!(
        sheet.imported_row_axis_measures().get(&0),
        Some(&ImportedAxisMeasure::Twips(720))
    );
    assert_eq!(
        sheet.imported_column_axis_measures().get(&0),
        Some(&ImportedAxisMeasure::MillimeterHundredths(2_000))
    );
    assert_eq!(
        sheet.imported_column_axis_measures().get(&1),
        Some(&ImportedAxisMeasure::MillimeterHundredths(2_000))
    );

    let inherited = sheet.resolved_cell_style(0, 0).expect("row style");
    assert!(inherited.font.as_ref().is_some_and(|font| font.bold));
    assert_eq!(inherited.num_fmt.as_deref(), Some(r"\₩#,##0.00"));
    assert_eq!(inherited.fill, Some(Color::rgb(0xff, 0xee, 0xcc)));

    let explicit = sheet.cell_style(0, 1).expect("child style");
    let font = explicit.font.as_ref().expect("font");
    assert_eq!(font.name.as_deref(), Some("Noto Sans"));
    assert!(font.bold);
    assert!(font.italic);
    assert_eq!(font.color, Some(Color::rgb(0x22, 0x44, 0xaa)));
    assert_eq!(explicit.num_fmt.as_deref(), Some(r"\₩#,##0.00"));
    assert_eq!(
        explicit.border.as_ref().map(|border| border.left),
        Some(BorderStyle::Thin)
    );

    let runs = sheet.rich_text_runs(0, 1).expect("text style run");
    assert_eq!(runs.len(), 1);
    assert!(runs[0].font.italic);
    assert_eq!(runs[0].font.color, Some(Color::rgb(0x00, 0x88, 0x00)));
}

#[test]
fn ods_optimal_row_height_is_retained_through_direct_default_and_parent_styles() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Rows"><table:table-row><table:table-cell/></table:table-row><table:table-row table:style-name="DirectAuto"><table:table-cell/></table:table-row><table:table-row table:style-name="InheritedAuto"><table:table-cell/></table:table-row><table:table-row table:style-name="ManualOverride"><table:table-cell/></table:table-row><table:table-row table:style-name="DirectManual"><table:table-cell/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:default-style style:family="table-row"><style:table-row-properties style:row-height="0.20in" style:use-optimal-row-height="true"/></style:default-style><style:style style:name="DirectAuto" style:family="table-row"><style:table-row-properties style:row-height="0.25in" style:use-optimal-row-height="true"/></style:style><style:style style:name="AutoParent" style:family="table-row"><style:table-row-properties style:row-height="0.30in" style:use-optimal-row-height="true"/></style:style><style:style style:name="InheritedAuto" style:family="table-row" style:parent-style-name="AutoParent"/><style:style style:name="ManualOverride" style:family="table-row" style:parent-style-name="AutoParent"><style:table-row-properties style:use-optimal-row-height="false"/></style:style><style:style style:name="DirectManual" style:family="table-row"><style:table-row-properties style:row-height="0.35in" style:use-optimal-row-height="false"/></style:style></office:styles></office:document-styles>"#;

    let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &workbook.sheets[0];

    assert_eq!(
        sheet
            .automatic_row_height_candidates
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    for row in 0..=2 {
        assert!(sheet.row_heights().contains_key(&row));
        assert!(!sheet.row_height_is_manual(row));
    }
    for row in 3..=4 {
        assert!(sheet.row_heights().contains_key(&row));
        assert!(sheet.row_height_is_manual(row));
    }
    assert!((sheet.row_heights()[&0] - 14.4).abs() < 0.001);
    assert!((sheet.row_heights()[&1] - 18.0).abs() < 0.001);
    assert!((sheet.row_heights()[&2] - 21.6).abs() < 0.001);
    assert!((sheet.row_heights()[&3] - 21.6).abs() < 0.001);
    assert!((sheet.row_heights()[&4] - 25.2).abs() < 0.001);
}

#[test]
fn ods_undeclared_row_retains_calc_application_default_height() {
    // A row with no table:style-name, and no default-style declared for
    // table-row anywhere in styles.xml, persists no height at all: not
    // per-row, not through a sheet-wide default-style cascade. Calc's own
    // no-information application default (0.5 cm) must still be exposed
    // as sheet-wide provenance, mirroring the unconditional 64-point
    // column default a few lines above, so renderers that reconstruct
    // Calc's native cumulative row geometry do not silently substitute an
    // unrelated fallback for this row.
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(
        sheet.imported_default_row_axis_measure(),
        Some(ImportedAxisMeasure::MillimeterHundredths(500))
    );
    assert!(!sheet.imported_row_axis_measures().contains_key(&0));
    assert!(!sheet.row_heights().contains_key(&0));
}

#[test]
fn ods_empty_table_retains_calc_application_default_row_height() {
    // A self-closing <table:table/> with no rows at all takes the other
    // sheet-construction branch in the importer; it needs the same
    // sheet-wide provenance for renderers that populate rows later
    // through the public API.
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Empty"/></office:spreadsheet></office:body></office:document-content>"#;
    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(
        sheet.imported_default_row_axis_measure(),
        Some(ImportedAxisMeasure::MillimeterHundredths(500))
    );
}

#[test]
fn ods_child_geometry_clears_unrepresentable_parent_axis_provenance() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Geometry"><table:table-column table:style-name="ChildColumn"/><table:table-column table:style-name="InheritedColumn"/><table:table-row table:style-name="ChildRow"><table:table-cell/><table:table-cell/></table:table-row><table:table-row table:style-name="InheritedRow"><table:table-cell/><table:table-cell/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:style style:name="BaseRow" style:family="table-row"><style:table-row-properties style:row-height="1cm"/></style:style><style:style style:name="ChildRow" style:family="table-row" style:parent-style-name="BaseRow"><style:table-row-properties style:row-height="0.00000000000000000001cm"/></style:style><style:style style:name="InheritedRow" style:family="table-row" style:parent-style-name="BaseRow"/><style:style style:name="BaseColumn" style:family="table-column"><style:table-column-properties style:column-width="1cm"/></style:style><style:style style:name="ChildColumn" style:family="table-column" style:parent-style-name="BaseColumn"><style:table-column-properties style:column-width="0.00000000000000000001cm"/></style:style><style:style style:name="InheritedColumn" style:family="table-column" style:parent-style-name="BaseColumn"/></office:styles></office:document-styles>"#;

    let mut workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &mut workbook.sheets[0];
    let inherited_points = (1.0 * 72.0 / 2.54) as f32;

    assert_eq!(
        parse_ods_axis_measure("1.23456cm"),
        Some(ImportedAxisMeasure::PointRatio(555_552, 15_875))
    );
    assert!(sheet.row_heights()[&0] < 1e-10);
    assert!(sheet.physical_column_widths()[&0] < 1e-10);
    assert!(!sheet.imported_row_axis_measures().contains_key(&0));
    assert!(!sheet.imported_column_axis_measures().contains_key(&0));
    assert!((sheet.row_heights()[&1] - inherited_points).abs() < 0.001);
    assert!((sheet.physical_column_widths()[&1] - inherited_points).abs() < 0.001);
    assert_eq!(
        sheet.imported_row_axis_measures().get(&1),
        Some(&ImportedAxisMeasure::MillimeterHundredths(1_000))
    );
    assert_eq!(
        sheet.imported_column_axis_measures().get(&1),
        Some(&ImportedAxisMeasure::MillimeterHundredths(1_000))
    );

    sheet.set_col_width(1, 9.0);
    assert_eq!(sheet.column_widths().get(&1), Some(&9.0));
    assert!(!sheet.physical_column_widths().contains_key(&1));
    assert!(!sheet.imported_column_axis_measures().contains_key(&1));
    sheet.set_row_height(1, 9.0);
    assert_eq!(sheet.row_heights().get(&1), Some(&9.0));
    assert!(!sheet.imported_row_axis_measures().contains_key(&1));
}

#[test]
fn ods_default_family_styles_apply_without_named_references() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Defaults"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>defaulted</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r##"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:default-style style:family="table-cell"><style:text-properties fo:font-family="Noto Sans KR" fo:font-weight="bold"/><style:table-cell-properties fo:background-color="#ddeeff"/></style:default-style><style:default-style style:family="table-row"><style:table-row-properties style:row-height="18pt"/></style:default-style><style:default-style style:family="table-column"><style:table-column-properties style:column-width="1in"/></style:default-style></office:styles></office:document-styles>"##;

    let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &workbook.sheets[0];
    let style = sheet.resolved_cell_style(0, 0).expect("default style");
    assert!(style.font.as_ref().is_some_and(|font| font.bold));
    assert_eq!(style.fill, Some(Color::rgb(0xDD, 0xEE, 0xFF)));
    assert_eq!(sheet.row_heights().get(&0), Some(&18.0));
    assert!((sheet.column_widths()[&0] - (72.0 / 5.25) as f32).abs() < 0.01);
    assert_eq!(sheet.physical_column_widths().get(&0), Some(&72.0));
}

#[test]
fn ods_text_and_paragraph_style_names_do_not_collide() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Style namespaces"><table:table-row><table:table-cell office:value-type="string"><text:p text:style-name="Same">paragraph<text:span text:style-name="Same">span</text:span></text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r##"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:style style:name="Same" style:family="paragraph"><style:text-properties fo:font-weight="bold" fo:color="#aa0000"/></style:style><style:style style:name="Same" style:family="text"><style:text-properties fo:font-style="italic" fo:color="#008800"/></style:style></office:styles></office:document-styles>"##;

    let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
    let runs = sheet.rich_text_runs(0, 0).expect("paragraph and span runs");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].text, "paragraph");
    assert!(runs[0].font.bold);
    assert!(!runs[0].font.italic);
    assert_eq!(runs[0].font.color, Some(Color::rgb(0xaa, 0x00, 0x00)));
    assert_eq!(runs[1].text, "span");
    assert!(runs[1].font.bold);
    assert!(runs[1].font.italic);
    assert_eq!(runs[1].font.color, Some(Color::rgb(0x00, 0x88, 0x00)));
}

#[test]
fn ods_missing_content_style_references_are_aggregated() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Missing" table:style-name="MissingTable" table:default-cell-style-name="MissingTableCell"><table:table-column table:style-name="MissingColumn" table:default-cell-style-name="MissingColumnCell"/><table:table-row table:style-name="MissingRow" table:default-cell-style-name="MissingRowCell"><table:table-cell table:style-name="MissingCell" office:value-type="string"><text:p text:style-name="MissingParagraph">plain<text:span text:style-name="MissingText">span</text:span></text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:default-style style:family="table-cell"/></office:styles></office:document-styles>"#;

    let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
    assert_eq!(
        sheet
            .style_losses()
            .iter()
            .find(|loss| loss.kind == StyleLossKind::MissingReference)
            .map(|loss| loss.occurrences),
        Some(9)
    );
}

#[test]
fn ods_transparent_child_fill_clears_parent_fill() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Transparent"><table:table-row><table:table-cell table:style-name="Child" office:value-type="string" office:string-value="clear"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r##"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:style style:name="Base" style:family="table-cell"><style:table-cell-properties fo:background-color="#cc3300"/></style:style><style:style style:name="Child" style:family="table-cell" style:parent-style-name="Base"><style:table-cell-properties fo:background-color="transparent"/></style:style></office:styles></office:document-styles>"##;

    let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
    let style = sheet.cell_style(0, 0).expect("explicit child style");
    assert_eq!(style.fill, None);
    assert_eq!(style.pattern_fill, None);
}

#[test]
fn ods_scientific_and_fixed_fraction_number_styles_are_retained() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Numbers"><table:table-row><table:table-cell table:style-name="ScientificCell" office:value-type="float" office:value="12345"/><table:table-cell table:style-name="FractionCell" office:value-type="float" office:value="0.375"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"><office:styles><number:number-style style:name="Scientific"><number:scientific-number number:decimal-places="2" number:min-decimal-places="2" number:min-exponent-digits="3" number:forced-exponent-sign="true"/></number:number-style><number:number-style style:name="Fraction"><number:fraction number:min-numerator-digits="2" number:denominator-value="8"/></number:number-style><style:style style:name="ScientificCell" style:family="table-cell" style:data-style-name="Scientific"/><style:style style:name="FractionCell" style:family="table-cell" style:data-style-name="Fraction"/></office:styles></office:document-styles>"#;

    let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
    assert_eq!(
        sheet
            .cell_style(0, 0)
            .and_then(|style| style.num_fmt.as_deref()),
        Some("0.00E+000")
    );
    assert_eq!(
        sheet
            .cell_style(0, 1)
            .and_then(|style| style.num_fmt.as_deref()),
        Some("# ??/8")
    );
}

#[test]
fn ods_sheet_level_page_image_retains_absolute_geometry() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="Page image"><table:shapes><draw:frame draw:name="Page logo" draw:z-index="5" text:anchor-type="page" svg:x="1cm" svg:y="2cm" svg:width="3cm" svg:height="4cm"><draw:image xlink:href="Pictures/page.png"/><svg:desc>Page-level description</svg:desc></draw:frame></table:shapes></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let workbook =
        Workbook::open(&ods_bytes_with_part(content, "Pictures/page.png", PNG_1X1)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.images().len(), 1);
    assert_eq!(sheet.images()[0].from, (0, 0));
    assert_eq!(sheet.images()[0].to, None);
    let metadata = &sheet.drawing_metadata()[0];
    assert_eq!(metadata.kind, DrawingObjectKind::Image);
    assert_eq!(metadata.object_index, 0);
    assert_eq!(metadata.from_cell, None);
    assert_eq!(metadata.to_cell, None);
    assert_eq!(metadata.from_offset_emu, Some((360_000, 720_000)));
    assert_eq!(metadata.absolute_size_emu, Some((1_080_000, 1_440_000)));
    assert_eq!(metadata.z_order, Some(5));
    assert_eq!(metadata.name.as_deref(), Some("Page logo"));
    assert_eq!(metadata.alt_text.as_deref(), Some("Page-level description"));
    assert_eq!(metadata.behavior, DrawingAnchorBehavior::Absolute);
}

#[test]
fn ods_missing_image_target_is_a_typed_fidelity_loss() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:automatic-styles><style:default-style style:family="table-cell"/></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Missing image"><table:shapes><draw:frame text:anchor-type="page"><draw:image xlink:href="Pictures/missing.png"/></draw:frame></table:shapes></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert!(sheet.images().is_empty());
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
    assert!(sheet
        .style_losses()
        .iter()
        .any(|loss| loss.kind == StyleLossKind::DrawingMetadataPartial));
}

#[test]
fn ods_parent_style_cycle_is_bounded_and_typed() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Cycle"><table:table-row><table:table-cell table:style-name="A" office:value-type="string"><text:p>safe</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:style style:name="A" style:family="table-cell" style:parent-style-name="B"><style:text-properties fo:font-weight="bold"/></style:style><style:style style:name="B" style:family="table-cell" style:parent-style-name="A"><style:text-properties fo:font-style="italic"/></style:style></office:styles></office:document-styles>"#;

    let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("safe".to_string())));
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
    assert!(sheet
        .style_losses()
        .iter()
        .any(|loss| loss.kind == StyleLossKind::InheritanceCycle));
}

#[test]
fn ods_drawing_frame_retains_physical_geometry_and_accessibility() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="Images"><table:table-row><table:table-cell/><table:table-cell><draw:frame draw:name="Korean logo" draw:z-index="7" text:anchor-type="cell" svg:x="-1cm" svg:y="2cm" svg:width="3cm" svg:height="4cm" table:end-cell-address=".D5" table:end-x="0.5cm" table:end-y="-0.25cm" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><draw:image xlink:href="Pictures/logo.png"/><svg:desc>접근 가능한 설명</svg:desc></draw:frame></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let workbook =
        Workbook::open(&ods_bytes_with_part(content, "Pictures/logo.png", PNG_1X1)).expect("ods");
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.images()[0].from, (0, 1));
    assert_eq!(sheet.images()[0].to, Some((4, 3)));
    let metadata = &sheet.drawing_metadata()[0];
    assert_eq!(metadata.kind, DrawingObjectKind::Image);
    assert_eq!(metadata.object_index, 0);
    assert_eq!(metadata.from_cell, Some((0, 1)));
    assert_eq!(metadata.to_cell, Some((4, 3)));
    assert_eq!(metadata.from_offset_emu, Some((-360_000, 720_000)));
    assert_eq!(metadata.to_offset_emu, Some((180_000, -90_000)));
    assert_eq!(metadata.absolute_size_emu, Some((1_080_000, 1_440_000)));
    assert_eq!(metadata.z_order, Some(7));
    assert_eq!(metadata.name.as_deref(), Some("Korean logo"));
    assert_eq!(metadata.alt_text.as_deref(), Some("접근 가능한 설명"));
    assert_eq!(metadata.behavior, DrawingAnchorBehavior::MoveAndSize);
}

#[test]
fn ods_graphic_style_inheritance_normalizes_clip_to_crop_ppm() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:automatic-styles><style:style style:name="CropBase" style:family="graphic"><style:graphic-properties fo:clip="rect(0.01cm, 0.02cm, 0.005cm, 0.01cm)"/></style:style><style:style style:name="CropChild" style:family="graphic" style:parent-style-name="CropBase"/></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Crop"><table:table-row><table:table-cell><draw:frame draw:style-name="CropChild"><draw:image xlink:href="Pictures/crop.png"/></draw:frame></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let png = png_1x1_with_physical_density();

    let workbook =
        Workbook::open(&ods_bytes_with_part(content, "Pictures/crop.png", &png)).expect("ods");
    let sheet = &workbook.sheets[0];

    assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
    assert_eq!(sheet.images().len(), 1);
    assert_eq!(
        sheet.drawing_metadata()[0].crop,
        Some(DrawingCrop {
            left_ppm: 100_000,
            top_ppm: 200_000,
            right_ppm: 200_000,
            bottom_ppm: 100_000,
        })
    );
}

#[test]
fn ods_clip_without_physical_density_is_a_typed_partial_loss() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:automatic-styles><style:style style:name="Crop" style:family="graphic"><style:graphic-properties fo:clip="rect(0.01cm, 0cm, 0cm, 0cm)"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Crop"><table:table-row><table:table-cell><draw:frame draw:style-name="Crop"><draw:image xlink:href="Pictures/crop.png"/></draw:frame></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let workbook =
        Workbook::open(&ods_bytes_with_part(content, "Pictures/crop.png", PNG_1X1)).expect("ods");
    let sheet = &workbook.sheets[0];

    assert_eq!(sheet.drawing_metadata()[0].crop, None);
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
    assert!(sheet
        .style_losses()
        .iter()
        .any(|loss| loss.kind == StyleLossKind::DrawingMetadataPartial));
}

#[test]
fn ods_jpeg_jfif_density_produces_bounded_physical_dimensions() {
    let jpeg = [
        0xff, 0xd8, // SOI
        0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x01, 0x00, 0x48, 0x00,
        0x90, 0x00, 0x00, // JFIF: 72 x 144 dpi
        0xff, 0xc0, 0x00, 0x07, 0x08, 0x00, 0xc8, 0x00, 0x64, // 100 x 200 px
        0xff, 0xd9, // EOI
    ];

    assert_eq!(jpeg_physical_size_points(&jpeg), Some((100.0, 100.0)));
}

#[test]
fn ods_unsupported_shape_retains_cell_anchor_sidecar() {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Shape"><table:table-row><table:table-cell/><table:table-cell><draw:frame text:anchor-type="cell" table:end-cell-address=".D3" svg:width="2cm" svg:height="1cm"></draw:frame></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
    let metadata = &workbook.sheets[0].drawing_metadata()[0];

    assert_eq!(metadata.kind, DrawingObjectKind::Shape);
    assert_eq!(metadata.from_cell, Some((0, 1)));
    assert_eq!(metadata.to_cell, Some((2, 3)));
    assert_eq!(metadata.absolute_size_emu, Some((720_000, 360_000)));
}
