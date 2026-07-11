//! Shared String Table (SST) decoding.
//!
//! The SST record (`0x00FC`) holds every distinct string in a BIFF8 workbook,
//! referenced by index from `LABELSST` cells. Large SSTs overflow the 8224-byte
//! record cap and continue into `CONTINUE` (`0x003C`) records — and a single
//! `XLUnicodeRichExtendedString` can be split across that boundary, with the
//! compression flag (`fHighByte`) **re-read** as the first byte of the next
//! chunk. This module handles that correctly.
//!
//! Reference: [MS-XLS] 2.4.265 (SST), 2.5.293 (XLUnicodeRichExtendedString).

/// A cursor that walks a sequence of record chunks (the SST body followed by
/// each CONTINUE body) as one logical byte stream.
struct Cursor<'a> {
    chunks: &'a [&'a [u8]],
    ci: usize,
    off: usize,
}

impl<'a> Cursor<'a> {
    fn new(chunks: &'a [&'a [u8]]) -> Self {
        Self {
            chunks,
            ci: 0,
            off: 0,
        }
    }

    /// Read one byte, advancing across chunk boundaries automatically.
    fn u8(&mut self) -> Option<u8> {
        while self.ci < self.chunks.len() {
            let chunk = self.chunks[self.ci];
            if self.off < chunk.len() {
                let b = chunk[self.off];
                self.off += 1;
                return Some(b);
            }
            self.ci += 1;
            self.off = 0;
        }
        None
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes([self.u8()?, self.u8()?]))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes([
            self.u8()?,
            self.u8()?,
            self.u8()?,
            self.u8()?,
        ]))
    }

    fn skip(&mut self, mut n: usize) {
        while n > 0 && self.u8().is_some() {
            n -= 1;
        }
    }

    /// Read `cch` code units, honoring the CONTINUE-boundary rule that the
    /// compression flag (`grbit`) is **re-read** as the first byte of each
    /// continuation chunk. `grbit` is the flag in effect on entry.
    fn read_units(&mut self, cch: usize, mut grbit: u8) -> Vec<u16> {
        let mut units: Vec<u16> = Vec::with_capacity(cch.min(1 << 20));
        let mut read = 0usize;
        while read < cch {
            // Cross into the next chunk if the current one is exhausted; the
            // first byte after a split re-specifies the compression flag.
            while self.ci < self.chunks.len() && self.off >= self.chunks[self.ci].len() {
                self.ci += 1;
                self.off = 0;
                if self.ci < self.chunks.len() && !self.chunks[self.ci].is_empty() {
                    grbit = self.chunks[self.ci][0];
                    self.off = 1;
                }
            }
            if self.ci >= self.chunks.len() {
                break;
            }
            if grbit & 0x01 != 0 {
                // fHighByte: uncompressed UTF-16LE (2 bytes/char). The split is
                // on a character boundary, so both bytes are in this chunk.
                let (Some(lo), Some(hi)) = (self.u8(), self.u8()) else {
                    break;
                };
                units.push(u16::from_le_bytes([lo, hi]));
            } else {
                // Compressed: 1 byte = the low byte of the code unit (Latin-1).
                match self.u8() {
                    Some(b) => units.push(u16::from(b)),
                    None => break,
                }
            }
            read += 1;
        }
        units
    }

    /// Read one `XLUnicodeRichExtendedString` (an SST entry), handling CONTINUE
    /// splits and skipping its trailing rich-run / phonetic data.
    fn read_string(&mut self) -> Option<String> {
        let cch = self.u16()? as usize;
        let grbit = self.u8()?;
        let rich = if grbit & 0x08 != 0 {
            self.u16()? as usize
        } else {
            0
        };
        let ext = if grbit & 0x04 != 0 {
            self.u32()? as usize
        } else {
            0
        };
        let units = self.read_units(cch, grbit);
        // Rich-formatting runs (4 bytes each) and extended (phonetic) data
        // trail the character array and never carry a continuation grbit.
        self.skip(rich.saturating_mul(4));
        self.skip(ext);
        Some(String::from_utf16_lossy(&units))
    }

    /// Read one plain `XLUnicodeString` (`cch` + `grbit` + chars, no rich/ext
    /// run tables), handling CONTINUE splits. Used for `LABEL`/`STRING` cells.
    fn read_plain(&mut self) -> Option<String> {
        let cch = self.u16()? as usize;
        let grbit = self.u8()?;
        Some(String::from_utf16_lossy(&self.read_units(cch, grbit)))
    }
}

/// Parse the SST into its ordered list of unique strings.
///
/// `chunks[0]` is the SST record body (which begins with `cstTotal:u32` and
/// `cstUnique:u32`); `chunks[1..]` are the bodies of the following `CONTINUE`
/// records.
pub(crate) fn parse(chunks: &[&[u8]]) -> Vec<String> {
    let mut cur = Cursor::new(chunks);
    let _total = cur.u32();
    let unique = cur.u32().unwrap_or(0) as usize;
    let mut out = Vec::with_capacity(unique.min(1 << 20));
    for _ in 0..unique {
        match cur.read_string() {
            Some(s) => out.push(s),
            None => break,
        }
    }
    out
}

/// Read one plain `XLUnicodeString` that may span CONTINUE records, starting
/// `skip` bytes into the first chunk (to step over a cell-record header). The
/// chunk list is the record body followed by each following CONTINUE body.
/// Used by `LABEL`/`STRING` cell decoding in BIFF8.
pub(crate) fn read_continued_plain(chunks: &[&[u8]], skip: usize) -> Option<String> {
    let mut cur = Cursor::new(chunks);
    cur.skip(skip);
    cur.read_plain()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_and_uncompressed_strings() {
        // SST header: cstTotal=2, cstUnique=2.
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&2u32.to_le_bytes());
        // string 1: "AB" compressed (cch=2, grbit=0)
        body.extend_from_slice(&2u16.to_le_bytes());
        body.push(0x00);
        body.extend_from_slice(b"AB");
        // string 2: "가" uncompressed (cch=1, grbit=1, UTF-16LE)
        body.extend_from_slice(&1u16.to_le_bytes());
        body.push(0x01);
        body.extend_from_slice(&('가' as u16).to_le_bytes());

        let strings = parse(&[&body]);
        assert_eq!(strings, vec!["AB".to_string(), "가".to_string()]);
    }

    #[test]
    fn string_split_across_continue_reuses_grbit() {
        // One uncompressed 4-char string "ABCD" split after 2 chars; the second
        // chunk begins with a fresh grbit byte (still uncompressed).
        let mut head = Vec::new();
        head.extend_from_slice(&1u32.to_le_bytes()); // cstTotal
        head.extend_from_slice(&1u32.to_le_bytes()); // cstUnique
        head.extend_from_slice(&4u16.to_le_bytes()); // cch = 4
        head.push(0x01); // grbit uncompressed
        head.extend_from_slice(&('A' as u16).to_le_bytes());
        head.extend_from_slice(&('B' as u16).to_le_bytes());

        let mut cont = vec![0x01u8]; // continuation grbit
        cont.extend_from_slice(&('C' as u16).to_le_bytes());
        cont.extend_from_slice(&('D' as u16).to_le_bytes());

        let strings = parse(&[&head, &cont]);
        assert_eq!(strings, vec!["ABCD".to_string()]);
    }
}
