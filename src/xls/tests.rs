use super::*;
use crate::{extract_text, format_number, SheetMetadata, SheetType, SheetVisible};
use std::io::{Cursor, Write};

fn ctx8() -> Ctx {
    Ctx {
        biff8: true,
        enc: WINDOWS_1252,
    }
}

fn rec(typ: u16, body: &[u8]) -> Vec<u8> {
    let mut v = typ.to_le_bytes().to_vec();
    v.extend_from_slice(&(body.len() as u16).to_le_bytes());
    v.extend_from_slice(body);
    v
}

fn wrap_xls(stream: &[u8], name: &str) -> Vec<u8> {
    wrap_xls_with_extra_streams(stream, name, &[])
}

fn encode_legacy_text(encoding: &'static Encoding, value: &str) -> Vec<u8> {
    let (bytes, _, had_errors) = encoding.encode(value);
    assert!(
        !had_errors,
        "{value:?} is not representable in {}",
        encoding.name()
    );
    bytes.into_owned()
}

/// Build one complete BIFF5 `Book` stream so codepage tests exercise the
/// global declaration, sheet-name path, and cell-string path together.
fn biff5_single_label(
    declared_codepage: Option<u16>,
    encoding: &'static Encoding,
    sheet_name: &str,
    label: &str,
) -> Vec<u8> {
    let sheet_name = encode_legacy_text(encoding, sheet_name);
    let label = encode_legacy_text(encoding, label);

    let mut global_bof = vec![0x00, 0x05, 0x05, 0x00];
    global_bof.extend_from_slice(&[0u8; 4]);
    let mut stream = rec(BOF, &global_bof);
    if let Some(codepage) = declared_codepage {
        stream.extend_from_slice(&rec(CODEPAGE, &codepage.to_le_bytes()));
    }

    let mut boundsheet = vec![0, 0, 0, 0, 0, 0, sheet_name.len() as u8];
    boundsheet.extend_from_slice(&sheet_name);
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut sheet_bof = vec![0x00, 0x05, 0x10, 0x00];
    sheet_bof.extend_from_slice(&[0u8; 4]);
    stream.extend_from_slice(&rec(BOF, &sheet_bof));
    let mut label_record = vec![0, 0, 0, 0, 0, 0];
    label_record.extend_from_slice(&(label.len() as u16).to_le_bytes());
    label_record.extend_from_slice(&label);
    stream.extend_from_slice(&rec(LABEL, &label_record));
    stream.extend_from_slice(&rec(EOF, &[]));

    wrap_xls(&stream, "/Book")
}

fn wrap_xls_with_extra_streams(stream: &[u8], name: &str, extra: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    comp.create_stream(name).unwrap().write_all(stream).unwrap();
    for (stream_name, body) in extra {
        comp.create_stream(stream_name)
            .unwrap()
            .write_all(body)
            .unwrap();
    }
    comp.flush().unwrap();
    comp.into_inner().into_inner()
}

fn cfb_directory_entry_offset(bytes: &[u8], name: &str) -> usize {
    let directory_sector = u32::from_le_bytes(bytes[48..52].try_into().unwrap()) as usize;
    let sector_shift = u16::from_le_bytes(bytes[30..32].try_into().unwrap()) as usize;
    let sector_size = 1usize << sector_shift;
    let directory_offset = (directory_sector + 1) * sector_size;
    let encoded = name
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let relative = bytes[directory_offset..directory_offset + sector_size]
        .windows(encoded.len())
        .position(|window| window == encoded)
        .expect("CFB directory entry");
    let offset = directory_offset + relative;
    assert_eq!((offset - directory_offset) % 128, 0);
    offset
}

#[derive(Clone, Copy)]
enum TestPropertyValue<'a> {
    Lpstr(&'a str),
    Filetime(u64),
}

fn property_set_stream(fmtid: [u8; 16], properties: &[(u32, &str)]) -> Vec<u8> {
    let properties = properties
        .iter()
        .map(|(id, value)| (*id, TestPropertyValue::Lpstr(value)))
        .collect::<Vec<_>>();
    property_set_stream_values(fmtid, &properties)
}

fn property_set_stream_values(
    fmtid: [u8; 16],
    properties: &[(u32, TestPropertyValue<'_>)],
) -> Vec<u8> {
    let section_offset = 48u32;
    let mut section = Vec::new();
    section.extend_from_slice(&0u32.to_le_bytes()); // section size, patched below
    section.extend_from_slice(&(properties.len() as u32).to_le_bytes());

    let table_start = section.len();
    section.resize(section.len() + properties.len() * 8, 0);

    let mut value_offsets = Vec::new();
    for &(_id, value) in properties {
        value_offsets.push(section.len() as u32);
        match value {
            TestPropertyValue::Lpstr(value) => {
                section.extend_from_slice(&0x1Eu32.to_le_bytes()); // VT_LPSTR
                section.extend_from_slice(&((value.len() + 1) as u32).to_le_bytes());
                section.extend_from_slice(value.as_bytes());
                section.push(0);
            }
            TestPropertyValue::Filetime(value) => {
                section.extend_from_slice(&0x40u32.to_le_bytes()); // VT_FILETIME
                section.extend_from_slice(&value.to_le_bytes());
            }
        }
        while section.len() % 4 != 0 {
            section.push(0);
        }
    }

    let section_size = section.len() as u32;
    section[0..4].copy_from_slice(&section_size.to_le_bytes());
    for (idx, ((id, _value), value_offset)) in properties.iter().zip(value_offsets).enumerate() {
        let entry = table_start + idx * 8;
        section[entry..entry + 4].copy_from_slice(&id.to_le_bytes());
        section[entry + 4..entry + 8].copy_from_slice(&value_offset.to_le_bytes());
    }

    let mut stream = Vec::new();
    stream.extend_from_slice(&0xFFFEu16.to_le_bytes()); // little endian property set
    stream.extend_from_slice(&0u16.to_le_bytes()); // version
    stream.extend_from_slice(&0u32.to_le_bytes()); // system identifier
    stream.extend_from_slice(&[0u8; 16]); // CLSID
    stream.extend_from_slice(&1u32.to_le_bytes()); // one property set
    stream.extend_from_slice(&fmtid);
    stream.extend_from_slice(&section_offset.to_le_bytes());
    stream.extend_from_slice(&section);
    stream
}

#[test]
fn rk_decoding() {
    // integer 12, not /100: rk = (12 << 2) | 0x02
    assert_eq!(rk_to_f64((12i32 << 2) as u32 | 0x02), 12.0);
    // integer 250 with /100 flag => 2.5
    assert_eq!(rk_to_f64((250i32 << 2) as u32 | 0x03), 2.5);
}

#[test]
fn number_formatting() {
    assert_eq!(format_number(10.0), "10");
    assert_eq!(format_number(2.5), "2.5");
}

#[test]
fn short_and_long_strings() {
    // ShortXLUnicodeString "Hi" compressed
    let mut d = vec![0u8; 6];
    d[5] = 0x00; // worksheet
    d.push(2); // cch
    d.push(0x00); // grbit compressed
    d.extend_from_slice(b"Hi");
    let (name, sheet_type, hidden, very_hidden) = parse_boundsheet(&d, ctx8());
    assert_eq!(name, "Hi");
    assert_eq!(sheet_type, SheetType::WorkSheet);
    assert!(!hidden);
    assert!(!very_hidden);
}

#[test]
fn boundsheet_hsstate_visibility() {
    // hsState (byte at offset 4) low 2 bits: 0 visible, 1 hidden, 2 veryHidden.
    let boundsheet = |hs_state: u8| {
        let mut d = vec![0u8; 6];
        d[4] = hs_state;
        d[5] = 0x00; // dt = worksheet
        d.push(2); // cch
        d.push(0x00); // grbit compressed
        d.extend_from_slice(b"S1");
        parse_boundsheet(&d, ctx8())
    };
    let (_, _, hidden, very_hidden) = boundsheet(0);
    assert!(!hidden && !very_hidden, "0 => visible");
    let (_, _, hidden, very_hidden) = boundsheet(1);
    assert!(hidden && !very_hidden, "1 => hidden");
    let (_, _, hidden, very_hidden) = boundsheet(2);
    assert!(!hidden && very_hidden, "2 => veryHidden");
}

#[test]
fn xls_hidden_sheet_end_to_end() {
    // A workbook with a visible "S1" and a hidden "S2" (BOUNDSHEET hsState=1)
    // must surface `is_hidden()` on the second sheet.
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    // BOUNDSHEET "S1" visible (hsState=0).
    let mut bs1 = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs1.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs1));
    // BOUNDSHEET "S2" hidden (hsState=1 at offset 4).
    let mut bs2 = vec![0, 0, 0, 0, 1, 0, 2, 0x00];
    bs2.extend_from_slice(b"S2");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs2));
    stream.extend_from_slice(&rec(EOF, &[]));
    // Two empty worksheet substreams (sheets map to top-level BOFs in order).
    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(EOF, &[]));
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    assert_eq!(wb.sheets.len(), 2);
    assert!(!wb.sheets[0].is_hidden(), "S1 visible");
    assert!(wb.sheets[1].is_hidden(), "S2 hidden");
    assert!(!wb.sheets[1].is_very_hidden());
}

#[test]
fn xls_boundsheet_preserves_sheet_types_end_to_end() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);

    let boundsheet = |name: &str, dt: u8, hs_state: u8| {
        let mut bs = vec![0, 0, 0, 0, hs_state, dt, name.len() as u8, 0x00];
        bs.extend_from_slice(name.as_bytes());
        rec(BOUNDSHEET, &bs)
    };
    stream.extend_from_slice(&boundsheet("Data", 0x00, 0));
    stream.extend_from_slice(&boundsheet("Macro", 0x01, 1));
    stream.extend_from_slice(&boundsheet("Chart", 0x02, 0));
    stream.extend_from_slice(&boundsheet("Vba", 0x06, 2));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    for _ in 0..4 {
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(EOF, &[]));
    }

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    assert_eq!(
        wb.sheets_metadata(),
        vec![
            SheetMetadata {
                name: "Data".to_string(),
                typ: SheetType::WorkSheet,
                visible: SheetVisible::Visible,
            },
            SheetMetadata {
                name: "Macro".to_string(),
                typ: SheetType::MacroSheet,
                visible: SheetVisible::Hidden,
            },
            SheetMetadata {
                name: "Chart".to_string(),
                typ: SheetType::ChartSheet,
                visible: SheetVisible::Visible,
            },
            SheetMetadata {
                name: "Vba".to_string(),
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
fn xls_window1_active_tab_surfaces_workbook_metadata() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);

    for name in ["Data", "Summary"] {
        let mut bs = vec![0, 0, 0, 0, 0, 0, name.len() as u8, 0x00];
        bs.extend_from_slice(name.as_bytes());
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    }

    let mut window1 = Vec::new();
    window1.extend_from_slice(&0i16.to_le_bytes()); // xWn
    window1.extend_from_slice(&0i16.to_le_bytes()); // yWn
    window1.extend_from_slice(&1i16.to_le_bytes()); // dxWn
    window1.extend_from_slice(&1i16.to_le_bytes()); // dyWn
    window1.extend_from_slice(&0u16.to_le_bytes()); // flags
    window1.extend_from_slice(&1u16.to_le_bytes()); // itabCur
    window1.extend_from_slice(&0u16.to_le_bytes()); // itabFirst
    window1.extend_from_slice(&1u16.to_le_bytes()); // ctabSel
    window1.extend_from_slice(&600u16.to_le_bytes()); // wTabRatio
    stream.extend_from_slice(&rec(WINDOW1, &window1));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    for _ in 0..2 {
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(EOF, &[]));
    }

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
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
fn xls_global_protect_record_surfaces_workbook_metadata() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);

    let mut bs = vec![0, 0, 0, 0, 0, 0, 4, 0x00];
    bs.extend_from_slice(b"Data");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(PROTECT, &1u16.to_le_bytes()));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

    assert!(wb.is_structure_protected());
    assert!(wb.metadata().structure_protected);
    assert!(<Workbook as crate::Reader>::metadata(&wb).structure_protected);
    assert!(
        !wb.sheet_by_name("Data").unwrap().is_protected(),
        "global Protect must not be treated as worksheet protection"
    );
}

#[test]
fn xls_selected_window2_falls_back_to_active_sheet_metadata() {
    const WINDOW2: u16 = 0x023E;

    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);

    for name in ["Data", "Summary"] {
        let mut bs = vec![0, 0, 0, 0, 0, 0, name.len() as u8, 0x00];
        bs.extend_from_slice(name.as_bytes());
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    }
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(EOF, &[]));

    stream.extend_from_slice(&rec(BOF, &s_bof));
    let mut window2 = (1u16 << 9).to_le_bytes().to_vec(); // fSelected
    window2.extend_from_slice(&0u16.to_le_bytes()); // top visible row
    window2.extend_from_slice(&0u16.to_le_bytes()); // left visible column
    window2.extend_from_slice(&0u32.to_le_bytes()); // header color index
    window2.extend_from_slice(&0u16.to_le_bytes()); // page-break preview zoom
    window2.extend_from_slice(&0u16.to_le_bytes()); // normal zoom
    stream.extend_from_slice(&rec(WINDOW2, &window2));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
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
fn xls_defined_name_is_read_from_lbl_record() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);

    // Lbl: visible, non-built-in, workbook-global name "Answer" whose
    // NameParsedFormula is PtgInt(42).
    let mut lbl = Vec::new();
    lbl.extend_from_slice(&0u16.to_le_bytes()); // flags
    lbl.push(0); // chKey
    lbl.push(6); // cch
    lbl.extend_from_slice(&3u16.to_le_bytes()); // cce
    lbl.extend_from_slice(&0u16.to_le_bytes()); // reserved3
    lbl.extend_from_slice(&0u16.to_le_bytes()); // itab: workbook global
    lbl.extend_from_slice(&[0, 0, 0, 0]); // reserved4..7
    lbl.push(0x00); // Name grbit: compressed
    lbl.extend_from_slice(b"Answer");
    lbl.extend_from_slice(&[0x1E, 42, 0]); // PtgInt(42)
    stream.extend_from_slice(&rec(0x0018, &lbl));

    let mut local_lbl = Vec::new();
    local_lbl.extend_from_slice(&0u16.to_le_bytes());
    local_lbl.push(0);
    local_lbl.push(4);
    local_lbl.extend_from_slice(&3u16.to_le_bytes());
    local_lbl.extend_from_slice(&0u16.to_le_bytes());
    local_lbl.extend_from_slice(&1u16.to_le_bytes()); // one-based sheet scope
    local_lbl.extend_from_slice(&[0, 0, 0, 0]);
    local_lbl.push(0x00);
    local_lbl.extend_from_slice(b"Rate");
    local_lbl.extend_from_slice(&[0x1E, 7, 0]);
    stream.extend_from_slice(&rec(0x0018, &local_lbl));

    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    let mut formula = vec![0, 0, 0, 0, 0, 0];
    formula.extend_from_slice(&42.0f64.to_le_bytes());
    formula.extend_from_slice(&[0, 0]);
    formula.extend_from_slice(&[0, 0, 0, 0]);
    let rgce = [0x23, 1, 0, 0, 0]; // PtgName, one-based Lbl index 1
    formula.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
    formula.extend_from_slice(&rgce);
    stream.extend_from_slice(&rec(FORMULA, &formula));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
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
fn xls_sheet_local_builtin_names_surface_filter_and_print_area() {
    fn builtin_name(id: u8, itab: u16, rgce: &[u8]) -> Vec<u8> {
        let mut lbl = Vec::new();
        lbl.extend_from_slice(&0x0020u16.to_le_bytes()); // fBuiltin
        lbl.push(0); // chKey
        lbl.push(1); // cch: one built-in id byte
        lbl.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
        lbl.extend_from_slice(&0u16.to_le_bytes()); // reserved3
        lbl.extend_from_slice(&itab.to_le_bytes()); // 1-based sheet scope
        lbl.extend_from_slice(&[0, 0, 0, 0]); // reserved4..7
        lbl.push(id);
        lbl.extend_from_slice(rgce);
        lbl
    }

    fn area3d(r0: u16, c0: u16, r1: u16, c1: u16) -> Vec<u8> {
        let mut rgce = vec![0x3B, 0, 0]; // PtgArea3d, ixti=0
        rgce.extend_from_slice(&r0.to_le_bytes());
        rgce.extend_from_slice(&r1.to_le_bytes());
        rgce.extend_from_slice(&c0.to_le_bytes());
        rgce.extend_from_slice(&c1.to_le_bytes());
        rgce
    }

    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(0x0018, &builtin_name(0x0D, 1, &area3d(0, 0, 4, 2))));
    let mut print_areas = area3d(1, 1, 5, 3);
    print_areas.extend_from_slice(&area3d(9, 4, 11, 6));
    print_areas.push(0x10); // PtgUnion
    stream.extend_from_slice(&rec(0x0018, &builtin_name(0x06, 1, &print_areas)));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

    assert_eq!(wb.sheets[0].autofilter_range(), Some((0, 0, 4, 2)));
    assert_eq!(
        wb.sheets[0].page_setup().and_then(|ps| ps.print_area),
        Some((1, 1, 5, 3))
    );
    assert_eq!(
        wb.sheets[0].print_metadata().print_areas(),
        &[(1, 1, 5, 3), (9, 4, 11, 6)]
    );
    assert!(wb.defined_names().is_empty());
}

#[test]
fn xls_sheet_local_print_titles_surface_repeat_rows_and_cols() {
    fn builtin_name(id: u8, itab: u16, rgce: &[u8]) -> Vec<u8> {
        let mut lbl = Vec::new();
        lbl.extend_from_slice(&0x0020u16.to_le_bytes()); // fBuiltin
        lbl.push(0); // chKey
        lbl.push(1); // cch: one built-in id byte
        lbl.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
        lbl.extend_from_slice(&0u16.to_le_bytes()); // reserved3
        lbl.extend_from_slice(&itab.to_le_bytes()); // 1-based sheet scope
        lbl.extend_from_slice(&[0, 0, 0, 0]); // reserved4..7
        lbl.push(id);
        lbl.extend_from_slice(rgce);
        lbl
    }

    fn area3d(r0: u16, c0: u16, r1: u16, c1: u16) -> Vec<u8> {
        let mut rgce = vec![0x3B, 0, 0]; // PtgArea3d, ixti=0
        rgce.extend_from_slice(&r0.to_le_bytes());
        rgce.extend_from_slice(&r1.to_le_bytes());
        rgce.extend_from_slice(&c0.to_le_bytes());
        rgce.extend_from_slice(&c1.to_le_bytes());
        rgce
    }

    let mut print_titles = area3d(0, 0, 1, 255);
    print_titles.extend_from_slice(&area3d(0, 0, u16::MAX, 2));
    print_titles.push(0x10); // PtgUnion

    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(0x0018, &builtin_name(0x07, 1, &print_titles)));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let page_setup = wb.sheets[0].page_setup().expect("page setup");

    assert_eq!(page_setup.repeat_rows, Some((0, 1)));
    assert_eq!(page_setup.repeat_cols, Some((0, 2)));
    assert!(wb.defined_names().is_empty());
}

#[test]
fn xls_page_setup_records_surface_public_metadata() {
    fn xl_string(value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        out.push(0x00); // compressed BIFF8 string
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn margin(value: f64) -> Vec<u8> {
        value.to_le_bytes().to_vec()
    }

    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(0x0014, &xl_string("&CQuarterly report")));
    stream.extend_from_slice(&rec(0x0015, &xl_string("&CPage &P")));
    stream.extend_from_slice(&rec(0x0026, &margin(0.5)));
    stream.extend_from_slice(&rec(0x0027, &margin(0.6)));
    stream.extend_from_slice(&rec(0x0028, &margin(0.7)));
    stream.extend_from_slice(&rec(0x0029, &margin(0.8)));
    stream.extend_from_slice(&rec(0x002A, &1u16.to_le_bytes()));
    stream.extend_from_slice(&rec(0x002B, &1u16.to_le_bytes()));
    stream.extend_from_slice(&rec(0x0083, &1u16.to_le_bytes()));
    stream.extend_from_slice(&rec(0x0084, &1u16.to_le_bytes()));

    let mut row_breaks = 2u16.to_le_bytes().to_vec();
    for row in [10u16, 4u16] {
        row_breaks.extend_from_slice(&row.to_le_bytes());
        row_breaks.extend_from_slice(&0u16.to_le_bytes());
        row_breaks.extend_from_slice(&255u16.to_le_bytes());
    }
    stream.extend_from_slice(&rec(HORIZONTALPAGEBREAKS, &row_breaks));
    let mut col_breaks = 2u16.to_le_bytes().to_vec();
    for col in [7u16, 2u16] {
        col_breaks.extend_from_slice(&col.to_le_bytes());
        col_breaks.extend_from_slice(&0u16.to_le_bytes());
        col_breaks.extend_from_slice(&u16::MAX.to_le_bytes());
    }
    stream.extend_from_slice(&rec(VERTICALPAGEBREAKS, &col_breaks));

    let extended_strings = ["&LEven", "&REvenF", "&LFirst", "&RFirstF"];
    let mut header_footer = vec![0u8; 38];
    header_footer[0..2].copy_from_slice(&HEADERFOOTER.to_le_bytes());
    header_footer[28..30].copy_from_slice(&0x000Fu16.to_le_bytes());
    for (index, text) in extended_strings.iter().enumerate() {
        let offset = 30 + index * 2;
        header_footer[offset..offset + 2].copy_from_slice(&(text.len() as u16).to_le_bytes());
        header_footer.extend_from_slice(&xl_string(text));
    }
    stream.extend_from_slice(&rec(HEADERFOOTER, &header_footer));

    let mut setup = Vec::new();
    setup.extend_from_slice(&9u16.to_le_bytes()); // A4
    setup.extend_from_slice(&80u16.to_le_bytes()); // 80%
    setup.extend_from_slice(&3i16.to_le_bytes()); // first page number
    setup.extend_from_slice(&1u16.to_le_bytes()); // fit width
    setup.extend_from_slice(&2u16.to_le_bytes()); // fit height
    setup.extend_from_slice(&0x0081u16.to_le_bytes()); // fUsePage + left-to-right
    setup.extend_from_slice(&300u16.to_le_bytes()); // horizontal DPI
    setup.extend_from_slice(&300u16.to_le_bytes()); // vertical DPI
    setup.extend_from_slice(&0.2f64.to_le_bytes()); // header margin
    setup.extend_from_slice(&0.25f64.to_le_bytes()); // footer margin
    setup.extend_from_slice(&1u16.to_le_bytes()); // copies
    stream.extend_from_slice(&rec(0x00A1, &setup));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let ps = wb.sheets[0].page_setup().expect("page setup");

    assert!(ps.landscape);
    assert_eq!(ps.paper_size, Some(9));
    assert_eq!(ps.scale, Some(80));
    assert_eq!(ps.first_page_number, Some(3));
    assert_eq!(ps.fit_to_width, Some(1));
    assert_eq!(ps.fit_to_height, Some(2));
    assert_eq!(ps.header.as_deref(), Some("&CQuarterly report"));
    assert_eq!(ps.footer.as_deref(), Some("&CPage &P"));
    assert!(ps.center_horizontally);
    assert!(ps.center_vertically);
    assert!(wb.sheets[0].print_headings());
    assert!(wb.sheets[0].print_gridlines());
    assert_eq!(ps.margins, Some((0.5, 0.6, 0.7, 0.8, 0.2, 0.25)));
    let metadata = wb.sheets[0].print_metadata();
    assert_eq!(metadata.fidelity(), crate::PrintFidelity::Retained);
    assert_eq!(metadata.manual_row_breaks(), &[4, 10]);
    assert_eq!(metadata.manual_col_breaks(), &[2, 7]);
    assert_eq!(metadata.page_order(), Some(PrintPageOrder::OverThenDown));
    assert_eq!(metadata.print_headings(), Some(true));
    assert_eq!(metadata.print_gridlines(), Some(true));
    assert_eq!(metadata.center_horizontally(), Some(true));
    assert_eq!(metadata.center_vertically(), Some(true));
    assert_eq!(
        metadata.header_footer().odd_header(),
        Some("&CQuarterly report")
    );
    assert_eq!(metadata.header_footer().odd_footer(), Some("&CPage &P"));
    assert_eq!(metadata.header_footer().even_header(), Some("&LEven"));
    assert_eq!(metadata.header_footer().even_footer(), Some("&REvenF"));
    assert_eq!(metadata.header_footer().first_header(), Some("&LFirst"));
    assert_eq!(metadata.header_footer().first_footer(), Some("&RFirstF"));
    assert_eq!(metadata.header_footer().different_odd_even(), Some(true));
    assert_eq!(metadata.header_footer().different_first(), Some(true));
}

#[test]
fn xls_wsbool_controls_fit_mode_and_preserves_active_zero_dimension() {
    fn workbook(
        wsbool_flags: u16,
        fit_width: u16,
        fit_height: u16,
        wsbool_before_setup: bool,
    ) -> Workbook {
        let mut global_bof = vec![0x00, 0x06, 0x05, 0x00];
        global_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &global_bof);
        let mut bound_sheet = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bound_sheet.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bound_sheet));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
        sheet_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &sheet_bof));

        let mut setup = Vec::new();
        setup.extend_from_slice(&9u16.to_le_bytes());
        setup.extend_from_slice(&80u16.to_le_bytes());
        setup.extend_from_slice(&1i16.to_le_bytes());
        setup.extend_from_slice(&fit_width.to_le_bytes());
        setup.extend_from_slice(&fit_height.to_le_bytes());
        setup.extend_from_slice(&0u16.to_le_bytes());
        setup.extend_from_slice(&300u16.to_le_bytes());
        setup.extend_from_slice(&300u16.to_le_bytes());
        setup.extend_from_slice(&0.2f64.to_le_bytes());
        setup.extend_from_slice(&0.25f64.to_le_bytes());
        setup.extend_from_slice(&1u16.to_le_bytes());

        if wsbool_before_setup {
            stream.extend_from_slice(&rec(WSBOOL, &wsbool_flags.to_le_bytes()));
        }
        stream.extend_from_slice(&rec(SETUP, &setup));
        if !wsbool_before_setup {
            stream.extend_from_slice(&rec(WSBOOL, &wsbool_flags.to_le_bytes()));
        }
        stream.extend_from_slice(&rec(EOF, &[]));

        Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap()
    }

    let fixed = workbook(0x0000, 1, 2, true);
    let fixed_setup = fixed.sheets[0].page_setup().expect("fixed setup");
    assert_eq!(fixed.sheets[0].print_metadata().fit_to_page(), Some(false));
    assert_eq!(fixed_setup.scale, Some(80));
    assert_eq!(fixed_setup.fit_to_width, Some(1));
    assert_eq!(fixed_setup.fit_to_height, Some(2));

    let fit = workbook(0x0100, 1, 0, false);
    let fit_setup = fit.sheets[0].page_setup().expect("fit setup");
    assert_eq!(fit.sheets[0].print_metadata().fit_to_page(), Some(true));
    assert_eq!(fit_setup.scale, Some(80));
    assert_eq!(fit_setup.fit_to_width, Some(1));
    assert_eq!(fit_setup.fit_to_height, Some(0));

    #[cfg(feature = "xlsx")]
    for (label, source, expected_mode, expected_width, expected_height) in [
        (
            "fixed scale with stale fit counts",
            &fixed,
            false,
            Some(1),
            Some(2),
        ),
        (
            "active one by unconstrained fit",
            &fit,
            true,
            Some(1),
            Some(0),
        ),
    ] {
        let output = source.to_xlsx();
        let reopened = Workbook::open(&output).expect(label);
        let sheet = &reopened.sheets[0];
        assert_eq!(
            sheet.print_metadata().fit_to_page(),
            Some(expected_mode),
            "{label}"
        );
        let setup = sheet.page_setup().expect(label);
        assert_eq!(setup.scale, Some(80), "{label}");
        assert_eq!(setup.fit_to_width, expected_width, "{label}");
        assert_eq!(setup.fit_to_height, expected_height, "{label}");
    }
}

#[test]
fn malformed_biff_print_records_report_typed_losses() {
    let mut setup = XlsPageSetup::default();
    setup.apply_record(HORIZONTALPAGEBREAKS, &[2, 0, 4, 0], ctx8());
    setup.apply_record(HEADERFOOTER, &[0; 12], ctx8());

    assert_eq!(
        setup.print_metadata.fidelity(),
        crate::PrintFidelity::Partial
    );
    assert!(setup
        .print_metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::InvalidPageBreak));
    assert!(setup
        .print_metadata
        .losses()
        .iter()
        .any(|loss| loss.kind == PrintLossKind::MalformedHeaderFooter));
}

#[test]
fn xls_outline_records_surface_public_metadata() {
    fn row_record(row: u16, options: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&row.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // first described column
        out.extend_from_slice(&1u16.to_le_bytes()); // last described column + 1
        out.extend_from_slice(&400u16.to_le_bytes()); // manually assigned 20 pt height
        out.extend_from_slice(&0u16.to_le_bytes()); // unused
        out.extend_from_slice(&0u16.to_le_bytes()); // unused in BIFF5+
        out.extend_from_slice(&options.to_le_bytes());
        out
    }

    fn col_info(first: u16, last: u16, options: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&first.to_le_bytes());
        out.extend_from_slice(&last.to_le_bytes());
        out.extend_from_slice(&0x08FFu16.to_le_bytes()); // default width
        out.extend_from_slice(&0u16.to_le_bytes()); // default XF
        out.extend_from_slice(&options.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // unused
        out
    }

    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(0x0081, &0u16.to_le_bytes())); // summaries above/left
    stream.extend_from_slice(&rec(
        0x0208,
        &row_record(2, 2 | (1 << 4) | (1 << 5) | BIFF_ROW_FLAG_UNSYNCED),
    ));
    stream.extend_from_slice(&rec(0x0208, &row_record(3, 2)));
    stream.extend_from_slice(&rec(0x007D, &col_info(1, 3, (3 << 8) | 1)));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let sheet = &wb.sheets[0];

    assert_eq!(sheet.row_outline_levels().get(&2), Some(&2));
    assert_eq!(sheet.row_outline_levels().get(&3), Some(&2));
    assert!(sheet.collapsed_rows().contains(&2));
    assert_eq!(sheet.col_outline_levels().get(&1), Some(&3));
    assert_eq!(sheet.col_outline_levels().get(&3), Some(&3));
    assert_eq!(sheet.row_heights().get(&2), Some(&20.0));
    assert!(sheet.hidden_rows().contains(&2));
    assert_eq!(
        sheet.column_widths().get(&1),
        Some(&(0x08FF as f32 / 256.0))
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
fn xls_protect_record_surfaces_public_metadata() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);

    for name in ["Protected", "Plain"] {
        let mut bs = vec![0, 0, 0, 0, 0, 0, name.len() as u8, 0x00];
        bs.extend_from_slice(name.as_bytes());
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    }
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(PROTECT, &1u16.to_le_bytes()));
    stream.extend_from_slice(&rec(EOF, &[]));
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let protected = wb.sheet_by_name("Protected").unwrap();
    let plain = wb.sheet_by_name("Plain").unwrap();

    assert!(protected.is_protected());
    assert_eq!(protected.protection_options(), None);
    assert!(!plain.is_protected());

    let metadata = wb.worksheet_metadata("Protected").unwrap();
    assert!(metadata.protected);
    assert_eq!(metadata.protection_options, None);

    let generic_metadata =
        <Workbook as crate::Reader>::worksheet_metadata(&wb, "Protected").unwrap();
    assert!(generic_metadata.protected);
    assert_eq!(generic_metadata.protection_options, None);
}

#[test]
fn xls_sheet_ext_tab_color_surfaces_public_metadata() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));

    let mut sheet_ext = Vec::new();
    sheet_ext.extend_from_slice(&0x0862u16.to_le_bytes()); // FrtHeader.rt = SheetExt
    sheet_ext.extend_from_slice(&0u16.to_le_bytes()); // FrtHeader.grbitFrt
    sheet_ext.extend_from_slice(&[0u8; 8]); // FrtHeader reserved fields
    sheet_ext.extend_from_slice(&0x14u32.to_le_bytes()); // record size without optional tail
    sheet_ext.extend_from_slice(&0x0Au32.to_le_bytes()); // icvPlain: indexed red
    stream.extend_from_slice(&rec(0x0862, &sheet_ext));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

    assert_eq!(
        wb.sheets[0].tab_color(),
        Some(crate::Color::rgb(0xFF, 0, 0))
    );
}

#[test]
fn xls_sheet_ext_tab_color_respects_custom_palette_record() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);

    let mut palette = 56u16.to_le_bytes().to_vec();
    for (idx, color) in BIFF_DEFAULT_PALETTE.iter().enumerate() {
        let rgb = if idx == 2 {
            [0x12, 0x34, 0x56]
        } else {
            color.as_rgb()
        };
        palette.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 0]);
    }
    stream.extend_from_slice(&rec(0x0092, &palette));

    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));

    let mut sheet_ext = Vec::new();
    sheet_ext.extend_from_slice(&0x0862u16.to_le_bytes());
    sheet_ext.extend_from_slice(&0u16.to_le_bytes());
    sheet_ext.extend_from_slice(&[0u8; 8]);
    sheet_ext.extend_from_slice(&0x14u32.to_le_bytes());
    sheet_ext.extend_from_slice(&0x0Au32.to_le_bytes());
    stream.extend_from_slice(&rec(0x0862, &sheet_ext));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

    assert_eq!(
        wb.sheets[0].tab_color(),
        Some(crate::Color::rgb(0x12, 0x34, 0x56))
    );
}

#[test]
fn xls_data_validation_records_surface_public_metadata() {
    fn xl_unicode(value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        out.push(0x00); // compressed BIFF8 string
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn dv_formula_string(value: &str) -> Vec<u8> {
        let mut rgce = Vec::new();
        rgce.push(0x17); // PtgStr
        rgce.push(value.len() as u8);
        rgce.push(0x00); // compressed
        rgce.extend_from_slice(value.as_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // unused
        out.extend_from_slice(&rgce);
        out
    }

    fn empty_dv_formula() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));

    let mut dval = Vec::new();
    dval.extend_from_slice(&0u16.to_le_bytes()); // DVal flags
    dval.extend_from_slice(&0u32.to_le_bytes()); // xLeft
    dval.extend_from_slice(&0u32.to_le_bytes()); // yTop
    dval.extend_from_slice(&(-1i32).to_le_bytes()); // idObj: none
    dval.extend_from_slice(&1u32.to_le_bytes()); // idvMac
    stream.extend_from_slice(&rec(0x01B2, &dval));

    let mut dv = Vec::new();
    let flags = 3u32 // valType=list
            | (1u32 << 7) // fStrLookup
            | (1u32 << 8) // fAllowBlank
            | (1u32 << 18) // fShowInputMsg
            | (1u32 << 19); // fShowErrorMsg
    dv.extend_from_slice(&flags.to_le_bytes());
    dv.extend_from_slice(&xl_unicode("Pick"));
    dv.extend_from_slice(&xl_unicode("Invalid"));
    dv.extend_from_slice(&xl_unicode("Choose one"));
    dv.extend_from_slice(&xl_unicode("Use the list"));
    dv.extend_from_slice(&dv_formula_string("Yes,No"));
    dv.extend_from_slice(&empty_dv_formula());
    dv.extend_from_slice(&2u16.to_le_bytes()); // SqRefU.cref
    for value in [1u16, 3, 0, 0, 5, 5, 2, 4] {
        dv.extend_from_slice(&value.to_le_bytes()); // A2:A4, C6:E6
    }
    stream.extend_from_slice(&rec(0x01BE, &dv));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let validations = wb.sheets[0].data_validations();

    assert_eq!(validations.len(), 2);
    assert_eq!(validations[0].sqref, (1, 0, 3, 0));
    assert_eq!(validations[1].sqref, (5, 2, 5, 4));
    assert_eq!(validations[0].kind, crate::DvKind::List);
    assert_eq!(validations[0].operator, crate::DvOp::Between);
    assert_eq!(validations[0].formula1, "\"Yes,No\"");
    assert!(validations[0].allow_blank);
    assert!(validations[0].show_input_message);
    assert!(validations[0].show_error_message);
    assert_eq!(
        validations[0].prompt.as_ref(),
        Some(&("Pick".to_string(), "Choose one".to_string()))
    );
    assert_eq!(
        validations[0].error.as_ref(),
        Some(&("Invalid".to_string(), "Use the list".to_string()))
    );
}

#[test]
fn xls_note_records_surface_public_comments() {
    fn xl_unicode(value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        out.push(0x00); // compressed BIFF8 string
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn txo(text: &str) -> Vec<Vec<u8>> {
        let mut record = Vec::new();
        record.extend_from_slice(&0u16.to_le_bytes()); // alignment/flags
        record.extend_from_slice(&0u16.to_le_bytes()); // rot
        record.extend_from_slice(&0u16.to_le_bytes()); // reserved4
        record.extend_from_slice(&0u32.to_le_bytes()); // reserved5
        record.extend_from_slice(&(text.len() as u16).to_le_bytes()); // cchText
        record.extend_from_slice(&16u16.to_le_bytes()); // cbRuns
        record.extend_from_slice(&0u16.to_le_bytes()); // ifntEmpty
        record.extend_from_slice(&0u16.to_le_bytes()); // empty ObjFmla.cce

        let mut text_continue = vec![0x00]; // compressed XLUnicodeStringNoCch
        text_continue.extend_from_slice(text.as_bytes());

        let mut run_continue = Vec::new();
        run_continue.extend_from_slice(&0u16.to_le_bytes()); // first run starts at 0
        run_continue.extend_from_slice(&0u16.to_le_bytes()); // ifnt
        run_continue.extend_from_slice(&0u32.to_le_bytes()); // reserved
        run_continue.extend_from_slice(&(text.len() as u16).to_le_bytes()); // last run
        run_continue.extend_from_slice(&0u16.to_le_bytes()); // ifnt
        run_continue.extend_from_slice(&0u32.to_le_bytes()); // reserved

        vec![
            rec(0x01B5, &record),
            rec(0x003C, &text_continue),
            rec(0x003C, &run_continue),
        ]
    }

    fn note_obj(id_obj: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x0015u16.to_le_bytes()); // FtCmo.ft
        out.extend_from_slice(&0x0012u16.to_le_bytes()); // FtCmo.cb
        out.extend_from_slice(&0x0019u16.to_le_bytes()); // FtCmo.ot = Note
        out.extend_from_slice(&id_obj.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // common object flags
        out.extend_from_slice(&[0u8; 12]); // FtCmo unused fields
        out.extend_from_slice(&0x000Du16.to_le_bytes()); // FtNts.ft
        out.extend_from_slice(&0x0016u16.to_le_bytes()); // FtNts.cb
        out.extend_from_slice(&[0u8; 16]); // guid
        out.extend_from_slice(&0u16.to_le_bytes()); // fSharedNote
        out.extend_from_slice(&0u32.to_le_bytes()); // unused
        out.extend_from_slice(&0u16.to_le_bytes()); // FtEnd.ft
        out.extend_from_slice(&0u16.to_le_bytes()); // FtEnd.cb
        out
    }

    fn note(row: u16, col: u16, id_obj: u16, author: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&row.to_le_bytes());
        out.extend_from_slice(&col.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // hidden unless hovered
        out.extend_from_slice(&id_obj.to_le_bytes());
        out.extend_from_slice(&xl_unicode(author));
        out.push(0); // unused2
        out
    }

    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(0x005D, &note_obj(1025)));
    for part in txo("Check source total") {
        stream.extend_from_slice(&part);
    }
    stream.extend_from_slice(&rec(0x001C, &note(2, 1, 1025, "Auditor")));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let comments = wb.sheets[0].comments();

    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].row, 2);
    assert_eq!(comments[0].col, 1);
    assert_eq!(comments[0].text, "Check source total");
    assert_eq!(comments[0].author.as_deref(), Some("Auditor"));
}

#[test]
fn xls_doc_properties_surface_through_workbook_metadata() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&rec(EOF, &[]));

    let summary = property_set_stream_values(
        [
            0xE0, 0x85, 0x9F, 0xF2, 0xF9, 0x4F, 0x68, 0x10, 0xAB, 0x91, 0x08, 0x00, 0x2B, 0x27,
            0xB3, 0xD9,
        ],
        &[
            (2, TestPropertyValue::Lpstr("Legacy Report")),
            (3, TestPropertyValue::Lpstr("Operations")),
            (4, TestPropertyValue::Lpstr("rxls reader")),
            (5, TestPropertyValue::Lpstr("ops,legacy")),
            (6, TestPropertyValue::Lpstr("XLS public metadata")),
            (8, TestPropertyValue::Lpstr("reviewer")),
            (12, TestPropertyValue::Filetime(0x01DD_0375_1176_3780)),
        ],
    );
    let doc_summary = property_set_stream(
        [
            0x02, 0xD5, 0xCD, 0xD5, 0x9C, 0x2E, 0x1B, 0x10, 0x93, 0x97, 0x08, 0x00, 0x2B, 0x2C,
            0xF9, 0xAE,
        ],
        &[(15, "ACME")],
    );

    let wb = Workbook::open(&wrap_xls_with_extra_streams(
        &stream,
        "/Workbook",
        &[
            ("/\u{0005}SummaryInformation", summary),
            ("/\u{0005}DocumentSummaryInformation", doc_summary),
        ],
    ))
    .unwrap();
    let metadata = wb.metadata();

    assert_eq!(metadata.properties.title.as_deref(), Some("Legacy Report"));
    assert_eq!(metadata.properties.subject.as_deref(), Some("Operations"));
    assert_eq!(metadata.properties.creator.as_deref(), Some("rxls reader"));
    assert_eq!(metadata.properties.keywords.as_deref(), Some("ops,legacy"));
    assert_eq!(
        metadata.properties.description.as_deref(),
        Some("XLS public metadata")
    );
    assert_eq!(
        metadata.properties.last_modified_by.as_deref(),
        Some("reviewer")
    );
    assert_eq!(
        metadata.properties.created.as_deref(),
        Some("2026-06-24T01:02:03Z")
    );
    assert_eq!(metadata.properties.company.as_deref(), Some("ACME"));
    assert_eq!(metadata.sheets[0].name, "S1");
}

#[test]
fn biff5_label_decodes_cp949() {
    // BIFF5 string: u16 byte-length, then raw codepage bytes (no grbit).
    let (kr, _, _) = EUC_KR.encode("한글");
    let mut data = (kr.len() as u16).to_le_bytes().to_vec();
    data.extend_from_slice(&kr);
    let ctx5 = Ctx {
        biff8: false,
        enc: EUC_KR,
    };
    assert_eq!(read_xl_string(&data, 0, ctx5).as_deref(), Some("한글"));
    // The same bytes under cp1252 would be mojibake (not "한글").
    let ctx_western = Ctx {
        biff8: false,
        enc: WINDOWS_1252,
    };
    assert_ne!(
        read_xl_string(&data, 0, ctx_western).as_deref(),
        Some("한글")
    );
}

#[test]
fn rstring_cell_is_decoded_like_label() {
    // RSTRING: row,col,ixfe, XLUnicodeString "Hi" (compressed), + run table.
    let mut data = vec![0u8; 6]; // row=0,col=0,ixfe=0
    data.extend_from_slice(&2u16.to_le_bytes()); // cch
    data.push(0x08); // grbit compressed + rich runs
    data.extend_from_slice(&2u16.to_le_bytes()); // cRun
    data.extend_from_slice(b"Hi");
    data.extend_from_slice(&0u16.to_le_bytes()); // ich
    data.extend_from_slice(&1u16.to_le_bytes()); // ifnt
    data.extend_from_slice(&1u16.to_le_bytes()); // ich
    data.extend_from_slice(&2u16.to_le_bytes()); // ifnt
    let mut cells = Vec::new();
    let mut rich = BTreeMap::new();
    let mut lf = None;
    let mut budget = MAX_TEXT_BYTES;
    let formats = Formats::default();
    let styles = XlsStyles::default();
    let mut style_budget = MAX_XLS_RETAINED_STYLE_BYTES;
    decode_string_cell(
        RSTRING,
        &[&data],
        0,
        &mut cells,
        &mut rich,
        &mut lf,
        ctx8(),
        &mut budget,
        &formats,
        &styles,
        &mut style_budget,
    );
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].value, Cell::Text("Hi".to_string()));
    assert_eq!(cells[0].text, "Hi");
    assert_eq!(
        rich[&(0, 0)]
            .iter()
            .map(|run| run.text.as_str())
            .collect::<Vec<_>>(),
        ["H", "i"]
    );
}

#[test]
fn label_spanning_continue_is_reassembled() {
    // A LABEL whose characters overflow the record cap continues into a
    // CONTINUE record; the bytes after the split must be reassembled, not
    // truncated. BIFF8 re-reads the compression flag at each chunk.
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));

    // Compressed LABEL at (0,0): cch=10 ("HELLOWORLD") split 5 / 5.
    let mut lbl = vec![0, 0, 0, 0, 0, 0];
    lbl.extend_from_slice(&10u16.to_le_bytes());
    lbl.push(0x00); // grbit compressed
    lbl.extend_from_slice(b"HELLO");
    stream.extend_from_slice(&rec(LABEL, &lbl));
    let mut cont = vec![0x00u8]; // continuation grbit, still compressed
    cont.extend_from_slice(b"WORLD");
    stream.extend_from_slice(&rec(CONTINUE, &cont));

    // Uncompressed LABEL at (1,0): cch=4 ("업무보고") split 2 / 2 chars.
    let kr: Vec<u16> = "업무보고".encode_utf16().collect();
    let mut lbl2 = vec![1, 0, 0, 0, 0, 0];
    lbl2.extend_from_slice(&(kr.len() as u16).to_le_bytes());
    lbl2.push(0x01); // grbit uncompressed (UTF-16LE)
    for u in &kr[..2] {
        lbl2.extend_from_slice(&u.to_le_bytes());
    }
    stream.extend_from_slice(&rec(LABEL, &lbl2));
    let mut cont2 = vec![0x01u8]; // continuation grbit, still uncompressed
    for u in &kr[2..] {
        cont2.extend_from_slice(&u.to_le_bytes());
    }
    stream.extend_from_slice(&rec(CONTINUE, &cont2));

    stream.extend_from_slice(&rec(EOF, &[]));

    let text = extract_text(&wrap_xls(&stream, "/Workbook")).unwrap();
    assert!(
        text.contains("HELLOWORLD"),
        "compressed split truncated: {text:?}"
    );
    assert!(
        text.contains("업무보고"),
        "uncompressed split truncated: {text:?}"
    );
}

#[test]
fn xls_merged_ranges_are_read() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));
    // sheet: BOF, MERGECELLS(1× Ref8U {rwFirst=0,rwLast=1,colFirst=0,colLast=2}), EOF
    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    let mut mc = 1u16.to_le_bytes().to_vec(); // count
    for v in [0u16, 1, 0, 2] {
        mc.extend_from_slice(&v.to_le_bytes());
    }
    stream.extend_from_slice(&rec(MERGECELLS, &mc));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    // (first_row, first_col, last_row, last_col) = A1:C2.
    assert_eq!(wb.sheets[0].merged_ranges(), &[(0, 0, 1, 2)]);
}

#[test]
fn xls_hlink_record_surfaces_public_hyperlinks() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    let url = "https://example.com/report";
    let mut hlink = Vec::new();
    for v in [0u16, 1, 1, 1] {
        hlink.extend_from_slice(&v.to_le_bytes());
    }
    hlink.extend_from_slice(&[0u8; 16]); // StdLink GUID placeholder.
    hlink.extend_from_slice(&1u32.to_le_bytes()); // link options placeholder.
    hlink.extend_from_slice(&[0u8; 16]); // URL moniker GUID placeholder.
    hlink.extend_from_slice(&((url.encode_utf16().count() + 1) as u32).to_le_bytes());
    for ch in url.encode_utf16() {
        hlink.extend_from_slice(&ch.to_le_bytes());
    }
    hlink.extend_from_slice(&0u16.to_le_bytes());
    stream.extend_from_slice(&rec(0x01B8, &hlink));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

    assert_eq!(
        wb.sheets[0].hyperlinks(),
        &[(0, 1, url.to_string()), (1, 1, url.to_string()),]
    );
}

#[test]
fn xls_window2_and_pane_surface_sheet_view_metadata() {
    const PANE: u16 = 0x0041;
    const WINDOW2: u16 = 0x023E;

    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    let window2_flags = (1u16 << 3) // frozen panes
            | (1u16 << 6); // right-to-left view; gridlines/headers bits remain unset
    let mut window2 = window2_flags.to_le_bytes().to_vec();
    window2.extend_from_slice(&0u16.to_le_bytes()); // top visible row
    window2.extend_from_slice(&0u16.to_le_bytes()); // left visible column
    window2.extend_from_slice(&0u32.to_le_bytes()); // header color index
    window2.extend_from_slice(&0u16.to_le_bytes()); // page-break preview zoom
    window2.extend_from_slice(&0u16.to_le_bytes()); // normal zoom
    stream.extend_from_slice(&rec(WINDOW2, &window2));
    let mut pane = Vec::new();
    for value in [2u16, 1, 1, 2, 0] {
        pane.extend_from_slice(&value.to_le_bytes()); // freeze at C2
    }
    stream.extend_from_slice(&rec(PANE, &pane));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

    assert_eq!(
        wb.sheets[0].sheet_view(),
        crate::SheetView {
            freeze: Some((1, 2)),
            hide_gridlines: true,
            zoom: None,
            show_headers: Some(false),
            right_to_left: true,
        }
    );
}

#[test]
fn xls_window2_explicit_visible_headers_are_preserved() {
    const WINDOW2: u16 = 0x023E;

    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    let window2_flags = (1u16 << 1) // display gridlines
            | (1u16 << 2); // display row/column headings
    let mut window2 = window2_flags.to_le_bytes().to_vec();
    window2.extend_from_slice(&0u16.to_le_bytes()); // top visible row
    window2.extend_from_slice(&0u16.to_le_bytes()); // left visible column
    window2.extend_from_slice(&0u32.to_le_bytes()); // header color index
    window2.extend_from_slice(&0u16.to_le_bytes()); // page-break preview zoom
    window2.extend_from_slice(&0u16.to_le_bytes()); // normal zoom
    stream.extend_from_slice(&rec(WINDOW2, &window2));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

    assert_eq!(wb.sheets[0].sheet_view().show_headers, Some(true));
}

#[test]
fn typed_cells_expose_value_kinds() {
    // globals: BOF, XF(ifmt=14 date), BOUNDSHEET "S1", EOF
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut xf = vec![0u8; 20];
    xf[2] = 14; // date format
    stream.extend_from_slice(&rec(XF, &xf));
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));
    // sheet: BOF, NUMBER(r0c0,ixfe0=date,45366), NUMBER(r0c1,ixfe?plain,12),
    //        BOOLERR(r1c0, TRUE), EOF
    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    let mut num = vec![0, 0, 0, 0, 0, 0];
    num.extend_from_slice(&45366.0f64.to_le_bytes());
    stream.extend_from_slice(&rec(NUMBER, &num));
    let mut num2 = vec![0, 0, 1, 0, 1, 0]; // r0c1, ixfe=1 (no XF[1] -> plain)
    num2.extend_from_slice(&12.0f64.to_le_bytes());
    stream.extend_from_slice(&rec(NUMBER, &num2));
    stream.extend_from_slice(&rec(BOOLERR, &[1, 0, 0, 0, 0, 0, 1, 0])); // r1c0 TRUE
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let sheet = &wb.sheets[0];
    // Date keeps the raw serial; to_text renders the ISO string.
    assert_eq!(sheet.cell(0, 0), Some(&Cell::Date(45366.0)));
    assert!(sheet.to_text().contains("2024-03-15"));
    assert_eq!(sheet.cell(0, 1), Some(&Cell::Number(12.0)));
    assert_eq!(sheet.cell(1, 0), Some(&Cell::Bool(true)));
    assert_eq!(sheet.dimensions(), Some((0, 0, 1, 1)));
    assert_eq!(sheet.cells().count(), 3);
}

#[test]
fn biff8_custom_number_format_drives_display_text() {
    let mut global_bof = vec![0x00, 0x06, 0x05, 0x00];
    global_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &global_bof);

    let code = "[$₩-412]#,##0.00;[Red](#,##0.00);0;\"값: \"@";
    let encoded: Vec<u16> = code.encode_utf16().collect();
    let mut format = 164u16.to_le_bytes().to_vec();
    format.extend_from_slice(&(encoded.len() as u16).to_le_bytes());
    format.push(1); // uncompressed UTF-16
    for unit in encoded {
        format.extend_from_slice(&unit.to_le_bytes());
    }
    stream.extend_from_slice(&rec(FORMAT, &format));

    let mut xf = vec![0u8; 20];
    xf[2..4].copy_from_slice(&164u16.to_le_bytes());
    stream.extend_from_slice(&rec(XF, &xf));
    let mut boundsheet = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    boundsheet.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
    sheet_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &sheet_bof));
    let mut number = vec![0, 0, 0, 0, 0, 0];
    number.extend_from_slice(&1_234.5f64.to_le_bytes());
    stream.extend_from_slice(&rec(NUMBER, &number));
    stream.extend_from_slice(&rec(LABEL, &label8(1, 0, 0, "abc")));
    stream.extend_from_slice(&rec(EOF, &[]));

    let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    assert_eq!(workbook.sheets[0].cell(0, 0), Some(&Cell::Number(1_234.5)));
    assert_eq!(workbook.sheets[0].formatted(0, 0), Some("₩1,234.50"));
    assert_eq!(workbook.sheets[0].formatted(1, 0), Some("값: abc"));
}

#[test]
fn boolerr_and_formula_results() {
    let f = &Formats::default();
    let mut cells = Vec::new();
    let mut lf = None;
    let mut budget = MAX_TEXT_BYTES;
    let styles = XlsStyles::default();
    let mut style_budget = MAX_XLS_RETAINED_STYLE_BYTES;
    // BOOLERR error cell: bBoolErr=0x07 (#DIV/0!), fError=1.
    decode_cell(
        BOOLERR,
        &[0, 0, 0, 0, 0, 0, 0x07, 1],
        &[],
        0,
        &mut cells,
        &mut lf,
        f,
        &mut budget,
        &styles,
        &mut style_budget,
        &[],
        &[],
        &[],
        &[],
        ctx8(),
        &FormulaDefinitions::new(),
    );
    // BOOLERR bool FALSE at row 1.
    decode_cell(
        BOOLERR,
        &[1, 0, 0, 0, 0, 0, 0x00, 0],
        &[],
        0,
        &mut cells,
        &mut lf,
        f,
        &mut budget,
        &styles,
        &mut style_budget,
        &[],
        &[],
        &[],
        &[],
        ctx8(),
        &FormulaDefinitions::new(),
    );
    // FORMULA cached error: res[0]=0x02, res[2]=0x2A (#N/A), tail 0xFFFF.
    let mut fmla = vec![2, 0, 0, 0, 0, 0]; // row 2, col 0, ixfe 0
    fmla.extend_from_slice(&[0x02, 0x00, 0x2A, 0x00, 0x00, 0x00, 0xFF, 0xFF]);
    decode_cell(
        FORMULA,
        &fmla,
        &[],
        0,
        &mut cells,
        &mut lf,
        f,
        &mut budget,
        &styles,
        &mut style_budget,
        &[],
        &[],
        &[],
        &[],
        ctx8(),
        &FormulaDefinitions::new(),
    );
    assert_eq!(cells[0].value, Cell::Error("#DIV/0!".to_string()));
    assert_eq!(cells[1].value, Cell::Bool(false));
    assert_eq!(cells[2].value, Cell::Error("#N/A".to_string()));
}

#[test]
fn xls_formula_decompiled_to_source() {
    // A FORMULA record with a real rgce surfaces Cell::Formula { source, cached }.
    let f = &Formats::default();
    let mut cells = Vec::new();
    let mut lf = None;
    let mut budget = MAX_TEXT_BYTES;
    let styles = XlsStyles::default();
    let mut style_budget = MAX_XLS_RETAINED_STYLE_BYTES;
    let mut p = vec![0, 0, 0, 0, 0, 0]; // row, col, ixfe
    p.extend_from_slice(&30.0f64.to_le_bytes()); // cached result (numeric)
    p.extend_from_slice(&[0, 0]); // grbit
    p.extend_from_slice(&[0, 0, 0, 0]); // chn
                                        // rgce = SUM(A1:A2): PtgArea(A1:A2), PtgFuncVar(1 arg, SUM=4).
    let rgce: Vec<u8> = vec![0x25, 0, 0, 1, 0, 0, 0, 0, 0, 0x22, 1, 4, 0];
    p.extend_from_slice(&(rgce.len() as u16).to_le_bytes()); // cce
    p.extend_from_slice(&rgce);
    decode_cell(
        FORMULA,
        &p,
        &[],
        0,
        &mut cells,
        &mut lf,
        f,
        &mut budget,
        &styles,
        &mut style_budget,
        &[],
        &[],
        &[],
        &[],
        ctx8(),
        &FormulaDefinitions::new(),
    );
    assert_eq!(cells.len(), 1);
    match &cells[0].value {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "SUM($A$1:$A$2)");
            assert_eq!(**cached, Cell::Number(30.0));
        }
        other => panic!("expected a formula cell, got {other:?}"),
    }
}

#[test]
fn xls_formula_resolves_namex_from_supbook_externname_table() {
    let mut globals_bof = vec![0x00, 0x06, 0x05, 0x00];
    globals_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &globals_bof);
    stream.extend_from_slice(&rec(SUPBOOK, &[1, 0, 1, 4]));

    let mut extern_name = vec![0, 0, 0, 0, 0, 0, 12, 0];
    extern_name.extend_from_slice(b"ExternalRate");
    stream.extend_from_slice(&rec(EXTERNNAME, &extern_name));
    stream.extend_from_slice(&rec(EXTERNSHEET, &[1, 0, 0, 0, 0, 0, 0, 0]));

    let mut bound = vec![0, 0, 0, 0, 0, 0, 4, 0];
    bound.extend_from_slice(b"Data");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bound));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
    sheet_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &sheet_bof));
    let mut formula = vec![0, 0, 0, 0, 0, 0];
    formula.extend_from_slice(&1.0f64.to_le_bytes());
    formula.extend_from_slice(&[0, 0]);
    formula.extend_from_slice(&[0, 0, 0, 0]);
    let namex = [0x39, 0, 0, 1, 0, 0, 0];
    formula.extend_from_slice(&(namex.len() as u16).to_le_bytes());
    formula.extend_from_slice(&namex);
    stream.extend_from_slice(&rec(FORMULA, &formula));
    stream.extend_from_slice(&rec(EOF, &[]));

    let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    match workbook.sheets[0].cell(0, 0).unwrap() {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "[ixti:0]!ExternalRate");
            assert_eq!(cached.as_ref(), &Cell::Number(1.0));
        }
        other => panic!("expected resolved NameX formula, got {other:?}"),
    }
    assert_eq!(
        workbook.evaluate_cell("Data", 0, 0),
        crate::FormulaEvaluation::Fallback {
            cached: Cell::Number(1.0),
            reason: crate::FormulaUnsupportedReason::ExternalRef,
        }
    );
}

#[test]
fn xls_formula_record_type_0406_uses_formula_decoder() {
    // Apache POI's WrongFormulaRecordType.xls carries formula records with
    // sid 0x0406. The payload is the standard BIFF8 FORMULA layout.
    let f = &Formats::default();
    let mut cells = Vec::new();
    let mut lf = None;
    let mut budget = MAX_TEXT_BYTES;
    let styles = XlsStyles::default();
    let mut style_budget = MAX_XLS_RETAINED_STYLE_BYTES;
    let mut p = vec![3, 0, 0, 0, 0, 0]; // row 3, col 0, ixfe 0
    p.extend_from_slice(&3.0f64.to_le_bytes()); // cached result (numeric)
    p.extend_from_slice(&[0, 0]); // grbit
    p.extend_from_slice(&[0, 0, 0, 0]); // chn
    let rgce: Vec<u8> = vec![0x25, 0, 0, 1, 0, 0, 0, 0, 0, 0x22, 1, 4, 0];
    p.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
    p.extend_from_slice(&rgce);

    decode_cell(
        0x0406,
        &p,
        &[],
        0,
        &mut cells,
        &mut lf,
        f,
        &mut budget,
        &styles,
        &mut style_budget,
        &[],
        &[],
        &[],
        &[],
        ctx8(),
        &FormulaDefinitions::new(),
    );

    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].text, "3");
    match &cells[0].value {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "SUM($A$1:$A$2)");
            assert_eq!(**cached, Cell::Number(3.0));
        }
        other => panic!("expected a formula cell, got {other:?}"),
    }
}

fn numeric_formula_body(row: u16, col: u16, cached: f64, rgce: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&row.to_le_bytes());
    body.extend_from_slice(&col.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&cached.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
    body.extend_from_slice(rgce);
    body
}

#[test]
fn xls_shared_formula_is_reconstructed_for_each_cell() {
    let mut global_bof = vec![0x00, 0x06, 0x05, 0x00];
    global_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &global_bof);
    let mut boundsheet = vec![0, 0, 0, 0, 0, 0, 4, 0];
    boundsheet.extend_from_slice(b"Data");
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
    sheet_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &sheet_bof));
    let exp = [0x01, 0, 0, 1, 0]; // shared anchor B1
    stream.extend_from_slice(&rec(FORMULA, &numeric_formula_body(0, 1, 10.0, &exp)));

    let shared_rgce = [0x2C, 0, 0, 0xFF, 0xFF]; // PtgRefN: one column left
    let mut shared = Vec::new();
    shared.extend_from_slice(&0u16.to_le_bytes());
    shared.extend_from_slice(&1u16.to_le_bytes());
    shared.extend_from_slice(&[1, 1, 0, 2]);
    shared.extend_from_slice(&(shared_rgce.len() as u16).to_le_bytes());
    shared.extend_from_slice(&shared_rgce);
    stream.extend_from_slice(&rec(SHRFMLA, &shared));
    stream.extend_from_slice(&rec(FORMULA, &numeric_formula_body(1, 1, 20.0, &exp)));
    stream.extend_from_slice(&rec(EOF, &[]));

    let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    for (row, expected) in [(0, "A1"), (1, "A2")] {
        match workbook.sheets[0].cell(row, 1).unwrap() {
            Cell::Formula { formula, .. } => assert_eq!(formula, expected),
            other => panic!("expected shared formula at row {row}, got {other:?}"),
        }
    }
}

#[test]
fn xls_array_formula_and_array_constant_are_reconstructed() {
    let mut global_bof = vec![0x00, 0x06, 0x05, 0x00];
    global_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &global_bof);
    let mut boundsheet = vec![0, 0, 0, 0, 0, 0, 4, 0];
    boundsheet.extend_from_slice(b"Data");
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
    sheet_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &sheet_bof));
    let exp = [0x01, 0, 0, 0, 0];
    stream.extend_from_slice(&rec(FORMULA, &numeric_formula_body(0, 0, 1.0, &exp)));

    let array_rgce = [0x20, 0, 0, 0, 0, 0, 0, 0];
    let mut array = Vec::new();
    array.extend_from_slice(&0u16.to_le_bytes());
    array.extend_from_slice(&0u16.to_le_bytes());
    array.extend_from_slice(&[0, 1]); // A1:B1
    array.extend_from_slice(&0u16.to_le_bytes());
    array.extend_from_slice(&0u32.to_le_bytes());
    array.extend_from_slice(&(array_rgce.len() as u16).to_le_bytes());
    array.extend_from_slice(&array_rgce);
    array.extend_from_slice(&[1, 0, 0]); // two columns, one row
    array.push(0x01);
    array.extend_from_slice(&1.0f64.to_le_bytes());
    array.push(0x01);
    array.extend_from_slice(&2.0f64.to_le_bytes());
    stream.extend_from_slice(&rec(ARRAY, &array));
    stream.extend_from_slice(&rec(FORMULA, &numeric_formula_body(0, 1, 2.0, &exp)));
    stream.extend_from_slice(&rec(EOF, &[]));

    let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    for col in 0..=1 {
        match workbook.sheets[0].cell(0, col).unwrap() {
            Cell::Formula { formula, .. } => assert_eq!(formula, "{1,2}"),
            other => panic!("expected array formula at col {col}, got {other:?}"),
        }
    }
}

#[test]
fn xls_formula_resolves_3d_sheet_names_and_absolute_markers() {
    let mut global_bof = vec![0x00, 0x06, 0x05, 0x00];
    global_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &global_bof);
    for name in ["Calc", "Input Data"] {
        let mut boundsheet = vec![0, 0, 0, 0, 0, 0, name.len() as u8, 0];
        boundsheet.extend_from_slice(name.as_bytes());
        stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
    }
    let extern_sheet = [
        1, 0, // cXTI
        0, 0, // iSupBook
        1, 0, // itabFirst
        1, 0, // itabLast
    ];
    stream.extend_from_slice(&rec(EXTERNSHEET, &extern_sheet));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
    sheet_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &sheet_bof));
    let mut formula = vec![0, 0, 0, 0, 0, 0];
    formula.extend_from_slice(&7.0f64.to_le_bytes());
    formula.extend_from_slice(&[0, 0]);
    formula.extend_from_slice(&[0, 0, 0, 0]);
    let rgce = [
        0x3A, 0, 0, // PtgRef3d, ixti 0
        2, 0, // absolute row 2
        1, 0, // absolute column 1
    ];
    formula.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
    formula.extend_from_slice(&rgce);
    stream.extend_from_slice(&rec(FORMULA, &formula));
    stream.extend_from_slice(&rec(EOF, &[]));
    stream.extend_from_slice(&rec(BOF, &sheet_bof));
    stream.extend_from_slice(&rec(EOF, &[]));

    let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    match workbook.sheets[0].cell(0, 0).unwrap() {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "'Input Data'!$B$3");
            assert_eq!(cached.as_ref(), &Cell::Number(7.0));
        }
        other => panic!("expected 3D formula, got {other:?}"),
    }
}

#[test]
fn xls_blank_result_formula_keeps_identity() {
    // A FORMULA whose cached result is blank (res[0]=0x03) must still surface
    // as Cell::Formula when the rgce decompiles instead of being dropped.
    let f = &Formats::default();
    let mut cells = Vec::new();
    let mut lf = None;
    let mut budget = MAX_TEXT_BYTES;
    let styles = XlsStyles::default();
    let mut style_budget = MAX_XLS_RETAINED_STYLE_BYTES;
    let mut p = vec![0, 0, 0, 0, 0, 0]; // row, col, ixfe
    p.extend_from_slice(&[0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF]); // blank cached result
    p.extend_from_slice(&[0, 0]); // grbit
    p.extend_from_slice(&[0, 0, 0, 0]); // chn
    let rgce: Vec<u8> = vec![0x25, 0, 0, 1, 0, 0, 0, 0, 0, 0x22, 1, 4, 0]; // SUM(A1:A2)
    p.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
    p.extend_from_slice(&rgce);
    decode_cell(
        FORMULA,
        &p,
        &[],
        0,
        &mut cells,
        &mut lf,
        f,
        &mut budget,
        &styles,
        &mut style_budget,
        &[],
        &[],
        &[],
        &[],
        ctx8(),
        &FormulaDefinitions::new(),
    );
    assert_eq!(cells.len(), 1, "blank-result formula must still surface");
    match &cells[0].value {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "SUM($A$1:$A$2)");
            assert_eq!(**cached, Cell::Text(String::new()));
        }
        other => panic!("expected a formula cell, got {other:?}"),
    }
}

#[test]
fn biff8_empty_display_formula_retains_its_typed_cached_value() {
    let mut stream = rec(BOF, &biff8_bof(0x0005));

    let code = r#"##;##;"""#;
    let mut format = 164u16.to_le_bytes().to_vec();
    format.extend_from_slice(&(code.len() as u16).to_le_bytes());
    format.push(0); // compressed BIFF8 characters
    format.extend_from_slice(code.as_bytes());
    stream.extend_from_slice(&rec(FORMAT, &format));

    let mut xf = vec![0u8; 20];
    xf[2..4].copy_from_slice(&164u16.to_le_bytes());
    stream.extend_from_slice(&rec(XF, &xf));
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet8("Hidden zero")));
    stream.extend_from_slice(&rec(EOF, &[]));

    stream.extend_from_slice(&rec(BOF, &biff8_bof(0x0010)));
    let rgce = [0x1E, 0, 0]; // PtgInt(0)
    stream.extend_from_slice(&rec(FORMULA, &numeric_formula_body(0, 0, 0.0, &rgce)));
    stream.extend_from_slice(&rec(EOF, &[]));

    let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.formatted(0, 0), Some(""));
    assert_eq!(sheet.display_cells().count(), 1);
    assert_eq!(
        sheet.cell(0, 0),
        Some(&Cell::Formula {
            formula: "0".to_string(),
            cached: Box::new(Cell::Number(0.0)),
        })
    );
}

#[test]
fn empty_display_value_cells_remain_typed_and_bounded() {
    let styles = XlsStyles::default();
    let mut style_budget = MAX_XLS_RETAINED_STYLE_BYTES;
    let values = vec![
        Cell::Number(0.0),
        Cell::Date(0.0),
        Cell::Formula {
            formula: "A1".to_string(),
            cached: Box::new(Cell::Number(0.0)),
        },
        Cell::Bool(false),
        Cell::Error("#N/A".to_string()),
    ];
    let empty = String::new();
    let mut budget = values
        .iter()
        .map(|value| retained_cell_cost(value, &empty))
        .sum();
    let starting_budget = budget;
    let mut cells = Vec::new();

    push_cell(
        &mut cells,
        0,
        0,
        Cell::Text(String::new()),
        String::new(),
        0,
        &styles,
        &mut style_budget,
        &mut budget,
    );
    assert!(cells.is_empty());
    assert_eq!(budget, starting_budget);

    for (col, value) in values.into_iter().enumerate() {
        push_cell(
            &mut cells,
            0,
            col as u16,
            value,
            String::new(),
            0,
            &styles,
            &mut style_budget,
            &mut budget,
        );
    }
    assert_eq!(cells.len(), 5);
    assert_eq!(budget, 0);

    push_cell(
        &mut cells,
        0,
        6,
        Cell::Number(1.0),
        String::new(),
        0,
        &styles,
        &mut style_budget,
        &mut budget,
    );
    assert_eq!(cells.len(), 5, "zero budget must bound retained cells");
}

#[test]
fn hidden_formula_strings_charge_source_cache_and_box_storage() {
    let code = r#"0;0;0;"""#;
    let mut formats = Formats::default();
    let mut format = 164u16.to_le_bytes().to_vec();
    format.extend_from_slice(&(code.len() as u16).to_le_bytes());
    format.push(0);
    format.extend_from_slice(code.as_bytes());
    formats.push_format(&format, || Some(code.to_string()));
    let mut xf = vec![0u8; 20];
    xf[2..4].copy_from_slice(&164u16.to_le_bytes());
    formats.push_xf(&xf);

    // Keep capacities deliberately larger than lengths: retained bytes,
    // rather than visible bytes, are the security boundary.
    let mut formula = String::with_capacity(4 << 10);
    formula.push_str("A1");
    let mut cached = String::with_capacity(8 << 10);
    cached.push_str("secret");
    let display = formats.render_text(&cached, 0);
    assert!(display.is_empty());
    let formula_capacity = formula.capacity();
    let cached_capacity = cached.capacity();
    let display_capacity = display.capacity();
    let value = Cell::Formula {
        formula,
        cached: Box::new(Cell::Text(cached)),
    };
    let expected = std::mem::size_of::<CellEntry>()
        .saturating_add(display_capacity)
        .saturating_add(formula_capacity)
        .saturating_add(std::mem::size_of::<Cell>())
        .saturating_add(cached_capacity);
    assert_eq!(retained_cell_cost(&value, &display), expected);

    let mut cells = Vec::new();
    let styles = XlsStyles::default();
    let mut style_budget = MAX_XLS_RETAINED_STYLE_BYTES;
    let mut budget = expected;
    push_cell(
        &mut cells,
        0,
        0,
        value,
        display,
        0,
        &styles,
        &mut style_budget,
        &mut budget,
    );
    assert_eq!(cells.len(), 1);
    assert_eq!(budget, 0);
    let Cell::Formula { formula, cached } = &cells[0].value else {
        panic!("hidden formula must remain typed");
    };
    assert_eq!(formula.capacity(), formula_capacity);
    let Cell::Text(cached) = cached.as_ref() else {
        panic!("formula cache must remain text");
    };
    assert_eq!(cached.capacity(), cached_capacity);

    push_cell(
        &mut cells,
        0,
        1,
        Cell::Formula {
            formula: "B1".to_string(),
            cached: Box::new(Cell::Text("another secret".to_string())),
        },
        String::new(),
        0,
        &styles,
        &mut style_budget,
        &mut budget,
    );
    assert_eq!(cells.len(), 1, "zero budget must reject another formula");
}

#[test]
fn late_formula_definition_growth_is_charged_and_atomic() {
    fn entry(value: Cell) -> CellEntry {
        CellEntry {
            row: 0,
            col: 0,
            value,
            text: "cached".to_string(),
            style: None,
            xlsx_font_size_pt: None,
            hyperlink: None,
        }
    }

    let definition = FormulaDefinition {
        anchor: (0, 0),
        range: (0, 0, 0, 0),
        rgce: vec![0x1E, 42, 0], // PtgInt(42)
        rgb_extra: Vec::new(),
        is_array: false,
    };

    let mut cells = vec![entry(Cell::Error("cached".to_string()))];
    let mut last_formula = None;
    let starting_budget = 4096;
    let mut budget = starting_budget;
    apply_formula_definition(
        0,
        &definition,
        &mut cells,
        &mut last_formula,
        &mut budget,
        ctx8(),
        &[],
        &[],
        &[],
        &[],
    );
    let Cell::Formula { formula, cached } = &cells[0].value else {
        panic!("definition must wrap the cached scalar");
    };
    assert_eq!(formula, "42");
    assert_eq!(cached.as_ref(), &Cell::Error("cached".to_string()));
    let growth = formula
        .capacity()
        .saturating_add(std::mem::size_of::<Cell>());
    assert_eq!(budget, starting_budget - growth);

    let original = Cell::Error("cached".to_string());
    let mut cells = vec![entry(original.clone())];
    let mut last_formula = None;
    let mut budget = std::mem::size_of::<Cell>().saturating_sub(1);
    apply_formula_definition(
        0,
        &definition,
        &mut cells,
        &mut last_formula,
        &mut budget,
        ctx8(),
        &[],
        &[],
        &[],
        &[],
    );
    assert_eq!(cells[0].value, original);
    assert_eq!(budget, 0);

    let original = Cell::Formula {
        formula: String::new(),
        cached: Box::new(Cell::Number(42.0)),
    };
    let mut cells = vec![entry(original.clone())];
    let mut last_formula = None;
    let mut budget = 0;
    apply_formula_definition(
        0,
        &definition,
        &mut cells,
        &mut last_formula,
        &mut budget,
        ctx8(),
        &[],
        &[],
        &[],
        &[],
    );
    assert_eq!(cells[0].value, original);
    assert_eq!(budget, 0);
}

#[test]
fn repeated_hidden_labelsst_clones_exhaust_the_retained_cell_budget() {
    let code = r#"0;0;0;"""#;
    let mut formats = Formats::default();
    let mut format = 164u16.to_le_bytes().to_vec();
    format.extend_from_slice(&(code.len() as u16).to_le_bytes());
    format.push(0);
    format.extend_from_slice(code.as_bytes());
    formats.push_format(&format, || Some(code.to_string()));
    let mut xf = vec![0u8; 20];
    xf[2..4].copy_from_slice(&164u16.to_le_bytes());
    formats.push_xf(&xf);

    let sst = vec!["x".repeat(64 << 10)];
    let hidden = formats.render_text(&sst[0], 0);
    assert!(hidden.is_empty());
    let mut budget = retained_cell_cost(&Cell::Text(sst[0].clone()), &hidden);
    let mut cells = Vec::new();
    let mut last_formula = None;
    let styles = XlsStyles::default();
    let mut style_budget = MAX_XLS_RETAINED_STYLE_BYTES;
    let definitions = FormulaDefinitions::new();

    for row in 0..2u16 {
        let mut labelsst = Vec::with_capacity(10);
        labelsst.extend_from_slice(&row.to_le_bytes());
        labelsst.extend_from_slice(&0u16.to_le_bytes());
        labelsst.extend_from_slice(&0u16.to_le_bytes());
        labelsst.extend_from_slice(&0u32.to_le_bytes());
        decode_cell(
            LABELSST,
            &labelsst,
            &sst,
            0,
            &mut cells,
            &mut last_formula,
            &formats,
            &mut budget,
            &styles,
            &mut style_budget,
            &[],
            &[],
            &[],
            &[],
            ctx8(),
            &definitions,
        );
    }

    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].text, "");
    assert_eq!(cells[0].value, Cell::Text(sst[0].clone()));
    assert_eq!(budget, 0);
}

fn biff8_bof(substream_type: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&0x0600u16.to_le_bytes());
    body.extend_from_slice(&substream_type.to_le_bytes());
    body.extend_from_slice(&[0; 12]);
    body
}

fn biff5_bof(substream_type: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0x0500u16.to_le_bytes());
    body.extend_from_slice(&substream_type.to_le_bytes());
    body.extend_from_slice(&[0; 4]);
    body
}

fn test_palette(colors: &[[u8; 3]]) -> Vec<u8> {
    let mut body = Vec::with_capacity(2 + colors.len() * 4);
    body.extend_from_slice(&(colors.len() as u16).to_le_bytes());
    for color in colors {
        body.extend_from_slice(color);
        body.push(0);
    }
    body
}

fn boundsheet8(name: &str) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 0, 0, 0, name.len() as u8, 0];
    body.extend_from_slice(name.as_bytes());
    body
}

fn label8(row: u16, col: u16, ixfe: u16, text: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&row.to_le_bytes());
    body.extend_from_slice(&col.to_le_bytes());
    body.extend_from_slice(&ixfe.to_le_bytes());
    body.extend_from_slice(&(text.len() as u16).to_le_bytes());
    body.push(0); // compressed BIFF8 characters
    body.extend_from_slice(text.as_bytes());
    body
}

fn workbook_with_geometry_records(biff8: bool, records: &[(u16, Vec<u8>)]) -> Workbook {
    let (global_bof, sheet_bof, boundsheet, stream_name) = if biff8 {
        (
            biff8_bof(0x0005),
            biff8_bof(0x0010),
            boundsheet8("Geometry"),
            "/Workbook",
        )
    } else {
        let mut boundsheet = vec![0, 0, 0, 0, 0, 0, 8];
        boundsheet.extend_from_slice(b"Geometry");
        (biff5_bof(0x0005), biff5_bof(0x0010), boundsheet, "/Book")
    };
    let mut stream = rec(BOF, &global_bof);
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
    stream.extend_from_slice(&rec(EOF, &[]));
    stream.extend_from_slice(&rec(BOF, &sheet_bof));
    for (typ, body) in records {
        stream.extend_from_slice(&rec(*typ, body));
    }
    stream.extend_from_slice(&rec(EOF, &[]));
    Workbook::open(&wrap_xls(&stream, stream_name)).expect("synthetic geometry workbook")
}

fn test_colinfo_with_options(first: u16, last: u16, width_256: u16, options: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&first.to_le_bytes());
    body.extend_from_slice(&last.to_le_bytes());
    body.extend_from_slice(&width_256.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // default XF
    body.extend_from_slice(&options.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // unused
    body
}

fn test_colinfo(first: u16, last: u16, width_256: u16) -> Vec<u8> {
    test_colinfo_with_options(first, last, width_256, 0)
}

fn test_row(row: u16, height_twips: u16, options: u32) -> Vec<u8> {
    let mut body = vec![0; 16];
    body[0..2].copy_from_slice(&row.to_le_bytes());
    body[6..8].copy_from_slice(&height_twips.to_le_bytes());
    body[12..16].copy_from_slice(&options.to_le_bytes());
    body
}

#[test]
fn biff8_retains_default_geometry_with_standard_width_precedence() {
    let default_row_height = [0x0D, 0x00, 0x2C, 0x01].to_vec(); // flags, 300 twips
    let records = vec![
        (STANDARDWIDTH, 2432u16.to_le_bytes().to_vec()), // 9.5 characters
        (DEFAULTCOLWIDTH, 8u16.to_le_bytes().to_vec()),
        (DEFAULTROWHEIGHT, default_row_height),
        (COLINFO, test_colinfo(2, 2, 3072)), // 12 characters
        (ROW, test_row(3, 360, BIFF_ROW_FLAG_UNSYNCED)), // 18 points
    ];
    let workbook = workbook_with_geometry_records(true, &records);
    let sheet = &workbook.sheets[0];

    assert_eq!(sheet.default_column_width(), Some(9.5));
    assert_eq!(sheet.default_row_height(), Some(15.0));
    assert_eq!(sheet.column_widths().get(&2), Some(&12.0));
    assert_eq!(sheet.row_heights().get(&3), Some(&18.0));
    assert_eq!(sheet.column_widths().len(), 1);
    assert_eq!(sheet.row_heights().len(), 1);
    assert_eq!(
        sheet.imported_default_column_axis_measure(),
        Some(ImportedAxisMeasure::CharacterWidth256(2_432))
    );
    assert_eq!(
        sheet.imported_default_row_axis_measure(),
        Some(ImportedAxisMeasure::Twips(300))
    );
    assert_eq!(
        sheet.imported_column_axis_measures().get(&2),
        Some(&ImportedAxisMeasure::CharacterWidth256(3_072))
    );
    assert_eq!(
        sheet.imported_row_axis_measures().get(&3),
        Some(&ImportedAxisMeasure::Twips(360))
    );
    assert!(!sheet.biff_uses_application_default_column_width());
    assert!(!sheet.biff_uses_application_default_row_height());
}

#[test]
fn biff_row_height_requires_funsynced() {
    let records = vec![
        (ROW, test_row(3, 360, 0)),
        (ROW, test_row(4, 360, BIFF_ROW_FLAG_UNSYNCED)),
    ];
    let workbook = workbook_with_geometry_records(true, &records);
    let sheet = &workbook.sheets[0];

    assert_eq!(
        sheet.row_heights().get(&3),
        None,
        "miyRw is not an explicit override while fUnsynced is clear"
    );
    assert_eq!(sheet.row_heights().get(&4), Some(&18.0));
    assert_eq!(sheet.row_heights().len(), 1);
    assert_eq!(sheet.default_row_height(), None);
    assert!(sheet.biff_uses_application_default_row_height());
}

#[test]
fn biff_default_row_height_retains_funsynced_manuality() {
    for (flags, expected_manual) in [(0_u16, false), (0x0001, true)] {
        let records = vec![(
            DEFAULTROWHEIGHT,
            [flags.to_le_bytes(), 300_u16.to_le_bytes()].concat(),
        )];
        let workbook = workbook_with_geometry_records(true, &records);
        let sheet = &workbook.sheets[0];

        assert_eq!(sheet.default_row_height(), Some(15.0));
        assert_eq!(
            sheet.imported_default_row_axis_measure(),
            Some(ImportedAxisMeasure::Twips(300))
        );
        assert_eq!(
            sheet.default_row_height_is_manual(),
            expected_manual,
            "DEFAULTROWHEIGHT flags {flags:#06x}"
        );
    }
}

#[test]
fn biff_duplicate_rows_keep_manual_provenance_and_latest_valid_height() {
    for (records, expected) in [
        (
            vec![
                (ROW, test_row(3, 360, BIFF_ROW_FLAG_UNSYNCED)),
                (ROW, test_row(3, 255, 0)),
            ],
            12.75,
        ),
        (
            vec![
                (ROW, test_row(3, 255, 0)),
                (ROW, test_row(3, 360, BIFF_ROW_FLAG_UNSYNCED)),
            ],
            18.0,
        ),
    ] {
        let workbook = workbook_with_geometry_records(true, &records);
        assert_eq!(workbook.sheets[0].row_heights().get(&3), Some(&expected));
        assert_eq!(workbook.sheets[0].row_heights().len(), 1);
    }
}

#[test]
fn biff_row_metadata_obeys_generation_specific_row_bounds() {
    let biff5 = workbook_with_geometry_records(
        false,
        &[
            (ROW, test_row(16_383, 360, BIFF_ROW_FLAG_UNSYNCED | 0x20)),
            (ROW, test_row(16_384, 420, BIFF_ROW_FLAG_UNSYNCED | 0x20)),
        ],
    );
    assert_eq!(biff5.sheets[0].row_heights().get(&16_383), Some(&18.0));
    assert_eq!(biff5.sheets[0].row_heights().get(&16_384), None);
    assert!(biff5.sheets[0].hidden_rows().contains(&16_383));
    assert!(!biff5.sheets[0].hidden_rows().contains(&16_384));

    let biff8 = workbook_with_geometry_records(
        true,
        &[(ROW, test_row(65_535, 420, BIFF_ROW_FLAG_UNSYNCED | 0x20))],
    );
    assert_eq!(biff8.sheets[0].row_heights().get(&65_535), Some(&21.0));
    assert!(biff8.sheets[0].hidden_rows().contains(&65_535));
}

#[test]
fn biff_row_height_rejects_malformed_and_out_of_range_values() {
    let mut truncated = test_row(7, 360, BIFF_ROW_FLAG_UNSYNCED);
    truncated.truncate(15);
    let records = vec![
        (ROW, test_row(1, 0, BIFF_ROW_FLAG_UNSYNCED)),
        (
            ROW,
            test_row(2, MIN_BIFF_ROW_HEIGHT_TWIPS - 1, BIFF_ROW_FLAG_UNSYNCED),
        ),
        (
            ROW,
            test_row(3, MIN_BIFF_ROW_HEIGHT_TWIPS, BIFF_ROW_FLAG_UNSYNCED),
        ),
        (
            ROW,
            test_row(4, MAX_BIFF_ROW_HEIGHT_TWIPS, BIFF_ROW_FLAG_UNSYNCED),
        ),
        (
            ROW,
            test_row(5, MAX_BIFF_ROW_HEIGHT_TWIPS + 1, BIFF_ROW_FLAG_UNSYNCED),
        ),
        (ROW, truncated),
    ];
    let workbook = workbook_with_geometry_records(true, &records);
    let sheet = &workbook.sheets[0];

    assert_eq!(
        sheet.row_heights().get(&3),
        Some(&(f32::from(MIN_BIFF_ROW_HEIGHT_TWIPS) / 20.0))
    );
    assert_eq!(
        sheet.row_heights().get(&4),
        Some(&(f32::from(MAX_BIFF_ROW_HEIGHT_TWIPS) / 20.0))
    );
    assert_eq!(sheet.row_heights().len(), 2);
}

#[test]
fn biff5_standardwidth_overrides_defcolwidth_and_ignored_gcw() {
    for records in [
        vec![
            (STANDARDWIDTH, 2432u16.to_le_bytes().to_vec()),
            (DEFAULTCOLWIDTH, 10u16.to_le_bytes().to_vec()),
            (0x00AB, vec![0xAA; 34]), // GCW is ignored by Calc's importer.
        ],
        vec![
            (DEFAULTCOLWIDTH, 10u16.to_le_bytes().to_vec()),
            (0x00AB, vec![0x55; 34]),
            (STANDARDWIDTH, 2432u16.to_le_bytes().to_vec()),
        ],
    ] {
        let mut records = records;
        records.push((
            DEFAULTROWHEIGHT,
            [0x02, 0x00, 0xFF, 0x00].to_vec(), // fDyZero, 255 unhidden twips
        ));
        let workbook = workbook_with_geometry_records(false, &records);
        let sheet = &workbook.sheets[0];

        assert_eq!(sheet.default_column_width(), Some(9.5));
        assert_eq!(sheet.default_row_height(), Some(12.75));
        assert!(sheet.column_widths().is_empty());
        assert!(sheet.row_heights().is_empty());
        assert!(!sheet.biff_uses_application_default_column_width());
        assert_eq!(
            sheet.default_hidden_row_exceptions().map(BTreeSet::len),
            Some(0)
        );
    }
}

#[test]
fn biff_missing_width_records_retain_calc_application_default_provenance() {
    for biff8 in [false, true] {
        let mut workbook = workbook_with_geometry_records(biff8, &[]);
        let sheet = &mut workbook.sheets[0];
        assert_eq!(sheet.default_column_width(), None);
        assert!(sheet.biff_uses_application_default_column_width());
        assert_eq!(
            sheet.imported_default_column_axis_measure(),
            Some(ImportedAxisMeasure::Twips(1_280))
        );

        sheet.set_default_col_width(12.0);
        assert_eq!(sheet.default_column_width(), Some(12.0));
        assert!(!sheet.biff_uses_application_default_column_width());
        assert_eq!(sheet.imported_default_column_axis_measure(), None);
    }
}

#[test]
fn biff_default_hidden_rows_retain_visible_exceptions_and_zero_unhidden_height() {
    for biff8 in [false, true] {
        let records = vec![
            (DEFAULTROWHEIGHT, [0x02, 0x00, 0x00, 0x00].to_vec()),
            (ROW, test_row(1, 255, 0)),
            (ROW, test_row(2, 255, 0x20)),
        ];
        let workbook = workbook_with_geometry_records(biff8, &records);
        let sheet = &workbook.sheets[0];

        assert_eq!(sheet.default_row_height(), Some(0.0));
        assert_eq!(
            sheet.imported_default_row_axis_measure(),
            Some(ImportedAxisMeasure::Twips(0))
        );
        assert!(!sheet.biff_uses_application_default_row_height());
        assert_eq!(
            sheet
                .default_hidden_row_exceptions()
                .expect("fDyZero provenance")
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [1]
        );
        assert!(sheet.hidden_rows().contains(&2));
    }
}

#[test]
fn biff_missing_row_height_retains_calc_application_default_provenance() {
    for biff8 in [false, true] {
        let mut workbook = workbook_with_geometry_records(biff8, &[]);
        let sheet = &mut workbook.sheets[0];
        assert_eq!(sheet.default_row_height(), None);
        assert!(sheet.biff_uses_application_default_row_height());

        sheet.set_default_row_height(12.0);
        assert_eq!(sheet.default_row_height(), Some(12.0));
        assert!(!sheet.biff_uses_application_default_row_height());
        assert_eq!(sheet.imported_default_row_axis_measure(), None);
    }
}

#[test]
fn biff_colinfo_zero_width_hides_and_restores_sheet_default() {
    let records = vec![
        (COLINFO, test_colinfo(2, 2, 3072)),
        (COLINFO, test_colinfo(2, 2, 0)),
    ];
    let workbook = workbook_with_geometry_records(true, &records);
    let sheet = &workbook.sheets[0];

    assert!(sheet.hidden_columns().contains(&2));
    assert_eq!(sheet.column_widths().get(&2), None);
    assert!(sheet.biff_uses_application_default_column_width());
}

#[test]
fn biff_colinfo_visibility_uses_final_width_and_monotonic_explicit_hidden() {
    let zero_then_nonzero = workbook_with_geometry_records(
        true,
        &[
            (COLINFO, test_colinfo(1, 3, 0)),
            (COLINFO, test_colinfo(2, 4, 3072)),
        ],
    );
    let sheet = &zero_then_nonzero.sheets[0];
    assert!(sheet.hidden_columns().contains(&1));
    assert_eq!(sheet.column_widths().get(&1), None);
    for col in 2..=4 {
        assert!(!sheet.hidden_columns().contains(&col));
        assert_eq!(sheet.column_widths().get(&col), Some(&12.0));
    }

    let nonzero_then_zero = workbook_with_geometry_records(
        true,
        &[
            (COLINFO, test_colinfo(1, 3, 3072)),
            (COLINFO, test_colinfo(2, 4, 0)),
        ],
    );
    let sheet = &nonzero_then_zero.sheets[0];
    assert!(!sheet.hidden_columns().contains(&1));
    assert_eq!(sheet.column_widths().get(&1), Some(&12.0));
    for col in 2..=4 {
        assert!(sheet.hidden_columns().contains(&col));
        assert_eq!(sheet.column_widths().get(&col), None);
    }

    let explicit_hidden_then_visible_width = workbook_with_geometry_records(
        true,
        &[
            (COLINFO, test_colinfo_with_options(1, 3, 2048, 0x01)),
            (COLINFO, test_colinfo(2, 4, 3072)),
        ],
    );
    let sheet = &explicit_hidden_then_visible_width.sheets[0];
    for col in 1..=3 {
        assert!(sheet.hidden_columns().contains(&col));
    }
    assert!(!sheet.hidden_columns().contains(&4));
    assert_eq!(sheet.column_widths().get(&1), Some(&8.0));
    for col in 2..=4 {
        assert_eq!(sheet.column_widths().get(&col), Some(&12.0));
    }
}

#[test]
fn biff_default_geometry_ignores_malformed_and_nonpositive_records() {
    let malformed = vec![
        (DEFAULTCOLWIDTH, vec![8]),
        (STANDARDWIDTH, vec![0, 9, 0]),
        (DEFAULTROWHEIGHT, 300u16.to_le_bytes().to_vec()),
    ];
    let workbook = workbook_with_geometry_records(true, &malformed);
    assert_eq!(workbook.sheets[0].default_column_width(), None);
    assert_eq!(workbook.sheets[0].default_row_height(), None);
    assert!(workbook.sheets[0].biff_uses_application_default_column_width());
    assert!(workbook.sheets[0].biff_uses_application_default_row_height());

    let invalid = vec![
        (DEFAULTCOLWIDTH, 0u16.to_le_bytes().to_vec()),
        (STANDARDWIDTH, 0u16.to_le_bytes().to_vec()),
        (DEFAULTROWHEIGHT, [0x00, 0x00, 0x00, 0x00].to_vec()),
        (DEFAULTROWHEIGHT, [0x00, 0x00, 0xFF, 0xFF].to_vec()),
    ];
    let workbook = workbook_with_geometry_records(false, &invalid);
    assert_eq!(workbook.sheets[0].default_column_width(), None);
    assert_eq!(workbook.sheets[0].default_row_height(), None);
    assert!(workbook.sheets[0].biff_uses_application_default_column_width());
    assert!(workbook.sheets[0].biff_uses_application_default_row_height());

    let bounded = vec![
        (DEFAULTCOLWIDTH, 256u16.to_le_bytes().to_vec()),
        (
            DEFAULTROWHEIGHT,
            [0x00, 0x00, 0xF4, 0x1F].to_vec(), // 8180 twips
        ),
    ];
    let workbook = workbook_with_geometry_records(true, &bounded);
    assert_eq!(workbook.sheets[0].default_column_width(), None);
    assert_eq!(workbook.sheets[0].default_row_height(), None);
    assert!(workbook.sheets[0].biff_uses_application_default_row_height());
}

#[test]
fn malformed_duplicate_geometry_records_do_not_erase_valid_values() {
    let records = vec![
        (DEFAULTCOLWIDTH, 8u16.to_le_bytes().to_vec()),
        (DEFAULTCOLWIDTH, vec![8]),
        (STANDARDWIDTH, 2304u16.to_le_bytes().to_vec()),
        (STANDARDWIDTH, 0u16.to_le_bytes().to_vec()),
        (
            DEFAULTROWHEIGHT,
            [0x01, 0x00, 0x04, 0x01].to_vec(), // fUnsynced, 260 twips
        ),
        (DEFAULTROWHEIGHT, vec![0, 0, 4]),
    ];
    let workbook = workbook_with_geometry_records(true, &records);
    assert_eq!(workbook.sheets[0].default_column_width(), Some(9.0));
    assert_eq!(workbook.sheets[0].default_row_height(), Some(13.0));
}

#[test]
fn biff8_retains_cell_font_fill_border_alignment_number_format_and_protection() {
    let colors = [
        [1, 2, 3],
        [4, 5, 6],
        [7, 8, 9],
        [10, 11, 12],
        [13, 14, 15],
        [16, 17, 18],
        [19, 20, 21],
    ];
    let mut stream = rec(BOF, &biff8_bof(0x0005));
    stream.extend_from_slice(&rec(CODEPAGE, &1200u16.to_le_bytes()));
    stream.extend_from_slice(&rec(PALETTE, &test_palette(&colors)));

    let font_name = "맑은 고딕";
    let mut font = Vec::new();
    font.extend_from_slice(&240u16.to_le_bytes()); // 12 pt
    font.extend_from_slice(&0x000Au16.to_le_bytes()); // italic + strikeout
    font.extend_from_slice(&8u16.to_le_bytes()); // first custom palette color
    font.extend_from_slice(&700u16.to_le_bytes()); // bold
    font.extend_from_slice(&1u16.to_le_bytes()); // superscript
    font.extend_from_slice(&[1, 0, 0x81, 0]); // underline, family, charset, unused
    font.push(font_name.encode_utf16().count() as u8);
    font.push(1); // uncompressed UTF-16
    for unit in font_name.encode_utf16() {
        font.extend_from_slice(&unit.to_le_bytes());
    }
    stream.extend_from_slice(&rec(FONT, &font));

    let format_code = "0.000";
    let mut format = 164u16.to_le_bytes().to_vec();
    format.extend_from_slice(&(format_code.len() as u16).to_le_bytes());
    format.push(0); // compressed BIFF8 characters
    format.extend_from_slice(format_code.as_bytes());
    stream.extend_from_slice(&rec(FORMAT, &format));

    let mut xf = vec![0u8; 20];
    xf[2..4].copy_from_slice(&164u16.to_le_bytes());
    xf[4..6].copy_from_slice(&0xFFF3u16.to_le_bytes()); // locked, hidden, no parent
    xf[6] = 2 | 0x08 | (1 << 4); // centered, wrapped, vertically centered
    xf[7] = 135; // -45 degrees
    xf[8] = 3 | 0x10; // indent 3, shrink-to-fit
    xf[9] = 0xFC; // all six cell attributes are local
    let border1 = 1u32 | (2 << 4) | (5 << 8) | (6 << 12) | (9 << 16) | (10 << 23);
    let border2 = 11u32 | (12 << 7) | (1 << 26);
    xf[10..14].copy_from_slice(&border1.to_le_bytes());
    xf[14..18].copy_from_slice(&border2.to_le_bytes());
    xf[18..20].copy_from_slice(&(13u16 | (14 << 7)).to_le_bytes());
    stream.extend_from_slice(&rec(XF, &xf));
    stream.extend_from_slice(&rec(STYLE, &[0x00, 0x80, 0x00, 0x00]));
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet8("Styled")));
    stream.extend_from_slice(&rec(EOF, &[]));

    stream.extend_from_slice(&rec(BOF, &biff8_bof(0x0010)));
    stream.extend_from_slice(&rec(LABEL, &label8(0, 0, 0, "Styled text")));
    stream.extend_from_slice(&rec(EOF, &[]));

    let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
    assert_eq!(sheet.formatted(0, 0), Some("Styled text"));
    let style = sheet.cell_style(0, 0).expect("retained BIFF8 XF");
    assert_eq!(sheet.default_cell_style(), Some(style));

    let retained_font = style.font.as_ref().expect("retained BIFF8 FONT");
    assert_eq!(retained_font.name.as_deref(), Some(font_name));
    assert_eq!(retained_font.size_pt, Some(12));
    assert_eq!(retained_font.color, Some(Color::from(colors[0])));
    assert!(retained_font.bold);
    assert!(retained_font.italic);
    assert!(retained_font.underline);
    assert!(retained_font.strikethrough);
    assert_eq!(retained_font.script, FormatScript::Superscript);

    assert_eq!(style.num_fmt.as_deref(), Some(format_code));
    assert_eq!(style.fill, Some(Color::from(colors[5])));
    assert_eq!(
        style.pattern_fill,
        Some(Fill {
            pattern: FormatPattern::Solid,
            foreground: Some(Color::from(colors[5])),
            background: Some(Color::from(colors[6])),
        })
    );
    let border = style.border.as_ref().expect("retained BIFF8 borders");
    assert_eq!(border.left, BorderStyle::Thin);
    assert_eq!(border.right, BorderStyle::Medium);
    assert_eq!(border.top, BorderStyle::Thick);
    assert_eq!(border.bottom, BorderStyle::Double);
    assert_eq!(border.left_color, Some(Color::from(colors[1])));
    assert_eq!(border.right_color, Some(Color::from(colors[2])));
    assert_eq!(border.top_color, Some(Color::from(colors[3])));
    assert_eq!(border.bottom_color, Some(Color::from(colors[4])));
    assert_eq!(
        style.align,
        Some(Alignment {
            horizontal: Some(HAlign::Center),
            vertical: Some(VAlign::Middle),
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
fn biff5_retains_codepage_font_and_xf_style() {
    let colors = [
        [21, 22, 23],
        [24, 25, 26],
        [27, 28, 29],
        [30, 31, 32],
        [33, 34, 35],
        [36, 37, 38],
    ];
    let mut stream = rec(BOF, &biff5_bof(0x0005));
    stream.extend_from_slice(&rec(CODEPAGE, &949u16.to_le_bytes()));
    stream.extend_from_slice(&rec(PALETTE, &test_palette(&colors)));

    let font_name = "굴림";
    let encoded_font = encode_legacy_text(EUC_KR, font_name);
    let mut font = Vec::new();
    font.extend_from_slice(&200u16.to_le_bytes()); // 10 pt
    font.extend_from_slice(&0x0002u16.to_le_bytes()); // italic
    font.extend_from_slice(&8u16.to_le_bytes());
    font.extend_from_slice(&400u16.to_le_bytes());
    font.extend_from_slice(&2u16.to_le_bytes()); // subscript
    font.extend_from_slice(&[0, 0, 0x81, 0]);
    font.push(encoded_font.len() as u8);
    font.extend_from_slice(&encoded_font);
    stream.extend_from_slice(&rec(FONT, &font));

    let mut xf = vec![0u8; 16];
    xf[2..4].copy_from_slice(&2u16.to_le_bytes()); // built-in 0.00
    xf[4..6].copy_from_slice(&0xFFF1u16.to_le_bytes());
    xf[6] = 3 | 0x08 | (2 << 4); // right, wrap, bottom
    xf[7] = 0xFC | 2; // all local attributes + 90-degree orientation
    let border_fill1 = 9u32 | (10 << 7) | (1 << 16) | (6 << 22) | (10 << 25);
    let border2 = 1u32 | (2 << 3) | (5 << 6) | (11 << 9) | (12 << 16) | (13 << 23);
    xf[8..12].copy_from_slice(&border_fill1.to_le_bytes());
    xf[12..16].copy_from_slice(&border2.to_le_bytes());
    stream.extend_from_slice(&rec(XF, &xf));
    stream.extend_from_slice(&rec(STYLE, &[0x00, 0x80, 0x00, 0x00]));

    let sheet_name = encode_legacy_text(EUC_KR, "서식");
    let mut boundsheet = vec![0, 0, 0, 0, 0, 0, sheet_name.len() as u8];
    boundsheet.extend_from_slice(&sheet_name);
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
    stream.extend_from_slice(&rec(EOF, &[]));

    stream.extend_from_slice(&rec(BOF, &biff5_bof(0x0010)));
    let label = encode_legacy_text(EUC_KR, "스타일");
    let mut label_record = vec![0, 0, 0, 0, 0, 0];
    label_record.extend_from_slice(&(label.len() as u16).to_le_bytes());
    label_record.extend_from_slice(&label);
    stream.extend_from_slice(&rec(LABEL, &label_record));
    stream.extend_from_slice(&rec(EOF, &[]));

    let workbook = Workbook::open(&wrap_xls(&stream, "/Book")).unwrap();
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.name, "서식");
    assert_eq!(sheet.formatted(0, 0), Some("스타일"));
    assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
    let style = sheet.cell_style(0, 0).expect("retained BIFF5 XF");
    let retained_font = style.font.as_ref().expect("retained BIFF5 FONT");
    assert_eq!(retained_font.name.as_deref(), Some(font_name));
    assert_eq!(retained_font.size_pt, Some(10));
    assert_eq!(retained_font.color, Some(Color::from(colors[0])));
    assert!(!retained_font.bold);
    assert!(retained_font.italic);
    assert!(!retained_font.underline);
    assert_eq!(retained_font.script, FormatScript::Subscript);
    assert_eq!(style.num_fmt.as_deref(), Some("0.00"));
    assert_eq!(style.fill, Some(Color::from(colors[1])));
    let fill = style.pattern_fill.expect("retained BIFF5 fill");
    assert_eq!(fill.pattern, FormatPattern::Solid);
    assert_eq!(fill.foreground, Some(Color::from(colors[1])));
    assert_eq!(fill.background, Some(Color::from(colors[2])));
    let border = style.border.as_ref().expect("retained BIFF5 border");
    assert_eq!(border.left, BorderStyle::Medium);
    assert_eq!(border.right, BorderStyle::Thick);
    assert_eq!(border.top, BorderStyle::Thin);
    assert_eq!(border.bottom, BorderStyle::Double);
    assert_eq!(border.left_color, Some(Color::from(colors[4])));
    assert_eq!(border.right_color, Some(Color::from(colors[5])));
    assert_eq!(border.top_color, Some(Color::from(colors[3])));
    assert_eq!(border.bottom_color, Some(Color::from(colors[2])));
    assert_eq!(
        style.align,
        Some(Alignment {
            horizontal: Some(HAlign::Right),
            vertical: Some(VAlign::Bottom),
            wrap: true,
            rotation: 90,
            indent: 0,
            shrink_to_fit: false,
        })
    );
}

#[test]
fn biff8_retained_number_format_matches_the_raw_cell_xf() {
    let mut stream = rec(BOF, &biff8_bof(0x0005));

    let mut parent = vec![0u8; 20];
    parent[4..6].copy_from_slice(&0xFFF4u16.to_le_bytes()); // parentless style XF
    stream.extend_from_slice(&rec(XF, &parent));

    let mut cell_xf = vec![0u8; 20];
    cell_xf[2..4].copy_from_slice(&10u16.to_le_bytes()); // built-in 0.00%
    cell_xf[4..6].copy_from_slice(&0u16.to_le_bytes()); // cell XF, parent 0
    cell_xf[9] = 0; // fAtrNum clear: future parent ifmt edits propagate
    stream.extend_from_slice(&rec(XF, &cell_xf));
    stream.extend_from_slice(&rec(STYLE, &[0x00, 0x80, 0x00, 0x00]));
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet8("Percent")));
    stream.extend_from_slice(&rec(EOF, &[]));

    stream.extend_from_slice(&rec(BOF, &biff8_bof(0x0010)));
    let mut number = Vec::new();
    number.extend_from_slice(&0u16.to_le_bytes());
    number.extend_from_slice(&0u16.to_le_bytes());
    number.extend_from_slice(&1u16.to_le_bytes());
    number.extend_from_slice(&0.0042f64.to_le_bytes());
    stream.extend_from_slice(&rec(NUMBER, &number));
    stream.extend_from_slice(&rec(EOF, &[]));

    let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.formatted(0, 0), Some("0.42%"));
    assert_eq!(
        sheet
            .cell_style(0, 0)
            .and_then(|style| style.num_fmt.as_deref()),
        Some("0.00%")
    );
}

#[test]
fn biff8_cell_xf_keeps_all_raw_components_when_fatr_bits_are_clear() {
    let font_record = |name: &str, height_twips: u16, weight: u16| {
        let mut font = Vec::new();
        font.extend_from_slice(&height_twips.to_le_bytes());
        font.extend_from_slice(&0u16.to_le_bytes());
        font.extend_from_slice(&8u16.to_le_bytes());
        font.extend_from_slice(&weight.to_le_bytes());
        font.extend_from_slice(&0u16.to_le_bytes());
        font.extend_from_slice(&[0, 0, 0, 0]);
        font.push(name.len() as u8);
        font.push(0);
        font.extend_from_slice(name.as_bytes());
        font
    };

    let mut styles = XlsStyles::default();
    styles.push_font(&font_record("Parent", 200, 400));
    styles.push_font(&font_record("Child", 240, 700));

    let mut parent = vec![0u8; 20];
    parent[4..6].copy_from_slice(&0xFFF7u16.to_le_bytes());
    styles.push_xf(&parent);

    let mut child = vec![0u8; 20];
    child[0..2].copy_from_slice(&1u16.to_le_bytes());
    child[2..4].copy_from_slice(&10u16.to_le_bytes());
    child[4..6].copy_from_slice(&0u16.to_le_bytes());
    child[6] = 3 | 0x08 | (2 << 4);
    child[7] = 45;
    child[8] = 2 | 0x10;
    child[9] = 0; // all fAtr* clear: future style edits may update the child
    child[10..14].copy_from_slice(&1u32.to_le_bytes());
    child[14..18].copy_from_slice(&(1u32 << 26).to_le_bytes());
    child[18..20].copy_from_slice(&(10u16 | (11 << 7)).to_le_bytes());
    styles.push_xf(&child);

    styles.compile(ctx8(), &Formats::default(), &BIFF_DEFAULT_PALETTE);
    let style = styles.xfs[1].as_ref().expect("compiled child XF");

    let font = style.font.as_ref().expect("child font");
    assert_eq!(font.name.as_deref(), Some("Child"));
    assert_eq!(font.size_pt, Some(12));
    assert!(font.bold);
    assert_eq!(style.num_fmt.as_deref(), Some("0.00%"));
    assert_eq!(
        style.align,
        Some(Alignment {
            horizontal: Some(HAlign::Right),
            vertical: Some(VAlign::Bottom),
            wrap: true,
            rotation: 45,
            indent: 2,
            shrink_to_fit: true,
        })
    );
    assert_eq!(
        style.border.as_ref().map(|border| border.left),
        Some(BorderStyle::Thin)
    );
    assert_eq!(
        style.pattern_fill.as_ref().map(|fill| fill.pattern),
        Some(FormatPattern::Solid)
    );
    assert_eq!(
        style.protection,
        Some(CellProtection {
            locked: Some(false),
            hidden: false,
        })
    );
}

fn solid_biff8_xf(color_index: u16) -> Vec<u8> {
    let mut xf = vec![0u8; 20];
    xf[4..6].copy_from_slice(&0xFFF1u16.to_le_bytes());
    xf[9] = 0xFC;
    xf[14..18].copy_from_slice(&(1u32 << 26).to_le_bytes());
    xf[18..20].copy_from_slice(&(color_index | (65 << 7)).to_le_bytes());
    xf
}

#[test]
fn biff8_retains_default_column_row_and_explicit_style_precedence() {
    let mut stream = rec(BOF, &biff8_bof(0x0005));
    for index in 0..16 {
        let color = match index {
            0 => 4,  // blue column default
            1 => 3,  // green row default
            15 => 2, // red worksheet/default-cell XF
            _ => 0,
        };
        stream.extend_from_slice(&rec(XF, &solid_biff8_xf(color)));
    }
    stream.extend_from_slice(&rec(STYLE, &[0x00, 0x80, 0x00, 0x00]));
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet8("Inheritance")));
    stream.extend_from_slice(&rec(EOF, &[]));

    stream.extend_from_slice(&rec(BOF, &biff8_bof(0x0010)));
    let mut colinfo = Vec::new();
    colinfo.extend_from_slice(&0u16.to_le_bytes());
    colinfo.extend_from_slice(&0u16.to_le_bytes());
    colinfo.extend_from_slice(&2048u16.to_le_bytes());
    colinfo.extend_from_slice(&0u16.to_le_bytes());
    colinfo.extend_from_slice(&0u16.to_le_bytes());
    colinfo.extend_from_slice(&0u16.to_le_bytes());
    stream.extend_from_slice(&rec(COLINFO, &colinfo));

    let mut row = vec![0u8; 16];
    row[0..2].copy_from_slice(&1u16.to_le_bytes());
    row[6..8].copy_from_slice(&300u16.to_le_bytes());
    row[12..16].copy_from_slice(&(0x80u32 | (1 << 16)).to_le_bytes());
    stream.extend_from_slice(&rec(ROW, &row));
    stream.extend_from_slice(&rec(LABEL, &label8(2, 0, 15, "Explicit")));
    let mut blank = Vec::new();
    blank.extend_from_slice(&4u16.to_le_bytes());
    blank.extend_from_slice(&4u16.to_le_bytes());
    blank.extend_from_slice(&1u16.to_le_bytes());
    stream.extend_from_slice(&rec(BLANK, &blank));
    let mut mulblank = Vec::new();
    mulblank.extend_from_slice(&5u16.to_le_bytes());
    mulblank.extend_from_slice(&2u16.to_le_bytes());
    mulblank.extend_from_slice(&0u16.to_le_bytes());
    mulblank.extend_from_slice(&1u16.to_le_bytes());
    mulblank.extend_from_slice(&3u16.to_le_bytes());
    stream.extend_from_slice(&rec(MULBLANK, &mulblank));
    stream.extend_from_slice(&rec(EOF, &[]));

    let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    let sheet = &workbook.sheets[0];
    let red = Color::rgb(255, 0, 0);
    let green = Color::rgb(0, 255, 0);
    let blue = Color::rgb(0, 0, 255);
    assert_eq!(
        sheet.default_cell_style().and_then(|style| style.fill),
        Some(red)
    );
    assert_eq!(sheet.column_styles()[&0].fill, Some(blue));
    assert_eq!(sheet.row_styles()[&1].fill, Some(green));
    assert_eq!(sheet.blank_cell_styles().len(), 3);
    assert_eq!(sheet.blank_cell_styles()[&(4, 4)].fill, Some(green));
    assert_eq!(sheet.blank_cell_styles()[&(5, 2)].fill, Some(blue));
    assert_eq!(sheet.blank_cell_styles()[&(5, 3)].fill, Some(green));
    assert_eq!(
        sheet.resolved_cell_style(0, 0).and_then(|style| style.fill),
        Some(blue)
    );
    assert_eq!(
        sheet.resolved_cell_style(1, 0).and_then(|style| style.fill),
        Some(green)
    );
    assert_eq!(
        sheet.resolved_cell_style(0, 1).and_then(|style| style.fill),
        Some(red)
    );
    assert_eq!(
        sheet.resolved_cell_style(2, 0).and_then(|style| style.fill),
        Some(red)
    );
    assert_eq!(sheet.visual_dimensions(), Some((2, 0, 5, 4)));
}

#[test]
fn xls_style_tables_are_bounded_and_unknown_values_are_not_invented() {
    let hostile = vec![0xFF; u16::MAX as usize];
    let mut styles = XlsStyles::default();
    for _ in 0..MAX_XLS_STYLE_RECORDS + 16 {
        styles.push_font(&hostile);
        styles.push_xf(&hostile);
    }
    assert_eq!(styles.font_records.len(), MAX_XLS_STYLE_RECORDS);
    assert_eq!(styles.xf_records.len(), MAX_XLS_STYLE_RECORDS);
    assert!(styles
        .font_records
        .iter()
        .flatten()
        .all(|record| record.len() <= MAX_BIFF_FONT_RECORD_BYTES));
    assert!(styles
        .xf_records
        .iter()
        .all(|record| record.len() <= MAX_BIFF_XF_RECORD_BYTES));
    styles.compile(ctx8(), &Formats::default(), &BIFF_DEFAULT_PALETTE);
    let hostile_font = styles.fonts[0].as_ref().expect("bounded FONT decode");
    assert_eq!(hostile_font.name, None);
    assert_eq!(hostile_font.size_pt, None);
    assert_eq!(hostile_font.color, None);
    assert!(!hostile_font.bold);
    assert!(!hostile_font.underline);

    let mut unknown = vec![0u8; 20];
    unknown[4..6].copy_from_slice(&0xFFF1u16.to_le_bytes());
    unknown[6] = 0x77; // unsupported horizontal/vertical values
    unknown[7] = 0xFF; // stacked text cannot be represented by Alignment
    unknown[8] = 0xE0; // reserved bits only
    unknown[9] = 0xFC;
    unknown[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
    unknown[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
    unknown[18..20].copy_from_slice(&u16::MAX.to_le_bytes());
    let parsed = parse_biff_xf(
        &unknown,
        true,
        &[],
        &Formats::default(),
        &BIFF_DEFAULT_PALETTE,
    )
    .expect("fixed-width XF remains structurally valid")
    .components
    .into_cell_style();
    assert_eq!(parsed.align, Some(Alignment::default()));
    assert_eq!(parsed.border, Some(Border::default()));
    assert_eq!(parsed.pattern_fill, Some(Fill::default()));
    assert_eq!(parsed.fill, None);
}

#[test]
fn libreoffice_biff8_fixture_exposes_retained_styles() {
    let workbook = Workbook::open(include_bytes!(
        "../../tests/fixtures/formula/biff8/formula-source.xls"
    ))
    .unwrap();
    assert!(!workbook.sheets.is_empty());
    for sheet in &workbook.sheets {
        assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
        let display_cells = sheet.display_cells().collect::<Vec<_>>();
        let styled = display_cells
            .iter()
            .filter(|cell| cell.explicit_style.is_some())
            .count();
        assert_eq!(styled, display_cells.len(), "sheet {:?}", sheet.name);
        assert!(sheet.default_cell_style().is_some());
    }
}

#[test]
fn filepass_workbook_is_refused() {
    let mut bof = vec![0x00, 0x06, 0x05, 0x00]; // vers BIFF8, dt globals
    bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &bof);
    stream.extend_from_slice(&rec(FILEPASS, &[0x01, 0x00])); // RC4
    stream.extend_from_slice(&rec(EOF, &[]));
    let bytes = wrap_xls(&stream, "/Workbook");
    assert!(matches!(Workbook::open(&bytes), Err(Error::Encrypted)));
}

fn encrypt_default_xor_payload(data: &mut [u8], initial_index: usize) {
    // MS-OFFCRYPTO XOR array for "VelvetSweatshop" (key 0xB359, verifier
    // 0x9A0A), precomputed from Method 1 so this test fixture does not call
    // the production key-derivation path.
    const DEFAULT_XOR_ARRAY: [u8; 16] = [
        0x87, 0x6B, 0x9A, 0xE2, 0x1E, 0xE3, 0x05, 0x62, 0x1E, 0x69, 0x96, 0x60, 0x98, 0x6E, 0x94,
        0x04,
    ];
    let mut index = initial_index % DEFAULT_XOR_ARRAY.len();
    for byte in data {
        *byte = byte.rotate_left(5) ^ DEFAULT_XOR_ARRAY[index];
        index = (index + 1) % DEFAULT_XOR_ARRAY.len();
    }
}

fn encrypt_default_xor_workbook_stream(stream: &mut [u8]) {
    let mut pos = 0usize;
    while pos + 4 <= stream.len() {
        let typ = u16le(stream, pos).unwrap_or(0);
        let len = u16le(stream, pos + 2).unwrap_or(0) as usize;
        let start = pos + 4;
        let end = start.saturating_add(len);
        if end > stream.len() {
            break;
        }
        match typ {
            BOF | FILEPASS => {}
            BOUNDSHEET => {
                if start + 4 < end {
                    encrypt_default_xor_payload(&mut stream[start + 4..end], end + 4);
                }
            }
            _ => encrypt_default_xor_payload(&mut stream[start..end], end),
        }
        pos = end;
    }
}

#[test]
fn xor_default_password_workbook_is_deobfuscated() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00]; // vers BIFF8, dt globals
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    // FilePass: wEncryptionType=0 (XOR), key=0xB359, verifier=0x9A0A for
    // the default Excel password "VelvetSweatshop".
    stream.extend_from_slice(&rec(FILEPASS, &[0x00, 0x00, 0x59, 0xB3, 0x0A, 0x9A]));
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00]; // dt worksheet
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    let mut num = vec![0, 0, 0, 0, 0, 0]; // row 0, col 0, ixfe 0
    num.extend_from_slice(&42.0f64.to_le_bytes());
    stream.extend_from_slice(&rec(NUMBER, &num));
    stream.extend_from_slice(&rec(EOF, &[]));

    encrypt_default_xor_workbook_stream(&mut stream);
    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    assert_eq!(wb.sheets[0].name, "S1");
    assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Number(42.0)));
}

#[test]
fn biff8_end_to_end_with_sst_and_codepage() {
    // globals: BOF, CODEPAGE(949), BOUNDSHEET "S1", SST["셀A"], EOF
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    stream.extend_from_slice(&rec(CODEPAGE, &949u16.to_le_bytes()));
    // BOUNDSHEET: lbPlyPos(4)=0, hsState(1)=0, dt(1)=0, name "S1" (cch=2,grbit=0)
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    // SST: cstTotal=1, cstUnique=1, "셀A" uncompressed (cch=2, grbit=1)
    let mut sst = 1u32.to_le_bytes().to_vec();
    sst.extend_from_slice(&1u32.to_le_bytes());
    sst.extend_from_slice(&2u16.to_le_bytes());
    sst.push(0x01);
    for u in "셀A".encode_utf16() {
        sst.extend_from_slice(&u.to_le_bytes());
    }
    stream.extend_from_slice(&rec(SST, &sst));
    stream.extend_from_slice(&rec(EOF, &[]));
    // sheet substream: BOF, LABELSST(row0,col0,ixfe0,isst0), EOF
    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    let labelsst = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // row,col,ixfe,isst=0
    stream.extend_from_slice(&rec(LABELSST, &labelsst));
    stream.extend_from_slice(&rec(EOF, &[]));

    let bytes = wrap_xls(&stream, "/Workbook");
    let text = extract_text(&bytes).unwrap();
    assert!(text.contains("# S1"), "{text:?}");
    assert!(text.contains("셀A"), "{text:?}");
}

#[test]
fn biff5_supported_codepages_have_golden_text_end_to_end() {
    let cases = [
        // 949 is Windows Korean/UHC (a superset of KS X 1001).
        (949, EUC_KR, "내역서", "뷁 테스트"),
        // 51949 is the BIFF declaration used for EUC-KR.
        (51949, EUC_KR, "한국어", "월간 업무보고"),
        (932, SHIFT_JIS, "集計", "日本語テスト"),
        (1252, WINDOWS_1252, "Résumé", "Café € – naïve"),
    ];

    for (codepage, encoding, sheet_name, expected) in cases {
        let bytes = biff5_single_label(Some(codepage), encoding, sheet_name, expected);
        let workbook = Workbook::open(&bytes).expect("open BIFF5 codepage fixture");
        assert_eq!(workbook.sheets[0].name, sheet_name, "codepage {codepage}");
        assert_eq!(
            workbook.sheets[0].cell(0, 0),
            Some(&Cell::Text(expected.to_string())),
            "codepage {codepage}"
        );
    }
}

#[test]
fn biff5_codepage_fallback_and_override_policy_is_stable() {
    // Missing and unknown declarations use the documented cp1252 fallback.
    for declared in [None, Some(65_000)] {
        let bytes = biff5_single_label(declared, WINDOWS_1252, "Western", "Café €");
        let workbook = Workbook::open(&bytes).expect("open fallback fixture");
        assert_eq!(
            workbook.sheets[0].cell(0, 0),
            Some(&Cell::Text("Café €".to_string()))
        );
    }

    // A caller can correct a wrong declaration without changing the file.
    let wrongly_declared = biff5_single_label(Some(1252), EUC_KR, "Sheet", "한글");
    assert_ne!(
        Workbook::open(&wrongly_declared).unwrap().sheets[0].cell(0, 0),
        Some(&Cell::Text("한글".to_string()))
    );
    let forced = Workbook::open_with_codepage(&wrongly_declared, Some(949)).unwrap();
    assert_eq!(
        forced.sheets[0].cell(0, 0),
        Some(&Cell::Text("한글".to_string()))
    );

    // Malformed byte sequences are replaced with U+FFFD, never panicked or
    // silently dropped. 0x81 is an incomplete Shift-JIS lead byte.
    let malformed = [1, 0, 0x81];
    let context = Ctx {
        biff8: false,
        enc: SHIFT_JIS,
    };
    assert_eq!(read_xl_string(&malformed, 0, context).as_deref(), Some("�"));
}

#[test]
fn truncated_korean_cp949_label_recovers_bounded_replacement_without_panicking() {
    let sheet_name = encode_legacy_text(EUC_KR, "오류");
    let mut global_bof = vec![0x00, 0x05, 0x05, 0x00];
    global_bof.extend_from_slice(&[0u8; 4]);
    let mut stream = rec(BOF, &global_bof);
    stream.extend_from_slice(&rec(CODEPAGE, &949u16.to_le_bytes()));
    let mut boundsheet = vec![0, 0, 0, 0, 0, 0, sheet_name.len() as u8];
    boundsheet.extend_from_slice(&sheet_name);
    stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut sheet_bof = vec![0x00, 0x05, 0x10, 0x00];
    sheet_bof.extend_from_slice(&[0u8; 4]);
    stream.extend_from_slice(&rec(BOF, &sheet_bof));
    let mut label = vec![0, 0, 0, 0, 0, 0];
    label.extend_from_slice(&1u16.to_le_bytes());
    label.push(0xB0); // Truncated lead byte of CP949 "가" (B0 A1).
    stream.extend_from_slice(&rec(LABEL, &label));
    stream.extend_from_slice(&rec(EOF, &[]));
    let bytes = wrap_xls(&stream, "/Book");

    let opened = std::panic::catch_unwind(|| Workbook::open(&bytes));
    let workbook = opened
        .expect("malformed CP949 input must not panic")
        .unwrap();
    assert_eq!(workbook.sheets[0].name, "오류");
    assert_eq!(
        workbook.sheets[0].cell(0, 0),
        Some(&Cell::Text("�".to_string()))
    );
    assert_eq!(workbook.sheets[0].cells().count(), 1);
}

#[test]
fn invalid_cfb_directory_entry_uses_typed_bounded_recovery_after_success() {
    let mut global_bof = vec![0x00, 0x06, 0x05, 0x00];
    global_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &global_bof);
    stream.extend_from_slice(&rec(EOF, &[]));
    let mut bytes = wrap_xls(&stream, "/Workbook");
    let workbook_entry = cfb_directory_entry_offset(&bytes, "Workbook");
    // [MS-CFB] permits only red (0) and black (1). An out-of-range color
    // invalidates the primary directory-tree reader while leaving the
    // stream bytes and bounded linear directory scan intact.
    bytes[workbook_entry + 67] = 2;

    assert!(cfb::CompoundFile::open(Cursor::new(bytes.clone())).is_err());
    let workbook = Workbook::open(&bytes).expect("bounded CFB recovery");
    let provenance = workbook.parse_provenance();
    assert_eq!(
        provenance.container,
        crate::ContainerParseMode::TolerantCfbDirectoryWalk
    );
    assert_eq!(
        provenance.recoveries(),
        &[crate::RecoveryCode::TolerantCfbDirectoryWalk]
    );
    assert!(!provenance.recoveries_truncated());
    assert!(!provenance.partial);
    assert!(provenance.is_recovered());
    assert_eq!(
        crate::WorkbookReport::from_workbook("xls", &workbook)
            .provenance
            .recoveries(),
        &[crate::RecoveryCode::TolerantCfbDirectoryWalk]
    );
}

#[test]
fn biff5_custom_format_record_uses_short_string_length() {
    // BIFF5 FORMAT stores ifmt:u16, cch:u8, then raw codepage bytes. The
    // percent format must feed the same XF rendering path used by BIFF8.
    let mut g_bof = vec![0x00, 0x05, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 4]);
    let mut stream = rec(BOF, &g_bof);

    let mut fmt = 165u16.to_le_bytes().to_vec();
    fmt.push(5);
    fmt.extend_from_slice(b"0.00%");
    stream.extend_from_slice(&rec(FORMAT, &fmt));

    let mut xf = vec![0u8; 16];
    xf[2..4].copy_from_slice(&165u16.to_le_bytes());
    stream.extend_from_slice(&rec(XF, &xf));

    let mut bs = vec![0, 0, 0, 0, 0, 0, 2];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));

    let mut s_bof = vec![0x00, 0x05, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 4]);
    stream.extend_from_slice(&rec(BOF, &s_bof));

    let mut num = vec![0, 0, 0, 0, 0, 0];
    num.extend_from_slice(&1.0f64.to_le_bytes());
    stream.extend_from_slice(&rec(NUMBER, &num));
    stream.extend_from_slice(&rec(EOF, &[]));

    let bytes = wrap_xls(&stream, "/Book");
    let text = extract_text(&bytes).unwrap();
    assert!(text.contains("100.00%"), "{text:?}");
}

#[test]
fn date_cell_renders_iso_end_to_end() {
    // globals: BOF, XF(ifmt=14 date), BOUNDSHEET "S1", EOF
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    let mut xf = vec![0u8; 20]; // ifnt(2), ifmt(2)=14, ...
    xf[2] = 14;
    stream.extend_from_slice(&rec(XF, &xf));
    let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
    bs.extend_from_slice(b"S1");
    stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    stream.extend_from_slice(&rec(EOF, &[]));
    // sheet: BOF, NUMBER(row0,col0,ixfe0, serial 45366.0), EOF
    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    let mut num = vec![0, 0, 0, 0, 0, 0]; // row,col,ixfe=0
    num.extend_from_slice(&45366.0f64.to_le_bytes());
    stream.extend_from_slice(&rec(NUMBER, &num));
    stream.extend_from_slice(&rec(EOF, &[]));

    let bytes = wrap_xls(&stream, "/Workbook");
    let text = extract_text(&bytes).unwrap();
    assert!(text.contains("2024-03-15"), "{text:?}");
}

#[test]
fn nested_substream_does_not_desync_sheets() {
    // A worksheet substream may embed nested substreams (charts, pivot
    // tables) as `BOF … EOF`. Those nested BOFs must not advance the sheet
    // index; otherwise every sheet after the first embedded object is
    // silently dropped. This reproduces that real-world bug.
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    for nm in ["S1", "S2"] {
        let mut bs = vec![0, 0, 0, 0, 0, 0, nm.len() as u8, 0x00];
        bs.extend_from_slice(nm.as_bytes());
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
    }
    stream.extend_from_slice(&rec(EOF, &[]));

    // BIFF8 LABEL cell (compressed text) at (row, col).
    let label = |row: u16, col: u16, s: &str| {
        let mut d = vec![
            row as u8,
            (row >> 8) as u8,
            col as u8,
            (col >> 8) as u8,
            0,
            0,
        ];
        d.extend_from_slice(&(s.len() as u16).to_le_bytes());
        d.push(0x00); // grbit: compressed
        d.extend_from_slice(s.as_bytes());
        rec(LABEL, &d)
    };

    // Sheet 1: BOF, LABEL "AAA", embedded chart substream (BOF dt=0x20, EOF),
    // then the sheet's own EOF.
    let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
    s_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&label(0, 0, "AAA"));
    let mut chart_bof = vec![0x00, 0x06, 0x20, 0x00]; // dt = 0x0020 (chart)
    chart_bof.extend_from_slice(&[0u8; 12]);
    stream.extend_from_slice(&rec(BOF, &chart_bof));
    stream.extend_from_slice(&rec(EOF, &[])); // end chart
    stream.extend_from_slice(&rec(EOF, &[])); // end sheet 1

    // Sheet 2: BOF, LABEL "BBB", EOF.
    stream.extend_from_slice(&rec(BOF, &s_bof));
    stream.extend_from_slice(&label(0, 0, "BBB"));
    stream.extend_from_slice(&rec(EOF, &[]));

    let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
    assert_eq!(wb.sheets.len(), 2);
    assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Text("AAA".into())));
    // Without the depth fix, the embedded chart BOF shifts S2 out of range
    // and "BBB" is lost.
    assert_eq!(wb.sheets[1].cell(0, 0), Some(&Cell::Text("BBB".into())));
}

#[test]
fn rejects_non_ole2() {
    assert!(matches!(extract_text(b"not an xls"), Err(Error::NotOle2)));
}

#[test]
fn rejects_empty_workbook_stream() {
    let bytes = wrap_xls(&[], "/Workbook");
    assert!(matches!(Workbook::open(&bytes), Err(Error::Biff(_))));
    assert!(matches!(extract_text(&bytes), Err(Error::Biff(_))));
}

#[test]
fn rejects_random_workbook_stream() {
    let bytes = wrap_xls(b"random stream payload", "/Workbook");
    assert!(matches!(Workbook::open(&bytes), Err(Error::Biff(_))));
    assert!(matches!(extract_text(&bytes), Err(Error::Biff(_))));
}

#[test]
fn rejects_unsupported_biff_version() {
    let mut g_bof = vec![0x34, 0x12, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    stream.extend_from_slice(&rec(EOF, &[]));

    let bytes = wrap_xls(&stream, "/Workbook");
    assert!(matches!(Workbook::open(&bytes), Err(Error::Biff(_))));
}

#[test]
fn rejects_non_workbook_global_bof() {
    let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
    sheet_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &sheet_bof);
    stream.extend_from_slice(&rec(EOF, &[]));

    let bytes = wrap_xls(&stream, "/Workbook");
    assert!(matches!(Workbook::open(&bytes), Err(Error::Biff(_))));
}

#[test]
fn rejects_truncated_biff_header_and_body() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);

    let mut truncated_header = rec(BOF, &g_bof);
    truncated_header.extend_from_slice(&rec(EOF, &[]));
    truncated_header.extend_from_slice(&[0x09, 0x08, 0x01]);
    assert!(matches!(
        Workbook::open(&wrap_xls(&truncated_header, "/Workbook")),
        Err(Error::Biff(_))
    ));

    let mut truncated_body = rec(BOF, &g_bof);
    truncated_body.extend_from_slice(&LABEL.to_le_bytes());
    truncated_body.extend_from_slice(&8u16.to_le_bytes());
    truncated_body.extend_from_slice(&[0x00, 0x00]);
    assert!(matches!(
        Workbook::open(&wrap_xls(&truncated_body, "/Workbook")),
        Err(Error::Biff(_))
    ));
}

#[test]
fn rejects_unbalanced_biff_substreams() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);

    let unterminated = rec(BOF, &g_bof);
    assert!(matches!(
        Workbook::open(&wrap_xls(&unterminated, "/Workbook")),
        Err(Error::Biff(_))
    ));

    let mut extra_eof = rec(BOF, &g_bof);
    extra_eof.extend_from_slice(&rec(EOF, &[]));
    extra_eof.extend_from_slice(&rec(EOF, &[]));
    assert!(matches!(
        Workbook::open(&wrap_xls(&extra_eof, "/Workbook")),
        Err(Error::Biff(_))
    ));
}

#[test]
fn preserves_empty_biff_semantics_with_valid_headers() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    stream.extend_from_slice(&rec(EOF, &[]));

    let bytes = wrap_xls(&stream, "/Workbook");
    let wb = Workbook::open(&bytes).unwrap();
    assert!(wb.sheets.is_empty());
    assert!(matches!(extract_text(&bytes), Err(Error::NoText)));
}

#[test]
fn accepts_cfb_allocation_padding_after_balanced_stream() {
    let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
    g_bof.extend_from_slice(&[0u8; 12]);
    let mut stream = rec(BOF, &g_bof);
    stream.extend_from_slice(&rec(EOF, &[]));
    stream.extend_from_slice(&[0u8; 17]);

    let bytes = wrap_xls(&stream, "/Workbook");
    let wb = Workbook::open(&bytes).unwrap();
    assert!(wb.sheets.is_empty());
}
