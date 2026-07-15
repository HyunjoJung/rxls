#!/usr/bin/env python3
"""Generate a deterministic medium-sized XLSX for release performance gates."""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import sys
import zipfile


MIB = 1024 * 1024
MIN_ARCHIVE_BYTES = 10 * MIB
MAX_ARCHIVE_BYTES = 50 * MIB
DEFAULT_PAYLOAD_MIB = 16
ZIP_TIMESTAMP = (1980, 1, 1, 0, 0, 0)
PAYLOAD_ALPHABET = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
PAYLOAD_BLOCK_BYTES = 64 * 1024


def deterministic_payload_chunk() -> bytes:
    """Return stable, XML-safe data that does not collapse under DEFLATE."""
    blocks = bytearray()
    seed = b"rxls-medium-performance-fixture-v1\0"
    for counter in range(PAYLOAD_BLOCK_BYTES // hashlib.sha256().digest_size):
        blocks.extend(hashlib.sha256(seed + counter.to_bytes(4, "big")).digest())
    return bytes(PAYLOAD_ALPHABET[value & 63] for value in blocks)


PAYLOAD_CHUNK = deterministic_payload_chunk()

CONTENT_TYPES = b"""<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>
"""
ROOT_RELS = b"""<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml" Target="customXml/item1.xml"/>
</Relationships>
"""
WORKBOOK = b"""<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets>
</workbook>
"""
WORKBOOK_RELS = b"""<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>
"""
WORKSHEET = b"""<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>medium performance fixture</t></is></c></row>
    <row r="2"><c r="A2"><v>42</v></c></row>
  </sheetData>
</worksheet>
"""
CUSTOM_XML_PREFIX = (
    b'<?xml version="1.0" encoding="UTF-8"?>\n'
    b'<rxlsPerformancePayload xmlns="urn:rxls:performance">'
)
CUSTOM_XML_SUFFIX = b"</rxlsPerformancePayload>\n"
CUSTOM_XML_BLOCK_PREFIX = b"<block>"
CUSTOM_XML_BLOCK_SUFFIX = b"</block>"


def zip_info(name: str) -> zipfile.ZipInfo:
    """Return stable ZIP metadata for one generated package member."""
    info = zipfile.ZipInfo(name, ZIP_TIMESTAMP)
    info.compress_type = zipfile.ZIP_STORED
    info.create_system = 3
    info.external_attr = 0o100644 << 16
    return info


def generate(output: Path, payload_mib: int = DEFAULT_PAYLOAD_MIB) -> int:
    """Write a valid deterministic XLSX and return its archive size."""
    if not 10 <= payload_mib < 50:
        raise ValueError("payload size must be at least 10 MiB and below 50 MiB")
    output.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(output, "w", allowZip64=False) as archive:
        for name, data in (
            ("[Content_Types].xml", CONTENT_TYPES),
            ("_rels/.rels", ROOT_RELS),
            ("xl/workbook.xml", WORKBOOK),
            ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
            ("xl/worksheets/sheet1.xml", WORKSHEET),
        ):
            archive.writestr(zip_info(name), data)
        with archive.open(zip_info("customXml/item1.xml"), "w") as payload:
            payload.write(CUSTOM_XML_PREFIX)
            for _ in range(payload_mib * (MIB // PAYLOAD_BLOCK_BYTES)):
                payload.write(CUSTOM_XML_BLOCK_PREFIX)
                payload.write(PAYLOAD_CHUNK)
                payload.write(CUSTOM_XML_BLOCK_SUFFIX)
            payload.write(CUSTOM_XML_SUFFIX)

    size = output.stat().st_size
    if not MIN_ARCHIVE_BYTES <= size <= MAX_ARCHIVE_BYTES:
        output.unlink(missing_ok=True)
        raise ValueError(
            f"generated archive is {size} bytes; expected a 10-50 MiB workbook"
        )
    return size


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--payload-mib", type=int, default=DEFAULT_PAYLOAD_MIB)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        size = generate(args.output, args.payload_mib)
    except (OSError, ValueError, zipfile.BadZipFile) as error:
        print(f"generate-performance-fixture: {error}", file=sys.stderr)
        return 2
    print(
        f"performance fixture: {args.output.as_posix()} bytes={size} "
        f"payload_mib={args.payload_mib}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
