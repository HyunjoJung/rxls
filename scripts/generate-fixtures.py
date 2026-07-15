#!/usr/bin/env python3
"""Generate deterministic spreadsheet fixtures for public integration tests."""

from __future__ import annotations

from pathlib import Path
import struct
from zipfile import ZIP_STORED, ZipFile, ZipInfo


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests" / "fixtures"
DOS_EPOCH = (1980, 1, 1, 0, 0, 0)
# Store committed fixture ZIP parts instead of deflating them. Deflated bytes can
# vary across zlib versions, while these tiny fixtures need stable manifest hashes.
FIXTURE_COMPRESSION = ZIP_STORED
PNG_1X1 = bytes.fromhex(
    "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c489"
    "0000000a49444154789c63000100000500010d0a2db40000000049454e44ae426082"
)
CFB_FREE = 0xFFFFFFFF
CFB_END = 0xFFFFFFFE
CFB_FAT = 0xFFFFFFFD
CFB_SECTOR_SIZE = 512
CFB_MINI_STREAM_CUTOFF = 4096


def u16(value: int) -> bytes:
    return value.to_bytes(2, "little")


def u32(value: int) -> bytes:
    return value.to_bytes(4, "little")


def u64(value: int) -> bytes:
    return value.to_bytes(8, "little")


def write_zip(path: Path, parts: list[tuple[str, str | bytes, int]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with ZipFile(path, "w") as zf:
        for name, data, compression in parts:
            info = ZipInfo(name, DOS_EPOCH)
            info.compress_type = compression
            payload = data if isinstance(data, bytes) else data.encode("utf-8")
            zf.writestr(info, payload)


def cfb_directory_entry(
    name: str,
    object_type: int,
    child: int,
    start_sector: int,
    stream_size: int,
) -> bytes:
    entry = bytearray(128)
    name_bytes = name.encode("utf-16le") + b"\x00\x00"
    if len(name_bytes) > 64:
        raise ValueError(f"CFB directory name too long: {name}")
    entry[0 : len(name_bytes)] = name_bytes
    entry[64:66] = u16(len(name_bytes))
    entry[66] = object_type
    entry[67] = 1  # black node; the tiny directory tree is already balanced.
    entry[68:72] = u32(CFB_FREE)
    entry[72:76] = u32(CFB_FREE)
    entry[76:80] = u32(child)
    entry[116:120] = u32(start_sector)
    entry[120:128] = u64(stream_size)
    return bytes(entry)


def write_cfb(path: Path, workbook_stream: bytes, stream_name: str = "Workbook") -> None:
    """Write a minimal deterministic CFB v3 file with one BIFF workbook stream."""

    # Streams below the mini-stream cutoff are normally stored through MiniFAT.
    # Padding the logical Workbook stream to the cutoff keeps the file simpler:
    # the reader sees harmless zero-length BIFF records after EOF.
    stream_size = max(CFB_MINI_STREAM_CUTOFF, len(workbook_stream))
    stream_size = ((stream_size + CFB_SECTOR_SIZE - 1) // CFB_SECTOR_SIZE) * CFB_SECTOR_SIZE
    workbook_payload = workbook_stream.ljust(stream_size, b"\x00")
    workbook_sector_count = stream_size // CFB_SECTOR_SIZE
    fat_sector = 0
    directory_sector = 1
    workbook_start_sector = 2
    total_sectors = workbook_start_sector + workbook_sector_count
    if total_sectors > CFB_SECTOR_SIZE // 4:
        raise ValueError("fixture CFB grew beyond one FAT sector")

    fat = [CFB_FREE] * (CFB_SECTOR_SIZE // 4)
    fat[fat_sector] = CFB_FAT
    fat[directory_sector] = CFB_END
    for offset in range(workbook_sector_count):
        sector = workbook_start_sector + offset
        fat[sector] = CFB_END if offset == workbook_sector_count - 1 else sector + 1
    fat_payload = b"".join(u32(value) for value in fat)

    directory_payload = b"".join(
        [
            cfb_directory_entry("Root Entry", 5, 1, CFB_END, 0),
            cfb_directory_entry(stream_name, 2, CFB_FREE, workbook_start_sector, stream_size),
            bytes(128),
            bytes(128),
        ]
    )

    header = bytearray(CFB_SECTOR_SIZE)
    header[0:8] = bytes.fromhex("d0cf11e0a1b11ae1")
    header[24:26] = u16(0x003E)  # minor version
    header[26:28] = u16(0x0003)  # major version: 512-byte sectors
    header[28:30] = u16(0xFFFE)  # little endian
    header[30:32] = u16(9)  # sector shift
    header[32:34] = u16(6)  # mini sector shift
    header[40:44] = u32(0)  # v3: directory sector count unused
    header[44:48] = u32(1)  # FAT sectors
    header[48:52] = u32(directory_sector)
    header[56:60] = u32(CFB_MINI_STREAM_CUTOFF)
    header[60:64] = u32(CFB_END)
    header[64:68] = u32(0)
    header[68:72] = u32(CFB_END)
    header[72:76] = u32(0)
    header[76:80] = u32(fat_sector)
    for offset in range(1, 109):
        header[76 + offset * 4 : 80 + offset * 4] = u32(CFB_FREE)

    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(header + fat_payload + directory_payload + workbook_payload)


def biff_record(record_type: int, payload: bytes) -> bytes:
    return u16(record_type) + u16(len(payload)) + payload


def biff_bof(stream_type: int) -> bytes:
    return biff_record(0x0809, u16(0x0600) + u16(stream_type) + bytes(12))


def biff8_string(value: str, wide_only: bool = False) -> bytes:
    if not wide_only and all(ord(ch) <= 0xFF for ch in value):
        return u16(len(value)) + b"\x00" + value.encode("latin1")
    return u16(len(value)) + b"\x01" + value.encode("utf-16le")


def biff8_short_string(value: str) -> bytes:
    if len(value) > 255:
        raise ValueError("BIFF short string too long")
    if all(ord(ch) <= 0xFF for ch in value):
        return bytes([len(value), 0x00]) + value.encode("latin1")
    return bytes([len(value), 0x01]) + value.encode("utf-16le")


def biff_boundsheet(name: str, hidden_state: int = 0, sheet_type: int = 0) -> bytes:
    payload = bytearray(4)
    payload.extend(bytes([hidden_state, sheet_type]))
    payload.extend(biff8_short_string(name))
    return biff_record(0x0085, bytes(payload))


def biff_sst(strings: list[str]) -> bytes:
    payload = bytearray()
    payload.extend(u32(len(strings)))
    payload.extend(u32(len(strings)))
    for value in strings:
        payload.extend(biff8_string(value))
    return biff_record(0x00FC, bytes(payload))


def biff_labelsst(row: int, col: int, shared_index: int, style_index: int = 0) -> bytes:
    return biff_record(0x00FD, u16(row) + u16(col) + u16(style_index) + u32(shared_index))


def biff_number(row: int, col: int, value: float, style_index: int = 0) -> bytes:
    return biff_record(
        0x0203,
        u16(row) + u16(col) + u16(style_index) + struct.pack("<d", value),
    )


def biff_defined_name(name: str, formula_tokens: bytes) -> bytes:
    payload = bytearray()
    payload.extend(u16(0))  # flags
    payload.append(0)  # keyboard shortcut
    payload.append(len(name))
    payload.extend(u16(len(formula_tokens)))
    payload.extend(u16(0))  # reserved
    payload.extend(u16(0))  # workbook-global scope
    payload.extend(bytes(4))
    payload.append(0x00)  # compressed BIFF8 name
    payload.extend(name.encode("latin1"))
    payload.extend(formula_tokens)
    return biff_record(0x0018, bytes(payload))


def biff_mergecells(ranges: list[tuple[int, int, int, int]]) -> bytes:
    payload = bytearray()
    payload.extend(u16(len(ranges)))
    for first_row, first_col, last_row, last_col in ranges:
        payload.extend(u16(first_row))
        payload.extend(u16(last_row))
        payload.extend(u16(first_col))
        payload.extend(u16(last_col))
    return biff_record(0x00E5, bytes(payload))


def biff_hlink(first_row: int, first_col: int, last_row: int, last_col: int, url: str) -> bytes:
    payload = bytearray()
    for value in [first_row, last_row, first_col, last_col]:
        payload.extend(u16(value))
    payload.extend(bytes(16))  # StdLink GUID placeholder
    payload.extend(u32(1))  # link options placeholder
    payload.extend(bytes(16))  # URL moniker GUID placeholder
    payload.extend(u32(len(url) + 1))
    payload.extend(url.encode("utf-16le"))
    payload.extend(u16(0))
    return biff_record(0x01B8, bytes(payload))


def biff_note_obj(object_id: int) -> bytes:
    payload = bytearray()
    payload.extend(u16(0x0015))
    payload.extend(u16(0x0012))
    payload.extend(u16(0x0019))
    payload.extend(u16(object_id))
    payload.extend(u16(0))
    payload.extend(bytes(12))
    payload.extend(u16(0x000D))
    payload.extend(u16(0x0016))
    payload.extend(bytes(16))
    payload.extend(u16(0))
    payload.extend(u32(0))
    payload.extend(u16(0))
    payload.extend(u16(0))
    return biff_record(0x005D, bytes(payload))


def biff_txo_records(text: str) -> list[bytes]:
    txo = bytearray()
    txo.extend(u16(0))
    txo.extend(u16(0))
    txo.extend(u16(0))
    txo.extend(u32(0))
    txo.extend(u16(len(text)))
    txo.extend(u16(16))
    txo.extend(u16(0))
    txo.extend(u16(0))

    text_continue = b"\x00" + text.encode("latin1")
    run_continue = bytearray()
    run_continue.extend(u16(0))
    run_continue.extend(u16(0))
    run_continue.extend(u32(0))
    run_continue.extend(u16(len(text)))
    run_continue.extend(u16(0))
    run_continue.extend(u32(0))
    return [
        biff_record(0x01B5, bytes(txo)),
        biff_record(0x003C, text_continue),
        biff_record(0x003C, bytes(run_continue)),
    ]


def biff_note(row: int, col: int, object_id: int, author: str) -> bytes:
    payload = bytearray()
    payload.extend(u16(row))
    payload.extend(u16(col))
    payload.extend(u16(0))
    payload.extend(u16(object_id))
    payload.extend(biff8_string(author))
    payload.append(0)
    return biff_record(0x001C, bytes(payload))


def generate_xls() -> None:
    strings = ["item", "amount", "road", "입찰공고", "secret"]

    xf_plain = bytes(20)
    xf_date = bytearray(20)
    xf_date[2:4] = u16(14)

    workbook = bytearray()
    workbook.extend(biff_bof(0x0005))
    workbook.extend(biff_record(0x0042, u16(949)))
    workbook.extend(biff_record(0x00E0, xf_plain))
    workbook.extend(biff_record(0x00E0, bytes(xf_date)))
    workbook.extend(biff_defined_name("LegacyAnswer", bytes([0x1E, 42, 0])))
    workbook.extend(biff_boundsheet("Data"))
    workbook.extend(biff_boundsheet("Hidden", hidden_state=1))
    workbook.extend(biff_sst(strings))
    workbook.extend(biff_record(0x000A, b""))

    workbook.extend(biff_bof(0x0010))
    workbook.extend(biff_labelsst(0, 0, 0))
    workbook.extend(biff_labelsst(0, 1, 1))
    workbook.extend(biff_labelsst(1, 0, 2))
    workbook.extend(biff_number(1, 1, 42.0))
    workbook.extend(biff_labelsst(2, 0, 3))
    workbook.extend(biff_number(2, 1, 45366.0, style_index=1))
    workbook.extend(biff_mergecells([(3, 0, 3, 2)]))
    workbook.extend(biff_hlink(4, 0, 4, 0, "https://example.com/xls"))
    workbook.extend(biff_note_obj(1025))
    for record in biff_txo_records("legacy review"):
        workbook.extend(record)
    workbook.extend(biff_note(1, 1, 1025, "fixture"))
    workbook.extend(biff_record(0x000A, b""))

    workbook.extend(biff_bof(0x0010))
    workbook.extend(biff_labelsst(0, 0, 4))
    workbook.extend(biff_record(0x000A, b""))

    write_cfb(FIXTURES / "xls" / "reader-basic.xls", bytes(workbook))


def biff5_bof(stream_type: int) -> bytes:
    return biff_record(0x0809, u16(0x0500) + u16(stream_type) + bytes(4))


def biff5_short_string(value: str, encoding: str = "cp949") -> bytes:
    encoded = value.encode(encoding)
    if len(encoded) > 255:
        raise ValueError("BIFF5 short string too long")
    return bytes([len(encoded)]) + encoded


def biff5_label(row: int, col: int, value: str, encoding: str = "cp949") -> bytes:
    encoded = value.encode(encoding)
    return biff_record(
        0x0204,
        u16(row) + u16(col) + u16(0) + u16(len(encoded)) + encoded,
    )


def generate_korean_biff5() -> None:
    """Generate a BIFF5 CP949 derivative of Apache POI's Korean 15556.xls.

    The source workbook is Apache-2.0 licensed. Keeping a tiny deterministic
    derivative in-tree exercises the legacy `Book` stream and CP949 byte-string
    paths without making the test suite depend on the optional public corpus.
    """

    workbook = bytearray()
    workbook.extend(biff5_bof(0x0005))
    workbook.extend(biff_record(0x0042, u16(949)))
    workbook.extend(
        biff_record(
            0x0085,
            bytes(6) + biff5_short_string("작업표"),
        )
    )
    workbook.extend(biff_record(0x000A, b""))

    workbook.extend(biff5_bof(0x0010))
    workbook.extend(biff5_label(0, 0, "조립 작업 표준서"))
    workbook.extend(biff5_label(1, 0, "체결(TIGHTENING)"))
    workbook.extend(biff5_label(2, 0, "클램핑(CLAMPING)"))
    workbook.extend(biff5_label(3, 0, "확인(CONFIRMATION)"))
    workbook.extend(biff_record(0x000A, b""))

    write_cfb(
        FIXTURES / "xls" / "korean-cp949-biff5.xls",
        bytes(workbook),
        stream_name="Book",
    )


def biff12_var_uint(value: int) -> bytes:
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            byte |= 0x80
        out.append(byte)
        if not value:
            return bytes(out)


def biff12_record(record_type: int, payload: bytes) -> bytes:
    return biff12_var_uint(record_type) + biff12_var_uint(len(payload)) + payload


def xlsb_wstr(value: str) -> bytes:
    encoded = value.encode("utf-16le")
    return len(value).to_bytes(4, "little") + encoded


def xlsb_null_wstr() -> bytes:
    return (0xFFFFFFFF).to_bytes(4, "little")


def xlsb_bundle_sheet(name: str, rel_id: str, hidden_state: int) -> bytes:
    payload = bytearray()
    payload.extend(hidden_state.to_bytes(4, "little"))
    payload.extend((0).to_bytes(4, "little"))  # iTabID
    payload.extend(xlsb_wstr(rel_id))
    payload.extend(xlsb_wstr(name))
    return biff12_record(156, bytes(payload))  # BrtBundleSh


def xlsb_sst_item(value: str) -> bytes:
    return biff12_record(19, b"\x00" + xlsb_wstr(value))  # BrtSSTItem


def xlsb_xf(num_format: int) -> bytes:
    payload = bytearray()
    payload.extend((0).to_bytes(2, "little"))  # ixfeParent
    payload.extend(num_format.to_bytes(2, "little"))  # iFmt
    return biff12_record(47, bytes(payload))  # BrtXF


def xlsb_cell_xfs(*records: bytes) -> bytes:
    return b"".join(
        [
            biff12_record(0x0269, len(records).to_bytes(4, "little")),  # BrtBeginCellXFs
            *records,
            biff12_record(0x026A, b""),  # BrtEndCellXFs
        ]
    )


def xlsb_row(row: int) -> bytes:
    return biff12_record(0, row.to_bytes(4, "little"))  # BrtRowHdr


def xlsb_style_ref(style_index: int) -> bytes:
    if not 0 <= style_index <= 0xFFFFFF:
        raise ValueError("XLSB style index must fit in 24 bits")
    return style_index.to_bytes(3, "little") + b"\x00"


def xlsb_cell_isst(col: int, shared_index: int, style_index: int = 0) -> bytes:
    payload = bytearray()
    payload.extend(col.to_bytes(4, "little"))
    payload.extend(xlsb_style_ref(style_index))
    payload.extend(shared_index.to_bytes(4, "little"))
    return biff12_record(7, bytes(payload))  # BrtCellIsst


def xlsb_cell_real(col: int, value: float, style_index: int = 0) -> bytes:
    import struct

    payload = bytearray()
    payload.extend(col.to_bytes(4, "little"))
    payload.extend(xlsb_style_ref(style_index))
    payload.extend(struct.pack("<d", value))
    return biff12_record(5, bytes(payload))  # BrtCellReal


def xlsb_merge_cell(first_row: int, first_col: int, last_row: int, last_col: int) -> bytes:
    payload = bytearray()
    for value in [first_row, last_row, first_col, last_col]:
        payload.extend(value.to_bytes(4, "little"))
    return biff12_record(176, bytes(payload))  # BrtMergeCell


def xlsb_hlink(first_row: int, first_col: int, last_row: int, last_col: int, rel_id: str) -> bytes:
    payload = bytearray()
    for value in [first_row, last_row, first_col, last_col]:
        payload.extend(value.to_bytes(4, "little"))
    payload.extend(xlsb_wstr(rel_id))
    payload.extend(xlsb_wstr(""))  # location
    payload.extend(xlsb_wstr("Open XLSB link"))  # display text
    payload.extend(xlsb_wstr(""))  # tooltip
    return biff12_record(0x01EE, bytes(payload))  # BrtHLink


def xlsb_rich_text(value: str) -> bytes:
    payload = bytearray([0x01])  # fRichStr=1, fExtStr=0
    payload.extend(xlsb_wstr(value))
    payload.extend((0).to_bytes(4, "little"))  # zero StrRun entries
    return bytes(payload)


def xlsb_comments_part(author: str, row: int, col: int, text: str) -> bytes:
    comment = bytearray()
    comment.extend((0).to_bytes(4, "little"))  # iauthor
    for value in [row, row, col, col]:
        comment.extend(value.to_bytes(4, "little"))
    comment.extend(bytes(16))  # guid

    return b"".join(
        [
            biff12_record(628, b""),  # BrtBeginComments
            biff12_record(630, b""),  # BrtBeginCommentAuthors
            biff12_record(632, xlsb_wstr(author)),  # BrtCommentAuthor
            biff12_record(631, b""),  # BrtEndCommentAuthors
            biff12_record(633, b""),  # BrtBeginCommentList
            biff12_record(635, bytes(comment)),  # BrtBeginComment
            biff12_record(637, xlsb_rich_text(text)),  # BrtCommentText
            biff12_record(636, b""),  # BrtEndComment
            biff12_record(634, b""),  # BrtEndCommentList
            biff12_record(629, b""),  # BrtEndComments
        ]
    )


def xlsb_list_part(rel_id: str) -> bytes:
    return biff12_record(550, xlsb_wstr(rel_id))  # BrtListPart


def xlsb_table_part(name: str, columns: list[str], last_row: int, last_col: int) -> bytes:
    begin_list = bytearray()
    for value in [0, last_row, 0, last_col]:
        begin_list.extend(value.to_bytes(4, "little"))  # A1-style range in BIFF12 coords
    begin_list.extend((0).to_bytes(4, "little"))  # lt = LTRANGE
    begin_list.extend((1).to_bytes(4, "little"))  # idList
    begin_list.extend((1).to_bytes(4, "little"))  # crwHeader
    begin_list.extend((0).to_bytes(4, "little"))  # crwTotals
    begin_list.extend((0).to_bytes(4, "little"))  # table flags
    for _ in range(6):
        begin_list.extend((0xFFFFFFFF).to_bytes(4, "little"))  # DXF ids
    begin_list.extend((0).to_bytes(4, "little"))  # dwConnID
    begin_list.extend(xlsb_null_wstr())  # stName
    begin_list.extend(xlsb_wstr(name))  # stDisplayName
    begin_list.extend(xlsb_wstr(""))  # stComment
    begin_list.extend(xlsb_null_wstr())  # stStyleHeader
    begin_list.extend(xlsb_null_wstr())  # stStyleData
    begin_list.extend(xlsb_null_wstr())  # stStyleAgg

    table = bytearray(biff12_record(288, bytes(begin_list)))  # BrtBeginList
    table.extend(biff12_record(293, len(columns).to_bytes(4, "little")))  # BrtBeginListCols
    for idx, caption in enumerate(columns, start=1):
        column = bytearray()
        column.extend(idx.to_bytes(4, "little"))  # idField
        column.extend((0).to_bytes(4, "little"))  # ilta
        column.extend((0xFFFFFFFF).to_bytes(4, "little"))  # nDxfHdr
        column.extend((0xFFFFFFFF).to_bytes(4, "little"))  # nDxfInsertRow
        column.extend((0xFFFFFFFF).to_bytes(4, "little"))  # nDxfAgg
        column.extend((0).to_bytes(4, "little"))  # idqsif
        column.extend(xlsb_null_wstr())  # stName
        column.extend(xlsb_wstr(caption))  # stCaption
        column.extend(xlsb_null_wstr())  # stTotal
        column.extend(xlsb_null_wstr())  # stStyleHeader
        column.extend(xlsb_null_wstr())  # stStyleInsertRow
        column.extend(xlsb_null_wstr())  # stStyleAgg
        table.extend(biff12_record(291, bytes(column)))  # BrtBeginListCol
    style = bytearray((0b100).to_bytes(2, "little"))  # fRowStripes
    style.extend(xlsb_wstr("TableStyleMedium9"))
    table.extend(biff12_record(649, bytes(style)))  # BrtTableStyleClient
    return bytes(table)


def generate_xlsx() -> None:
    write_zip(
        FIXTURES / "xlsx" / "reader-structural.xlsx",
        [
            (
                "[Content_Types].xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/tables/table1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml"/>
  <Override PartName="/xl/comments1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml"/>
  <Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>
  <Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>
</Types>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "_rels/.rels",
                """<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>
</Relationships>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/workbook.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="Data" sheetId="1" r:id="rId1"/>
    <sheet name="Hidden" sheetId="2" state="hidden" r:id="rId2"/>
  </sheets>
  <definedNames>
    <definedName name="NamedTotal">Data!$B$2</definedName>
    <definedName name="_xlnm.Print_Area" localSheetId="0">Data!$A$1:$E$10</definedName>
    <definedName name="_xlnm.Print_Titles" localSheetId="0">Data!$1:$2,Data!$A:$C</definedName>
  </definedNames>
</workbook>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                """<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
</Relationships>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/worksheets/sheet1.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheetPr><tabColor rgb="FF123456"/></sheetPr>
  <sheetViews>
    <sheetView showGridLines="0" showRowColHeaders="0" rightToLeft="1" zoomScale="125" workbookViewId="0">
      <pane xSplit="1" ySplit="1" topLeftCell="B2" activePane="bottomRight" state="frozen"/>
    </sheetView>
  </sheetViews>
  <sheetData>
    <row r="1">
      <c r="A1" t="inlineStr"><is><t>item</t></is></c>
      <c r="B1" t="inlineStr"><is><t>amount</t></is></c>
      <c r="C1" t="inlineStr"><is><t>ok</t></is></c>
    </row>
    <row r="2">
      <c r="A2" t="inlineStr"><is><t>road</t></is></c>
      <c r="B2"><v>12.5</v></c>
      <c r="C2" t="b"><v>1</v></c>
    </row>
    <row r="3">
      <c r="A3" t="inlineStr"><is><t>bridge</t></is></c>
      <c r="B3"><v>7</v></c>
      <c r="C3" t="b"><v>0</v></c>
    </row>
    <row r="4"><c r="A4" t="inlineStr"><is><t>merged</t></is></c></row>
    <row r="5"><c r="A5" t="inlineStr"><is><t>link</t></is></c></row>
  </sheetData>
  <autoFilter ref="A1:C3"/>
  <mergeCells count="1"><mergeCell ref="A4:C4"/></mergeCells>
  <hyperlinks><hyperlink ref="A5" r:id="rId1"/></hyperlinks>
  <printOptions gridLines="1" headings="1" horizontalCentered="1" verticalCentered="1"/>
  <pageMargins left="0.5" right="0.6" top="0.7" bottom="0.8" header="0.2" footer="0.25"/>
  <pageSetup orientation="landscape" paperSize="9" scale="85" fitToWidth="1" fitToHeight="2" firstPageNumber="3" useFirstPageNumber="1"/>
  <headerFooter><oddHeader>&amp;CFixture</oddHeader><oddFooter>&amp;RPage &amp;P</oddFooter></headerFooter>
  <tableParts count="1"><tablePart r:id="rId2"/></tableParts>
</worksheet>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/worksheets/sheet2.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>secret</t></is></c></row></sheetData>
</worksheet>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                """<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/rxls" TargetMode="External"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.xml"/>
</Relationships>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/tables/table1.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="DataTable" displayName="DataTable" ref="A1:C3" totalsRowShown="0">
  <autoFilter ref="A1:C3"/>
  <tableColumns count="3">
    <tableColumn id="1" name="item"/>
    <tableColumn id="2" name="amount"/>
    <tableColumn id="3" name="ok"/>
  </tableColumns>
  <tableStyleInfo name="TableStyleMedium2" showFirstColumn="0" showLastColumn="0" showRowStripes="1" showColumnStripes="0"/>
</table>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/comments1.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<comments xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <authors><author>fixture</author></authors>
  <commentList>
    <comment ref="B2" authorId="0"><text><t>needs review</t></text></comment>
  </commentList>
</comments>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "docProps/core.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/">
  <dc:title>rxls structural fixture</dc:title>
  <dc:creator>rxls fixture generator</dc:creator>
  <dcterms:created>2024-01-01T00:00:00Z</dcterms:created>
</cp:coreProperties>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "docProps/app.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties">
  <Application>rxls</Application>
  <Company>rxls</Company>
</Properties>
""",
                FIXTURE_COMPRESSION,
            ),
        ],
    )


def generate_formula_source() -> None:
    """Create the independent OOXML source converted into the BIFF8 oracle."""
    write_zip(
        FIXTURES / "formula" / "formula-source.xlsx",
        [
            (
                "[Content_Types].xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "_rels/.rels",
                """<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/workbook.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="Calc" sheetId="1" r:id="rId1"/>
    <sheet name="Input Data" sheetId="2" r:id="rId2"/>
  </sheets>
  <definedNames><definedName name="Answer">'Input Data'!$B$3</definedName></definedNames>
</workbook>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                """<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
</Relationships>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/worksheets/sheet1.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
  <row r="1">
    <c r="A1"><v>5</v></c>
    <c r="B1"><f>ABS($A$1)</f><v>5</v></c>
    <c r="C1" t="b"><f>TRUE()</f><v>1</v></c>
    <c r="D1" t="b"><f>FALSE()</f><v>0</v></c>
    <c r="E1"><f>NOW()</f><v>45000</v></c>
  </row>
  <row r="2"><c r="A2"><v>2</v></c><c r="B2"><f>$A$1+A$1+$A1+A1</f><v>20</v></c></row>
  <row r="3"><c r="B3"><f>'Input Data'!$B$3</f><v>7</v></c></row>
  <row r="4"><c r="B4"><f>Answer</f><v>7</v></c></row>
  <row r="5"><c r="A5"><v>3</v></c><c r="B5"><f>A5*2</f><v>6</v></c></row>
  <row r="6"><c r="A6"><v>4</v></c><c r="B6"><f>A6*2</f><v>8</v></c></row>
</sheetData></worksheet>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "xl/worksheets/sheet2.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
  <row r="3"><c r="B3"><v>7</v></c></row>
</sheetData></worksheet>
""",
                FIXTURE_COMPRESSION,
            ),
        ],
    )


def generate_xlsb() -> None:
    workbook = b"".join(
        [
            xlsb_bundle_sheet("Data", "rId1", 0),
            xlsb_bundle_sheet("Hidden", "rId2", 1),
        ]
    )
    shared_strings = b"".join(
        [
            xlsb_sst_item("item"),
            xlsb_sst_item("amount"),
            xlsb_sst_item("road"),
            xlsb_sst_item("reported"),
            xlsb_sst_item("merged"),
            xlsb_sst_item("link"),
        ]
    )
    styles = xlsb_cell_xfs(xlsb_xf(0), xlsb_xf(14))
    sheet1 = b"".join(
        [
            xlsb_row(0),
            xlsb_cell_isst(0, 0),
            xlsb_cell_isst(1, 1),
            xlsb_row(1),
            xlsb_cell_isst(0, 2),
            xlsb_cell_real(1, 42.0),
            xlsb_row(2),
            xlsb_cell_isst(0, 3),
            xlsb_cell_real(1, 45366.0, style_index=1),
            xlsb_row(3),
            xlsb_cell_isst(0, 4),
            xlsb_row(4),
            xlsb_cell_isst(0, 5),
            xlsb_merge_cell(3, 0, 3, 2),
            xlsb_hlink(4, 0, 4, 0, "rId3"),
            xlsb_list_part("rId5"),
        ]
    )
    workbook_rels = """<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.bin"/>
</Relationships>
"""
    sheet_rels = """<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/xlsb" TargetMode="External"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.bin"/>
  <Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.bin"/>
</Relationships>
"""
    comments = xlsb_comments_part("fixture", 1, 1, "binary review")
    table = xlsb_table_part("BinaryTable", ["item", "amount"], 2, 1)

    write_zip(
        FIXTURES / "xlsb" / "reader-basic.xlsb",
        [
            ("xl/workbook.bin", workbook, FIXTURE_COMPRESSION),
            ("xl/_rels/workbook.bin.rels", workbook_rels, FIXTURE_COMPRESSION),
            ("xl/sharedStrings.bin", shared_strings, FIXTURE_COMPRESSION),
            ("xl/styles.bin", styles, FIXTURE_COMPRESSION),
            ("xl/worksheets/sheet1.bin", sheet1, FIXTURE_COMPRESSION),
            ("xl/worksheets/_rels/sheet1.bin.rels", sheet_rels, FIXTURE_COMPRESSION),
            ("xl/comments1.bin", comments, FIXTURE_COMPRESSION),
            ("xl/tables/table1.bin", table, FIXTURE_COMPRESSION),
            ("xl/worksheets/sheet2.bin", b"", FIXTURE_COMPRESSION),
        ],
    )


def generate_ods() -> None:
    write_zip(
        FIXTURES / "ods" / "repeated-hidden.ods",
        [
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet", ZIP_STORED),
            (
                "styles.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0">
  <office:styles>
    <style:style style:name="hidden-table" style:family="table">
      <style:table-properties table:display="false"/>
    </style:style>
  </office:styles>
</office:document-styles>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "content.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink">
  <office:body>
    <office:spreadsheet>
      <table:content-validations>
        <table:content-validation table:name="PositiveAmount" table:condition="cell-content() &gt;= 0" table:allow-empty-cell="false"/>
      </table:content-validations>
      <table:table table:name="Visible" table:print-ranges="$Visible.$A$1:$Visible.$B$6">
        <table:table-header-columns>
          <table:table-column table:number-columns-repeated="2"/>
        </table:table-header-columns>
        <table:table-header-rows>
          <table:table-row>
            <table:table-cell office:value-type="string"><text:p>name</text:p></table:table-cell>
            <table:table-cell office:value-type="string"><text:p>amount</text:p></table:table-cell>
          </table:table-row>
        </table:table-header-rows>
        <table:table-row table:number-rows-repeated="2">
          <table:table-cell office:value-type="string"><text:p>road</text:p></table:table-cell>
          <table:table-cell table:content-validation-name="PositiveAmount" office:value-type="float" office:value="125"><text:p>125</text:p></table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell office:value-type="string" table:number-columns-spanned="2"><text:p>merged</text:p></table:table-cell>
          <table:covered-table-cell/>
        </table:table-row>
        <table:table-row>
          <table:table-cell office:value-type="string"><text:p><text:a xlink:href="https://example.com/ods">link</text:a></text:p><office:annotation><dc:creator>fixture</dc:creator><text:p>verify external link</text:p></office:annotation></table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell office:value-type="string"><text:p>image</text:p></table:table-cell>
          <table:table-cell><draw:frame draw:name="Logo"><draw:image xlink:href="Pictures/logo.png" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/></draw:frame></table:table-cell>
        </table:table-row>
      </table:table>
      <table:table table:name="Hidden" table:style-name="hidden-table">
        <table:table-row>
          <table:table-cell office:value-type="string"><text:p>secret</text:p></table:table-cell>
        </table:table-row>
      </table:table>
      <table:database-ranges>
        <table:database-range table:name="VisibleBlock" table:target-range-address="$Visible.$A$1:$Visible.$B$3" table:display-filter-buttons="true"/>
      </table:database-ranges>
      <table:named-expressions>
        <table:named-range table:name="VisibleTotal" table:cell-range-address="$Visible.$B$2"/>
      </table:named-expressions>
    </office:spreadsheet>
  </office:body>
</office:document-content>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "meta.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0">
  <office:meta>
    <dc:title>rxls ODS fixture</dc:title>
    <meta:initial-creator>rxls fixture generator</meta:initial-creator>
    <dc:creator>rxls fixture reviewer</dc:creator>
  </office:meta>
</office:document-meta>
""",
                FIXTURE_COMPRESSION,
            ),
            (
                "settings.xml",
                """<?xml version="1.0" encoding="UTF-8"?>
<office:document-settings xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:config="urn:oasis:names:tc:opendocument:xmlns:config:1.0">
  <office:settings>
    <config:config-item-set config:name="ooo:view-settings">
      <config:config-item-map-indexed config:name="Views">
        <config:config-item-map-entry>
          <config:config-item-map-named config:name="Tables">
            <config:config-item-map-entry config:name="Visible">
              <config:config-item config:name="HorizontalSplitMode" config:type="short">2</config:config-item>
              <config:config-item config:name="VerticalSplitMode" config:type="short">2</config:config-item>
              <config:config-item config:name="HorizontalSplitPosition" config:type="int">1</config:config-item>
              <config:config-item config:name="VerticalSplitPosition" config:type="int">1</config:config-item>
              <config:config-item config:name="PositionRight" config:type="int">1</config:config-item>
              <config:config-item config:name="PositionBottom" config:type="int">1</config:config-item>
              <config:config-item config:name="ZoomValue" config:type="short">125</config:config-item>
              <config:config-item config:name="ShowGrid" config:type="boolean">false</config:config-item>
              <config:config-item config:name="HasColumnRowHeaders" config:type="boolean">false</config:config-item>
            </config:config-item-map-entry>
          </config:config-item-map-named>
          <config:config-item config:name="ActiveTable" config:type="string">Visible</config:config-item>
        </config:config-item-map-entry>
      </config:config-item-map-indexed>
    </config:config-item-set>
  </office:settings>
</office:document-settings>
""",
                FIXTURE_COMPRESSION,
            ),
            ("Pictures/logo.png", PNG_1X1, FIXTURE_COMPRESSION),
        ],
    )


def main() -> None:
    generate_xls()
    generate_korean_biff5()
    generate_xlsx()
    generate_ods()
    generate_formula_source()
    generate_xlsb()


if __name__ == "__main__":
    main()
