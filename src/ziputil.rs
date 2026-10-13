use std::io::{Read, Seek};

use crate::{Error, Result};

pub(crate) fn validate_compression<R: Read + Seek>(zip: &mut zip::ZipArchive<R>) -> Result<()> {
    validate_limits(zip, 256 << 20, 512 << 20, 65_536)?;
    validate_methods(zip)
}

fn validate_limits<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    max_part: u64,
    max_total: u64,
    max_entries: usize,
) -> Result<()> {
    if zip.len() > max_entries {
        return Err(Error::Zip("ZIP package has too many entries"));
    }
    let mut total = 0u64;
    for index in 0..zip.len() {
        let file = zip
            .by_index_raw(index)
            .map_err(|_| Error::Zip("invalid ZIP central-directory entry"))?;
        let name = crate::zipname::entry_name(file.name_raw(), file.flags().as_u16());
        if name.len() > 4096 {
            return Err(Error::Zip("ZIP package entry name is too long"));
        }
        if file.size() > max_part {
            return Err(Error::Zip("ZIP package entry is too large"));
        }
        total = total
            .checked_add(file.size())
            .ok_or(Error::Zip("ZIP package is too large"))?;
        if total > max_total {
            return Err(Error::Zip("ZIP package is too large"));
        }
    }
    Ok(())
}

fn validate_methods<R: Read + Seek>(zip: &mut zip::ZipArchive<R>) -> Result<()> {
    for index in 0..zip.len() {
        let file = zip
            .by_index_raw(index)
            .map_err(|_| Error::Zip("invalid ZIP central-directory entry"))?;
        let name = crate::zipname::entry_name(file.name_raw(), file.flags().as_u16());
        if name.ends_with('/') {
            continue;
        }
        let method = file.compression();
        if !matches!(
            method,
            zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
        ) {
            return Err(Error::UnsupportedCompression {
                part: name.into_owned(),
                method: compression_method_id(method),
            });
        }
    }
    Ok(())
}

#[allow(deprecated)]
fn compression_method_id(method: zip::CompressionMethod) -> u16 {
    match method {
        zip::CompressionMethod::Stored => 0,
        zip::CompressionMethod::Deflated => 8,
        zip::CompressionMethod::Unsupported(value) => value,
        _ => u16::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    #[test]
    fn unsupported_method_reports_part_and_numeric_method() {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(
                "xl/workbook.xml",
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(b"<workbook/>").unwrap();
        let mut bytes = writer.finish().unwrap().into_inner();

        let local = bytes
            .windows(4)
            .position(|window| window == b"PK\x03\x04")
            .unwrap();
        bytes[local + 8..local + 10].copy_from_slice(&12u16.to_le_bytes());
        let central = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        bytes[central + 10..central + 12].copy_from_slice(&12u16.to_le_bytes());

        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert!(matches!(
            validate_compression(&mut archive),
            Err(Error::UnsupportedCompression { part, method })
                if part == "xl/workbook.xml" && method == 12
        ));
    }

    #[test]
    fn malformed_utf8_entry_names_keep_lossy_compatibility() {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("a", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"value").unwrap();
        let mut bytes = writer.finish().unwrap().into_inner();
        let local = bytes
            .windows(4)
            .position(|window| window == b"PK\x03\x04")
            .unwrap();
        let central = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        // Set the UTF-8 flag and invalidate both copies of the one-byte name.
        for (header, flag_offset, name_offset) in [(local, 6, 30), (central, 8, 46)] {
            let flags = u16::from_le_bytes(
                bytes[header + flag_offset..header + flag_offset + 2]
                    .try_into()
                    .unwrap(),
            ) | (1 << 11);
            bytes[header + flag_offset..header + flag_offset + 2]
                .copy_from_slice(&flags.to_le_bytes());
            bytes[header + name_offset] = 0xff;
        }
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        validate_compression(&mut archive).unwrap();
        let file = archive.by_index_raw(0).unwrap();
        assert_eq!(
            crate::zipname::entry_name(file.name_raw(), file.flags().as_u16()),
            "�"
        );
    }

    #[test]
    fn declared_uncompressed_sizes_are_bounded_before_reading() {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("a", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"12345").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert!(matches!(
            validate_limits(&mut archive, 4, 100, 10),
            Err(Error::Zip("ZIP package entry is too large"))
        ));
    }
}
