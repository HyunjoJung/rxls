#!/usr/bin/env python3
"""Generate the deterministic, project-owned spreadsheet render corpus.

The generated payload is deliberately local-only.  The CLI refuses to write
outside ``local/render-corpus-generated`` so large binary corpora cannot be
accidentally added to the repository.  Every workbook is built from
project-authored strings and format primitives; no external templates or
downloaded assets are used.

Examples::

    python3 scripts/generate-render-corpus.py --list --profile pilot
    python3 scripts/generate-render-corpus.py --generate --profile pilot
    python3 scripts/generate-render-corpus.py --verify --profile pilot
    python3 scripts/generate-render-corpus.py --generate --profile full
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from hashlib import sha256
from html import escape
import io
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import struct
import tempfile
from typing import Callable, Iterable
import zlib
from zipfile import ZIP_STORED, ZipFile, ZipInfo


ROOT = Path(__file__).resolve().parents[1]
OUTPUT_BASE = ROOT / "local" / "render-corpus-generated"
MANIFEST_NAME = "manifest.json"

SCHEMA_VERSION = 1
GENERATOR = "rxls-synthetic-render-corpus"
GENERATOR_VERSION = "1.3.0"
LICENSE = "MIT"
REDISTRIBUTION = "allowed"

FORMATS = ("xls", "xlsx", "xlsb", "ods")
PROFILE_COUNTS = {"pilot": 10, "full": 200}
# The pilot is a reviewed sample of the first complete 128-row orthogonal
# lattice.  Index 24 replaces the redundant index 9 in every format: this
# retains every claimed feature while providing ten workbooks without any of
# the broad-only rendering features used by the absolute fidelity gate.  The
# full profile deliberately remains the unmodified 0..199 sequence.
PILOT_INDICES = {
    fmt: (0, 1, 2, 3, 4, 5, 6, 7, 8, 24) for fmt in FORMATS
}
EXTENSIONS = {fmt: f".{fmt}" for fmt in FORMATS}
SEED_BASE = {"xls": 110_000, "xlsx": 220_000, "xlsb": 330_000, "ods": 440_000}
CUSTOM_DATE_FORMAT_ID = 164
CUSTOM_DATE_FORMAT_CODE = "yyyy-mm-dd"
ODS_DEFAULT_ROW_HEIGHT = "15pt"
ODS_WRAPPED_ROW_HEIGHT = "45pt"

# Generation and verification caps are intentionally well above the tiny
# authored workbooks while remaining bounded if this script is changed later.
MAX_CASES = 800
MAX_CASE_BYTES = 2 * 1024 * 1024
MAX_TOTAL_BYTES = 64 * 1024 * 1024
MAX_MANIFEST_BYTES = 4 * 1024 * 1024
MAX_TEXT_CHARS = 4_096
MAX_ZIP_PARTS = 32
MAX_PACKAGE_RELATIONSHIPS = 16
MAX_IMAGE_BYTES = 64 * 1024
MAX_DRAWING_OBJECTS = 4
MAX_CHART_POINTS = 64
PAIRWISE_PERIOD = 128

DOS_EPOCH = (1980, 1, 1, 0, 0, 0)
CFB_FREE = 0xFFFFFFFF
CFB_END = 0xFFFFFFFE
CFB_FAT = 0xFFFFFFFD
CFB_SECTOR_SIZE = 512
CFB_MINI_STREAM_CUTOFF = 4096


class CorpusError(RuntimeError):
    """Raised when generation or verification violates a corpus contract."""


@dataclass(frozen=True)
class CaseSpec:
    format: str
    index: int
    seed: int
    case_id: str
    features: tuple[str, ...]

    @property
    def relative_path(self) -> str:
        return f"payload/{self.format}/{self.case_id}{EXTENSIONS[self.format]}"


# Every case has a small visible baseline.  The remaining features are selected
# by a binary orthogonal array: feature N uses the parity of a distinct nonzero
# seven-bit mask.  Across the first 128 rows every pair of lattice features has
# all four on/off combinations exactly 32 times.  The 200-row full profile
# therefore gives every lattice feature and pair a substantial sample without
# pretending that every workbook exercises every rendering primitive.
BASE_FEATURES = {
    "xls": ("latin-text", "noto-ofl-font", "number-cell"),
    "xlsx": ("latin-text", "noto-ofl-font", "number-cell"),
    "xlsb": ("latin-text", "number-cell"),
    "ods": ("latin-text", "noto-ofl-font", "number-cell"),
}

COMMON_LATTICE_FEATURES = (
    "chinese-text",
    "column-width",
    "date-format",
    "formula-cached",
    "hidden-column",
    "hidden-row",
    "japanese-text",
    "korean-text",
    "merged-cells",
    "percent-format",
    "print-settings",
    "row-height",
    "rtl-text",
)

FORMAT_LATTICE_FEATURES = {
    "xls": COMMON_LATTICE_FEATURES,
    "xlsx": COMMON_LATTICE_FEATURES
    + (
        "border",
        "cell-fill",
        "chart",
        "conditional-format",
        "image-drawing",
        "right-to-left-layout",
        "sparkline",
        "wrapped-text",
    ),
    "xlsb": COMMON_LATTICE_FEATURES,
    "ods": COMMON_LATTICE_FEATURES
    + (
        "border",
        "cell-fill",
        "right-to-left-layout",
        "wrapped-text",
    ),
}

DERIVED_FEATURES = ("unicode-text",)
FORMAT_FEATURES = {
    fmt: tuple(
        sorted(BASE_FEATURES[fmt] + FORMAT_LATTICE_FEATURES[fmt] + DERIVED_FEATURES)
    )
    for fmt in FORMATS
}

# Avoid high-bit-only masks so every feature is represented in the ten-case
# pilot as well as in the mathematically complete 128-row lattice prefix.
PAIRWISE_MASKS = tuple(
    mask for mask in range(1, PAIRWISE_PERIOD) if mask & 0x0F
)
if max(len(features) for features in FORMAT_LATTICE_FEATURES.values()) > len(
    PAIRWISE_MASKS
):
    raise RuntimeError("not enough pairwise masks for the feature lattice")


def _lattice_enabled(index: int, ordinal: int) -> bool:
    """Return a deterministic orthogonal-array bit for one feature."""

    mask = PAIRWISE_MASKS[ordinal]
    return ((index % PAIRWISE_PERIOD) & mask).bit_count() % 2 == 0


def case_features(fmt: str, index: int) -> tuple[str, ...]:
    if fmt not in FORMATS or index < 0 or index >= PROFILE_COUNTS["full"]:
        raise CorpusError(f"invalid feature lattice coordinate: {fmt}/{index}")
    features = set(BASE_FEATURES[fmt])
    for ordinal, feature in enumerate(FORMAT_LATTICE_FEATURES[fmt]):
        if _lattice_enabled(index, ordinal):
            features.add(feature)
    if features.intersection(
        {"chinese-text", "japanese-text", "korean-text", "rtl-text"}
    ):
        features.add("unicode-text")
    return tuple(sorted(features))


def _has(spec: CaseSpec, feature: str) -> bool:
    return feature in spec.features


def _feature_counts(specs: Iterable[CaseSpec]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for spec in specs:
        for feature in spec.features:
            counts[feature] = counts.get(feature, 0) + 1
    return dict(sorted(counts.items()))


def _format_feature_counts(specs: Iterable[CaseSpec]) -> dict[str, dict[str, int]]:
    rows = list(specs)
    return {
        fmt: _feature_counts(spec for spec in rows if spec.format == fmt)
        for fmt in FORMATS
    }


def _validate_full_lattice(specs: Iterable[CaseSpec]) -> None:
    rows = list(specs)
    for fmt in FORMATS:
        format_rows = [spec for spec in rows if spec.format == fmt]
        for feature in FORMAT_FEATURES[fmt]:
            count = sum(feature in spec.features for spec in format_rows)
            if count < 25:
                raise CorpusError(
                    f"feature bucket under 25 cases: {fmt}/{feature}={count}"
                )
        lattice = FORMAT_LATTICE_FEATURES[fmt]
        for left_index, left in enumerate(lattice):
            for right in lattice[left_index + 1 :]:
                combinations = {
                    (left in spec.features, right in spec.features)
                    for spec in format_rows[:PAIRWISE_PERIOD]
                }
                if len(combinations) != 4:
                    raise CorpusError(
                        f"incomplete pairwise coverage: {fmt}/{left}/{right}"
                    )


def _case_texts(spec: CaseSpec) -> tuple[str, ...]:
    suffix = f"{spec.case_id} seed {spec.seed}"
    values = (
        f"Latin render case {suffix}",
        f"한국어 렌더링 사례 {spec.index:04d}"
        if _has(spec, "korean-text")
        else f"Cell B {spec.index:04d}",
        f"日本語レンダリング事例 {spec.index:04d}"
        if _has(spec, "japanese-text")
        else f"Cell C {spec.index:04d}",
        f"中文渲染案例 {spec.index:04d}"
        if _has(spec, "chinese-text")
        else f"Cell D {spec.index:04d}",
        f"مرحبا بالعالم {spec.index:04d}"
        if _has(spec, "rtl-text")
        else f"Cell E {spec.index:04d}",
        f"Wrapped project-authored text for {suffix}",
        f"Merged {spec.case_id}",
        f"Hidden {spec.case_id}",
    )
    if sum(len(value) for value in values) > MAX_TEXT_CHARS:
        raise CorpusError(f"text cap exceeded by {spec.case_id}")
    return values


def profile_specs(profile: str) -> list[CaseSpec]:
    try:
        per_format = PROFILE_COUNTS[profile]
    except KeyError as exc:
        raise CorpusError(f"unknown profile: {profile}") from exc
    profile_indices = {
        fmt: PILOT_INDICES[fmt] if profile == "pilot" else tuple(range(per_format))
        for fmt in FORMATS
    }
    specs = [
        CaseSpec(
            format=fmt,
            index=index,
            seed=SEED_BASE[fmt] + index,
            case_id=f"{fmt}-{index:04d}",
            features=case_features(fmt, index),
        )
        for fmt in FORMATS
        for index in profile_indices[fmt]
    ]
    if len(specs) > MAX_CASES:
        raise CorpusError(f"case cap exceeded: {len(specs)} > {MAX_CASES}")
    if per_format >= PAIRWISE_PERIOD:
        _validate_full_lattice(specs)
    return specs


def _zip_bytes(parts: Iterable[tuple[str, str | bytes]]) -> bytes:
    parts = list(parts)
    if len(parts) > MAX_ZIP_PARTS:
        raise CorpusError(f"ZIP part cap exceeded: {len(parts)} > {MAX_ZIP_PARTS}")
    seen: set[str] = set()
    output = io.BytesIO()
    with ZipFile(output, "w") as archive:
        for name, body in parts:
            pure = PurePosixPath(name)
            if pure.is_absolute() or ".." in pure.parts or name in seen:
                raise CorpusError(f"unsafe or duplicate ZIP part: {name}")
            seen.add(name)
            info = ZipInfo(name, DOS_EPOCH)
            info.compress_type = ZIP_STORED
            info.create_system = 0
            info.external_attr = 0
            payload = body.encode("utf-8") if isinstance(body, str) else body
            archive.writestr(info, payload)
    return output.getvalue()


def _u16(value: int) -> bytes:
    return value.to_bytes(2, "little")


def _u32(value: int) -> bytes:
    return value.to_bytes(4, "little")


def _u64(value: int) -> bytes:
    return value.to_bytes(8, "little")


def _cfb_directory_entry(
    name: str, object_type: int, child: int, start_sector: int, stream_size: int
) -> bytes:
    entry = bytearray(128)
    name_bytes = name.encode("utf-16le") + b"\x00\x00"
    if len(name_bytes) > 64:
        raise CorpusError(f"CFB directory name too long: {name}")
    entry[: len(name_bytes)] = name_bytes
    entry[64:66] = _u16(len(name_bytes))
    entry[66] = object_type
    entry[67] = 1
    entry[68:72] = _u32(CFB_FREE)
    entry[72:76] = _u32(CFB_FREE)
    entry[76:80] = _u32(child)
    entry[116:120] = _u32(start_sector)
    entry[120:128] = _u64(stream_size)
    return bytes(entry)


def _cfb_bytes(workbook_stream: bytes) -> bytes:
    stream_size = max(CFB_MINI_STREAM_CUTOFF, len(workbook_stream))
    stream_size = (
        (stream_size + CFB_SECTOR_SIZE - 1) // CFB_SECTOR_SIZE * CFB_SECTOR_SIZE
    )
    workbook_payload = workbook_stream.ljust(stream_size, b"\x00")
    workbook_sectors = stream_size // CFB_SECTOR_SIZE
    fat_sector = 0
    directory_sector = 1
    workbook_start = 2
    total_sectors = workbook_start + workbook_sectors
    if total_sectors > CFB_SECTOR_SIZE // 4:
        raise CorpusError("CFB workbook exceeds the one-FAT-sector cap")

    fat = [CFB_FREE] * (CFB_SECTOR_SIZE // 4)
    fat[fat_sector] = CFB_FAT
    fat[directory_sector] = CFB_END
    for offset in range(workbook_sectors):
        sector = workbook_start + offset
        fat[sector] = CFB_END if offset == workbook_sectors - 1 else sector + 1
    fat_payload = b"".join(_u32(value) for value in fat)
    directory = b"".join(
        (
            _cfb_directory_entry("Root Entry", 5, 1, CFB_END, 0),
            _cfb_directory_entry("Workbook", 2, CFB_FREE, workbook_start, stream_size),
            bytes(128),
            bytes(128),
        )
    )

    header = bytearray(CFB_SECTOR_SIZE)
    header[:8] = bytes.fromhex("d0cf11e0a1b11ae1")
    header[24:26] = _u16(0x003E)
    header[26:28] = _u16(3)
    header[28:30] = _u16(0xFFFE)
    header[30:32] = _u16(9)
    header[32:34] = _u16(6)
    header[44:48] = _u32(1)
    header[48:52] = _u32(directory_sector)
    header[56:60] = _u32(CFB_MINI_STREAM_CUTOFF)
    header[60:64] = _u32(CFB_END)
    header[68:72] = _u32(CFB_END)
    header[76:80] = _u32(fat_sector)
    for offset in range(1, 109):
        header[76 + offset * 4 : 80 + offset * 4] = _u32(CFB_FREE)
    return bytes(header) + fat_payload + directory + workbook_payload


def _biff_record(record_type: int, payload: bytes) -> bytes:
    if len(payload) > 0xFFFF:
        raise CorpusError("BIFF record payload is too large")
    return _u16(record_type) + _u16(len(payload)) + payload


def _biff_bof(stream_type: int) -> bytes:
    return _biff_record(0x0809, _u16(0x0600) + _u16(stream_type) + bytes(12))


def _biff_short_string(value: str) -> bytes:
    if len(value) > 31:
        raise CorpusError("BIFF sheet name is too long")
    return bytes((len(value), 1)) + value.encode("utf-16le")


def _biff_boundsheet(name: str, stream_offset: int) -> bytes:
    return _biff_record(
        0x0085, _u32(stream_offset) + b"\x00\x00" + _biff_short_string(name)
    )


def _biff_string(value: str) -> bytes:
    return _u16(len(value)) + b"\x01" + value.encode("utf-16le")


def _biff_sst(strings: tuple[str, ...]) -> bytes:
    payload = bytearray(_u32(len(strings)) + _u32(len(strings)))
    for value in strings:
        payload.extend(_biff_string(value))
    return _biff_record(0x00FC, bytes(payload))


def _biff_font(name: str) -> bytes:
    encoded = name.encode("utf-16le")
    if len(name) > 31:
        raise CorpusError("BIFF font name is too long")
    payload = (
        _u16(220)
        + _u16(0)
        + _u16(0x7FFF)
        + _u16(400)
        + _u16(0)
        + bytes((0, 2, 0, 0, len(name), 1))
        + encoded
    )
    return _biff_record(0x0031, payload)


def _biff_format(format_id: int, format_code: str) -> bytes:
    """Author a BIFF8 FORMAT record with an uncompressed Unicode code."""

    encoded = format_code.encode("utf-16le")
    if not 164 <= format_id <= 392 or not 1 <= len(format_code) <= 255:
        raise CorpusError("BIFF8 custom number format is out of range")
    payload = _u16(format_id) + _u16(len(format_code)) + b"\x01" + encoded
    return _biff_record(0x041E, payload)


def _biff_labelsst(row: int, col: int, shared_index: int, style: int = 0) -> bytes:
    return _biff_record(
        0x00FD, _u16(row) + _u16(col) + _u16(style) + _u32(shared_index)
    )


def _biff_number(row: int, col: int, value: float, style: int = 0) -> bytes:
    return _biff_record(
        0x0203, _u16(row) + _u16(col) + _u16(style) + struct.pack("<d", value)
    )


def _biff_formula(row: int, col: int, cached: float, seed: int) -> bytes:
    left = seed % 1000
    rgce = bytes((0x1E, left & 0xFF, left >> 8, 0x1E, 2, 0, 0x03))
    payload = bytearray(_u16(row) + _u16(col) + _u16(0))
    payload.extend(struct.pack("<d", cached))
    payload.extend(_u16(0))
    payload.extend(_u32(0))
    payload.extend(_u16(len(rgce)))
    payload.extend(rgce)
    return _biff_record(0x0006, bytes(payload))


def _biff_row(row: int, height_twips: int, *, hidden: bool = False) -> bytes:
    options = 0x20 if hidden else 0
    payload = bytearray(_u16(row) + _u16(0) + _u16(6) + _u16(height_twips))
    payload.extend(_u16(0) + _u16(0) + _u32(options))
    return _biff_record(0x0208, bytes(payload))


def _biff_col(first: int, last: int, width_256: int, *, hidden: bool = False) -> bytes:
    options = 1 if hidden else 0
    payload = _u16(first) + _u16(last) + _u16(width_256) + _u16(0)
    payload += _u16(options) + _u16(0)
    return _biff_record(0x007D, payload)


def _biff_merge(first_row: int, first_col: int, last_row: int, last_col: int) -> bytes:
    payload = _u16(1) + _u16(first_row) + _u16(last_row)
    payload += _u16(first_col) + _u16(last_col)
    return _biff_record(0x00E5, payload)


def _biff_page_setup() -> bytes:
    records = bytearray()
    for record_type, value in ((0x0026, 0.5), (0x0027, 0.5), (0x0028, 0.75), (0x0029, 0.75)):
        records.extend(_biff_record(record_type, struct.pack("<d", value)))
    records.extend(_biff_record(0x002A, _u16(1)))
    records.extend(_biff_record(0x002B, _u16(1)))
    records.extend(_biff_record(0x0083, _u16(1)))
    records.extend(_biff_record(0x0084, _u16(1)))
    setup = bytearray()
    setup.extend(_u16(9) + _u16(85) + struct.pack("<h", 1))
    setup.extend(_u16(1) + _u16(1) + _u16(0x0080))
    setup.extend(_u16(300) + _u16(300))
    setup.extend(struct.pack("<d", 0.2) + struct.pack("<d", 0.25) + _u16(1))
    records.extend(_biff_record(0x00A1, bytes(setup)))
    return bytes(records)


def _build_xls(spec: CaseSpec) -> bytes:
    texts = _case_texts(spec)
    sheet = bytearray(_biff_bof(0x0010))
    sheet.extend(
        _biff_record(
            0x0200, _u32(0) + _u32(5) + _u16(0) + _u16(6) + _u16(0)
        )
    )
    if _has(spec, "column-width"):
        sheet.extend(_biff_col(0, 0, 18 * 256))
        sheet.extend(_biff_col(1, 4, 14 * 256))
    if _has(spec, "hidden-column"):
        sheet.extend(_biff_col(5, 5, 8 * 256, hidden=True))
    sheet.extend(_biff_row(0, 600 if _has(spec, "row-height") else 255))
    for col in range(5):
        sheet.extend(_biff_labelsst(0, col, col))
    sheet.extend(_biff_row(1, 360 if _has(spec, "row-height") else 255))
    number = float((spec.seed % 1000) + 0.25)
    sheet.extend(_biff_number(1, 0, number))
    sheet.extend(
        _biff_number(
            1,
            1,
            45_366 + spec.index,
            style=1 if _has(spec, "date-format") else 0,
        )
    )
    sheet.extend(
        _biff_number(
            1,
            2,
            ((spec.index % 90) + 5) / 100.0,
            style=2 if _has(spec, "percent-format") else 0,
        )
    )
    if _has(spec, "formula-cached"):
        sheet.extend(
            _biff_formula(1, 3, float((spec.seed % 1000) + 2), spec.seed)
        )
    else:
        sheet.extend(_biff_number(1, 3, float((spec.seed % 1000) + 2)))
    sheet.extend(_biff_row(2, 480 if _has(spec, "row-height") else 255))
    sheet.extend(_biff_labelsst(2, 0, 6))
    if _has(spec, "merged-cells"):
        sheet.extend(_biff_merge(2, 0, 2, 2))
    sheet.extend(
        _biff_row(3, 255, hidden=_has(spec, "hidden-row"))
    )
    sheet.extend(_biff_labelsst(3, 0, 7))
    if _has(spec, "print-settings"):
        sheet.extend(_biff_page_setup())
    sheet.extend(_biff_record(0x000A, b""))

    name = "Render"
    globals_prefix = bytearray(_biff_bof(0x0005))
    globals_prefix.extend(_biff_record(0x0042, _u16(1200)))
    globals_prefix.extend(_biff_font("Noto Sans CJK KR"))
    globals_prefix.extend(
        _biff_format(CUSTOM_DATE_FORMAT_ID, CUSTOM_DATE_FORMAT_CODE)
    )
    globals_prefix.extend(_biff_record(0x00E0, bytes(20)))
    date_xf = bytearray(20)
    date_xf[2:4] = _u16(CUSTOM_DATE_FORMAT_ID)
    globals_prefix.extend(_biff_record(0x00E0, bytes(date_xf)))
    percent_xf = bytearray(20)
    percent_xf[2:4] = _u16(10)
    globals_prefix.extend(_biff_record(0x00E0, bytes(percent_xf)))
    placeholder = _biff_boundsheet(name, 0)
    sst = _biff_sst(texts)
    eof = _biff_record(0x000A, b"")
    sheet_offset = len(globals_prefix) + len(placeholder) + len(sst) + len(eof)
    stream = bytes(globals_prefix) + _biff_boundsheet(name, sheet_offset) + sst + eof + bytes(sheet)
    return _cfb_bytes(stream)


def _xlsx_styles() -> str:
    return f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <numFmts count="1"><numFmt numFmtId="{CUSTOM_DATE_FORMAT_ID}" formatCode="{CUSTOM_DATE_FORMAT_CODE}"/></numFmts>
  <fonts count="2"><font><sz val="11"/><name val="Noto Sans CJK KR"/><family val="2"/></font><font><b/><sz val="11"/><name val="Noto Sans CJK KR"/><family val="2"/></font></fonts>
  <fills count="3"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill><fill><patternFill patternType="solid"><fgColor rgb="FFFFE699"/><bgColor indexed="64"/></patternFill></fill></fills>
  <borders count="2"><border><left/><right/><top/><bottom/><diagonal/></border><border><left style="thin"><color rgb="FF336699"/></left><right style="thin"><color rgb="FF336699"/></right><top style="thin"><color rgb="FF336699"/></top><bottom style="thin"><color rgb="FF336699"/></bottom><diagonal/></border></borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="7">
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
    <xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1"/>
    <xf numFmtId="{CUSTOM_DATE_FORMAT_ID}" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/>
    <xf numFmtId="10" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/>
    <xf numFmtId="0" fontId="0" fillId="0" borderId="1" xfId="0" applyBorder="1"/>
    <xf numFmtId="0" fontId="0" fillId="2" borderId="0" xfId="0" applyFill="1"/>
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf>
  </cellXfs>
  <cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles>
  <dxfs count="1"><dxf><fill><patternFill patternType="solid"><fgColor rgb="FFFFC7CE"/><bgColor indexed="64"/></patternFill></fill><font><color rgb="FF9C0006"/></font></dxf></dxfs>
</styleSheet>
"""


def _xlsx_inline(ref: str, value: str, style: int = 0) -> str:
    return f'<c r="{ref}" s="{style}" t="inlineStr"><is><t>{escape(value)}</t></is></c>'


def _relationships_xml(
    relationships: Iterable[tuple[str, str, str]],
) -> str:
    rows = list(relationships)
    if len(rows) > MAX_PACKAGE_RELATIONSHIPS:
        raise CorpusError(
            f"relationship cap exceeded: {len(rows)} > {MAX_PACKAGE_RELATIONSHIPS}"
        )
    body = "".join(
        f'<Relationship Id="{escape(identifier, quote=True)}" '
        f'Type="{escape(kind, quote=True)}" '
        f'Target="{escape(target, quote=True)}"/>'
        for identifier, kind, target in rows
    )
    return (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f"{body}</Relationships>"
    )


def _png_chunk(kind: bytes, payload: bytes) -> bytes:
    checksum = zlib.crc32(kind)
    checksum = zlib.crc32(payload, checksum) & 0xFFFFFFFF
    return _u32_be(len(payload)) + kind + payload + _u32_be(checksum)


def _u32_be(value: int) -> bytes:
    return value.to_bytes(4, "big")


def _stored_zlib(payload: bytes) -> bytes:
    if len(payload) > 0xFFFF:
        raise CorpusError("deterministic PNG scanlines exceed one DEFLATE block")
    length = len(payload)
    adler = zlib.adler32(payload) & 0xFFFFFFFF
    return (
        b"\x78\x01\x01"
        + length.to_bytes(2, "little")
        + ((~length) & 0xFFFF).to_bytes(2, "little")
        + payload
        + _u32_be(adler)
    )


def _project_png(spec: CaseSpec) -> bytes:
    width = 24
    height = 24
    scanlines = bytearray()
    for row in range(height):
        scanlines.append(0)
        for col in range(width):
            scanlines.extend(
                (
                    (spec.seed + row * 11 + col * 7) & 0xFF,
                    (80 + row * 5 + col * 3) & 0xFF,
                    (160 + row * 3 + col * 13) & 0xFF,
                )
            )
    payload = (
        b"\x89PNG\r\n\x1a\n"
        + _png_chunk(
            b"IHDR",
            _u32_be(width)
            + _u32_be(height)
            + b"\x08\x02\x00\x00\x00",
        )
        + _png_chunk(b"IDAT", _stored_zlib(bytes(scanlines)))
        + _png_chunk(b"IEND", b"")
    )
    if len(payload) > MAX_IMAGE_BYTES:
        raise CorpusError(f"deterministic image cap exceeded by {spec.case_id}")
    return payload


def _xlsx_chart(spec: CaseSpec) -> str:
    values = [((spec.seed // 7) + offset * 13) % 90 + 10 for offset in range(4)]
    if len(values) > MAX_CHART_POINTS:
        raise CorpusError("chart point cap exceeded")
    points = "".join(
        f'<c:pt idx="{index}"><c:v>{value}</c:v></c:pt>'
        for index, value in enumerate(values)
    )
    return f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <c:chart><c:autoTitleDeleted val="1"/><c:plotArea><c:layout/><c:lineChart><c:grouping val="standard"/><c:varyColors val="0"/>
    <c:ser><c:idx val="0"/><c:order val="0"/><c:tx><c:strRef><c:f>Render!$A$7</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>Series {spec.index:04d}</c:v></c:pt></c:strCache></c:strRef></c:tx>
      <c:marker><c:symbol val="circle"/><c:size val="5"/></c:marker>
      <c:cat><c:strRef><c:f>Render!$B$6:$E$6</c:f><c:strCache><c:ptCount val="4"/><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt><c:pt idx="2"><c:v>Q3</c:v></c:pt><c:pt idx="3"><c:v>Q4</c:v></c:pt></c:strCache></c:strRef></c:cat>
      <c:val><c:numRef><c:f>Render!$B$7:$E$7</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="4"/>{points}</c:numCache></c:numRef></c:val><c:smooth val="0"/></c:ser>
    <c:axId val="31415926"/><c:axId val="27182818"/></c:lineChart>
    <c:catAx><c:axId val="31415926"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/><c:tickLblPos val="nextTo"/><c:crossAx val="27182818"/><c:crosses val="autoZero"/><c:auto val="1"/><c:lblAlgn val="ctr"/><c:lblOffset val="100"/></c:catAx>
    <c:valAx><c:axId val="27182818"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="l"/><c:numFmt formatCode="General" sourceLinked="1"/><c:tickLblPos val="nextTo"/><c:crossAx val="31415926"/><c:crosses val="autoZero"/><c:crossBetween val="between"/></c:valAx>
  </c:plotArea><c:plotVisOnly val="1"/><c:dispBlanksAs val="gap"/></c:chart></c:chartSpace>
"""


def _xlsx_drawing(spec: CaseSpec) -> tuple[str, str]:
    anchors: list[str] = []
    relationships: list[tuple[str, str, str]] = []
    if _has(spec, "image-drawing"):
        relationships.append(
            (
                "rIdImage",
                "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image",
                "../media/image1.png",
            )
        )
        anchors.append(
            """<xdr:twoCellAnchor editAs="oneCell"><xdr:from><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:to><xdr:col>6</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>4</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="Project-authored image"/><xdr:cNvPicPr><a:picLocks noChangeAspect="1"/></xdr:cNvPicPr></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="rIdImage"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill><xdr:spPr><a:xfrm/><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic><xdr:clientData/></xdr:twoCellAnchor>"""
        )
    if _has(spec, "chart"):
        relationships.append(
            (
                "rIdChart",
                "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart",
                "../charts/chart1.xml",
            )
        )
        anchors.append(
            """<xdr:twoCellAnchor><xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>7</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:to><xdr:col>6</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>18</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to><xdr:graphicFrame macro=""><xdr:nvGraphicFramePr><xdr:cNvPr id="3" name="Project-authored chart"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rIdChart"/></a:graphicData></a:graphic></xdr:graphicFrame><xdr:clientData/></xdr:twoCellAnchor>"""
        )
    if len(anchors) > MAX_DRAWING_OBJECTS:
        raise CorpusError("drawing object cap exceeded")
    drawing = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" '
        'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
        'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        + "".join(anchors)
        + "</xdr:wsDr>"
    )
    return drawing, _relationships_xml(relationships)


def _build_xlsx(spec: CaseSpec) -> bytes:
    texts = _case_texts(spec)
    numeric = (spec.seed % 1000) + 0.25
    percent = ((spec.index % 90) + 5) / 100.0
    result = (spec.seed % 1000) + 2
    heading_style = 4 if _has(spec, "border") else 1
    cells = "".join(
        _xlsx_inline(f"{chr(65 + col)}1", texts[col], heading_style)
        for col in range(5)
    )
    columns: list[str] = []
    if _has(spec, "column-width"):
        columns.extend(
            (
                '<col min="1" max="1" width="18" customWidth="1"/>',
                '<col min="2" max="5" width="14" customWidth="1"/>',
            )
        )
    if _has(spec, "hidden-column"):
        columns.append('<col min="6" max="6" hidden="1"/>')
    cols = f"<cols>{''.join(columns)}</cols>" if columns else ""
    row_one_height = ' ht="30" customHeight="1"' if _has(spec, "row-height") else ""
    row_two_height = ' ht="18" customHeight="1"' if _has(spec, "row-height") else ""
    date_style = 2 if _has(spec, "date-format") else 0
    percent_style = 3 if _has(spec, "percent-format") else 0
    if _has(spec, "formula-cached"):
        formula_cell = f'<c r="D2"><f>{spec.seed % 1000}+2</f><v>{result}</v></c>'
    else:
        formula_cell = f'<c r="D2"><v>{result}</v></c>'
    merge_cells = (
        '<mergeCells count="1"><mergeCell ref="A3:C3"/></mergeCells>'
        if _has(spec, "merged-cells")
        else ""
    )
    fill_style = 5 if _has(spec, "cell-fill") else 0
    hidden_attr = ' hidden="1"' if _has(spec, "hidden-row") else ""
    wrap_style = 6 if _has(spec, "wrapped-text") else 0
    wrapped_text = texts[5] if _has(spec, "wrapped-text") else f"Summary {spec.case_id}"
    data_rows = ""
    if _has(spec, "chart") or _has(spec, "sparkline"):
        chart_values = [
            ((spec.seed // 7) + offset * 13) % 90 + 10 for offset in range(4)
        ]
        data_rows = (
            '<row r="6">'
            + ('<c r="A6"><v>0</v></c>' if _has(spec, "sparkline") else "")
            + "".join(
                _xlsx_inline(f"{column}6", label, 1)
                for column, label in zip("BCDE", ("Q1", "Q2", "Q3", "Q4"), strict=True)
            )
            + '</row><row r="7">'
            + _xlsx_inline("A7", f"Series {spec.index:04d}")
            + "".join(
                f'<c r="{column}7"><v>{value}</v></c>'
                for column, value in zip("BCDE", chart_values, strict=True)
            )
            + "</row>"
        )
    conditional = (
        '<conditionalFormatting sqref="A2"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting>'
        if _has(spec, "conditional-format")
        else ""
    )
    print_settings = ""
    sheet_properties = ""
    if _has(spec, "print-settings"):
        if spec.index % 2 == 0:
            page_setup = '<pageSetup orientation="portrait" paperSize="1" scale="85" pageOrder="overThenDown"/>'
        else:
            sheet_properties = '<sheetPr><pageSetUpPr fitToPage="1"/></sheetPr>'
            page_setup = '<pageSetup orientation="portrait" paperSize="1" fitToWidth="2" fitToHeight="2" pageOrder="overThenDown"/>'
        print_settings = (
            '<printOptions gridLines="1" headings="1" horizontalCentered="1" verticalCentered="1"/>'
            '<pageMargins left="0.5" right="0.5" top="0.75" bottom="0.75" header="0.2" footer="0.25"/>'
            + page_setup
            + '<headerFooter differentOddEven="1" differentFirst="1"><oddHeader>&amp;LAuthored&amp;CPage &amp;P of &amp;N</oddHeader><oddFooter>&amp;RFooter &amp;P</oddFooter><evenHeader>&amp;CEven &amp;P of &amp;N</evenHeader><evenFooter>&amp;REven footer</evenFooter><firstHeader>&amp;CFirst &amp;P of &amp;N</firstHeader><firstFooter>&amp;RFirst footer</firstFooter></headerFooter>'
            '<rowBreaks count="1" manualBreakCount="1"><brk id="8" min="0" max="16383" man="1"/></rowBreaks>'
            '<colBreaks count="1" manualBreakCount="1"><brk id="3" min="0" max="1048575" man="1"/></colBreaks>'
        )
    has_drawing = _has(spec, "image-drawing") or _has(spec, "chart")
    drawing_ref = '<drawing r:id="rIdDrawing"/>' if has_drawing else ""
    sparkline = ""
    if _has(spec, "sparkline"):
        sparkline = """<extLst><ext uri="{05C60535-1F16-4fd2-B633-F4F36F0B64E0}" xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main"><x14:sparklineGroups xmlns:xm="http://schemas.microsoft.com/office/excel/2006/main"><x14:sparklineGroup type="line" displayEmptyCellsAs="gap"><x14:colorSeries rgb="FF376092"/><x14:colorNegative rgb="FFD00000"/><x14:colorAxis rgb="FF000000"/><x14:colorMarkers rgb="FF376092"/><x14:sparklines><x14:sparkline><xm:f>Render!B7:E7</xm:f><xm:sqref>A6</xm:sqref></x14:sparkline></x14:sparklines></x14:sparklineGroup></x14:sparklineGroups></ext></extLst>"""
    sheet = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  {sheet_properties}<dimension ref="A1:F{'10' if _has(spec, 'print-settings') else '7'}"/>
  <sheetViews><sheetView workbookViewId="0"{' rightToLeft="1"' if _has(spec, "right-to-left-layout") else ''} showGridLines="1"/></sheetViews>
  {cols}
  <sheetData>
    <row r="1"{row_one_height}>{cells}</row>
    <row r="2"{row_two_height}><c r="A2"><v>{numeric:.2f}</v></c><c r="B2" s="{date_style}"><v>{45366 + spec.index}</v></c><c r="C2" s="{percent_style}"><v>{percent:.4f}</v></c>{formula_cell}</row>
    <row r="3">{_xlsx_inline("A3", texts[6], fill_style)}</row>
    <row r="4"{hidden_attr}>{_xlsx_inline("A4", texts[7])}</row>
    <row r="5">{_xlsx_inline("A5", wrapped_text, wrap_style)}</row>
    {data_rows}
    {'<row r="10">' + _xlsx_inline("B10", f"Print body {spec.index:04d}") + _xlsx_inline("E10", f"Print tail {spec.index:04d}") + '</row>' if _has(spec, 'print-settings') else ''}
  </sheetData>
  {merge_cells}{conditional}{print_settings}{drawing_ref}{sparkline}
</worksheet>
"""
    content_defaults = [
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>',
        '<Default Extension="xml" ContentType="application/xml"/>',
    ]
    content_overrides = [
        '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>',
        '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>',
        '<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>',
    ]
    if _has(spec, "image-drawing"):
        content_defaults.append('<Default Extension="png" ContentType="image/png"/>')
    if has_drawing:
        content_overrides.append(
            '<Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/>'
        )
    if _has(spec, "chart"):
        content_overrides.append(
            '<Override PartName="/xl/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>'
        )
    content_types = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        + "".join(content_defaults + content_overrides)
        + "</Types>"
    )
    print_name = (
        '<definedNames><definedName name="_xlnm.Print_Area" localSheetId="0">Render!$A$1:$F$18</definedName><definedName name="_xlnm.Print_Titles" localSheetId="0">Render!$1:$1,Render!$F:$F</definedName></definedNames>'
        if _has(spec, "print-settings")
        else ""
    )
    workbook = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" '
        'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        '<sheets><sheet name="Render" sheetId="1" r:id="rId1"/></sheets>'
        f"{print_name}</workbook>"
    )
    parts: list[tuple[str, str | bytes]] = [
        ("[Content_Types].xml", content_types),
        (
            "_rels/.rels",
            _relationships_xml(
                (
                    (
                        "rId1",
                        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument",
                        "xl/workbook.xml",
                    ),
                )
            ),
        ),
        ("xl/workbook.xml", workbook),
        (
            "xl/_rels/workbook.xml.rels",
            _relationships_xml(
                (
                    (
                        "rId1",
                        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet",
                        "worksheets/sheet1.xml",
                    ),
                    (
                        "rId2",
                        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles",
                        "styles.xml",
                    ),
                )
            ),
        ),
        ("xl/styles.xml", _xlsx_styles()),
        ("xl/worksheets/sheet1.xml", sheet),
    ]
    if has_drawing:
        parts.append(
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                _relationships_xml(
                    (
                        (
                            "rIdDrawing",
                            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing",
                            "../drawings/drawing1.xml",
                        ),
                    )
                ),
            )
        )
        drawing, drawing_relationships = _xlsx_drawing(spec)
        parts.extend(
            (
                ("xl/drawings/drawing1.xml", drawing),
                ("xl/drawings/_rels/drawing1.xml.rels", drawing_relationships),
            )
        )
    if _has(spec, "image-drawing"):
        parts.append(("xl/media/image1.png", _project_png(spec)))
    if _has(spec, "chart"):
        parts.append(("xl/charts/chart1.xml", _xlsx_chart(spec)))
    relationship_count = 3
    if has_drawing:
        relationship_count += 1
    relationship_count += int(_has(spec, "image-drawing"))
    relationship_count += int(_has(spec, "chart"))
    if relationship_count > MAX_PACKAGE_RELATIONSHIPS:
        raise CorpusError("package relationship cap exceeded")
    return _zip_bytes(parts)


def _biff12_var_uint(value: int) -> bytes:
    if value < 0:
        raise CorpusError("BIFF12 variable integer cannot be negative")
    output = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            byte |= 0x80
        output.append(byte)
        if not value:
            return bytes(output)


def _biff12_record(record_type: int, payload: bytes) -> bytes:
    # BIFF12 record identifiers are at most two 7-bit bytes and record sizes
    # are at most four 7-bit bytes.  Enforce those wire bounds here so a future
    # corpus feature cannot silently author a non-canonical record header.
    if not 0 <= record_type <= 0x3FFF:
        raise CorpusError(f"BIFF12 record type is out of range: {record_type}")
    if len(payload) > 0x0FFFFFFF:
        raise CorpusError(f"BIFF12 record payload is too large: {len(payload)}")
    return _biff12_var_uint(record_type) + _biff12_var_uint(len(payload)) + payload


def _biff12_decode_var_uint(
    data: bytes, offset: int, *, max_bytes: int, field: str
) -> tuple[int, int]:
    """Decode one bounded, canonical BIFF12 variable-width integer."""

    start = offset
    value = 0
    for shift in range(0, max_bytes * 7, 7):
        if offset >= len(data):
            raise CorpusError(f"truncated BIFF12 {field}")
        byte = data[offset]
        offset += 1
        value |= (byte & 0x7F) << shift
        if byte & 0x80 == 0:
            if data[start:offset] != _biff12_var_uint(value):
                raise CorpusError(f"non-canonical BIFF12 {field}")
            return value, offset
    raise CorpusError(f"BIFF12 {field} exceeds {max_bytes} bytes")


def _biff12_records(data: bytes) -> tuple[tuple[int, bytes], ...]:
    """Return a fail-closed BIFF12 record stream for generator verification."""

    records: list[tuple[int, bytes]] = []
    offset = 0
    while offset < len(data):
        record_type, offset = _biff12_decode_var_uint(
            data, offset, max_bytes=2, field="record type"
        )
        size, offset = _biff12_decode_var_uint(
            data, offset, max_bytes=4, field="record size"
        )
        end = offset + size
        if end > len(data):
            raise CorpusError(
                f"truncated BIFF12 record payload: type={record_type} size={size}"
            )
        records.append((record_type, data[offset:end]))
        offset = end
    return tuple(records)


def _xlsb_wstr(value: str) -> bytes:
    return _u32(len(value)) + value.encode("utf-16le")


def _xlsb_bundle_sheet(name: str) -> bytes:
    # BrtBundleSh: visible sheet state, a non-zero sheet identifier, the
    # worksheet relationship identifier, and its display name.
    payload = _u32(0) + _u32(1) + _xlsb_wstr("rId1") + _xlsb_wstr(name)
    return _biff12_record(156, payload)


def _xlsb_file_version() -> bytes:
    payload = bytearray(16)  # no VBA type-library GUID
    payload.extend(_xlsb_wstr("rxls"))
    payload.extend(_xlsb_wstr(GENERATOR_VERSION))
    payload.extend(_xlsb_wstr(GENERATOR_VERSION))
    payload.extend(_xlsb_wstr("deterministic"))
    return _biff12_record(0x0080, bytes(payload))


def _xlsb_workbook(name: str) -> bytes:
    return b"".join(
        (
            _biff12_record(0x0083, b""),  # BrtBeginBook
            _xlsb_file_version(),
            _biff12_record(0x008F, b""),  # BrtBeginBundleShs
            _xlsb_bundle_sheet(name),
            _biff12_record(0x0090, b""),  # BrtEndBundleShs
            _biff12_record(0x0084, b""),  # BrtEndBook
        )
    )


def _xlsb_sst_item(value: str) -> bytes:
    return _biff12_record(19, b"\x00" + _xlsb_wstr(value))


def _xlsb_shared_strings(values: tuple[str, ...], reference_count: int) -> bytes:
    if len(set(values)) != len(values):
        raise CorpusError("XLSB shared strings must be unique")
    if reference_count < len(values):
        raise CorpusError("XLSB shared-string references cannot be below unique count")
    return b"".join(
        (
            _biff12_record(
                0x009F, _u32(reference_count) + _u32(len(values))
            ),  # BrtBeginSst
            *(_xlsb_sst_item(value) for value in values),
            _biff12_record(0x00A0, b""),  # BrtEndSst
        )
    )


def _xlsb_font(name: str) -> bytes:
    if not 1 <= len(name) <= 31:
        raise CorpusError("XLSB font name must contain 1..31 characters")
    payload = bytearray()
    payload.extend(_u16(220))  # 11 pt
    payload.extend(_u16(0))  # FontFlags
    payload.extend(_u16(400))  # normal weight
    payload.extend(_u16(0))  # no superscript/subscript
    payload.extend(bytes((0, 2, 1, 0)))  # underline, Swiss, default charset, unused
    payload.extend(bytes(8))  # automatically determined BrtColor
    payload.append(0)  # no theme font scheme
    payload.extend(_xlsb_wstr(name))
    return _biff12_record(0x002B, bytes(payload))


def _xlsb_xf(parent: int, number_format: int) -> bytes:
    if not 0 <= parent <= 0xFFFF or not 0 <= number_format <= 0xFFFF:
        raise CorpusError("XLSB XF reference is out of range")
    payload = bytearray(_u16(parent) + _u16(number_format))
    payload.extend(_u16(0) + _u16(0) + _u16(0))  # font, fill, and border zero
    payload.extend(bytes((0, 0)))  # rotation and indentation
    payload.extend(bytes.fromhex("10100000"))  # general alignment, locked
    return _biff12_record(0x002F, bytes(payload))


def _xlsb_fmt(format_id: int, format_code: str) -> bytes:
    """Author a standards-bounded BrtFmt custom number-format record."""

    if not 164 <= format_id <= 382 or not 1 <= len(format_code) <= 255:
        raise CorpusError("XLSB custom number format is out of range")
    return _biff12_record(0x002C, _u16(format_id) + _xlsb_wstr(format_code))


def _xlsb_normal_style() -> bytes:
    # Built-in Normal style referencing the sole cell-style XF.
    payload = _u32(0) + _u16(1) + bytes((0, 0xFF)) + _xlsb_wstr("Normal")
    return _biff12_record(0x0030, payload)


def _xlsb_styles() -> bytes:
    # A complete minimal Styles part.  Every XF reference resolves to an
    # authored font/fill/border.  Dates use a locale-independent custom format;
    # percentages retain their standard built-in format identifier.
    cell_xfs = tuple(
        _xlsb_xf(0, number_format)
        for number_format in (0, CUSTOM_DATE_FORMAT_ID, 10)
    )
    return b"".join(
        (
            _biff12_record(0x0116, b""),  # BrtBeginStyleSheet
            _biff12_record(0x0267, _u32(1)),  # BrtBeginFmts
            _xlsb_fmt(CUSTOM_DATE_FORMAT_ID, CUSTOM_DATE_FORMAT_CODE),
            _biff12_record(0x0268, b""),  # BrtEndFmts
            _biff12_record(0x0263, _u32(1)),  # BrtBeginFonts
            _xlsb_font("Calibri"),
            _biff12_record(0x0264, b""),  # BrtEndFonts
            _biff12_record(0x025B, _u32(1)),  # BrtBeginFills
            _biff12_record(0x002D, bytes(68)),  # no-fill BrtFill
            _biff12_record(0x025C, b""),  # BrtEndFills
            _biff12_record(0x0265, _u32(1)),  # BrtBeginBorders
            _biff12_record(0x002E, bytes(51)),  # borderless BrtBorder
            _biff12_record(0x0266, b""),  # BrtEndBorders
            _biff12_record(0x0272, _u32(1)),  # BrtBeginCellStyleXFs
            _xlsb_xf(0xFFFF, 0),
            _biff12_record(0x0273, b""),  # BrtEndCellStyleXFs
            _biff12_record(0x0269, _u32(len(cell_xfs))),  # BrtBeginCellXFs
            *cell_xfs,
            _biff12_record(0x026A, b""),  # BrtEndCellXFs
            _biff12_record(0x026B, _u32(1)),  # BrtBeginStyles
            _xlsb_normal_style(),
            _biff12_record(0x026C, b""),  # BrtEndStyles
            _biff12_record(0x0117, b""),  # BrtEndStyleSheet
        )
    )


def _xlsb_style_ref(index: int) -> bytes:
    return index.to_bytes(3, "little") + b"\x00"


def _xlsb_cell_isst(col: int, shared: int, style: int = 0) -> bytes:
    return _biff12_record(7, _u32(col) + _xlsb_style_ref(style) + _u32(shared))


def _xlsb_cell_real(col: int, value: float, style: int = 0) -> bytes:
    return _biff12_record(5, _u32(col) + _xlsb_style_ref(style) + struct.pack("<d", value))


def _xlsb_formula(col: int, cached: float, seed: int) -> bytes:
    left = seed % 1000
    rgce = bytes((0x1E, left & 0xFF, left >> 8, 0x1E, 2, 0, 0x03))
    payload = bytearray(_u32(col) + _xlsb_style_ref(0) + struct.pack("<d", cached))
    payload.extend(_u16(0) + _u32(len(rgce)) + rgce + _u32(0))
    return _biff12_record(9, bytes(payload))


def _xlsb_row(
    row: int,
    height_twips: int,
    *,
    first_col: int,
    last_col: int,
    hidden: bool = False,
    custom_height: bool = False,
) -> bytes:
    if not 0 <= row < 1_048_576 or not 0 <= first_col <= last_col < 16_384:
        raise CorpusError("XLSB row or column span is out of range")
    if not 0 <= height_twips <= 0x2000:
        raise CorpusError("XLSB row height is out of range")
    flags = 0
    if hidden:
        flags |= 1 << 12  # fDyZero
    if custom_height:
        flags |= 1 << 13  # fUnsynced, so miyRw is authoritative
    payload = bytearray(_u32(row) + _u32(0) + _u16(height_twips) + _u16(flags))
    payload.append(0)  # fPhShow and seven reserved bits
    payload.extend(_u32(1))  # one BrtColSpan segment
    payload.extend(_u32(first_col) + _u32(last_col))
    return _biff12_record(0, bytes(payload))


def _xlsb_col(first: int, last: int, width_256: int, *, hidden: bool = False) -> bytes:
    flags = 1 if hidden else 0
    return _biff12_record(60, _u32(first) + _u32(last) + _u32(width_256) + _u32(0) + _u16(flags))


def _xlsb_merge(first_row: int, first_col: int, last_row: int, last_col: int) -> bytes:
    return _biff12_record(
        176, _u32(first_row) + _u32(last_row) + _u32(first_col) + _u32(last_col)
    )


def _xlsb_print_settings() -> bytes:
    margins = b"".join(struct.pack("<d", value) for value in (0.5, 0.5, 0.75, 0.75, 0.2, 0.25))
    setup = bytearray(_u32(9) + _u32(85) + _u32(300) + _u32(300) + _u32(1))
    setup.extend(struct.pack("<i", 1) + _u32(1) + _u32(1) + _u16((1 << 1) | (1 << 7)))
    setup.extend(_u32(0xFFFFFFFF))
    return (
        _biff12_record(477, _u16(0b1111))  # BrtPrintOptions
        + _biff12_record(476, margins)  # BrtMargins
        + _biff12_record(478, bytes(setup))  # BrtPageSetup
    )


def _build_xlsb(spec: CaseSpec) -> bytes:
    texts = _case_texts(spec)
    # XLSB does not claim wrapped-text coverage, so omit the unused authored
    # wrapping string from the SST.  Seven unique strings have seven cell
    # references, keeping BrtBeginSst counts exact.
    shared_values = (*texts[:5], texts[6], texts[7])
    shared = _xlsb_shared_strings(shared_values, reference_count=7)
    columns = bytearray()
    if _has(spec, "column-width"):
        columns.extend(_xlsb_col(0, 0, 18 * 256))
        columns.extend(_xlsb_col(1, 4, 14 * 256))
    if _has(spec, "hidden-column"):
        columns.extend(_xlsb_col(5, 5, 8 * 256, hidden=True))

    custom_height = _has(spec, "row-height")
    sheet_data = bytearray(
        _xlsb_row(
            0,
            600 if custom_height else 255,
            first_col=0,
            last_col=4,
            custom_height=custom_height,
        )
    )
    for col in range(5):
        sheet_data.extend(_xlsb_cell_isst(col, col))
    sheet_data.extend(
        _xlsb_row(
            1,
            360 if custom_height else 255,
            first_col=0,
            last_col=3,
            custom_height=custom_height,
        )
    )
    sheet_data.extend(_xlsb_cell_real(0, float((spec.seed % 1000) + 0.25)))
    sheet_data.extend(
        _xlsb_cell_real(
            1,
            float(45_366 + spec.index),
            style=1 if _has(spec, "date-format") else 0,
        )
    )
    sheet_data.extend(
        _xlsb_cell_real(
            2,
            ((spec.index % 90) + 5) / 100.0,
            style=2 if _has(spec, "percent-format") else 0,
        )
    )
    if _has(spec, "formula-cached"):
        sheet_data.extend(
            _xlsb_formula(3, float((spec.seed % 1000) + 2), spec.seed)
        )
    else:
        sheet_data.extend(_xlsb_cell_real(3, float((spec.seed % 1000) + 2)))
    sheet_data.extend(
        _xlsb_row(
            2,
            480 if custom_height else 255,
            first_col=0,
            last_col=0,
            custom_height=custom_height,
        )
    )
    sheet_data.extend(_xlsb_cell_isst(0, 5))
    sheet_data.extend(
        _xlsb_row(
            3,
            255,
            first_col=0,
            last_col=0,
            hidden=_has(spec, "hidden-row"),
        )
    )
    sheet_data.extend(_xlsb_cell_isst(0, 6))

    sheet = bytearray(_biff12_record(0x0081, b""))  # BrtBeginSheet
    sheet.extend(
        _biff12_record(
            0x0094,
            _u32(0) + _u32(3) + _u32(0) + _u32(4),
        )
    )  # BrtWsDim: A1:E4
    if columns:
        sheet.extend(_biff12_record(0x0186, b""))  # BrtBeginColInfos
        sheet.extend(columns)
        sheet.extend(_biff12_record(0x0187, b""))  # BrtEndColInfos
    sheet.extend(_biff12_record(0x0091, b""))  # BrtBeginSheetData
    sheet.extend(sheet_data)
    sheet.extend(_biff12_record(0x0092, b""))  # BrtEndSheetData
    if _has(spec, "merged-cells"):
        sheet.extend(_biff12_record(177, _u32(1)))  # BrtBeginMergeCells
        sheet.extend(_xlsb_merge(2, 0, 2, 2))
        sheet.extend(_biff12_record(178, b""))  # BrtEndMergeCells
    if _has(spec, "print-settings"):
        sheet.extend(_xlsb_print_settings())
    sheet.extend(_biff12_record(0x0082, b""))  # BrtEndSheet

    for stream in (_xlsb_workbook("Render"), shared, _xlsb_styles(), bytes(sheet)):
        _biff12_records(stream)
    return _zip_bytes(
        (
            (
                "[Content_Types].xml",
                """<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/workbook.bin" ContentType="application/vnd.ms-excel.sheet.binary.macroEnabled.main"/><Override PartName="/xl/worksheets/sheet1.bin" ContentType="application/vnd.ms-excel.worksheet"/><Override PartName="/xl/sharedStrings.bin" ContentType="application/vnd.ms-excel.sharedStrings"/><Override PartName="/xl/styles.bin" ContentType="application/vnd.ms-excel.styles"/></Types>""",
            ),
            (
                "_rels/.rels",
                """<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.bin"/></Relationships>""",
            ),
            ("xl/workbook.bin", _xlsb_workbook("Render")),
            (
                "xl/_rels/workbook.bin.rels",
                """<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.bin"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.bin"/></Relationships>""",
            ),
            ("xl/sharedStrings.bin", shared),
            ("xl/styles.bin", _xlsb_styles()),
            ("xl/worksheets/sheet1.bin", bytes(sheet)),
        )
    )


def _ods_string(value: str, style: str = "") -> str:
    attr = f' table:style-name="{style}"' if style else ""
    return f'<table:table-cell{attr} office:value-type="string"><text:p>{escape(value)}</text:p></table:table-cell>'


def _build_ods(spec: CaseSpec) -> bytes:
    texts = _case_texts(spec)
    numeric = (spec.seed % 1000) + 0.25
    percent = ((spec.index % 90) + 5) / 100.0
    result = (spec.seed % 1000) + 2
    row_one_cell_style = "ce-border" if _has(spec, "border") else ""
    row_one = "".join(
        _ods_string(value, row_one_cell_style) for value in texts[:5]
    )
    columns: list[str] = []
    if _has(spec, "column-width"):
        columns.extend(
            (
                '<table:table-column table:style-name="co-wide"/>',
                '<table:table-column table:style-name="co-normal" table:number-columns-repeated="4"/>',
            )
        )
    else:
        columns.append('<table:table-column table:number-columns-repeated="5"/>')
    if _has(spec, "hidden-column"):
        columns.append('<table:table-column table:visibility="collapse"/>')
    else:
        columns.append("<table:table-column/>")
    row_one_row_style = "ro-tall" if _has(spec, "row-height") else "ro-default"
    date_cell = (
        f'<table:table-cell table:style-name="ce-date" office:value-type="date" office:date-value="{2024 + spec.index % 3}-03-15"><text:p>{2024 + spec.index % 3}-03-15</text:p></table:table-cell>'
        if _has(spec, "date-format")
        else f'<table:table-cell office:value-type="float" office:value="{45366 + spec.index}"><text:p>{45366 + spec.index}</text:p></table:table-cell>'
    )
    percent_cell = (
        f'<table:table-cell table:style-name="ce-percent" office:value-type="percentage" office:value="{percent:.4f}"><text:p>{percent * 100:.0f}%</text:p></table:table-cell>'
        if _has(spec, "percent-format")
        else f'<table:table-cell office:value-type="float" office:value="{percent:.4f}"><text:p>{percent:.4f}</text:p></table:table-cell>'
    )
    formula_attr = (
        f' table:formula="of:={spec.seed % 1000}+2"'
        if _has(spec, "formula-cached")
        else ""
    )
    fill_style = ' table:style-name="ce-fill"' if _has(spec, "cell-fill") else ""
    if _has(spec, "merged-cells"):
        merge_row = (
            f'<table:table-row table:style-name="ro-default"><table:table-cell{fill_style} office:value-type="string" table:number-columns-spanned="3"><text:p>{escape(texts[6])}</text:p></table:table-cell><table:covered-table-cell/><table:covered-table-cell/></table:table-row>'
        )
    else:
        merge_row = (
            f'<table:table-row table:style-name="ro-default"><table:table-cell{fill_style} office:value-type="string"><text:p>{escape(texts[6])}</text:p></table:table-cell></table:table-row>'
        )
    hidden_attr = ' table:visibility="collapse"' if _has(spec, "hidden-row") else ""
    wrap_style = "ce-wrap" if _has(spec, "wrapped-text") else ""
    wrap_row_style = "ro-wrap" if _has(spec, "wrapped-text") else "ro-default"
    wrapped_text = texts[5] if _has(spec, "wrapped-text") else f"Summary {spec.case_id}"
    table_style = "ta-rtl" if _has(spec, "right-to-left-layout") else "ta-ltr"
    print_range = (
        ' table:print-ranges="$Render.$A$1:$Render.$F$5"'
        if _has(spec, "print-settings")
        else ""
    )
    master_page = ' style:master-page-name="mp-render"' if _has(spec, "print-settings") else ""
    content = f"""<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0" xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2" office:version="1.3">
  <office:automatic-styles>
    <number:date-style style:name="Ndate" number:automatic-order="false"><number:year number:style="long"/><number:text>-</number:text><number:month number:style="long"/><number:text>-</number:text><number:day number:style="long"/></number:date-style>
    <number:percentage-style style:name="Npercent"><number:number number:decimal-places="0"/><number:text>%</number:text></number:percentage-style>
    <style:style style:name="co-wide" style:family="table-column"><style:table-column-properties style:column-width="3.2cm"/></style:style>
    <style:style style:name="co-normal" style:family="table-column"><style:table-column-properties style:column-width="2.5cm"/></style:style>
    <style:style style:name="co-hidden" style:family="table-column"><style:table-column-properties style:column-width="1.0cm"/></style:style>
    <style:style style:name="ro-default" style:family="table-row"><style:table-row-properties style:row-height="{ODS_DEFAULT_ROW_HEIGHT}" style:use-optimal-row-height="false"/></style:style>
    <style:style style:name="ro-tall" style:family="table-row"><style:table-row-properties style:row-height="0.9cm" style:use-optimal-row-height="false"/></style:style>
    <style:style style:name="ro-wrap" style:family="table-row"><style:table-row-properties style:row-height="{ODS_WRAPPED_ROW_HEIGHT}" style:use-optimal-row-height="false"/></style:style>
    <style:style style:name="ro-hidden" style:family="table-row"><style:table-row-properties style:row-height="0.5cm"/></style:style>
    <style:style style:name="ce-wrap" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap"/></style:style>
    <style:style style:name="ce-fill" style:family="table-cell"><style:table-cell-properties fo:background-color="#ffe699"/></style:style>
    <style:style style:name="ce-border" style:family="table-cell"><style:table-cell-properties fo:border="0.02cm solid #336699"/></style:style>
    <style:style style:name="ce-date" style:family="table-cell" style:data-style-name="Ndate"/>
    <style:style style:name="ce-percent" style:family="table-cell" style:data-style-name="Npercent"/>
    <style:style style:name="ta-rtl" style:family="table"{master_page}><style:table-properties style:writing-mode="rl-tb" table:display="true"/></style:style>
    <style:style style:name="ta-ltr" style:family="table"{master_page}><style:table-properties style:writing-mode="lr-tb" table:display="true"/></style:style>
  </office:automatic-styles>
  <office:body><office:spreadsheet>
    <table:table table:name="Render" table:style-name="{table_style}"{print_range}>
      {''.join(columns)}
      <table:table-row table:style-name="{row_one_row_style}">{row_one}</table:table-row>
      <table:table-row table:style-name="ro-default"><table:table-cell office:value-type="float" office:value="{numeric:.2f}"><text:p>{numeric:.2f}</text:p></table:table-cell>{date_cell}{percent_cell}<table:table-cell{formula_attr} office:value-type="float" office:value="{result}"><text:p>{result}</text:p></table:table-cell></table:table-row>
      {merge_row}
      <table:table-row table:style-name="ro-default"{hidden_attr}>{_ods_string(texts[7])}</table:table-row>
      <table:table-row table:style-name="{wrap_row_style}">{_ods_string(wrapped_text, wrap_style)}</table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>
"""
    styles = f"""<?xml version="1.0" encoding="UTF-8"?>
<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" office:version="1.3">
  <office:font-face-decls><style:font-face style:name="Noto Sans CJK KR" svg:font-family="'Noto Sans CJK KR'"/></office:font-face-decls>
  <office:styles>
    <style:default-style style:family="table-cell"><style:text-properties style:font-name="Noto Sans CJK KR" style:font-name-asian="Noto Sans CJK KR" style:font-name-complex="Noto Sans CJK KR" fo:font-family="Noto Sans CJK KR" fo:font-size="11pt" style:font-size-asian="11pt" style:font-size-complex="11pt"/></style:default-style>
    <style:default-style style:family="table-row"><style:table-row-properties style:row-height="{ODS_DEFAULT_ROW_HEIGHT}"/></style:default-style>
  </office:styles>
  <office:automatic-styles><style:page-layout style:name="pm-render"><style:page-layout-properties fo:page-width="29.7cm" fo:page-height="21cm" style:print-orientation="landscape" fo:margin="1.27cm" style:print="headers grid"/></style:page-layout></office:automatic-styles>
  <office:master-styles><style:master-page style:name="mp-render" style:page-layout-name="pm-render"/></office:master-styles>
</office:document-styles>
"""
    manifest = """<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/></manifest:manifest>
"""
    return _zip_bytes(
        (
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
            ("META-INF/manifest.xml", manifest),
            ("content.xml", content),
            ("styles.xml", styles),
        )
    )


BUILDERS: dict[str, Callable[[CaseSpec], bytes]] = {
    "xls": _build_xls,
    "xlsx": _build_xlsx,
    "xlsb": _build_xlsb,
    "ods": _build_ods,
}


def build_case(spec: CaseSpec) -> bytes:
    if spec.format not in FORMATS:
        raise CorpusError(f"unsupported case format: {spec.format}")
    expected = f"{spec.format}-{spec.index:04d}"
    if spec.case_id != expected or spec.seed != SEED_BASE[spec.format] + spec.index:
        raise CorpusError(f"invalid deterministic identity for {spec.case_id}")
    if spec.features != case_features(spec.format, spec.index):
        raise CorpusError(f"invalid capability tags for {spec.case_id}")
    payload = BUILDERS[spec.format](spec)
    if len(payload) > MAX_CASE_BYTES:
        raise CorpusError(
            f"case byte cap exceeded by {spec.case_id}: {len(payload)} > {MAX_CASE_BYTES}"
        )
    return payload


def _manifest_row(spec: CaseSpec, payload: bytes) -> dict[str, object]:
    return {
        "byte_length": len(payload),
        "case_id": spec.case_id,
        "features": list(spec.features),
        "format": spec.format,
        "generator": GENERATOR,
        "generator_version": GENERATOR_VERSION,
        "license": LICENSE,
        "path": spec.relative_path,
        "redistribution": REDISTRIBUTION,
        "render_redistributable": True,
        "rights_tier": "S",
        "seed": spec.seed,
        "sha256": sha256(payload).hexdigest(),
        "source_redistributable": True,
    }


def materialize(profile: str) -> tuple[dict[str, object], list[tuple[CaseSpec, bytes]]]:
    cases: list[tuple[CaseSpec, bytes]] = []
    rows: list[dict[str, object]] = []
    total_bytes = 0
    for spec in profile_specs(profile):
        payload = build_case(spec)
        total_bytes += len(payload)
        if total_bytes > MAX_TOTAL_BYTES:
            raise CorpusError(
                f"total byte cap exceeded: {total_bytes} > {MAX_TOTAL_BYTES}"
            )
        cases.append((spec, payload))
        rows.append(_manifest_row(spec, payload))
    format_counts = {
        fmt: sum(1 for spec, _ in cases if spec.format == fmt) for fmt in FORMATS
    }
    manifest: dict[str, object] = {
        "case_count": len(cases),
        "feature_counts": _feature_counts(spec for spec, _ in cases),
        "files": rows,
        "format_counts": format_counts,
        "format_feature_counts": _format_feature_counts(
            spec for spec, _ in cases
        ),
        "generator": GENERATOR,
        "generator_version": GENERATOR_VERSION,
        "license": LICENSE,
        "profile": profile,
        "redistribution": REDISTRIBUTION,
        "render_redistributable": True,
        "rights_tier": "S",
        "schema_version": SCHEMA_VERSION,
        "source_redistributable": True,
        "total_bytes": total_bytes,
    }
    encoded = _json_bytes(manifest)
    if len(encoded) > MAX_MANIFEST_BYTES:
        raise CorpusError("manifest byte cap exceeded")
    return manifest, cases


def _json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n").encode(
        "utf-8"
    )


def resolve_output(profile: str, output: str | None) -> Path:
    base = OUTPUT_BASE.resolve()
    candidate = Path(output) if output else OUTPUT_BASE / profile
    if not candidate.is_absolute():
        candidate = ROOT / candidate
    if candidate.is_symlink():
        raise CorpusError("output directory must not be a symlink")
    resolved = candidate.resolve()
    try:
        relative = resolved.relative_to(base)
    except ValueError as exc:
        raise CorpusError(
            f"output must be under {OUTPUT_BASE.relative_to(ROOT).as_posix()}"
        ) from exc
    if not relative.parts:
        raise CorpusError("output must be a named directory below the generated corpus root")
    return resolved


def _atomic_write(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary_path = Path(temporary)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary_path, 0o644)
        os.replace(temporary_path, path)
    except BaseException:
        temporary_path.unlink(missing_ok=True)
        raise


def generate(profile: str, output: Path) -> dict[str, object]:
    manifest, cases = materialize(profile)
    output.parent.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix=f".{output.name}.stage-", dir=output.parent))
    backup: Path | None = None
    try:
        for spec, payload in cases:
            _atomic_write(stage / spec.relative_path, payload)
        _atomic_write(stage / MANIFEST_NAME, _json_bytes(manifest))

        if output.exists():
            if not output.is_dir() or output.is_symlink():
                raise CorpusError(f"existing output is not a regular directory: {output}")
            backup = Path(
                tempfile.mkdtemp(prefix=f".{output.name}.backup-", dir=output.parent)
            )
            backup.rmdir()
            os.replace(output, backup)
        try:
            os.replace(stage, output)
        except BaseException:
            if backup is not None and backup.exists() and not output.exists():
                os.replace(backup, output)
            raise
        if backup is not None:
            shutil.rmtree(backup)
        return manifest
    finally:
        if stage.exists():
            shutil.rmtree(stage)
        if backup is not None and backup.exists() and backup != output:
            shutil.rmtree(backup)


def _safe_manifest_path(output: Path, value: object) -> Path:
    if not isinstance(value, str):
        raise CorpusError("manifest path must be a string")
    pure = PurePosixPath(value)
    if pure.is_absolute() or not pure.parts or ".." in pure.parts:
        raise CorpusError(f"unsafe manifest path: {value!r}")
    path = output.joinpath(*pure.parts)
    try:
        path.resolve().relative_to(output.resolve())
    except ValueError as exc:
        raise CorpusError(f"manifest path escapes output: {value!r}") from exc
    if path.is_symlink():
        raise CorpusError(f"payload must not be a symlink: {value}")
    return path


def verify(profile: str, output: Path) -> dict[str, object]:
    manifest_path = output / MANIFEST_NAME
    if not manifest_path.is_file() or manifest_path.is_symlink():
        raise CorpusError(f"missing regular manifest: {manifest_path}")
    if manifest_path.stat().st_size > MAX_MANIFEST_BYTES:
        raise CorpusError("manifest byte cap exceeded")
    try:
        actual = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise CorpusError(f"cannot read manifest: {exc}") from exc

    expected, cases = materialize(profile)
    if actual != expected:
        raise CorpusError("manifest does not match the deterministic generator contract")

    expected_paths = {MANIFEST_NAME}
    total_bytes = 0
    for spec, generated in cases:
        path = _safe_manifest_path(output, spec.relative_path)
        expected_paths.add(spec.relative_path)
        if not path.is_file():
            raise CorpusError(f"missing payload: {spec.relative_path}")
        size = path.stat().st_size
        if size > MAX_CASE_BYTES:
            raise CorpusError(f"case byte cap exceeded: {spec.relative_path}")
        payload = path.read_bytes()
        total_bytes += len(payload)
        if total_bytes > MAX_TOTAL_BYTES:
            raise CorpusError("total byte cap exceeded while verifying")
        if payload != generated:
            raise CorpusError(f"payload is not exactly reproducible: {spec.relative_path}")

    actual_paths: set[str] = set()
    for path in output.rglob("*"):
        if path.is_symlink():
            raise CorpusError(f"generated tree contains a symlink: {path}")
        if path.is_file():
            actual_paths.add(path.relative_to(output).as_posix())
    if actual_paths != expected_paths:
        extras = sorted(actual_paths - expected_paths)
        missing = sorted(expected_paths - actual_paths)
        raise CorpusError(f"generated tree differs: extras={extras}, missing={missing}")
    return expected


def _summary(manifest: dict[str, object], output: Path | None = None) -> str:
    destination = f" output={output}" if output is not None else ""
    return (
        f"profile={manifest['profile']} cases={manifest['case_count']} "
        f"formats={manifest['format_counts']} bytes={manifest['total_bytes']}{destination}"
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--list", action="store_true", help="print the deterministic manifest")
    action.add_argument("--generate", action="store_true", help="atomically generate the corpus")
    action.add_argument("--verify", action="store_true", help="verify manifest and exact bytes")
    parser.add_argument("--profile", choices=sorted(PROFILE_COUNTS), default="pilot")
    parser.add_argument(
        "--output",
        help="output directory below local/render-corpus-generated (default: PROFILE)",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        if args.list:
            manifest, _ = materialize(args.profile)
            print(_json_bytes(manifest).decode("utf-8"), end="")
            return 0
        output = resolve_output(args.profile, args.output)
        if args.generate:
            manifest = generate(args.profile, output)
            print(f"generated {_summary(manifest, output)}")
            return 0
        manifest = verify(args.profile, output)
        print(f"verified {_summary(manifest, output)}")
        return 0
    except (CorpusError, OSError) as exc:
        print(f"error: {exc}", file=os.sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
