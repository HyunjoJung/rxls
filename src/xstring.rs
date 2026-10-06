//! SpreadsheetML cell-text ST_Xstring codec (ECMA-376 §22.9.2.19).
//!
//! Use after XML decoding and once per text element, never for formula syntax
//! or arbitrary XML. Office's interoperability notes require CR escaping and
//! protection of literal `_xHHHH_` underscores:
//! https://learn.microsoft.com/en-us/openspecs/office_standards/ms-oi29500/d34ae755-c53f-4a44-a363-c6dd3ee018a4

use std::borrow::Cow;

fn hex_unit(bytes: &[u8]) -> Option<u16> {
    bytes.get(..4)?.iter().try_fold(0_u16, |unit, &byte| {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        Some((unit << 4) | u16::from(digit))
    })
}

fn escape_unit(bytes: &[u8]) -> Option<u16> {
    let token = bytes.get(..7)?;
    (token.starts_with(b"_x") && token[6] == b'_')
        .then(|| hex_unit(&token[2..6]))
        .flatten()
}

pub(crate) fn decode(text: &str) -> Cow<'_, str> {
    if !text.contains("_x") {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(unit) = escape_unit(&bytes[cursor..]) {
            let end = cursor + 7;
            if (0xD800..=0xDBFF).contains(&unit) {
                if let Some(low) =
                    escape_unit(&bytes[end..]).filter(|low| (0xDC00..=0xDFFF).contains(low))
                {
                    let scalar =
                        0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                    if let Some(character) = char::from_u32(scalar) {
                        out.push(character);
                        cursor = end + 7;
                        continue;
                    }
                }
            }
            if let Some(character) = char::from_u32(u32::from(unit)) {
                out.push(character);
            } else {
                // Rust strings cannot represent lone UTF-16 surrogates.
                out.push_str(&text[cursor..end]);
            }
            cursor = end;
        } else if let Some(character) = text[cursor..].chars().next() {
            out.push(character);
            cursor += character.len_utf8();
        }
    }
    Cow::Owned(out)
}

enum XmlChunk<'a> {
    Text(&'a str),
    Escape(u16),
}

// Both length accounting and emission consume the original input with the
// same scanner. Generated escapes are never scanned or escaped again.
fn xml_chunks(text: &str, mut visit: impl FnMut(XmlChunk<'_>)) {
    let mut cursor = 0;
    while cursor < text.len() {
        if escape_unit(&text.as_bytes()[cursor..]).is_some() {
            // Protect each original token-start underscore, including one that
            // also closes an overlapping token. Excel scans the remaining
            // original wire text after decoding a seven-byte escape.
            visit(XmlChunk::Text("_x005F_"));
            cursor += 1;
            continue;
        }
        if let Some(character) = text[cursor..].chars().next() {
            let end = cursor + character.len_utf8();
            visit(match character {
                '&' => XmlChunk::Text("&amp;"),
                '<' => XmlChunk::Text("&lt;"),
                '>' => XmlChunk::Text("&gt;"),
                '\r' | '\u{FFFE}' | '\u{FFFF}' => XmlChunk::Escape(character as u16),
                c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n') => XmlChunk::Escape(c as u16),
                _ => XmlChunk::Text(&text[cursor..end]),
            });
            cursor = end;
        }
    }
}

pub(crate) fn escaped_xml_len(text: &str) -> usize {
    let mut length = 0_usize;
    xml_chunks(text, |chunk| {
        length = length.saturating_add(match chunk {
            XmlChunk::Text(text) => text.len(),
            XmlChunk::Escape(_) => 7,
        });
    });
    length
}

pub(crate) fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(escaped_xml_len(text));
    xml_chunks(text, |chunk| match chunk {
        XmlChunk::Text(text) => out.push_str(text),
        XmlChunk::Escape(unit) => {
            out.push_str("_x");
            for shift in [12, 8, 4, 0] {
                out.push(b"0123456789ABCDEF"[usize::from((unit >> shift) & 15)] as char);
            }
            out.push('_');
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::{decode, escape_xml, escaped_xml_len};

    #[test]
    fn protected_and_adjacent_tokens_are_consumed_once() {
        for (encoded, decoded) in [
            ("_x005F_x0041_", "_x0041_"),
            ("_x005F_x005F_x0041_", "_x005FA"),
            ("_x005F_x005F_x005F_x0041_", "_x005F_x0041_"),
            ("_x005F_x0041_x0042_", "_x0041B"),
            ("_x0041_x0042_", "Ax0042_"),
            ("_x0041__x0042_", "AB"),
            ("_x005F_xD83D__xDE00_", "_xD83D__xDE00_"),
            ("_xD83D__xDE00_", "😀"),
            ("_xD800_ _xDC00_", "_xD800_ _xDC00_"),
            ("_xZZZZ_ _x123_ _X0041_", "_xZZZZ_ _x123_ _X0041_"),
        ] {
            assert_eq!(decode(encoded), decoded);
        }
    }

    #[test]
    fn encoding_preserves_overlapping_literals_and_exact_length_accounting() {
        for (text, encoded) in [
            ("_x0041_", "_x005F_x0041_"),
            ("_x005F_x0041_", "_x005F_x005F_x005F_x0041_"),
            ("_x0041_x0042_", "_x005F_x0041_x005F_x0042_"),
            ("a\0\r\u{1}\u{FFFF}", "a_x0000__x000D__x0001__xFFFF_"),
            ("한글😀\n\t&<>", "한글😀\n\t&amp;&lt;&gt;"),
        ] {
            assert_eq!(escape_xml(text), encoded);
            assert_eq!(escaped_xml_len(text), encoded.len());
            // XML entities are tested above; decode expects XML-decoded text.
            if !text.contains(['&', '<', '>']) {
                assert_eq!(decode(encoded), text);
            }
        }
    }
}
