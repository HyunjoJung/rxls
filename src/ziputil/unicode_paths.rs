//! Preserve ZIP 2's refusal of malformed Unicode-path metadata.
//!
//! ZIP 9 ignores a bad Unicode-path checksum or invalid UTF-8. Check the
//! original central-directory bytes before its raw-name index can replace
//! duplicate entries or normalize a valid Unicode override. No member is
//! decompressed here.

pub(crate) fn validate(bytes: &[u8], central_start: u64) -> std::result::Result<(), super::Error> {
    let invalid = || super::Error::Zip("invalid ZIP Unicode-path metadata");
    let mut position = usize::try_from(central_start).map_err(|_| invalid())?;
    while bytes.get(position..position.saturating_add(4)) == Some(b"PK\x01\x02") {
        let header = bytes
            .get(position..position.saturating_add(46))
            .ok_or_else(invalid)?;
        let length = |offset| u16::from_le_bytes([header[offset], header[offset + 1]]) as usize;
        let name_start = position.checked_add(46).ok_or_else(invalid)?;
        let name_end = name_start.checked_add(length(28)).ok_or_else(invalid)?;
        let extra_end = name_end.checked_add(length(30)).ok_or_else(invalid)?;
        let entry_end = extra_end.checked_add(length(32)).ok_or_else(invalid)?;
        let mut original_name = bytes.get(name_start..name_end).ok_or_else(invalid)?;
        let mut extra = bytes.get(name_end..extra_end).ok_or_else(invalid)?;
        if entry_end > bytes.len() {
            return Err(invalid());
        }
        while extra.len() >= 4 {
            let id = u16::from_le_bytes([extra[0], extra[1]]);
            let size = u16::from_le_bytes([extra[2], extra[3]]) as usize;
            let end = 4usize.checked_add(size).ok_or_else(invalid)?;
            let field = extra.get(4..end).ok_or_else(invalid)?;
            if id == 0x7075 {
                let checksum = field.get(1..5).ok_or_else(invalid)?;
                let checksum =
                    u32::from_le_bytes([checksum[0], checksum[1], checksum[2], checksum[3]]);
                let unicode = field.get(5..).ok_or_else(invalid)?;
                if crc32fast::hash(original_name) != checksum
                    || std::str::from_utf8(unicode).is_err()
                {
                    return Err(invalid());
                }
                // ZIP 2 applies repeated Unicode path fields in order.
                original_name = unicode;
            }
            extra = &extra[end..];
        }
        position = entry_end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use zip::write::FullFileOptions;

    fn unicode_field(original: &[u8], unicode: &[u8]) -> Vec<u8> {
        let mut field = vec![1];
        field.extend_from_slice(&crc32fast::hash(original).to_le_bytes());
        field.extend_from_slice(unicode);
        field
    }

    fn fixture(raw: &[u8], fields: &[Vec<u8>], overwritten_raw: bool) -> (Vec<u8>, u64) {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let mut options =
            FullFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (index, field) in fields.iter().enumerate() {
            // Reserve nonsemantic extra fields, then patch their IDs. The
            // writer cannot check a Unicode CRC against an unwritten raw name.
            options
                .add_extra_field(
                    0xbeef + index as u16,
                    field.clone().into_boxed_slice(),
                    false,
                )
                .unwrap();
        }
        writer.start_file("x".repeat(raw.len()), options).unwrap();
        writer.write_all(b"first payload").unwrap();
        if overwritten_raw {
            writer
                .start_file("y".repeat(raw.len()), FullFileOptions::default())
                .unwrap();
            writer.write_all(b"last payload").unwrap();
        }
        let mut bytes = writer.finish().unwrap().into_inner();
        let archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let central_start = archive.central_directory_start();
        let headers: Vec<_> = (0..archive.len())
            .map(|index| {
                let entry = archive.by_index_data(index).unwrap();
                (
                    entry.header_start() as usize,
                    entry.central_header_start() as usize,
                )
            })
            .collect();
        drop(archive);
        for (local, central) in headers {
            for (start, name_offset, flags_offset, extra_length) in
                [(local, 30, 6, 28), (central, 46, 8, 30)]
            {
                let name_start = start + name_offset;
                let name_end = name_start + raw.len();
                bytes[name_start..name_end].copy_from_slice(raw);
                let flags = u16::from_le_bytes([
                    bytes[start + flags_offset],
                    bytes[start + flags_offset + 1],
                ]) & !0x800;
                bytes[start + flags_offset..start + flags_offset + 2]
                    .copy_from_slice(&flags.to_le_bytes());
                let extra_bytes = u16::from_le_bytes([
                    bytes[start + extra_length],
                    bytes[start + extra_length + 1],
                ]) as usize;
                let mut extra = name_end;
                while extra + 4 <= name_end + extra_bytes {
                    let id = u16::from_le_bytes([bytes[extra], bytes[extra + 1]]);
                    let len = u16::from_le_bytes([bytes[extra + 2], bytes[extra + 3]]) as usize;
                    if (0xbeef..0xbeef + fields.len() as u16).contains(&id) {
                        bytes[extra..extra + 2].copy_from_slice(&0x7075u16.to_le_bytes());
                    }
                    extra += 4 + len;
                }
            }
        }
        (bytes, central_start)
    }

    #[test]
    fn valid_override_is_checked_against_original_central_name() {
        let raw = b"Parts/caf\x82.xml";
        let chosen = "Parts/원본.xml";
        let (bytes, central) = fixture(raw, &[unicode_field(raw, chosen.as_bytes())], false);
        let archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        assert_eq!(archive.by_index_data(0).unwrap().name().unwrap(), chosen);
        assert!(super::validate(&bytes, central).is_ok());
    }

    #[test]
    fn bad_unicode_crc_is_refused_instead_of_ignored() {
        let raw = b"Parts/original.xml";
        let mut field = unicode_field(raw, b"Parts/override.xml");
        field[1] ^= 1;
        let (bytes, central) = fixture(raw, &[field], false);
        assert!(zip::ZipArchive::new(Cursor::new(&bytes)).is_ok());
        assert!(super::validate(&bytes, central).is_err());
    }

    #[test]
    fn invalid_unicode_utf8_is_refused_instead_of_ignored() {
        let raw = b"Parts/original.xml";
        let (bytes, central) = fixture(raw, &[unicode_field(raw, b"Parts/\xff.xml")], false);
        assert!(zip::ZipArchive::new(Cursor::new(&bytes)).is_ok());
        assert!(super::validate(&bytes, central).is_err());
    }

    #[test]
    fn invalid_metadata_cannot_hide_behind_an_overwritten_raw_key() {
        let raw = b"same-part.xml";
        let mut field = unicode_field(raw, b"override.xml");
        field[1] ^= 1;
        let (bytes, central) = fixture(raw, &[field], true);
        let archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        assert_eq!(archive.len(), 1);
        assert_eq!(
            archive.by_index_data(0).unwrap().name().unwrap(),
            "same-part.xml"
        );
        assert!(super::validate(&bytes, central).is_err());
    }

    #[test]
    fn repeated_unicode_paths_validate_the_previous_override() {
        let raw = b"Parts/original.xml";
        let first = b"Parts/first.xml";
        let last = b"Parts/final.xml";
        let fields = [unicode_field(raw, first), unicode_field(first, last)];
        let (bytes, central) = fixture(raw, &fields, false);
        let archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        assert_eq!(
            archive.by_index_data(0).unwrap().name().unwrap(),
            "Parts/final.xml"
        );
        assert!(super::validate(&bytes, central).is_ok());

        let fields = [unicode_field(raw, first), unicode_field(raw, last)];
        let (invalid, central) = fixture(raw, &fields, false);
        assert!(super::validate(&invalid, central).is_err());
    }

    #[test]
    fn truncated_central_extra_fields_are_refused() {
        let raw = b"Parts/original.xml";
        let (mut bytes, central) = fixture(raw, &[unicode_field(raw, b"override.xml")], false);
        bytes[central as usize + 30..central as usize + 32]
            .copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(super::validate(&bytes, central).is_err());
    }
}
