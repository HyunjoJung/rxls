//! ZIP entry names retain the encoding selected by the language-encoding flag.

use std::borrow::Cow;

// Unicode mappings for the upper half of IBM code page 437.
const CP437_HIGH: [char; 128] = [
    '\u{00c7}', '\u{00fc}', '\u{00e9}', '\u{00e2}', '\u{00e4}', '\u{00e0}', '\u{00e5}', '\u{00e7}',
    '\u{00ea}', '\u{00eb}', '\u{00e8}', '\u{00ef}', '\u{00ee}', '\u{00ec}', '\u{00c4}', '\u{00c5}',
    '\u{00c9}', '\u{00e6}', '\u{00c6}', '\u{00f4}', '\u{00f6}', '\u{00f2}', '\u{00fb}', '\u{00f9}',
    '\u{00ff}', '\u{00d6}', '\u{00dc}', '\u{00a2}', '\u{00a3}', '\u{00a5}', '\u{20a7}', '\u{0192}',
    '\u{00e1}', '\u{00ed}', '\u{00f3}', '\u{00fa}', '\u{00f1}', '\u{00d1}', '\u{00aa}', '\u{00ba}',
    '\u{00bf}', '\u{2310}', '\u{00ac}', '\u{00bd}', '\u{00bc}', '\u{00a1}', '\u{00ab}', '\u{00bb}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255d}', '\u{255c}', '\u{255b}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252c}', '\u{251c}', '\u{2500}', '\u{253c}', '\u{255e}', '\u{255f}',
    '\u{255a}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256c}', '\u{2567}',
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256b}',
    '\u{256a}', '\u{2518}', '\u{250c}', '\u{2588}', '\u{2584}', '\u{258c}', '\u{2590}', '\u{2580}',
    '\u{03b1}', '\u{00df}', '\u{0393}', '\u{03c0}', '\u{03a3}', '\u{03c3}', '\u{00b5}', '\u{03c4}',
    '\u{03a6}', '\u{0398}', '\u{03a9}', '\u{03b4}', '\u{221e}', '\u{03c6}', '\u{03b5}', '\u{2229}',
    '\u{2261}', '\u{00b1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00f7}', '\u{2248}',
    '\u{00b0}', '\u{2219}', '\u{00b7}', '\u{221a}', '\u{207f}', '\u{00b2}', '\u{25a0}', '\u{00a0}',
];

/// Preserve the filename decoding used before the ZIP dependency migration.
pub(crate) fn entry_name(raw: &[u8], flags: u16) -> Cow<'_, str> {
    const UTF8_FLAG: u16 = 1 << 11;
    if flags & UTF8_FLAG != 0 || raw.is_ascii() {
        return String::from_utf8_lossy(raw);
    }
    Cow::Owned(
        raw.iter()
            .map(|&byte| {
                if byte < 0x80 {
                    char::from(byte)
                } else {
                    CP437_HIGH[usize::from(byte - 0x80)]
                }
            })
            .collect(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn cp437_collision_archive() -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let parts: &[(&str, &[u8])] = &[
            (
                "[Content_Types].xml",
                br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/></Types>"#,
            ),
            ("xl/workbook.xml", b"<workbook/>"),
            ("xl/aa.xml", b"first"),
            ("xl/b.xml", b"second"),
        ];
        for (name, data) in parts {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        let mut bytes = writer.finish().unwrap().into_inner();
        let headers = {
            let archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
            [
                (2, b"xl/\xc3\xa9.xml".as_slice()),
                (3, b"xl/\x82.xml".as_slice()),
            ]
            .into_iter()
            .map(|(index, raw)| {
                let file = archive.by_index_data(index).unwrap();
                assert_eq!(file.flags().as_u16() & (1 << 11), 0);
                assert_eq!(file.name_raw().len(), raw.len());
                (
                    usize::try_from(file.header_start()).unwrap(),
                    usize::try_from(file.central_header_start()).unwrap(),
                    raw,
                )
            })
            .collect::<Vec<_>>()
        };
        // Replace equal-length ASCII placeholders in both headers. The UTF-8
        // flag stays clear, so both filenames use CP437 even though the first
        // one's bytes also form a valid UTF-8 sequence.
        for (local, central, raw) in headers {
            bytes[local + 30..local + 30 + raw.len()].copy_from_slice(raw);
            bytes[central + 46..central + 46 + raw.len()].copy_from_slice(raw);
        }
        bytes
    }

    #[test]
    fn archive_names_keep_distinct_cp437_encodings() {
        let bytes = cp437_collision_archive();
        let archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        for (index, expected) in [(2, "xl/├⌐.xml"), (3, "xl/é.xml")] {
            let file = archive.by_index_data(index).unwrap();
            assert_eq!(entry_name(file.name_raw(), file.flags().as_u16()), expected);
        }
    }

    #[test]
    fn encoding_flag_controls_names_that_are_also_valid_utf8() {
        assert_eq!(entry_name(b"xl/\xc3\xa9.xml", 0), "xl/├⌐.xml");
        assert_eq!(entry_name(b"xl/\xc3\xa9.xml", 1 << 11), "xl/é.xml");
        assert_eq!(entry_name(b"xl/\x82.xml", 0), "xl/é.xml");
    }

    #[test]
    fn malformed_utf8_keeps_the_previous_lossy_decoding() {
        assert_eq!(entry_name(b"xl/\xff.xml", 1 << 11), "xl/�.xml");
        assert_eq!(entry_name(b"xl/\xff.xml", 0), "xl/\u{00a0}.xml");
    }

    #[test]
    fn ascii_and_valid_utf8_borrow_the_original_bytes() {
        assert!(matches!(
            entry_name(b"xl/workbook.xml", 0),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            entry_name("xl/é.xml".as_bytes(), 1 << 11),
            Cow::Borrowed(_)
        ));
    }
}
