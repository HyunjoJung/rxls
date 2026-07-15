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
        if file.name().len() > 4096 {
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
        if file.name().ends_with('/') {
            continue;
        }
        let method = file.compression();
        if !matches!(
            method,
            zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
        ) {
            return Err(Error::UnsupportedCompression {
                part: file.name().to_string(),
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
