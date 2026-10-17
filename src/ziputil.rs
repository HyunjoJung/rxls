use std::io::{Read, Seek};

use crate::{Error, Result};
mod unicode_paths;
pub(crate) use unicode_paths::validate as validate_unicode_paths;

/// Keep ZIP 2's decoded-name order and last payload when legacy encodings alias.
pub(crate) fn decoded_entry_indices<R: Read + Seek>(
    archive: &zip::ZipArchive<R>,
) -> zip::result::ZipResult<Vec<(String, usize)>> {
    let mut entries: Vec<(String, usize)> = Vec::new();
    let mut positions = std::collections::HashMap::<String, usize>::new();
    for (index, decoded) in archive.file_names().enumerate() {
        let name = decoded?.into_owned();
        if let Some(&position) = positions.get(&name) {
            let previous = entries[position].1;
            if archive.by_index_data(index)?.central_header_start()
                > archive.by_index_data(previous)?.central_header_start()
            {
                entries[position].1 = index;
            }
        } else {
            positions.insert(name.clone(), entries.len());
            entries.push((name, index));
        }
    }
    Ok(entries)
}

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
    for (_, index) in
        decoded_entry_indices(zip).map_err(|_| Error::Zip("invalid ZIP entry name"))?
    {
        let file = zip
            .by_index_raw(index)
            .map_err(|_| Error::Zip("invalid ZIP central-directory entry"))?;
        let name = file
            .name()
            .map_err(|_| Error::Zip("invalid ZIP entry name"))?;
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
    for (_, index) in
        decoded_entry_indices(zip).map_err(|_| Error::Zip("invalid ZIP entry name"))?
    {
        let file = zip
            .by_index_raw(index)
            .map_err(|_| Error::Zip("invalid ZIP central-directory entry"))?;
        let name = file
            .name()
            .map_err(|_| Error::Zip("invalid ZIP entry name"))?;
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
pub(crate) fn decoded_alias_fixture(legacy_first: bool, repeated_raw: bool) -> Vec<u8> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut names = if legacy_first {
        vec!["caf_.bin", "café.bin"]
    } else {
        vec!["café.bin", "caf_.bin"]
    };
    if repeated_raw {
        names.push(if legacy_first {
            "caf~.bin"
        } else {
            "cafè.bin"
        });
    }
    let last = names.len() - 1;
    for (index, name) in names.into_iter().enumerate() {
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer
            .write_all(if index == last { b"latest" } else { b"older!" })
            .unwrap();
    }
    let mut bytes = writer.finish().unwrap().into_inner();
    for (marker, replacement) in [
        (b"caf_.bin".as_slice(), b"caf\x82.bin".as_slice()),
        (b"caf~.bin", b"caf\x82.bin"),
        ("cafè.bin".as_bytes(), "café.bin".as_bytes()),
    ] {
        let offsets: Vec<_> = bytes
            .windows(marker.len())
            .enumerate()
            .filter_map(|(index, name)| (name == marker).then_some(index))
            .collect();
        for index in offsets {
            bytes[index..index + marker.len()].copy_from_slice(replacement);
        }
    }
    // Discarded versions must never be inflated or CRC-checked.
    let headers: Vec<_> = bytes
        .windows(4)
        .enumerate()
        .filter_map(|(index, sig)| (sig == b"PK\x01\x02").then_some(index))
        .collect();
    for &header in &headers[..headers.len() - 1] {
        bytes[header + 16] ^= 1;
    }
    bytes
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
    fn discarded_alias_methods_and_declared_sizes_do_not_replace_selected_limits() {
        for repeated_raw in [false, true] {
            let mut bytes = decoded_alias_fixture(true, repeated_raw);
            let headers: Vec<_> = bytes
                .windows(4)
                .enumerate()
                .filter_map(|(index, sig)| (sig == b"PK\x01\x02").then_some(index))
                .collect();
            for &header in &headers[..headers.len() - 1] {
                bytes[header + 10..header + 12].copy_from_slice(&12u16.to_le_bytes());
                bytes[header + 24..header + 28].copy_from_slice(&u32::MAX.to_le_bytes());
            }
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
            validate_limits(&mut archive, 6, 6, 10).unwrap();
            validate_methods(&mut archive).unwrap();
        }
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
