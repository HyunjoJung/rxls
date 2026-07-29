#!/usr/bin/env python3
"""Generate the deterministic project-owned OOXML row-height oracle matrix.

The 24 XLSX workbooks in this diagnostic corpus are intentionally separate from
the pilot/full render corpus. Twelve preserve the accepted implicit-row
baseline; twelve paired automatic-height cases isolate wrapping, font, row,
merge, RTL, width, and drawing-anchor effects without changing the release
lattice or its reviewed hashes. Generated files remain local-only below
``local/render-corpus-generated``.
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
import tempfile
from typing import Iterable
from xml.etree import ElementTree
import zlib
from zipfile import ZIP_STORED, BadZipFile, ZipFile, ZipInfo


ROOT = Path(__file__).resolve().parents[1]
OUTPUT_BASE = ROOT / "local" / "render-corpus-generated"
DEFAULT_OUTPUT = OUTPUT_BASE / "ooxml-row-diagnostic"
MANIFEST_NAME = "manifest.json"

SCHEMA_VERSION = 1
PROFILE = "ooxml-row-diagnostic"
GENERATOR = "rxls-ooxml-row-diagnostic"
GENERATOR_VERSION = "1.1.0"
LICENSE = "MIT"
REDISTRIBUTION = "allowed"
RIGHTS_TIER = "S"

DOS_EPOCH = (1980, 1, 1, 0, 0, 0)
MAX_CASES = 24
MAX_CASE_BYTES = 256 * 1024
MAX_TOTAL_BYTES = 3 * 1024 * 1024
MAX_MANIFEST_BYTES = 256 * 1024
MAX_ZIP_PARTS = 12
MAX_PACKAGE_RELATIONSHIPS = 4
MAX_IMAGE_BYTES = 16 * 1024

SHEET_NS = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
REL_NS = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
PACKAGE_REL_NS = "http://schemas.openxmlformats.org/package/2006/relationships"
DRAWING_NS = "http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"
DRAWING_MAIN_NS = "http://schemas.openxmlformats.org/drawingml/2006/main"

BASELINE_TOGGLES = (
    "explicit-row-height",
    "hidden-row",
    "right-to-left-layout",
    "image-drawing",
)
AUTOHEIGHT_TOGGLES = (
    "auto-long-unwrapped",
    "auto-wrapped-long",
    "auto-wrapped-wide",
    "auto-bold-font",
    "auto-large-font",
    "auto-bold-font-wrapped",
    "auto-wrapped-explicit",
    "auto-wrapped-hidden",
    "auto-wrapped-merged",
    "auto-wrapped-rtl",
    "auto-wrapped-image",
    "auto-wrapped-long-anchor",
)
WRAPPED_TOGGLES = frozenset(
    {
        "auto-wrapped-long",
        "auto-wrapped-wide",
        "auto-bold-font-wrapped",
        "auto-wrapped-explicit",
        "auto-wrapped-hidden",
        "auto-wrapped-merged",
        "auto-wrapped-rtl",
        "auto-wrapped-image",
        "auto-wrapped-long-anchor",
    }
)
BOLD_FONT_TOGGLES = frozenset(
    {"auto-bold-font", "auto-bold-font-wrapped"}
)
LARGE_FONT_TOGGLES = frozenset(
    {"auto-large-font"}
)
DRAWING_TOGGLES = frozenset(
    {
        "image-drawing",
        "auto-wrapped-image",
        "auto-wrapped-long-anchor",
    }
)
LONG_AUTO_TEXT = (
    "한국어 자동 줄바꿈 English 日本語 中文 0123456789 "
    "한국어 자동 줄바꿈 English 日本語 中文 0123456789 "
    "한국어 자동 줄바꿈 English 日本語 中文 0123456789"
)


class OracleCorpusError(RuntimeError):
    """Raised when generation or verification violates the diagnostic contract."""


@dataclass(frozen=True)
class CaseSpec:
    """One deterministic structural variant in the implicit-row matrix."""

    case_id: str
    sheet_format_present: bool
    font_family: str
    font_size: int
    toggle: str | None = None

    @property
    def features(self) -> tuple[str, ...]:
        """Return the sorted path-neutral structural feature vocabulary."""

        values = {
            "ooxml-implicit-row",
            (
                "sheet-format-present"
                if self.sheet_format_present
                else "sheet-format-missing"
            ),
            (
                "normal-font-noto"
                if self.font_family == "Noto Sans CJK KR"
                else "normal-font-carlito"
            ),
            f"normal-size-{self.font_size}",
        }
        if self.toggle is not None:
            values.add(self.toggle)
        return tuple(sorted(values))

    @property
    def relative_path(self) -> str:
        """Return the local generated payload path."""

        return f"payload/xlsx/{self.case_id}.xlsx"


def _matrix() -> tuple[CaseSpec, ...]:
    core = tuple(
        CaseSpec(
            case_id=(
                f"row-{sheet_state}-"
                f"{'noto' if family == 'Noto Sans CJK KR' else 'carlito'}-{size}"
            ),
            sheet_format_present=present,
            font_family=family,
            font_size=size,
        )
        for present, sheet_state in ((False, "missing"), (True, "present"))
        for family in ("Noto Sans CJK KR", "Carlito")
        for size in (11, 12)
    )
    stress = tuple(
        CaseSpec(
            case_id=f"row-missing-noto-11-{toggle}",
            sheet_format_present=False,
            font_family="Noto Sans CJK KR",
            font_size=11,
            toggle=toggle,
        )
        for toggle in BASELINE_TOGGLES
    )
    autoheight = tuple(
        CaseSpec(
            case_id=f"row-missing-noto-11-{toggle}",
            sheet_format_present=False,
            font_family="Noto Sans CJK KR",
            font_size=11,
            toggle=toggle,
        )
        for toggle in AUTOHEIGHT_TOGGLES
    )
    specs = core + stress + autoheight
    if len(specs) != MAX_CASES or len({spec.case_id for spec in specs}) != MAX_CASES:
        raise OracleCorpusError("diagnostic matrix identity")
    return specs


CASES = _matrix()


def _json_bytes(value: object) -> bytes:
    return (
        json.dumps(value, indent=2, sort_keys=True, ensure_ascii=True) + "\n"
    ).encode("utf-8")


def _relationships_xml(
    relationships: Iterable[tuple[str, str, str]],
) -> str:
    rows = tuple(relationships)
    if len(rows) > MAX_PACKAGE_RELATIONSHIPS:
        raise OracleCorpusError("relationship cap exceeded")
    body = "".join(
        f'<Relationship Id="{escape(identifier, quote=True)}" '
        f'Type="{escape(kind, quote=True)}" '
        f'Target="{escape(target, quote=True)}"/>'
        for identifier, kind, target in rows
    )
    return (
        '<?xml version="1.0" encoding="UTF-8"?>'
        f'<Relationships xmlns="{PACKAGE_REL_NS}">{body}</Relationships>'
    )


def _zip_bytes(parts: Iterable[tuple[str, str | bytes]]) -> bytes:
    rows = tuple(parts)
    if len(rows) > MAX_ZIP_PARTS:
        raise OracleCorpusError("ZIP part cap exceeded")
    seen: set[str] = set()
    output = io.BytesIO()
    with ZipFile(output, "w") as archive:
        for name, body in rows:
            pure = PurePosixPath(name)
            if (
                pure.is_absolute()
                or not pure.parts
                or ".." in pure.parts
                or name in seen
            ):
                raise OracleCorpusError(f"unsafe or duplicate ZIP part: {name}")
            seen.add(name)
            info = ZipInfo(name, DOS_EPOCH)
            info.compress_type = ZIP_STORED
            info.create_system = 0
            info.external_attr = 0
            payload = body.encode("utf-8") if isinstance(body, str) else body
            archive.writestr(info, payload)
    return output.getvalue()


def _u32_be(value: int) -> bytes:
    return value.to_bytes(4, "big")


def _png_chunk(kind: bytes, payload: bytes) -> bytes:
    checksum = zlib.crc32(kind)
    checksum = zlib.crc32(payload, checksum) & 0xFFFFFFFF
    return _u32_be(len(payload)) + kind + payload + _u32_be(checksum)


def _stored_zlib(payload: bytes) -> bytes:
    if len(payload) > 0xFFFF:
        raise OracleCorpusError("PNG scanline cap exceeded")
    length = len(payload)
    return (
        b"\x78\x01\x01"
        + length.to_bytes(2, "little")
        + ((~length) & 0xFFFF).to_bytes(2, "little")
        + payload
        + _u32_be(zlib.adler32(payload) & 0xFFFFFFFF)
    )


def _project_png(spec: CaseSpec) -> bytes:
    width = 8
    height = 8
    seed = int.from_bytes(sha256(spec.case_id.encode("ascii")).digest()[:4], "big")
    scanlines = bytearray()
    for row in range(height):
        scanlines.append(0)
        for col in range(width):
            scanlines.extend(
                (
                    (seed + row * 19 + col * 7) & 0xFF,
                    (80 + row * 11 + col * 5) & 0xFF,
                    (160 + row * 3 + col * 17) & 0xFF,
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
        raise OracleCorpusError("PNG byte cap exceeded")
    return payload


def _styles(spec: CaseSpec) -> str:
    family = escape(spec.font_family, quote=True)
    size = spec.font_size
    wrapped = spec.toggle in WRAPPED_TOGGLES
    bold_font = spec.toggle in BOLD_FONT_TOGGLES
    large_font = spec.toggle in LARGE_FONT_TOGGLES
    styled_font = bold_font or large_font
    secondary_size = 14 if large_font else size
    secondary_weight = "<b/>" if bold_font else ""
    fonts = (
        f'<fonts count="2"><font><sz val="{size}"/><name val="{family}"/>'
        f'<family val="2"/></font><font>{secondary_weight}'
        f'<sz val="{secondary_size}"/><name val="{family}"/>'
        '<family val="2"/></font></fonts>'
        if styled_font
        else (
            f'<fonts count="1"><font><sz val="{size}"/><name val="{family}"/>'
            "<family val=\"2\"/></font></fonts>"
        )
    )
    if wrapped or styled_font:
        font_id = 1 if styled_font else 0
        apply_font = ' applyFont="1"' if styled_font else ""
        alignment = (
            ' applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf>'
            if wrapped
            else "/>"
        )
        cell_xfs = (
            '<cellXfs count="2">'
            '<xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>'
            f'<xf numFmtId="0" fontId="{font_id}" fillId="0" borderId="0" '
            f'xfId="0"{apply_font}{alignment}</cellXfs>'
        )
    else:
        cell_xfs = (
            '<cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" '
            'borderId="0" xfId="0"/></cellXfs>'
        )
    return f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="{SHEET_NS}">
  {fonts}
  <fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills>
  <borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  {cell_xfs}
  <cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles>
</styleSheet>
"""


def _drawing_xml(spec: CaseSpec) -> str:
    if spec.toggle == "auto-wrapped-long-anchor":
        from_row, to_row = 8, 18
    else:
        from_row, to_row = 1, 7
    return f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<xdr:wsDr xmlns:xdr="{DRAWING_NS}" xmlns:a="{DRAWING_MAIN_NS}" xmlns:r="{REL_NS}">
  <xdr:twoCellAnchor editAs="oneCell">
    <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{from_row}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
    <xdr:to><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{to_row}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
    <xdr:pic>
      <xdr:nvPicPr><xdr:cNvPr id="2" name="Project-authored row diagnostic"/><xdr:cNvPicPr><a:picLocks noChangeAspect="1"/></xdr:cNvPicPr></xdr:nvPicPr>
      <xdr:blipFill><a:blip r:embed="rIdImage"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill>
      <xdr:spPr><a:xfrm/><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr>
    </xdr:pic>
    <xdr:clientData/>
  </xdr:twoCellAnchor>
</xdr:wsDr>
"""


def _worksheet(spec: CaseSpec) -> str:
    sheet_format = (
        '<sheetFormatPr defaultRowHeight="15" customHeight="1"/>'
        if spec.sheet_format_present
        else ""
    )
    right_to_left = (
        ' rightToLeft="1"'
        if spec.toggle in {"right-to-left-layout", "auto-wrapped-rtl"}
        else ""
    )
    columns = (
        '<cols><col min="1" max="1" width="24" customWidth="1"/></cols>'
        if spec.toggle == "auto-wrapped-wide"
        else ""
    )
    columns_line = f"  {columns}\n" if columns else ""
    row_four = ""
    if spec.toggle == "explicit-row-height":
        row_four = '<row r="4" ht="21" customHeight="1"/>'
    elif spec.toggle == "hidden-row":
        row_four = '<row r="4" hidden="1"/>'
    elif spec.toggle in AUTOHEIGHT_TOGGLES:
        text = (
            "Bold automatic row"
            if spec.toggle in BOLD_FONT_TOGGLES
            else (
                "Large font automatic row"
                if spec.toggle == "auto-large-font"
                else LONG_AUTO_TEXT
            )
        )
        style = "" if spec.toggle == "auto-long-unwrapped" else ' s="1"'
        row_attrs = ""
        if spec.toggle == "auto-wrapped-explicit":
            row_attrs = ' ht="42" customHeight="1"'
        elif spec.toggle == "auto-wrapped-hidden":
            row_attrs = ' hidden="1"'
        row_four = (
            f'<row r="4"{row_attrs}><c r="A4"{style} t="inlineStr">'
            f"<is><t>{escape(text)}</t></is></c></row>"
        )
    merge_cells = (
        '<mergeCells count="1"><mergeCell ref="A4:B4"/></mergeCells>'
        if spec.toggle == "auto-wrapped-merged"
        else ""
    )
    drawing = (
        '<drawing r:id="rIdDrawing"/>'
        if spec.toggle in DRAWING_TOGGLES
        else ""
    )
    return f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="{SHEET_NS}" xmlns:r="{REL_NS}">
  <dimension ref="A1:B8"/>
  <sheetViews><sheetView workbookViewId="0" showGridLines="1"{right_to_left}/></sheetViews>
  {sheet_format}
{columns_line}\
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>row oracle</t></is></c></row>
    {row_four}
    <row r="8"><c r="B8"><v>1</v></c></row>
  </sheetData>
  {merge_cells}{drawing}
</worksheet>
"""


def build_case(spec: CaseSpec) -> bytes:
    """Build one canonical XLSX package and validate its structural contract."""

    if spec not in CASES:
        raise OracleCorpusError("unknown case specification")
    drawing = spec.toggle in DRAWING_TOGGLES
    defaults = [
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>',
        '<Default Extension="xml" ContentType="application/xml"/>',
    ]
    overrides = [
        '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>',
        '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>',
        '<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>',
    ]
    if drawing:
        defaults.append('<Default Extension="png" ContentType="image/png"/>')
        overrides.append(
            '<Override PartName="/xl/drawings/drawing1.xml" '
            'ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/>'
        )
    content_types = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        + "".join(defaults + overrides)
        + "</Types>"
    )
    workbook = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        f'<workbook xmlns="{SHEET_NS}" xmlns:r="{REL_NS}">'
        '<sheets><sheet name="RowOracle" sheetId="1" r:id="rId1"/></sheets>'
        "</workbook>"
    )
    parts: list[tuple[str, str | bytes]] = [
        ("[Content_Types].xml", content_types),
        (
            "_rels/.rels",
            _relationships_xml(
                (
                    (
                        "rId1",
                        (
                            "http://schemas.openxmlformats.org/"
                            "officeDocument/2006/relationships/officeDocument"
                        ),
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
                        (
                            "http://schemas.openxmlformats.org/"
                            "officeDocument/2006/relationships/worksheet"
                        ),
                        "worksheets/sheet1.xml",
                    ),
                    (
                        "rId2",
                        (
                            "http://schemas.openxmlformats.org/"
                            "officeDocument/2006/relationships/styles"
                        ),
                        "styles.xml",
                    ),
                )
            ),
        ),
        ("xl/styles.xml", _styles(spec)),
        ("xl/worksheets/sheet1.xml", _worksheet(spec)),
    ]
    if drawing:
        parts.extend(
            (
                (
                    "xl/worksheets/_rels/sheet1.xml.rels",
                    _relationships_xml(
                        (
                            (
                                "rIdDrawing",
                                (
                                    "http://schemas.openxmlformats.org/"
                                    "officeDocument/2006/relationships/drawing"
                                ),
                                "../drawings/drawing1.xml",
                            ),
                        )
                    ),
                ),
                ("xl/drawings/drawing1.xml", _drawing_xml(spec)),
                (
                    "xl/drawings/_rels/drawing1.xml.rels",
                    _relationships_xml(
                        (
                            (
                                "rIdImage",
                                (
                                    "http://schemas.openxmlformats.org/"
                                    "officeDocument/2006/relationships/image"
                                ),
                                "../media/image1.png",
                            ),
                        )
                    ),
                ),
                ("xl/media/image1.png", _project_png(spec)),
            )
        )
    payload = _zip_bytes(parts)
    if len(payload) > MAX_CASE_BYTES:
        raise OracleCorpusError("case byte cap exceeded")
    _validate_package(spec, payload)
    return payload


def _validate_package(spec: CaseSpec, payload: bytes) -> None:
    drawing_root: ElementTree.Element | None = None
    try:
        with ZipFile(io.BytesIO(payload)) as archive:
            infos = archive.infolist()
            names = archive.namelist()
            if (
                archive.testzip() is not None
                or len(infos) > MAX_ZIP_PARTS
                or len(names) != len(set(names))
                or any(info.date_time != DOS_EPOCH for info in infos)
                or any(info.compress_type != ZIP_STORED for info in infos)
                or any(info.flag_bits & 0x1 for info in infos)
            ):
                raise OracleCorpusError("non-canonical XLSX package")
            for name in names:
                if name.endswith((".xml", ".rels")):
                    root = ElementTree.fromstring(archive.read(name))
                    if name.endswith(".rels") and any(
                        row.attrib.get("TargetMode") == "External" for row in root
                    ):
                        raise OracleCorpusError("external relationship")
            sheet = ElementTree.fromstring(
                archive.read("xl/worksheets/sheet1.xml")
            )
            styles = ElementTree.fromstring(archive.read("xl/styles.xml"))
            has_drawing_parts = "xl/drawings/drawing1.xml" in names
            if has_drawing_parts:
                drawing_root = ElementTree.fromstring(
                    archive.read("xl/drawings/drawing1.xml")
                )
    except (BadZipFile, KeyError, ElementTree.ParseError) as error:
        raise OracleCorpusError("invalid XLSX package") from error

    namespace = {"s": SHEET_NS}
    sheet_format = sheet.find("s:sheetFormatPr", namespace)
    if (sheet_format is not None) != spec.sheet_format_present:
        raise OracleCorpusError("sheet format feature mismatch")
    if sheet_format is not None and sheet_format.attrib != {
        "customHeight": "1",
        "defaultRowHeight": "15",
    }:
        raise OracleCorpusError("sheet format attributes")
    wrapped = spec.toggle in WRAPPED_TOGGLES
    bold_font = spec.toggle in BOLD_FONT_TOGGLES
    large_font = spec.toggle in LARGE_FONT_TOGGLES
    styled_font = bold_font or large_font
    fonts = styles.findall("s:fonts/s:font", namespace)
    expected_font_count = 2 if styled_font else 1
    if len(fonts) != expected_font_count:
        raise OracleCorpusError("Normal font missing")
    font_container = styles.find("s:fonts", namespace)
    if (
        font_container is None
        or font_container.attrib != {"count": str(expected_font_count)}
    ):
        raise OracleCorpusError("font count mismatch")
    name = fonts[0].find("s:name", namespace)
    size = fonts[0].find("s:sz", namespace)
    if (
        name is None
        or size is None
        or name.attrib.get("val") != spec.font_family
        or size.attrib.get("val") != str(spec.font_size)
    ):
        raise OracleCorpusError("Normal font mismatch")
    if styled_font:
        styled_name = fonts[1].find("s:name", namespace)
        styled_size = fonts[1].find("s:sz", namespace)
        bold = fonts[1].find("s:b", namespace)
        if (
            styled_name is None
            or styled_size is None
            or styled_name.attrib.get("val") != spec.font_family
            or styled_size.attrib.get("val")
            != ("14" if large_font else str(spec.font_size))
            or (bold is not None) != bold_font
        ):
            raise OracleCorpusError("styled font mismatch")
    a1 = sheet.find("s:sheetData/s:row[@r='1']/s:c[@r='A1']", namespace)
    style_xfs = styles.findall("s:cellStyleXfs/s:xf", namespace)
    cell_xfs = styles.findall("s:cellXfs/s:xf", namespace)
    expected_cell_xfs = 2 if wrapped or styled_font else 1
    if (
        a1 is None
        or "s" in a1.attrib
        or len(style_xfs) != 1
        or len(cell_xfs) != expected_cell_xfs
        or "applyAlignment" in style_xfs[0].attrib
        or style_xfs[0].find("s:alignment", namespace) is not None
        or "applyAlignment" in cell_xfs[0].attrib
        or cell_xfs[0].find("s:alignment", namespace) is not None
    ):
        raise OracleCorpusError("implicit vertical alignment contract")
    if expected_cell_xfs == 2:
        expected_xf = {
            "borderId": "0",
            "fillId": "0",
            "fontId": "1" if styled_font else "0",
            "numFmtId": "0",
            "xfId": "0",
        }
        if styled_font:
            expected_xf["applyFont"] = "1"
        if wrapped:
            expected_xf["applyAlignment"] = "1"
        alignment = cell_xfs[1].find("s:alignment", namespace)
        if (
            cell_xfs[1].attrib != expected_xf
            or (alignment is not None) != wrapped
            or (
                alignment is not None
                and alignment.attrib != {"vertical": "top", "wrapText": "1"}
            )
        ):
            raise OracleCorpusError("automatic row style contract")
    row_four = sheet.find("s:sheetData/s:row[@r='4']", namespace)
    expected_row = {
        "explicit-row-height": {"customHeight": "1", "ht": "21", "r": "4"},
        "hidden-row": {"hidden": "1", "r": "4"},
    }.get(spec.toggle)
    if spec.toggle in AUTOHEIGHT_TOGGLES:
        expected_row = {"r": "4"}
        if spec.toggle == "auto-wrapped-explicit":
            expected_row.update({"customHeight": "1", "ht": "42"})
        elif spec.toggle == "auto-wrapped-hidden":
            expected_row["hidden"] = "1"
        cell = (
            row_four.find("s:c[@r='A4']", namespace)
            if row_four is not None
            else None
        )
        inline_text = (
            cell.find("s:is/s:t", namespace)
            if cell is not None
            else None
        )
        expected_text = (
            "Bold automatic row"
            if spec.toggle in BOLD_FONT_TOGGLES
            else (
                "Large font automatic row"
                if spec.toggle == "auto-large-font"
                else LONG_AUTO_TEXT
            )
        )
        expected_cell = {"r": "A4", "t": "inlineStr"}
        if spec.toggle != "auto-long-unwrapped":
            expected_cell["s"] = "1"
        if (
            row_four is None
            or row_four.attrib != expected_row
            or cell is None
            or cell.attrib != expected_cell
            or inline_text is None
            or inline_text.text != expected_text
        ):
            raise OracleCorpusError("automatic row feature mismatch")
    elif expected_row is None:
        if row_four is not None:
            raise OracleCorpusError("unexpected row four")
    elif row_four is None or row_four.attrib != expected_row:
        raise OracleCorpusError("row four feature mismatch")
    view = sheet.find("s:sheetViews/s:sheetView", namespace)
    expected_rtl = spec.toggle in {
        "right-to-left-layout",
        "auto-wrapped-rtl",
    }
    if view is None or view.attrib.get("rightToLeft") != (
        "1" if expected_rtl else None
    ):
        raise OracleCorpusError("RTL feature mismatch")
    columns = sheet.findall("s:cols/s:col", namespace)
    if spec.toggle == "auto-wrapped-wide":
        if len(columns) != 1 or columns[0].attrib != {
            "customWidth": "1",
            "max": "1",
            "min": "1",
            "width": "24",
        }:
            raise OracleCorpusError("column width feature mismatch")
    elif columns:
        raise OracleCorpusError("unexpected column width")
    merges = sheet.findall("s:mergeCells/s:mergeCell", namespace)
    if spec.toggle == "auto-wrapped-merged":
        if len(merges) != 1 or merges[0].attrib != {"ref": "A4:B4"}:
            raise OracleCorpusError("merge feature mismatch")
    elif merges:
        raise OracleCorpusError("unexpected merge")
    drawing = sheet.find("s:drawing", namespace)
    if (
        (drawing is not None) != (spec.toggle in DRAWING_TOGGLES)
        or has_drawing_parts != (spec.toggle in DRAWING_TOGGLES)
    ):
        raise OracleCorpusError("drawing feature mismatch")
    if drawing_root is not None:
        drawing_namespace = {"xdr": DRAWING_NS}
        start = drawing_root.find(
            "xdr:twoCellAnchor/xdr:from/xdr:row",
            drawing_namespace,
        )
        end = drawing_root.find(
            "xdr:twoCellAnchor/xdr:to/xdr:row",
            drawing_namespace,
        )
        expected_anchor = (
            ("8", "18")
            if spec.toggle == "auto-wrapped-long-anchor"
            else ("1", "7")
        )
        if (
            start is None
            or end is None
            or (start.text, end.text) != expected_anchor
        ):
            raise OracleCorpusError("drawing anchor mismatch")


def _feature_counts(specs: Iterable[CaseSpec]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for spec in specs:
        for feature in spec.features:
            counts[feature] = counts.get(feature, 0) + 1
    return dict(sorted(counts.items()))


def materialize() -> tuple[dict[str, object], list[tuple[CaseSpec, bytes]]]:
    """Return the exact manifest and deterministic payload bytes."""

    cases: list[tuple[CaseSpec, bytes]] = []
    rows: list[dict[str, object]] = []
    total_bytes = 0
    for index, spec in enumerate(CASES):
        payload = build_case(spec)
        total_bytes += len(payload)
        if total_bytes > MAX_TOTAL_BYTES:
            raise OracleCorpusError("total byte cap exceeded")
        cases.append((spec, payload))
        rows.append(
            {
                "byte_length": len(payload),
                "case_id": spec.case_id,
                "features": list(spec.features),
                "format": "xlsx",
                "generator": GENERATOR,
                "generator_version": GENERATOR_VERSION,
                "license": LICENSE,
                "path": spec.relative_path,
                "redistribution": REDISTRIBUTION,
                "render_redistributable": True,
                "rights_tier": RIGHTS_TIER,
                "seed": 550_000 + index,
                "sha256": sha256(payload).hexdigest(),
                "source_redistributable": True,
            }
        )
    features = _feature_counts(spec for spec, _ in cases)
    manifest: dict[str, object] = {
        "case_count": len(cases),
        "feature_counts": features,
        "files": rows,
        "format_counts": {"xlsx": len(cases)},
        "format_feature_counts": {"xlsx": features},
        "generator": GENERATOR,
        "generator_version": GENERATOR_VERSION,
        "license": LICENSE,
        "profile": PROFILE,
        "redistribution": REDISTRIBUTION,
        "render_redistributable": True,
        "rights_tier": RIGHTS_TIER,
        "schema_version": SCHEMA_VERSION,
        "source_redistributable": True,
        "total_bytes": total_bytes,
    }
    if len(cases) != MAX_CASES or len(_json_bytes(manifest)) > MAX_MANIFEST_BYTES:
        raise OracleCorpusError("manifest contract")
    return manifest, cases


def resolve_output(value: str | None) -> Path:
    base = OUTPUT_BASE.resolve()
    candidate = Path(value) if value else DEFAULT_OUTPUT
    if not candidate.is_absolute():
        candidate = ROOT / candidate
    if candidate.is_symlink():
        raise OracleCorpusError("output directory must not be a symlink")
    resolved = candidate.resolve()
    try:
        relative = resolved.relative_to(base)
    except ValueError as error:
        raise OracleCorpusError(
            "output must be below local/render-corpus-generated"
        ) from error
    if not relative.parts:
        raise OracleCorpusError("output must be a named child directory")
    return resolved


def _atomic_write(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(
        prefix=f".{path.name}.", dir=path.parent
    )
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


def generate(output: Path) -> dict[str, object]:
    """Atomically generate the complete diagnostic tree."""

    manifest, cases = materialize()
    output.parent.mkdir(parents=True, exist_ok=True)
    stage = Path(
        tempfile.mkdtemp(prefix=f".{output.name}.stage-", dir=output.parent)
    )
    backup: Path | None = None
    try:
        for spec, payload in cases:
            _atomic_write(stage / spec.relative_path, payload)
        _atomic_write(stage / MANIFEST_NAME, _json_bytes(manifest))
        if output.exists():
            if not output.is_dir() or output.is_symlink():
                raise OracleCorpusError("existing output is not a regular directory")
            backup = Path(
                tempfile.mkdtemp(
                    prefix=f".{output.name}.backup-", dir=output.parent
                )
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
        raise OracleCorpusError("manifest path")
    pure = PurePosixPath(value)
    if pure.is_absolute() or not pure.parts or ".." in pure.parts:
        raise OracleCorpusError("manifest path")
    path = output.joinpath(*pure.parts)
    try:
        path.resolve().relative_to(output.resolve())
    except ValueError as error:
        raise OracleCorpusError("manifest path") from error
    if path.is_symlink():
        raise OracleCorpusError("payload symlink")
    return path


def verify(output: Path) -> dict[str, object]:
    """Verify exact manifest bytes, payload bytes, and generated namespace."""

    manifest_path = output / MANIFEST_NAME
    if (
        not manifest_path.is_file()
        or manifest_path.is_symlink()
        or manifest_path.stat().st_size > MAX_MANIFEST_BYTES
    ):
        raise OracleCorpusError("manifest file")
    try:
        actual = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise OracleCorpusError("manifest JSON") from error
    expected, cases = materialize()
    if actual != expected:
        raise OracleCorpusError("manifest contract mismatch")
    expected_paths = {MANIFEST_NAME}
    total = 0
    for spec, payload in cases:
        path = _safe_manifest_path(output, spec.relative_path)
        expected_paths.add(spec.relative_path)
        if not path.is_file():
            raise OracleCorpusError("payload missing")
        observed = path.read_bytes()
        total += len(observed)
        if total > MAX_TOTAL_BYTES or observed != payload:
            raise OracleCorpusError("payload mismatch")
    actual_paths: set[str] = set()
    for path in output.rglob("*"):
        if path.is_symlink():
            raise OracleCorpusError("generated tree symlink")
        if path.is_file():
            actual_paths.add(path.relative_to(output).as_posix())
    if actual_paths != expected_paths:
        raise OracleCorpusError("generated namespace mismatch")
    return expected


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--list", action="store_true")
    action.add_argument("--generate", action="store_true")
    action.add_argument("--verify", action="store_true")
    parser.add_argument("--output")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        if args.list:
            manifest, _ = materialize()
            print(_json_bytes(manifest).decode("utf-8"), end="")
            return 0
        output = resolve_output(args.output)
        manifest = generate(output) if args.generate else verify(output)
        print(
            f"{'generated' if args.generate else 'verified'} "
            f"profile={manifest['profile']} cases={manifest['case_count']} "
            f"bytes={manifest['total_bytes']} output={output}"
        )
        return 0
    except (OracleCorpusError, OSError) as error:
        print(f"ooxml-row-oracle-generator: {error}", file=os.sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
