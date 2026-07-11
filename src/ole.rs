//! OLE2/CFB access: open the container and read the workbook stream.

use std::io::{Cursor, Read};

use crate::error::{Error, Result};

/// `.xls` magic — OLE2/CFB compound file header.
pub(crate) fn is_ole2(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[0..8] == [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]
}

/// A raw (non-OLE2) stream that begins with an Excel 2.0/3.0/4.0 `BOF` record
/// (`0x0009` / `0x0209` / `0x0409`). These BIFF2–4 formats predate the
/// OLE2-wrapped `[MS-XLS]` workbook and are out of scope; detecting them lets us
/// return a precise [`Error::LegacyBiff`] instead of the generic `NotOle2`.
fn is_legacy_biff(bytes: &[u8]) -> bool {
    matches!(
        bytes.get(0..2).map(|b| u16::from_le_bytes([b[0], b[1]])),
        Some(0x0009 | 0x0209 | 0x0409)
    )
}

/// Read the BIFF workbook stream (`Workbook` for BIFF8, `Book` for BIFF5/7).
pub(crate) fn read_workbook_stream(bytes: &[u8]) -> Result<Vec<u8>> {
    if !is_ole2(bytes) {
        if is_legacy_biff(bytes) {
            return Err(Error::LegacyBiff);
        }
        return Err(Error::NotOle2);
    }
    // Fast path: the well-tested `cfb` crate.
    match cfb::CompoundFile::open(Cursor::new(bytes.to_vec())) {
        Ok(mut cfb) => {
            for name in ["/Workbook", "/Book"] {
                if cfb.exists(name) && cfb.is_stream(name) {
                    let mut s = cfb.open_stream(name)?;
                    let mut buf = Vec::new();
                    s.read_to_end(&mut buf)?;
                    return Ok(buf);
                }
            }
            if cfb.exists("/EncryptedPackage")
                && cfb.is_stream("/EncryptedPackage")
                && cfb.exists("/EncryptionInfo")
                && cfb.is_stream("/EncryptionInfo")
            {
                return Err(Error::EncryptedPackage);
            }
            Err(Error::MissingWorkbook)
        }
        // The strict `cfb` crate rejects some containers that Excel/POI/xlrd
        // accept — most commonly a directory whose red-black-tree sibling
        // ordering violates [MS-CFB] though the streams are otherwise intact.
        // Fall back to a lenient, bounds-checked walk that scans the directory
        // entries linearly and reads the workbook stream by name.
        Err(_) => {
            if tolerant::has_streams(bytes, &["EncryptedPackage", "EncryptionInfo"]) {
                return Err(Error::EncryptedPackage);
            }
            tolerant::read_workbook_stream(bytes)
                .ok_or(Error::InvalidCfb("not a valid .xls compound file"))
        }
    }
}

/// Read an optional root-level OLE stream by trying the provided names in order.
/// Metadata streams are advisory: callers should treat `None` as "not present"
/// rather than a workbook parse failure.
pub(crate) fn read_optional_stream(bytes: &[u8], names: &[&str]) -> Option<Vec<u8>> {
    if !is_ole2(bytes) {
        return None;
    }
    let mut cfb = cfb::CompoundFile::open(Cursor::new(bytes.to_vec())).ok()?;
    for name in names {
        if cfb.exists(name) && cfb.is_stream(name) {
            let mut stream = cfb.open_stream(name).ok()?;
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).ok()?;
            return Some(buf);
        }
    }
    None
}

/// A minimal, panic-free OLE2/CFB reader used only as a fallback for containers
/// the strict [`cfb`] crate refuses. It parses `[MS-CFB]` structurally (header →
/// DIFAT → FAT → directory) but, unlike the spec, walks the directory entries
/// **linearly** rather than trusting the red-black tree, so out-of-order or
/// lightly corrupt directories still yield their streams. It reads exactly one
/// stream (`Workbook` or `Book`) and never writes.
mod tolerant {
    /// Chain sentinels ([MS-CFB] 2.2): end-of-chain and free/unallocated.
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FREESECT: u32 = 0xFFFF_FFFF;

    #[inline]
    fn u16le(b: &[u8], o: usize) -> Option<u16> {
        b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
    }
    #[inline]
    fn u32le(b: &[u8], o: usize) -> Option<u32> {
        b.get(o..o + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    #[inline]
    fn u64le(b: &[u8], o: usize) -> Option<u64> {
        b.get(o..o + 8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap_or([0; 8])))
    }

    /// Byte offset of sector `id`. The 512-byte header occupies the space before
    /// sector 0, so sector `id` starts at `(id + 1) * sector_size` for both the
    /// 512- and 4096-byte sector geometries.
    fn sector_offset(id: u32, sector_size: usize) -> Option<usize> {
        (id as usize).checked_add(1)?.checked_mul(sector_size)
    }

    /// Follow an allocation-table chain from `start`, returning the ordered ids.
    /// Bounded by `max` to guarantee termination even on a corrupt cyclic table.
    fn chain(table: &[u32], start: u32, max: usize) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = start;
        while cur != ENDOFCHAIN && cur != FREESECT && (cur as usize) < table.len() {
            out.push(cur);
            if out.len() > max {
                break; // runaway / cycle guard
            }
            cur = table[cur as usize];
        }
        out
    }

    /// Concatenate the file bytes of every sector id in `ch` (skipping any that
    /// fall outside a truncated image).
    fn read_chain(bytes: &[u8], ch: &[u32], sector_size: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(ch.len().saturating_mul(sector_size));
        for &sid in ch {
            if let Some(off) = sector_offset(sid, sector_size) {
                if let Some(slice) = bytes.get(off..off.saturating_add(sector_size)) {
                    out.extend_from_slice(slice);
                }
            }
        }
        out
    }

    struct Parsed {
        sector_size: usize,
        mini_size: usize,
        mini_cutoff: u32,
        minifat_start: u32,
        total_sectors: usize,
        fat: Vec<u32>,
        dir: Vec<u8>,
    }

    fn parse(bytes: &[u8]) -> Option<Parsed> {
        // --- Header ([MS-CFB] 2.2) ---
        let sector_shift = u16le(bytes, 0x1E)?;
        if !(7..=12).contains(&sector_shift) {
            return None; // implausible sector size
        }
        let sector_size = 1usize << sector_shift;
        let mini_shift = u16le(bytes, 0x20)?;
        if !(2..=12).contains(&mini_shift) {
            return None;
        }
        let mini_size = 1usize << mini_shift;
        let dir_start = u32le(bytes, 0x30)?;
        let mini_cutoff = u32le(bytes, 0x38)?;
        let minifat_start = u32le(bytes, 0x3C)?;
        let difat_start = u32le(bytes, 0x44)?;
        let num_difat = u32le(bytes, 0x48)?;

        let total_sectors = bytes.len() / sector_size;
        let entries_per_sector = sector_size / 4;
        if entries_per_sector == 0 {
            return None;
        }

        // --- DIFAT: 109 inline FAT-sector ids + any chained DIFAT sectors ---
        let mut fat_sectors: Vec<u32> = Vec::new();
        for i in 0..109 {
            if let Some(s) = u32le(bytes, 0x4C + i * 4) {
                if (s as usize) < total_sectors {
                    fat_sectors.push(s);
                }
            }
        }
        let mut difat = difat_start;
        let mut guard = 0usize;
        while difat != ENDOFCHAIN && difat != FREESECT && (difat as usize) < total_sectors {
            guard += 1;
            if guard > total_sectors || guard as u32 > num_difat.saturating_add(1) {
                break;
            }
            let off = sector_offset(difat, sector_size)?;
            for i in 0..entries_per_sector - 1 {
                if let Some(s) = u32le(bytes, off + i * 4) {
                    if (s as usize) < total_sectors {
                        fat_sectors.push(s);
                    }
                }
            }
            difat = u32le(bytes, off + (entries_per_sector - 1) * 4)?;
        }

        // --- FAT: concatenate u32 entries from every FAT sector ---
        let mut fat: Vec<u32> = Vec::with_capacity(fat_sectors.len() * entries_per_sector);
        for &fs in &fat_sectors {
            let off = sector_offset(fs, sector_size)?;
            for i in 0..entries_per_sector {
                fat.push(u32le(bytes, off + i * 4).unwrap_or(FREESECT));
            }
        }
        if fat.is_empty() {
            return None;
        }

        // --- Directory stream (chain from dir_start through the FAT) ---
        let dir = read_chain(
            bytes,
            &chain(&fat, dir_start, total_sectors + 1),
            sector_size,
        );

        Some(Parsed {
            sector_size,
            mini_size,
            mini_cutoff,
            minifat_start,
            total_sectors,
            fat,
            dir,
        })
    }

    fn entry_name(entry: &[u8]) -> Option<String> {
        // name: UTF-16LE, name_len (bytes, incl. the terminating NUL) at 64.
        let name_len = u16le(entry, 64).unwrap_or(0) as usize;
        if !(2..=64).contains(&name_len) {
            return None;
        }
        let mut name = String::new();
        for k in 0..(name_len / 2).saturating_sub(1) {
            match u16le(entry, k * 2).and_then(|u| char::from_u32(u as u32)) {
                Some(c) => name.push(c),
                None => break,
            }
        }
        Some(name)
    }

    pub(super) fn has_streams(bytes: &[u8], names: &[&str]) -> bool {
        let Some(parsed) = parse(bytes) else {
            return false;
        };
        let mut seen = vec![false; names.len()];
        for entry in parsed.dir.chunks_exact(128) {
            if entry.get(66).copied().unwrap_or(0) != 2 {
                continue;
            }
            let Some(name) = entry_name(entry) else {
                continue;
            };
            for (idx, wanted) in names.iter().enumerate() {
                if name == *wanted {
                    seen[idx] = true;
                }
            }
        }
        seen.into_iter().all(|matched| matched)
    }

    pub(super) fn read_workbook_stream(bytes: &[u8]) -> Option<Vec<u8>> {
        let parsed = parse(bytes)?;

        // Linear scan: capture the root entry (object type 5 — anchors the mini
        // stream) and the workbook stream (object type 2, named Workbook/Book).
        let mut root: Option<u32> = None; // root entry's starting sector
        let mut wb: Option<(u32, u64)> = None; // (start sector, byte size)
        let mut wb_is_book = true; // prefer "Workbook" (BIFF8) over "Book" (BIFF5/7)
        for entry in parsed.dir.chunks_exact(128) {
            let obj_type = entry.get(66).copied().unwrap_or(0);
            if obj_type != 2 && obj_type != 5 {
                continue;
            }
            let start = u32le(entry, 116).unwrap_or(ENDOFCHAIN);
            if obj_type == 5 {
                root = Some(start);
                continue;
            }
            let Some(name) = entry_name(entry) else {
                continue;
            };
            let size = u64le(entry, 120).unwrap_or(0);
            if name == "Workbook" {
                wb = Some((start, size));
                wb_is_book = false;
            } else if name == "Book" && wb_is_book {
                wb = Some((start, size));
            }
        }

        let (wb_start, wb_size) = wb?;
        let wb_size = wb_size as usize;
        // The directory `size` field is attacker-controlled; a stream can never be
        // larger than its container, so reject an absurd declared size before any
        // allocation (otherwise a 2.5 KB file can demand a multi-GiB `Vec`).
        if wb_size > bytes.len() {
            return None;
        }

        // Streams >= the mini cutoff live in the regular FAT; smaller ones live
        // in the mini stream (the root entry's chain, carved into mini sectors).
        if wb_size as u64 >= parsed.mini_cutoff as u64 {
            let mut data = read_chain(
                bytes,
                &chain(&parsed.fat, wb_start, parsed.total_sectors + 1),
                parsed.sector_size,
            );
            data.truncate(wb_size);
            Some(data)
        } else {
            let mini_stream = read_chain(
                bytes,
                &chain(&parsed.fat, root?, parsed.total_sectors + 1),
                parsed.sector_size,
            );
            let minifat_bytes = read_chain(
                bytes,
                &chain(&parsed.fat, parsed.minifat_start, parsed.total_sectors + 1),
                parsed.sector_size,
            );
            let minifat: Vec<u32> = minifat_bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            if minifat.is_empty() {
                return None;
            }
            // Grow as mini-sectors are actually appended (bounded by the minifat
            // chain length) — never pre-reserve the attacker-declared `wb_size`.
            let mut data = Vec::new();
            for mid in chain(&minifat, wb_start, minifat.len() + 1) {
                let off = (mid as usize).checked_mul(parsed.mini_size)?;
                if let Some(slice) = mini_stream.get(off..off.saturating_add(parsed.mini_size)) {
                    data.extend_from_slice(slice);
                }
            }
            data.truncate(wb_size);
            Some(data)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_biff_detected_before_not_ole2() {
        // BIFF3 BOF (0x0209) as a raw, non-OLE2 stream.
        assert!(matches!(
            read_workbook_stream(&[0x09, 0x02, 0x06, 0x00, 0, 0, 0, 0]),
            Err(Error::LegacyBiff)
        ));
        // BIFF4 BOF (0x0409).
        assert!(matches!(
            read_workbook_stream(&[0x09, 0x04, 0x06, 0x00, 0, 0, 0, 0]),
            Err(Error::LegacyBiff)
        ));
        // Arbitrary non-OLE2, non-BIFF bytes stay NotOle2.
        assert!(matches!(
            read_workbook_stream(b"not excel at all"),
            Err(Error::NotOle2)
        ));
    }

    #[test]
    fn tolerant_reads_a_valid_cfb_via_fast_path() {
        // A clean cfb-written workbook still goes through the fast path; this
        // guards the fallback wiring against breaking the common case.
        use std::io::{Cursor, Write};
        let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        comp.create_stream("/Workbook")
            .unwrap()
            .write_all(b"hello-biff")
            .unwrap();
        comp.flush().unwrap();
        let bytes = comp.into_inner().into_inner();
        assert_eq!(read_workbook_stream(&bytes).unwrap(), b"hello-biff");
    }

    #[test]
    fn encrypted_package_container_is_reported_before_missing_workbook() {
        use std::io::{Cursor, Write};
        let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        comp.create_stream("/EncryptedPackage")
            .unwrap()
            .write_all(b"encrypted-payload")
            .unwrap();
        comp.create_stream("/EncryptionInfo")
            .unwrap()
            .write_all(b"encryption-info")
            .unwrap();
        comp.flush().unwrap();
        let bytes = comp.into_inner().into_inner();

        let err = read_workbook_stream(&bytes).unwrap_err();
        assert_eq!(err.to_string(), "unsupported encrypted OOXML package");
    }

    #[test]
    fn tolerant_encrypted_package_container_is_reported_before_cfb_error() {
        use std::io::{Cursor, Write};
        let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        comp.create_stream("/EncryptedPackage")
            .unwrap()
            .write_all(b"encrypted-payload")
            .unwrap();
        comp.create_stream("/EncryptionInfo")
            .unwrap()
            .write_all(b"encryption-info")
            .unwrap();
        comp.flush().unwrap();
        let mut bytes = comp.into_inner().into_inner();

        // Corrupt the byte-order mark (offset 0x1C, normally 0xFFFE) so
        // `cfb::open` rejects the file and the tolerant fallback path runs.
        bytes[0x1C] = 0x00;
        bytes[0x1D] = 0x00;

        let err = read_workbook_stream(&bytes).unwrap_err();
        assert_eq!(err.to_string(), "unsupported encrypted OOXML package");
    }

    #[test]
    fn malformed_ole2_container_reports_invalid_cfb_package() {
        let err =
            read_workbook_stream(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid CFB package: not a valid .xls compound file"
        );
    }

    #[test]
    fn rejects_oversized_declared_stream_size_without_aborting() {
        // A directory entry's declared `size` is attacker-controlled. A tiny file
        // that declares a multi-GiB Workbook stream must return an error, never
        // pre-allocate it (regression: `Vec::with_capacity(wb_size)` aborted the
        // process on a ~2.5 KB crafted input — uncatchable by `catch_unwind`).
        use std::io::{Cursor, Write};
        let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        comp.create_stream("/Workbook")
            .unwrap()
            .write_all(b"hi")
            .unwrap();
        comp.flush().unwrap();
        let mut bytes = comp.into_inner().into_inner();
        // Corrupt the byte-order mark (offset 0x1C, normally 0xFFFE) so `cfb::open`
        // rejects the file and the tolerant fallback path runs.
        bytes[0x1C] = 0x00;
        bytes[0x1D] = 0x00;
        // A huge mini-stream cutoff (header 0x38) so the small stream is read via
        // the mini path (where the over-allocation lived).
        bytes[0x38..0x3C].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        // Patch the "Workbook" directory entry's declared size (entry offset +120)
        // to ~2 GiB.
        let needle: Vec<u8> = "Workbook"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let pos = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("Workbook directory entry present");
        bytes[pos + 120..pos + 128].copy_from_slice(&0x7FFF_FFF0u64.to_le_bytes());
        assert!(read_workbook_stream(&bytes).is_err());
    }
}
