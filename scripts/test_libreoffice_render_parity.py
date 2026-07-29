#!/usr/bin/env python3
"""Unit tests for the bounded LibreOffice rendering parity harness."""

from __future__ import annotations

import copy
import hashlib
import importlib.metadata
import importlib.util
import json
import os
from pathlib import Path
import platform
import random
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from zipfile import ZIP_DEFLATED, ZipFile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "libreoffice-render-parity.py"


def load_module():
    spec = importlib.util.spec_from_file_location("libreoffice_render_parity", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def rgb_buffer(
    width: int,
    height: int,
    pixels: dict[tuple[int, int], tuple[int, int, int]] | None = None,
) -> bytes:
    payload = bytearray([255, 255, 255] * width * height)
    for (x, y), color in (pixels or {}).items():
        offset = (y * width + x) * 3
        payload[offset : offset + 3] = bytes(color)
    return bytes(payload)


def write_bundle(
    bundle_dir: Path,
    source: Path,
    *,
    visibilities: tuple[str, ...] = ("visible",),
    width: str = "96",
    height: str = "48",
    font_pack_sha256: str | None = None,
    render_schema_version: int = 1,
    font_faces: list[dict[str, object]] | None = None,
    print_pages: bool = False,
    print_schema_version: int = 1,
) -> None:
    bundle_dir.mkdir(parents=True, exist_ok=True)
    sheets = []
    for index, visibility in enumerate(visibilities):
        filename = f"sheet-{index:04d}.svg"
        payload = (
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" '
            f'height="{height}" viewBox="0 0 96 48">'
            f'<rect width="96" height="48" fill="#fff"/>'
            f'<text x="2" y="12">sheet {index}</text></svg>\n'
        ).encode()
        path = bundle_dir / filename
        path.write_bytes(payload)
        sheet = {
            "index": index,
            "name": f"Sheet {index}",
            "visibility": visibility,
            "file": filename,
            "canvas": {"width_raw": 96 * 1024, "height_raw": 48 * 1024},
            "svg": {
                "bytes": len(payload),
                "sha256": hashlib.sha256(payload).hexdigest(),
            },
            "scene": {
                "sha256": hashlib.sha256(f"scene {index}".encode()).hexdigest()
            },
            "report": {
                "schema_version": render_schema_version,
                "sheet_index": index,
                "sheet_name": f"Sheet {index}",
                "svg_bytes": len(payload),
                "warnings": [],
            },
        }
        if render_schema_version == 2:
            sheet["report"]["font_pack_sha256"] = font_pack_sha256
            sheet["report"]["font_faces"] = list(font_faces or [])
        if print_pages:
            pdf_filename = f"sheet-{index:04d}.pdf"
            pdf_payload = b"%PDF-mocked"
            (bundle_dir / pdf_filename).write_bytes(pdf_payload)
            page_filename = f"sheet-{index:04d}-pages/page-0001.svg"
            page_payload = (
                '<svg xmlns="http://www.w3.org/2000/svg" width="816" '
                'height="1056" viewBox="0 0 816 1056">'
                f'<g role="text" aria-label="sheet {index}" '
                f'data-rxls-visible-label="sheet {index}"><path/></g></svg>\n'
            ).encode()
            page_path = bundle_dir / page_filename
            page_path.parent.mkdir()
            page_path.write_bytes(page_payload)
            print_report = {
                "schema_version": print_schema_version,
                "sheet_index": index,
                "sheet_name": f"Sheet {index}",
                "source_report": sheet["report"],
                "layout_override": "single_page_sheets",
                "pages": [{"output_index": 0}],
                "warnings": [],
            }
            if print_schema_version == 2:
                print_report["source_reports"] = [print_report["source_report"]]
            report_payload = json.dumps(print_report, sort_keys=True).encode()
            report_filename = f"sheet-{index:04d}-pages.json"
            (bundle_dir / report_filename).write_bytes(report_payload)
            page_scene_sha = hashlib.sha256(
                f"print scene {index}".encode()
            ).hexdigest()
            sheet["print"] = {
                "schema": "rxls.render.print-bundle.v1",
                "layout_override": "single_page_sheets",
                "page_count": 1,
                "report": {
                    "file": report_filename,
                    "bytes": len(report_payload),
                    "sha256": hashlib.sha256(report_payload).hexdigest(),
                },
                "page_scenes": [{"index": 0, "sha256": page_scene_sha}],
                "svg_pages": [
                    {
                        "file": page_filename,
                        "bytes": len(page_payload),
                        "sha256": hashlib.sha256(page_payload).hexdigest(),
                    }
                ],
                "pdf": {
                    "file": pdf_filename,
                    "bytes": len(pdf_payload),
                    "sha256": hashlib.sha256(pdf_payload).hexdigest(),
                },
                "png_dpi": None,
                "png_pages": [],
            }
        sheets.append(sheet)
    source_bytes = source.read_bytes()
    manifest = {
        "schema": MODULE.RENDER_MANIFEST_SCHEMA,
        "source": {
            "sha256": hashlib.sha256(source_bytes).hexdigest(),
            "bytes": len(source_bytes),
        },
        "renderer": {
            "name": "rxls-render",
            "version": "0.1.0",
            "fixed_units_per_pixel": 1024,
            "font_pack_sha256": font_pack_sha256,
        },
        "sheets": sheets,
    }
    (bundle_dir / "render-manifest.json").write_text(
        json.dumps(manifest, sort_keys=True), encoding="utf-8"
    )


def write_authored_bundle(
    bundle_dir: Path,
    source: Path,
    *,
    visibilities: tuple[str, ...] = ("visible",),
    scale_mode: str = MODULE.AUTHORED_SCALE_MODE_SCALE,
) -> None:
    if scale_mode not in MODULE.AUTHORED_SCALE_MODES:
        raise ValueError("invalid authored scale mode")
    write_bundle(
        bundle_dir,
        source,
        visibilities=visibilities,
        print_pages=True,
        print_schema_version=2,
    )
    manifest_path = bundle_dir / "render-manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    for sheet_index, sheet in enumerate(manifest["sheets"]):
        print_bundle = sheet["print"]
        del print_bundle["layout_override"]
        page_count = (
            4 if scale_mode == MODULE.AUTHORED_SCALE_MODE_SCALE else 1
        )
        print_bundle["page_count"] = page_count
        first_page = bundle_dir / print_bundle["svg_pages"][0]["file"]
        page_payload = first_page.read_bytes()
        page_artifacts = []
        page_scenes = []
        for page_index in range(page_count):
            filename = (
                f"sheet-{sheet_index:04d}-pages/page-{page_index + 1:04d}.svg"
            )
            path = bundle_dir / filename
            if page_index:
                path.write_bytes(page_payload)
            page_artifacts.append(
                {
                    "file": filename,
                    "bytes": len(page_payload),
                    "sha256": hashlib.sha256(page_payload).hexdigest(),
                }
            )
            page_scenes.append(
                {
                    "index": page_index,
                    "sha256": hashlib.sha256(
                        f"authored scene {sheet_index}:{page_index}".encode()
                    ).hexdigest(),
                }
            )
        print_bundle["svg_pages"] = page_artifacts
        print_bundle["page_scenes"] = page_scenes
        report_path = bundle_dir / print_bundle["report"]["file"]
        report = json.loads(report_path.read_text(encoding="utf-8"))
        del report["layout_override"]
        fit_mode = scale_mode == MODULE.AUTHORED_SCALE_MODE_FIT
        report_pages = []
        for page_index in range(page_count):
            if fit_mode:
                horizontal_index = 0
                vertical_index = 0
                manual_col_break_before = False
                manual_row_break_before = False
                body_range = {
                    "first_row": 1,
                    "first_col": 0,
                    "last_row": 17,
                    "last_col": 4,
                }
            else:
                horizontal_index = page_index % 2
                vertical_index = page_index // 2
                manual_col_break_before = page_index % 2 == 1
                manual_row_break_before = page_index >= 2
                body_range = {
                    "first_row": 1 if page_index < 2 else 8,
                    "first_col": 0 if page_index % 2 == 0 else 3,
                    "last_row": 7 if page_index < 2 else 17,
                    "last_col": 2 if page_index % 2 == 0 else 5,
                }
            report_pages.append(
                {
                    "output_index": page_index,
                    "displayed_page_number": page_index + 1,
                    "area_index": 0,
                    "horizontal_index": horizontal_index,
                    "vertical_index": vertical_index,
                    "manual_col_break_before": manual_col_break_before,
                    "manual_row_break_before": manual_row_break_before,
                    "body_range": body_range,
                    "repeat_rows": [0, 0],
                    "repeat_cols": [5, 5],
                    "scale_permille": 1_000 if fit_mode else 850,
                }
            )
        report.update(
            {
                "paper": {"code": 1, "width_raw": 835584, "height_raw": 1081344},
                "content_rect": {
                    "x_raw": 49152,
                    "y_raw": 73728,
                    "width_raw": 737280,
                    "height_raw": 933888,
                },
                "page_order": "over_then_down",
                "manual_row_breaks": [] if fit_mode else [8],
                "manual_col_breaks": [] if fit_mode else [3],
                "scale_permille": 1_000 if fit_mode else 850,
                "logical_pages": page_count,
                "sparse_pages_omitted": 0,
                "pages": report_pages,
            }
        )
        report_payload = json.dumps(report, sort_keys=True).encode()
        report_path.write_bytes(report_payload)
        print_bundle["report"] = {
            "file": report_path.name,
            "bytes": len(report_payload),
            "sha256": hashlib.sha256(report_payload).hexdigest(),
        }
    manifest_path.write_text(json.dumps(manifest, sort_keys=True), encoding="utf-8")


def write_authored_print_xlsx(
    path: Path,
    *,
    fit: bool,
    setup_override: str | None = None,
    margins_override: str | None = None,
) -> None:
    setup = setup_override or (
        '<pageSetup orientation="portrait" paperSize="1" fitToWidth="2" fitToHeight="2" pageOrder="overThenDown"/>'
        if fit
        else '<pageSetup orientation="portrait" paperSize="1" scale="85" pageOrder="overThenDown"/>'
    )
    margins = margins_override or (
        '<pageMargins left="0.5" right="0.5" top="0.75" bottom="0.75" header="0.2" footer="0.25"/>'
    )
    sheet_pr = '<sheetPr><pageSetUpPr fitToPage="1"/></sheetPr>' if fit else ""
    workbook = (
        '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">'
        '<sheets><sheet name="Render" sheetId="1"/></sheets><definedNames>'
        '<definedName name="_xlnm.Print_Area" localSheetId="0">Render!$A$1:$F$18</definedName>'
        '<definedName name="_xlnm.Print_Titles" localSheetId="0">Render!$1:$1,Render!$F:$F</definedName>'
        '</definedNames></workbook>'
    )
    sheet = (
        '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">'
        + sheet_pr
        + '<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>title</t></is></c></row>'
        '<row r="10"><c r="E10" t="inlineStr"><is><t>body</t></is></c></row></sheetData>'
        + margins
        + setup
        + '<headerFooter><oddHeader>&amp;CPage &amp;P of &amp;N</oddHeader><oddFooter>&amp;RFooter &amp;P</oddFooter></headerFooter>'
        '<rowBreaks count="1" manualBreakCount="1"><brk id="8" min="0" max="16383" man="1"/></rowBreaks>'
        '<colBreaks count="1" manualBreakCount="1"><brk id="3" min="0" max="1048575" man="1"/></colBreaks>'
        '</worksheet>'
    )
    with ZipFile(path, "w", compression=ZIP_DEFLATED) as archive:
        archive.writestr("xl/workbook.xml", workbook)
        archive.writestr("xl/worksheets/sheet1.xml", sheet)


def minimal_sfnt_font(
    names: tuple[tuple[int, int, int, int, str], ...] = (
        (3, 1, 0x0409, 1, "Fixture Sans"),
        (3, 1, 0x0409, 4, "Fixture Sans Regular"),
        (3, 1, 0x0409, 6, "FixtureSans-Regular"),
    ),
    *,
    language_tags: tuple[str, ...] = (),
) -> bytes:
    ordered = tuple(sorted(names, key=lambda row: row[:4]))
    strings = bytearray()
    records = bytearray()
    for platform_id, encoding_id, language_id, name_id, value in ordered:
        if platform_id == 1 and encoding_id == 0:
            encoded = value.encode("mac_roman")
        else:
            encoded = value.encode("utf-16-be")
        records.extend(
            struct.pack(
                ">6H",
                platform_id,
                encoding_id,
                language_id,
                name_id,
                len(encoded),
                len(strings),
            )
        )
        strings.extend(encoded)
    language_tag_records = bytearray()
    for language_tag in language_tags:
        encoded = language_tag.encode("utf-16-be")
        language_tag_records.extend(
            struct.pack(">2H", len(encoded), len(strings))
        )
        strings.extend(encoded)
    format_fields = (
        struct.pack(">H", len(language_tags)) + bytes(language_tag_records)
        if language_tags
        else b""
    )
    storage_offset = 6 + len(records) + len(format_fields)
    name_table = (
        struct.pack(
            ">3H",
            1 if language_tags else 0,
            len(ordered),
            storage_offset,
        )
        + bytes(records)
        + format_fields
        + bytes(strings)
    )
    table_offset = 28
    header = (
        b"\x00\x01\x00\x00"
        + struct.pack(">4H", 1, 16, 0, 0)
        + b"name"
        + struct.pack(">3I", 0, table_offset, len(name_table))
    )
    payload = header + name_table
    return payload + b"\x00" * ((-len(payload)) % 4)


def minimal_ttc_font() -> bytes:
    first = minimal_sfnt_font()
    second = minimal_sfnt_font(
        (
            (3, 1, 0x0409, 1, "Second Serif"),
            (3, 1, 0x0409, 4, "Second Serif Regular"),
            (3, 1, 0x0409, 6, "SecondSerif-Regular"),
        )
    )

    def name_table(font: bytes) -> bytes:
        offset = int.from_bytes(font[20:24], "big")
        length = int.from_bytes(font[24:28], "big")
        return font[offset : offset + length]

    first_name = name_table(first)
    second_name = name_table(second)
    first_face_offset = 20
    second_face_offset = 48
    first_table_offset = 76
    second_table_offset = (
        first_table_offset + len(first_name) + 3
    ) & ~3

    def face(table_offset: int, table_length: int) -> bytes:
        return (
            b"\x00\x01\x00\x00"
            + struct.pack(">4H", 1, 16, 0, 0)
            + b"name"
            + struct.pack(">3I", 0, table_offset, table_length)
        )

    payload = bytearray(
        b"ttcf"
        + struct.pack(">2I", 0x00010000, 2)
        + struct.pack(">2I", first_face_offset, second_face_offset)
        + face(first_table_offset, len(first_name))
        + face(second_table_offset, len(second_name))
    )
    payload.extend(first_name)
    payload.extend(b"\x00" * (second_table_offset - len(payload)))
    payload.extend(second_name)
    payload.extend(b"\x00" * ((-len(payload)) % 4))
    return bytes(payload)


def minimal_shared_table_ttc_font() -> bytes:
    font = minimal_sfnt_font()
    name_offset = int.from_bytes(font[20:24], "big")
    name_length = int.from_bytes(font[24:28], "big")
    name_table = font[name_offset : name_offset + name_length]
    first_face_offset = 20
    table_offset = 48
    second_face_offset = (table_offset + len(name_table) + 3) & ~3

    def face() -> bytes:
        return (
            b"\x00\x01\x00\x00"
            + struct.pack(">4H", 1, 16, 0, 0)
            + b"name"
            + struct.pack(">3I", 0, table_offset, len(name_table))
        )

    payload = bytearray(
        b"ttcf"
        + struct.pack(">2I", 0x00010000, 2)
        + struct.pack(">2I", first_face_offset, second_face_offset)
        + face()
        + name_table
    )
    payload.extend(b"\x00" * (second_face_offset - len(payload)))
    payload.extend(face())
    payload.extend(b"\x00" * ((-len(payload)) % 4))
    return bytes(payload)


def write_font_pack(
    root: Path,
    *,
    font: bytes | None = None,
    family: str = "Fixture Sans",
    filename: str = "FixtureSans-Regular.ttf",
) -> tuple[Path, str]:
    font = minimal_sfnt_font() if font is None else font
    license_payload = b"fixture OFL license"
    configuration = b'<fontconfig><dir prefix="relative">fonts</dir></fontconfig>\n'
    font_path = root / "fonts" / filename
    license_path = root / "licenses" / "fixture-OFL.txt"
    font_path.parent.mkdir(parents=True)
    license_path.parent.mkdir(parents=True)
    font_path.write_bytes(font)
    license_path.write_bytes(license_payload)
    (root / "fonts.conf").write_bytes(configuration)
    fonts = [
        {
            "bytes": len(font),
            "family": family,
            "output": f"fonts/{filename}",
            "sha256": hashlib.sha256(font).hexdigest(),
            "style": "normal",
            "weight": 400,
        }
    ]
    licenses = [
        {
            "bytes": len(license_payload),
            "output": "licenses/fixture-OFL.txt",
            "sha256": hashlib.sha256(license_payload).hexdigest(),
        }
    ]
    identity = {
        "fonts": fonts,
        "fonts_conf_sha256": hashlib.sha256(configuration).hexdigest(),
        "licenses": licenses,
    }
    identity_bytes = (json.dumps(identity, indent=2, sort_keys=True) + "\n").encode()
    pack_sha = hashlib.sha256(identity_bytes).hexdigest()
    manifest = {
        **identity,
        "license": "SIL-OFL-1.1",
        "pack_sha256": pack_sha,
        "schema": "rxls.render-font-pack.v1",
        "total_bytes": len(font) + len(license_payload) + len(configuration),
    }
    manifest_path = root / "manifest.json"
    manifest_path.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return manifest_path, pack_sha


def pdffonts_payload(
    rows: list[
        tuple[str, str, str, str, str, str, int, int]
    ] | None = None,
) -> bytes:
    lines = [MODULE.PDFFONTS_HEADER, MODULE.PDFFONTS_SEPARATOR]
    for (
        name,
        font_type,
        encoding,
        embedded,
        subset,
        unicode_map,
        object_id,
        generation,
    ) in rows or []:
        row = (
            f"{name:<36} {font_type:<17} {encoding:<16} "
            f"{embedded:<3} {subset:<3} {unicode_map:<3} "
            f"{object_id:6d} {generation:2d}"
        )
        if len(row) != len(MODULE.PDFFONTS_HEADER):
            raise AssertionError(f"invalid pdffonts fixture row: {row!r}")
        lines.append(row)
    return ("\n".join(lines) + "\n").encode("ascii")


def poppler_identity(digest: str = "f") -> dict[str, str]:
    return {
        "host_tools_identity_sha256": "b" * 64,
        "kind": "poppler",
        "pdffonts_sha256": digest * 64,
        "pdfinfo_sha256": "e" * 64,
        "pdftoppm_sha256": "d" * 64,
        "pdftotext_sha256": "c" * 64,
    }


def write_oracle_lock(
    root: Path,
    *,
    libreoffice: Path,
    pdfinfo: Path,
    pdftoppm: Path,
    pdftotext: Path,
    pdffonts: Path,
    font_pack_sha256: str,
) -> Path:
    def digest(path: Path) -> str:
        return hashlib.sha256(path.read_bytes()).hexdigest()

    document = {
        "default_profile": "fixture-oracle",
        "profiles": [
            {
                "configuration": {
                    "dpi": 96,
                    "locale": "C.UTF-8",
                    "pdf_filter": MODULE.PDF_FILTER,
                    "profile_sha256": digest(MODULE.ORACLE_PROFILE_PATH),
                    "timezone": "UTC",
                },
                "font_pack_sha256": font_pack_sha256,
                "libreoffice": {
                    "executable_sha256": digest(libreoffice),
                    "version": "LibreOffice fixture",
                },
                "name": "fixture-oracle",
                "pdf_rasterizer": {
                    "kind": "poppler",
                    "pdffonts_sha256": digest(pdffonts),
                    "pdffonts_version": "pdffonts fixture",
                    "pdfinfo_sha256": digest(pdfinfo),
                    "pdfinfo_version": "pdfinfo fixture",
                    "pdftoppm_sha256": digest(pdftoppm),
                    "pdftoppm_version": "pdftoppm fixture",
                    "pdftotext_sha256": digest(pdftotext),
                    "pdftotext_version": "pdftotext fixture",
                },
                "platform": {
                    "machine": platform.machine().lower(),
                    "system": platform.system().lower(),
                },
                "python": {
                    "executable_sha256": digest(Path(sys.executable).resolve()),
                    "implementation": "cpython",
                    "numpy_version": importlib.metadata.version("numpy"),
                    "pillow_version": importlib.metadata.version("Pillow"),
                    "version": platform.python_version(),
                },
                "source": {
                    "artifact_bytes": 1,
                    "artifact_sha256": "1" * 64,
                    "artifact_url": (
                        "https://download.documentfoundation.org/"
                        "libreoffice/stable/fixture.dmg"
                    ),
                },
                "svg_rasterizer": {
                    "distribution": "CairoSVG",
                    "kind": "cairosvg",
                    "version": importlib.metadata.version("CairoSVG"),
                },
            }
        ],
        "schema": "rxls.render-oracle-lock.v1",
    }
    path = root / "oracle-lock.json"
    path.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    return path


class OracleVersionRunner:
    def run(self, command, **kwargs):
        name = Path(command[0]).name
        versions = {
            "lo-fixture": "LibreOffice fixture",
            "pdfinfo-fixture": "pdfinfo fixture",
            "pdftoppm-fixture": "pdftoppm fixture",
            "pdftotext-fixture": "pdftotext fixture",
            "pdffonts-fixture": "pdffonts fixture",
        }
        return MODULE.CommandResult("ok", 0, (versions[name] + "\n").encode(), b"")


class FakeRunner:
    def __init__(self, source: Path) -> None:
        self.source = source
        self.commands: list[list[str]] = []

    def run(
        self,
        command,
        *,
        cwd,
        env,
        timeout_seconds,
        output_limit_bytes,
        file_output_roots=(),
        file_output_limit_bytes=None,
        file_output_count_limit=None,
    ):
        command = list(command)
        self.commands.append(command)
        if "bundle" in command:
            output = Path(command[command.index("--output-dir") + 1])
            write_bundle(
                output,
                self.source,
                visibilities=("visible", "hidden"),
                print_pages=True,
            )
            return MODULE.CommandResult("ok", 0, b"bundle ok\n", b"")
        if "--convert-to" in command:
            output = Path(command[command.index("--outdir") + 1])
            output.mkdir(parents=True, exist_ok=True)
            (output / f"{self.source.stem}.pdf").write_bytes(b"%PDF-mocked")
            return MODULE.CommandResult("ok", 0, b"convert ok\n", b"")
        raise AssertionError(f"unexpected command: {command!r}")


class CommonPdfRunner(FakeRunner):
    """Exercise both native PDFs through the same mocked Poppler commands."""

    def run(self, command, **kwargs):
        command = list(command)
        if "bundle" in command:
            self.commands.append(command)
            output = Path(command[command.index("--output-dir") + 1])
            write_bundle(output, self.source, print_pages=True)
            return MODULE.CommandResult("ok", 0, b"bundle ok\n", b"")
        name = Path(command[0]).name
        if name == "pdfinfo":
            self.commands.append(command)
            return MODULE.CommandResult(
                "ok",
                0,
                (
                    b"Pages: 1\n"
                    b"Page size: 72 x 36 pts\n"
                    b"MediaBox: 0.00 0.00 72.00 36.00\n"
                    b"CropBox: 0.00 0.00 72.00 36.00\n"
                ),
                b"",
            )
        if name == "pdftoppm":
            from PIL import Image

            self.commands.append(command)
            prefix = Path(command[-1])
            Image.new("RGB", (96, 48), "white").save(
                prefix.with_name(prefix.name + "-1.png")
            )
            return MODULE.CommandResult("ok", 0, b"", b"")
        if name == "pdftotext":
            self.commands.append(command)
            if "-bbox-layout" in command:
                Path(command[-1]).write_text(
                    """<html xmlns="http://www.w3.org/1999/xhtml"><body><doc>
<page width="72" height="36"><flow><block>
<line xMin="2" yMin="2" xMax="32" yMax="12">
<word xMin="2" yMin="2" xMax="24" yMax="12">sheet</word>
<word xMin="25" yMin="2" xMax="32" yMax="12">0</word>
</line></block></flow></page></doc></body></html>""",
                    encoding="utf-8",
                )
                return MODULE.CommandResult("ok", 0, b"", b"")
            return MODULE.CommandResult("ok", 0, b"sheet 0\n", b"")
        return super().run(command, **kwargs)


class NoCallRunner:
    def run(self, *args, **kwargs):
        raise AssertionError("dry-run must not execute commands")


class RejectingLibreOfficeRunner(FakeRunner):
    def run(self, command, **kwargs):
        if "--convert-to" in command:
            self.commands.append(list(command))
            return MODULE.CommandResult("nonzero", 1, b"", b"source rejected")
        return super().run(command, **kwargs)


class FontMismatchRunner(FakeRunner):
    def __init__(self, source: Path, font_pack_sha256: str) -> None:
        super().__init__(source)
        self.font_pack_sha256 = font_pack_sha256

    def run(self, command, **kwargs):
        command = list(command)
        if "bundle" in command:
            self.commands.append(command)
            output = Path(command[command.index("--output-dir") + 1])
            write_bundle(
                output,
                self.source,
                font_pack_sha256=self.font_pack_sha256,
                render_schema_version=2,
                print_pages=True,
                print_schema_version=2,
            )
            return MODULE.CommandResult("ok", 0, b"bundle ok\n", b"")
        if Path(command[0]).name == "pdffonts":
            self.commands.append(command)
            return MODULE.CommandResult(
                "ok",
                0,
                pdffonts_payload(
                    [
                        (
                            "BAAAAA+LiberationSans",
                            "TrueType",
                            "WinAnsi",
                            "yes",
                            "yes",
                            "yes",
                            14,
                            0,
                        )
                    ]
                ),
                b"",
            )
        return super().run(command, **kwargs)


class ContainerOracleRunner(FakeRunner):
    def __init__(
        self,
        source: Path,
        font_pack_sha256: str,
        *,
        print_mode: str = MODULE.PRINT_MODE_SINGLE_PAGE,
        authored_scale_mode: str = MODULE.AUTHORED_SCALE_MODE_SCALE,
    ) -> None:
        super().__init__(source)
        self.font_pack_sha256 = font_pack_sha256
        self.print_mode = print_mode
        self.authored_scale_mode = authored_scale_mode

    def run(self, command, **kwargs):
        command = list(command)
        if "bundle" in command:
            self.commands.append(command)
            output = Path(command[command.index("--output-dir") + 1])
            if self.print_mode == MODULE.PRINT_MODE_AUTHORED:
                write_authored_bundle(
                    output,
                    self.source,
                    scale_mode=self.authored_scale_mode,
                )
                manifest_path = output / "render-manifest.json"
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                manifest["renderer"]["font_pack_sha256"] = self.font_pack_sha256
                manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            else:
                write_bundle(
                    output,
                    self.source,
                    visibilities=("visible", "hidden"),
                    font_pack_sha256=self.font_pack_sha256,
                    print_pages=True,
                )
            return MODULE.CommandResult("ok", 0, b"bundle ok\n", b"")
        if command and command[0] == "container-adapter":
            self.commands.append(command)
            output = Path(command[command.index("--evidence-dir") + 1])
            output.mkdir(parents=True, exist_ok=True)
            pdf = b"%PDF-mocked"
            source_payload = self.source.read_bytes()
            source_identity = {
                "bytes": len(source_payload),
                "path": f"source/input{self.source.suffix.lower()}",
                "sha256": hashlib.sha256(source_payload).hexdigest(),
            }
            artifact = {
                "bytes": len(pdf),
                "path": "oracle/oracle.pdf",
                "sha256": hashlib.sha256(pdf).hexdigest(),
            }
            lock_sha256 = "e" * 64
            manifest = {
                "artifact": artifact,
                "export": {
                    "filter": "calc_pdf_Export",
                    "single_page_sheets": self.print_mode
                    == MODULE.PRINT_MODE_SINGLE_PAGE,
                },
                "font_pack_sha256": self.font_pack_sha256,
                "lock_sha256": lock_sha256,
                "oracle": {
                    "artifact_sha256": MODULE.CONTAINER_LIBREOFFICE_ARTIFACT_SHA256,
                    "name": "LibreOffice",
                    "version": "26.2.3.2",
                },
                "schema": MODULE.CONTAINER_OUTPUT_SCHEMA,
                "source": source_identity,
            }
            execution = {
                "artifacts": {
                    "manifest": "oracle/oracle-manifest.json",
                    "pdf": artifact,
                },
                "font_pack_sha256": self.font_pack_sha256,
                "image": {
                    "architecture": "linux/amd64",
                    "expected_id": "sha256:" + "a" * 64,
                    "expected_manifest_digest": "sha256:" + "d" * 64,
                    "id": "sha256:" + "a" * 64,
                    "identity_status": "pinned_match",
                    "lock_sha256": lock_sha256,
                    "manifest_digest": "sha256:" + "d" * 64,
                },
                "isolation": {
                    "capabilities": "none",
                    "corpus_mount": "read_only",
                    "evidence_mount": "size_capped_tmpfs",
                    "external_links": "network_and_filesystem_isolated",
                    "font_mount": "read_only",
                    "macro_execution": "disabled",
                    "network": "none",
                    "no_new_privileges": True,
                    "root_filesystem": "read_only",
                    "source_mount": "read_only",
                    "unique_home_xdg_profile": True,
                },
                "limits": {
                    "cpus": "2.00",
                    "evidence_bytes": 268435456,
                    "memory_bytes": 2147483648,
                    "nofile": 256,
                    "pids": 128,
                    "timeout_milliseconds": 180000,
                },
                "lock_file_sha256": lock_sha256,
                "runtime": "docker",
                "schema": MODULE.CONTAINER_EXECUTION_SCHEMA,
                "source": source_identity,
            }
            (output / "oracle.pdf").write_bytes(pdf)
            (output / "oracle-manifest.json").write_bytes(
                MODULE._canonical_json_bytes(manifest)
            )
            (output / "execution.json").write_bytes(
                MODULE._canonical_json_bytes(execution)
            )
            return MODULE.CommandResult("ok", 0, b"container ok\n", b"")
        return super().run(command, **kwargs)


class LibreOfficeRenderParityTests(unittest.TestCase):
    def test_clean_libreoffice_profile_and_calculation_environment_are_seeded(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            profile = root / "profile"
            profile.mkdir()
            digest = MODULE.seed_libreoffice_profile(profile)
            seeded = profile / "user" / "registrymodifications.xcu"
            self.assertEqual(seeded.read_bytes(), MODULE.ORACLE_PROFILE_PATH.read_bytes())
            self.assertEqual(digest, hashlib.sha256(seeded.read_bytes()).hexdigest())
            with self.assertRaisesRegex(MODULE.HarnessError, "oracle_profile_unsafe"):
                MODULE.seed_libreoffice_profile(profile)

            environment = MODULE._job_environment(root / "job", "C.UTF-8")
            self.assertEqual(environment["SAL_DISABLE_OPENCL"], "1")
            self.assertEqual(environment["SC_FORCE_CALCULATION"], "core")
            self.assertEqual(environment["TZ"], "UTC")

    def test_direct_renderer_binary_identity_is_exact_and_path_neutral(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "rxls-render-fixture"
            path.write_bytes(b"renderer fixture")
            path.chmod(0o755)
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            identity = MODULE.renderer_binary_identity(
                (str(path),), digest, required=True
            )
            with self.assertRaisesRegex(MODULE.HarnessError, "identity$"):
                MODULE.renderer_binary_identity((str(path),), "0" * 64, required=True)

        self.assertEqual(identity, {"bytes": 16, "sha256": digest})
        self.assertNotIn(raw, json.dumps(identity, sort_keys=True))
        with self.assertRaisesRegex(MODULE.HarnessError, "direct_binary_required"):
            MODULE.renderer_binary_identity(("cargo", "run"), None, required=True)

    def test_adapter_poppler_binary_identity_is_exact_and_path_neutral(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            paths = {}
            contract = {}
            environment = {}
            for index, name in enumerate(
                ("pdffonts", "pdfinfo", "pdftoppm", "pdftotext")
            ):
                path = Path(raw) / f"{name}-fixture"
                path.write_bytes(f"locked {name} fixture {index}".encode())
                path.chmod(0o755)
                digest = hashlib.sha256(path.read_bytes()).hexdigest()
                paths[name] = path
                contract[name] = {
                    "bytes": path.stat().st_size,
                    "sha256": digest,
                }
                environment[name.upper()] = str(path)
            with (
                mock.patch.dict(os.environ, environment),
                mock.patch.object(
                    MODULE,
                    "_locked_poppler_tool_contract",
                    return_value=contract,
                ),
            ):
                identity = MODULE.poppler_binary_identity(
                    contract["pdffonts"]["sha256"], required=True
                )
                with self.assertRaisesRegex(
                    MODULE.HarnessError, "pdffonts_binary_identity$"
                ):
                    MODULE.poppler_binary_identity("0" * 64, required=True)

        self.assertEqual(identity["kind"], "poppler")
        for name in ("pdffonts", "pdfinfo", "pdftoppm", "pdftotext"):
            self.assertEqual(identity[f"{name}_sha256"], contract[name]["sha256"])
        self.assertNotIn(raw, json.dumps(identity, sort_keys=True))
        with self.assertRaisesRegex(
            MODULE.HarnessError, "pdffonts_binary_identity_required"
        ):
            MODULE.poppler_binary_identity(None, required=True)

    def test_rxls_and_libreoffice_commands_match_exact_contract(self) -> None:
        input_path = Path("input.xlsx")
        output = Path("out")
        profile = Path("profile")
        rxls = MODULE.build_rxls_command(
            ["cargo", "run", "--manifest-path", "render/Cargo.toml", "--"],
            input_path,
            output,
        )
        self.assertEqual(
            rxls[-7:],
            [
                "bundle",
                "input.xlsx",
                "--single-page-sheets",
                "--print-backends",
                "svg,pdf",
                "--output-dir",
                "out",
            ],
        )

        with_font_pack = MODULE.build_rxls_command(
            ["rxls-render"], input_path, output, Path("fonts/manifest.json")
        )
        self.assertEqual(
            with_font_pack[-9:],
            [
                "bundle",
                "input.xlsx",
                "--font-pack-manifest",
                "fonts/manifest.json",
                "--single-page-sheets",
                "--print-backends",
                "svg,pdf",
                "--output-dir",
                "out",
            ],
        )

        libreoffice = MODULE.build_libreoffice_command(
            "soffice", input_path, output, profile
        )
        self.assertEqual(libreoffice[0], "soffice")
        self.assertIn("--headless", libreoffice)
        self.assertIn("--norestore", libreoffice)
        self.assertIn(MODULE.PDF_FILTER, libreoffice)
        self.assertIn('"SinglePageSheets"', libreoffice[libreoffice.index("--convert-to") + 1])
        self.assertEqual(libreoffice[-1], "input.xlsx")

        authored_rxls = MODULE.build_rxls_command(
            ["rxls-render"],
            input_path,
            output,
            print_mode=MODULE.PRINT_MODE_AUTHORED,
        )
        self.assertNotIn("--single-page-sheets", authored_rxls)
        self.assertIn("--print-layout", authored_rxls)
        self.assertEqual(
            authored_rxls[authored_rxls.index("--print-backends") + 1],
            "svg,pdf",
        )
        authored_lo = MODULE.build_libreoffice_command(
            "soffice",
            input_path,
            output,
            profile,
            MODULE.PRINT_MODE_AUTHORED,
        )
        self.assertIn(MODULE.AUTHORED_PDF_FILTER, authored_lo)
        self.assertNotIn("SinglePageSheets", " ".join(authored_lo))

    def test_offline_oracle_adapter_expands_only_allowlisted_placeholders(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            manifest, _ = write_font_pack(root / "font-pack")
            font_pack = MODULE.load_font_pack(manifest)
            command = MODULE.build_libreoffice_oracle_command(
                (
                    "container-adapter",
                    "--source",
                    "{input}",
                    "--font-pack",
                    "{font_pack}",
                    "--evidence-dir",
                    "{output_dir}",
                    "--run-id={run_id}",
                ),
                root / "input.xlsx",
                root / "evidence",
                "case-0001-abcdef012345",
                font_pack,
            )
            self.assertEqual(command[0], "container-adapter")
            self.assertIn(str(root / "input.xlsx"), command)
            self.assertIn(str(font_pack.root), command)
            self.assertIn(str(root / "evidence"), command)
            self.assertIn("--run-id=case-0001-abcdef012345", command)
            with self.assertRaisesRegex(
                MODULE.HarnessError, "libreoffice_command_placeholder"
            ):
                MODULE.build_libreoffice_oracle_command(
                    ("adapter", "{input}", "{output_dir}", "{font_pack}"),
                    root / "input.xlsx",
                    root / "evidence",
                    "case-0001-abcdef012345",
                    font_pack,
                )

    def test_harness_routes_libreoffice_through_offline_oracle_adapter(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            manifest, pack_sha = write_font_pack(root / "font-pack")
            font_pack = MODULE.load_font_pack(manifest)
            runner = ContainerOracleRunner(source, pack_sha)
            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice="unused-soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=False,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
                font_pack=font_pack,
                libreoffice_command=(
                    "container-adapter",
                    "--source",
                    "{input}",
                    "--font-pack",
                    "{font_pack}",
                    "--evidence-dir",
                    "{output_dir}",
                    "--run-id",
                    "{run_id}",
                ),
                poppler_identity=poppler_identity(),
            )
            evidence, exit_code = MODULE.run_harness(
                [MODULE.InputCase(source, "fixture.xlsx", 7)],
                discovery={
                    "candidate_count": 1,
                    "selected_count": 1,
                    "truncated": False,
                },
                config=config,
                backends=MODULE.Backends(False, False, False),
                runner=runner,
            )

        self.assertEqual(exit_code, 0)
        self.assertEqual(evidence["files"][0]["status"], "skipped")
        self.assertEqual(evidence["preflight"]["libreoffice"]["mode"], "adapter_command")
        adapter = evidence["files"][0]["oracle_adapter"]
        self.assertEqual(adapter["image"]["id"], "sha256:" + "a" * 64)
        self.assertEqual(adapter["font_pack_sha256"], pack_sha)
        self.assertEqual(adapter["lock_file_sha256"], "e" * 64)
        aggregate = evidence["configuration"]["oracle_lock"]
        self.assertEqual(aggregate["schema"], MODULE.CONTAINER_IDENTITY_SCHEMA)
        self.assertEqual(
            aggregate["image"],
            {
                "architecture": "linux/amd64",
                "config_digest": "sha256:" + "a" * 64,
                "expected_config_digest": "sha256:" + "a" * 64,
                "expected_manifest_digest": "sha256:" + "d" * 64,
                "identity_status": "pinned_match",
                "manifest_digest": "sha256:" + "d" * 64,
            },
        )
        self.assertEqual(
            aggregate["pdf_font_inspector"],
            poppler_identity(),
        )
        self.assertEqual(runner.commands[1][0], "container-adapter")
        self.assertIn("--run-id", runner.commands[1])
        self.assertNotIn("--convert-to", runner.commands[1])

    def test_authored_print_lane_attests_source_and_never_forces_single_page(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            write_authored_print_xlsx(source, fit=False)
            manifest, pack_sha = write_font_pack(root / "font-pack")
            font_pack = MODULE.load_font_pack(manifest)
            runner = ContainerOracleRunner(
                source,
                pack_sha,
                print_mode=MODULE.PRINT_MODE_AUTHORED,
            )
            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice="unused-soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=False,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
                font_pack=font_pack,
                libreoffice_command=(
                    "container-adapter",
                    "--source",
                    "{input}",
                    "--font-pack",
                    "{font_pack}",
                    "--evidence-dir",
                    "{output_dir}",
                    "--run-id",
                    "{run_id}",
                ),
                poppler_identity=poppler_identity(),
                print_mode=MODULE.PRINT_MODE_AUTHORED,
            )
            case = MODULE.InputCase(
                source,
                "fixture.xlsx",
                source.stat().st_size,
                features=("print-settings",),
            )
            evidence, exit_code = MODULE.run_harness(
                [case],
                discovery={
                    "candidate_count": 1,
                    "selected_count": 1,
                    "truncated": False,
                },
                config=config,
                backends=MODULE.Backends(False, False, False),
                runner=runner,
            )

        self.assertEqual(exit_code, 0)
        self.assertEqual(evidence["configuration"]["print_mode"], "authored")
        self.assertEqual(
            evidence["summary"]["authored_print"]["expected_page_box_pixels"],
            {"height": 1056, "width": 816},
        )
        self.assertEqual(
            evidence["summary"]["authored_print"]["by_scale_mode"],
            {"scale": 1},
        )
        self.assertNotIn("--single-page-sheets", runner.commands[0])
        self.assertIn("--print-layout", runner.commands[0])
        self.assertEqual(
            runner.commands[1][runner.commands[1].index("--print-mode") + 1],
            "authored",
        )

    def test_evaluate_case_couples_authored_bundle_to_attested_scale_mode(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            manifest, pack_sha = write_font_pack(root / "font-pack")
            font_pack = MODULE.load_font_pack(manifest)
            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice="unused-soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=False,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
                font_pack=font_pack,
                libreoffice_command=(
                    "container-adapter",
                    "--source",
                    "{input}",
                    "--font-pack",
                    "{font_pack}",
                    "--evidence-dir",
                    "{output_dir}",
                    "--run-id",
                    "{run_id}",
                ),
                poppler_identity=poppler_identity(),
                print_mode=MODULE.PRINT_MODE_AUTHORED,
            )

            for index, (scale_mode, fit, expected_pages) in enumerate(
                (
                    (MODULE.AUTHORED_SCALE_MODE_SCALE, False, 4),
                    (MODULE.AUTHORED_SCALE_MODE_FIT, True, 1),
                )
            ):
                source = root / f"{scale_mode}.xlsx"
                write_authored_print_xlsx(source, fit=fit)
                case = MODULE.InputCase(
                    source,
                    source.name,
                    source.stat().st_size,
                    features=("print-settings",),
                )
                runner = ContainerOracleRunner(
                    source,
                    pack_sha,
                    print_mode=MODULE.PRINT_MODE_AUTHORED,
                    authored_scale_mode=scale_mode,
                )
                with self.subTest(scale_mode=scale_mode):
                    result = MODULE.evaluate_case(
                        case,
                        index=index,
                        work_root=root / f"work-{scale_mode}",
                        config=config,
                        backends=MODULE.Backends(False, False, False),
                        runner=runner,
                    )
                    self.assertEqual(result["status"], "skipped")
                    self.assertEqual(
                        result["classification"],
                        "visual_dependencies_missing",
                    )
                    self.assertEqual(
                        result["authored_print"]["scale_mode"],
                        scale_mode,
                    )
                    self.assertEqual(len(result["scenes"]), expected_pages)

            fit_source = root / "fit-cross-mode.xlsx"
            write_authored_print_xlsx(fit_source, fit=True)
            fit_case = MODULE.InputCase(
                fit_source,
                fit_source.name,
                fit_source.stat().st_size,
                features=("print-settings",),
            )
            mismatched_runner = ContainerOracleRunner(
                fit_source,
                pack_sha,
                print_mode=MODULE.PRINT_MODE_AUTHORED,
                authored_scale_mode=MODULE.AUTHORED_SCALE_MODE_SCALE,
            )
            mismatch = MODULE.evaluate_case(
                fit_case,
                index=2,
                work_root=root / "work-cross-mode",
                config=config,
                backends=MODULE.Backends(False, False, False),
                runner=mismatched_runner,
            )
            self.assertEqual(mismatch["status"], "error")
            self.assertRegex(
                mismatch["classification"],
                r"render_manifest_authored_",
            )

    def test_offline_adapter_output_fails_closed_on_identity_drift_and_extra_files(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            _, pack_sha = write_font_pack(root / "font-pack")
            output = root / "adapter-output"
            runner = ContainerOracleRunner(source, pack_sha)
            runner.run(
                ["container-adapter", "--evidence-dir", str(output)],
                cwd=root,
                env={},
                timeout_seconds=1,
                output_limit_bytes=1024,
            )
            verified = MODULE.validate_libreoffice_adapter_output(
                output,
                input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                input_bytes=7,
                extension=".xlsx",
                font_pack_sha256=pack_sha,
            )
            self.assertEqual(verified["schema"], MODULE.CONTAINER_EXECUTION_SCHEMA)

            execution_path = output / "execution.json"
            execution = json.loads(execution_path.read_text(encoding="utf-8"))
            execution["image"]["id"] = "sha256:" + "b" * 64
            execution["image"]["expected_id"] = "sha256:" + "a" * 64
            execution["image"]["identity_status"] = "pinned_match"
            execution_path.write_bytes(MODULE._canonical_json_bytes(execution))
            with self.assertRaisesRegex(
                MODULE.HarnessError, "libreoffice_adapter_image_identity"
            ):
                MODULE.validate_libreoffice_adapter_output(
                    output,
                    input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                    input_bytes=7,
                    extension=".xlsx",
                    font_pack_sha256=pack_sha,
                )

            execution["image"]["id"] = "sha256:" + "a" * 64
            execution["image"]["expected_id"] = None
            execution_path.write_bytes(MODULE._canonical_json_bytes(execution))
            with self.assertRaisesRegex(
                MODULE.HarnessError, "libreoffice_adapter_image_identity"
            ):
                MODULE.validate_libreoffice_adapter_output(
                    output,
                    input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                    input_bytes=7,
                    extension=".xlsx",
                    font_pack_sha256=pack_sha,
                )

            execution["image"]["expected_id"] = "sha256:" + "a" * 64
            execution["image"]["expected_manifest_digest"] = "sha256:" + "c" * 64
            execution_path.write_bytes(MODULE._canonical_json_bytes(execution))
            with self.assertRaisesRegex(
                MODULE.HarnessError, "libreoffice_adapter_image_identity"
            ):
                MODULE.validate_libreoffice_adapter_output(
                    output,
                    input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                    input_bytes=7,
                    extension=".xlsx",
                    font_pack_sha256=pack_sha,
                )

            execution["image"]["expected_manifest_digest"] = "sha256:" + "d" * 64
            del execution["image"]["manifest_digest"]
            execution_path.write_bytes(MODULE._canonical_json_bytes(execution))
            with self.assertRaisesRegex(
                MODULE.HarnessError, "libreoffice_adapter_image_keys"
            ):
                MODULE.validate_libreoffice_adapter_output(
                    output,
                    input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                    input_bytes=7,
                    extension=".xlsx",
                    font_pack_sha256=pack_sha,
                )

            execution["image"]["manifest_digest"] = "sha256:" + "d" * 64
            execution["schema"] = "rxls.render-oracle-container-execution.v2"
            execution_path.write_bytes(MODULE._canonical_json_bytes(execution))
            with self.assertRaisesRegex(MODULE.HarnessError, "oracle_lock_schema"):
                MODULE.validate_libreoffice_adapter_output(
                    output,
                    input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                    input_bytes=7,
                    extension=".xlsx",
                    font_pack_sha256=pack_sha,
                )

            execution["schema"] = MODULE.CONTAINER_EXECUTION_SCHEMA
            execution_path.write_bytes(MODULE._canonical_json_bytes(execution))
            (output / "unexpected.txt").write_text("unexpected", encoding="utf-8")
            with self.assertRaisesRegex(
                MODULE.HarnessError, "libreoffice_adapter_file_set"
            ):
                MODULE.validate_libreoffice_adapter_output(
                    output,
                    input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                    input_bytes=7,
                    extension=".xlsx",
                    font_pack_sha256=pack_sha,
                )

    def test_container_oracle_aggregate_rejects_mixed_missing_and_malicious_identity(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            manifest, pack_sha = write_font_pack(root / "font-pack")
            font_pack = MODULE.load_font_pack(manifest)
        config = MODULE.HarnessConfig(
            rxls_command=("rxls-render",),
            libreoffice="unused",
            svg_rasterizer_command=None,
            caps=MODULE.Caps(),
            dpi=96,
            locale="C.UTF-8",
            dry_run=False,
            min_similarity_ppm=None,
            fail_on_incomparable=False,
            font_pack=font_pack,
            libreoffice_command=("adapter",),
            poppler_identity=poppler_identity(),
        )
        adapter = {
            "font_pack_sha256": pack_sha,
            "image": {
                "architecture": "linux/amd64",
                "expected_id": "sha256:" + "a" * 64,
                "expected_manifest_digest": "sha256:" + "d" * 64,
                "id": "sha256:" + "a" * 64,
                "identity_status": "pinned_match",
                "manifest_digest": "sha256:" + "d" * 64,
            },
            "lock_sha256": "b" * 64,
            "lock_file_sha256": "c" * 64,
            "oracle": {
                "artifact_sha256": MODULE.CONTAINER_LIBREOFFICE_ARTIFACT_SHA256,
                "name": "LibreOffice",
                "version": "26.2.3.2",
            },
            "runtime": "docker",
            "schema": MODULE.CONTAINER_EXECUTION_SCHEMA,
        }
        identity = MODULE.aggregate_container_oracle_identity(
            [{"status": "compared", "oracle_adapter": adapter}], config=config
        )
        self.assertEqual(identity["build_contract_sha256"], "b" * 64)
        self.assertEqual(
            identity["image"]["config_digest"], "sha256:" + "a" * 64
        )
        self.assertEqual(
            identity["image"]["manifest_digest"], "sha256:" + "d" * 64
        )

        with self.assertRaisesRegex(MODULE.HarnessError, "identity_missing"):
            MODULE.aggregate_container_oracle_identity(
                [{"status": "compared"}], config=config
            )
        with self.assertRaisesRegex(
            MODULE.HarnessError,
            r"identity_missing:libreoffice_adapter_container_start_nonzero=2",
        ):
            MODULE.aggregate_container_oracle_identity(
                [
                    {
                        "status": "error",
                        "classification": "libreoffice_adapter_container_start_nonzero",
                    },
                    {
                        "status": "error",
                        "classification": "libreoffice_adapter_container_start_nonzero",
                    },
                ],
                config=config,
            )

        mixed = copy.deepcopy(adapter)
        mixed["lock_file_sha256"] = "d" * 64
        with self.assertRaisesRegex(MODULE.HarnessError, "identity_mixed"):
            MODULE.aggregate_container_oracle_identity(
                [
                    {"status": "compared", "oracle_adapter": adapter},
                    {"status": "compared", "oracle_adapter": mixed},
                ],
                config=config,
            )

        mixed_manifest = copy.deepcopy(adapter)
        mixed_manifest["image"]["manifest_digest"] = "sha256:" + "e" * 64
        mixed_manifest["image"]["expected_manifest_digest"] = (
            "sha256:" + "e" * 64
        )
        with self.assertRaisesRegex(MODULE.HarnessError, "identity_mixed"):
            MODULE.aggregate_container_oracle_identity(
                [
                    {"status": "compared", "oracle_adapter": adapter},
                    {"status": "compared", "oracle_adapter": mixed_manifest},
                ],
                config=config,
            )

        unpinned_manifest = copy.deepcopy(adapter)
        unpinned_manifest["image"]["expected_manifest_digest"] = None
        with self.assertRaisesRegex(
            MODULE.HarnessError, "aggregate_image"
        ):
            MODULE.aggregate_container_oracle_identity(
                [{"status": "compared", "oracle_adapter": unpinned_manifest}],
                config=config,
            )

        legacy = copy.deepcopy(adapter)
        legacy["schema"] = "rxls.render-oracle-container-execution.v2"
        with self.assertRaisesRegex(MODULE.HarnessError, "aggregate_identity"):
            MODULE.aggregate_container_oracle_identity(
                [{"status": "compared", "oracle_adapter": legacy}],
                config=config,
            )

        malicious = copy.deepcopy(adapter)
        malicious["host_path"] = "/private/oracle"
        with self.assertRaisesRegex(MODULE.HarnessError, "identity_keys"):
            MODULE.aggregate_container_oracle_identity(
                [{"status": "compared", "oracle_adapter": malicious}],
                config=config,
            )

    def test_adapter_failure_retains_only_a_bounded_typed_code(self) -> None:
        result = MODULE.CommandResult(
            "nonzero",
            2,
            b"",
            b"render_oracle_error:container_start_nonzero\n",
        )
        self.assertEqual(
            MODULE._libreoffice_command_failure(result, adapter_command=True),
            "libreoffice_adapter_container_start_nonzero",
        )
        self.assertEqual(
            MODULE._libreoffice_command_failure(result, adapter_command=False),
            "libreoffice_failed",
        )
        pathful = MODULE.CommandResult(
            "nonzero",
            2,
            b"",
            b"render_oracle_error:/private/input.xlsx\n",
        )
        self.assertEqual(
            MODULE._libreoffice_command_failure(pathful, adapter_command=True),
            "libreoffice_failed",
        )

    def test_bundle_validation_checks_hash_order_visibility_and_svg_dimensions(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            bundle_dir = root / "bundle"
            write_bundle(
                bundle_dir,
                source,
                visibilities=("visible", "hidden", "very_hidden"),
            )
            bundle = MODULE.validate_bundle(
                bundle_dir,
                input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                input_bytes=7,
                caps=MODULE.Caps(),
                dpi=96,
            )

            self.assertEqual(
                [page.source_sheet_index for page in bundle.pages],
                [0, 1, 2],
            )
        self.assertEqual(
            [page.visibility for page in bundle.pages],
            ["visible", "hidden", "very_hidden"],
        )
        self.assertEqual((bundle.pages[0].width_pixels, bundle.pages[0].height_pixels), (96, 48))
        self.assertEqual(bundle.renderer["name"], "rxls-render")
        self.assertIsNone(bundle.renderer["font_pack_sha256"])
        self.assertEqual(bundle.pages[0].warnings, ())

    def test_bundle_validation_requires_the_configured_font_pack_identity(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            bundle_dir = root / "bundle"
            digest = "a" * 64
            write_bundle(bundle_dir, source, font_pack_sha256=digest)
            bundle = MODULE.validate_bundle(
                bundle_dir,
                input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                input_bytes=7,
                caps=MODULE.Caps(),
                dpi=96,
                expected_font_pack_sha256=digest,
            )
            self.assertEqual(bundle.renderer["font_pack_sha256"], digest)
            with self.assertRaisesRegex(MODULE.HarnessError, "font_pack_mismatch"):
                MODULE.validate_bundle(
                    bundle_dir,
                    input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                    input_bytes=7,
                    caps=MODULE.Caps(),
                    dpi=96,
                )

    def test_bundle_validation_accepts_and_fail_closes_render_font_face_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            bundle_dir = root / "bundle"
            pack_digest = "a" * 64
            face_digest = "b" * 64
            face = {
                "source_pack_sha256": pack_digest,
                "face_sha256": face_digest,
                "family": "Fixture Sans",
                "weight": 400,
                "italic": False,
                "substituted": True,
            }
            write_bundle(
                bundle_dir,
                source,
                font_pack_sha256=pack_digest,
                render_schema_version=2,
                font_faces=[face],
            )
            bundle = MODULE.validate_bundle(
                bundle_dir,
                input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                input_bytes=7,
                caps=MODULE.Caps(),
                dpi=96,
                expected_font_pack_sha256=pack_digest,
            )
            self.assertEqual(bundle.renderer["font_pack_sha256"], pack_digest)

            manifest_path = bundle_dir / "render-manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["sheets"][0]["report"]["font_faces"][0]["face_sha256"] = "bad"
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(
                MODULE.HarnessError, "render_manifest_report_font_face"
            ):
                MODULE.validate_bundle(
                    bundle_dir,
                    input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                    input_bytes=7,
                    caps=MODULE.Caps(),
                    dpi=96,
                    expected_font_pack_sha256=pack_digest,
                )

    def test_bundle_validation_selects_the_single_page_print_scene(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            bundle_dir = root / "bundle"
            write_bundle(bundle_dir, source, print_pages=True)
            bundle = MODULE.validate_bundle(
                bundle_dir,
                input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                input_bytes=7,
                caps=MODULE.Caps(),
                dpi=96,
                require_single_page_print=True,
            )

            self.assertEqual(bundle.pages[0].svg_path.name, "page-0001.svg")
            self.assertEqual(
                (bundle.pages[0].width_pixels, bundle.pages[0].height_pixels),
                (816, 1056),
            )
            manifest_path = bundle_dir / "render-manifest.json"
            document = json.loads(manifest_path.read_text())
            report_path = bundle_dir / document["sheets"][0]["print"]["report"]["file"]
            report = json.loads(report_path.read_text())
            report["layout_override"] = "authored"
            report_payload = json.dumps(report, sort_keys=True).encode()
            report_path.write_bytes(report_payload)
            artifact = document["sheets"][0]["print"]["report"]
            artifact["bytes"] = len(report_payload)
            artifact["sha256"] = hashlib.sha256(report_payload).hexdigest()
            manifest_path.write_text(json.dumps(document, sort_keys=True))
            with self.assertRaisesRegex(MODULE.HarnessError, "print_report"):
                MODULE.validate_bundle(
                    bundle_dir,
                    input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                    input_bytes=7,
                    caps=MODULE.Caps(),
                    dpi=96,
                    require_single_page_print=True,
                )

    def test_bundle_validation_accepts_single_area_print_report_v2(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            bundle_dir = root / "bundle"
            write_bundle(
                bundle_dir,
                source,
                print_pages=True,
                print_schema_version=2,
            )

            bundle = MODULE.validate_bundle(
                bundle_dir,
                input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                input_bytes=7,
                caps=MODULE.Caps(),
                dpi=96,
                require_single_page_print=True,
            )

            self.assertEqual(len(bundle.pages), 1)

    def test_authored_print_source_and_multi_page_bundle_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            write_authored_print_xlsx(source, fit=False)
            case = MODULE.InputCase(
                source,
                "fixture.xlsx",
                source.stat().st_size,
                features=("print-settings",),
            )
            attestation = MODULE.attest_authored_print_source(case)
            self.assertEqual(attestation["scale_mode"], "scale")
            self.assertEqual(
                (attestation["expected_page_width_pixels"], attestation["expected_page_height_pixels"]),
                (816, 1056),
            )
            fit_source = root / "fit.xlsx"
            write_authored_print_xlsx(fit_source, fit=True)
            fit_case = MODULE.InputCase(
                fit_source,
                "fit.xlsx",
                fit_source.stat().st_size,
                features=("print-settings",),
            )
            self.assertEqual(
                MODULE.attest_authored_print_source(fit_case)["scale_mode"], "fit"
            )

            mixed_source = root / "mixed.xlsx"
            write_authored_print_xlsx(
                mixed_source,
                fit=False,
                setup_override=(
                    '<pageSetup orientation="portrait" paperSize="1" scale="85" '
                    'fitToWidth="2" fitToHeight="2" pageOrder="overThenDown"/>'
                ),
            )
            mixed_case = MODULE.InputCase(
                mixed_source,
                "mixed.xlsx",
                mixed_source.stat().st_size,
                features=("print-settings",),
            )
            with self.assertRaisesRegex(MODULE.HarnessError, "authored_print_scale_fit"):
                MODULE.attest_authored_print_source(mixed_case)

            loose_margins = root / "loose-margins.xlsx"
            write_authored_print_xlsx(
                loose_margins,
                fit=False,
                margins_override=(
                    '<pageMargins left="1" right="1" top="1" bottom="1" '
                    'header="0.2" footer="0.25"/>'
                ),
            )
            loose_case = MODULE.InputCase(
                loose_margins,
                "loose-margins.xlsx",
                loose_margins.stat().st_size,
                features=("print-settings",),
            )
            with self.assertRaisesRegex(MODULE.HarnessError, "authored_print_margins"):
                MODULE.attest_authored_print_source(loose_case)

            bundle_dir = root / "bundle"
            write_authored_bundle(bundle_dir, source)
            bundle = MODULE.validate_bundle(
                bundle_dir,
                input_sha256=hashlib.sha256(source.read_bytes()).hexdigest(),
                input_bytes=source.stat().st_size,
                caps=MODULE.Caps(),
                dpi=96,
                print_mode=MODULE.PRINT_MODE_AUTHORED,
                authored_scale_mode=MODULE.AUTHORED_SCALE_MODE_SCALE,
            )
            self.assertEqual(len(bundle.pages), 4)
            self.assertEqual(
                {(page.width_pixels, page.height_pixels) for page in bundle.pages},
                {(816, 1056)},
            )

            manifest_path = bundle_dir / "render-manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["sheets"][0]["print"]["layout_override"] = "single_page_sheets"
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(MODULE.HarnessError, "render_manifest_print"):
                MODULE.validate_bundle(
                    bundle_dir,
                    input_sha256=hashlib.sha256(source.read_bytes()).hexdigest(),
                    input_bytes=source.stat().st_size,
                    caps=MODULE.Caps(),
                    dpi=96,
                    print_mode=MODULE.PRINT_MODE_AUTHORED,
                    authored_scale_mode=MODULE.AUTHORED_SCALE_MODE_SCALE,
                )

    def test_authored_bundle_validation_is_bound_to_attested_scale_mode(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            source_sha256 = hashlib.sha256(b"fixture").hexdigest()
            bundles = {}
            for scale_mode, expected_pages in (
                (MODULE.AUTHORED_SCALE_MODE_SCALE, 4),
                (MODULE.AUTHORED_SCALE_MODE_FIT, 1),
            ):
                bundle_dir = root / scale_mode
                write_authored_bundle(
                    bundle_dir,
                    source,
                    scale_mode=scale_mode,
                )
                bundles[scale_mode] = bundle_dir
                with self.subTest(scale_mode=scale_mode):
                    bundle = MODULE.validate_bundle(
                        bundle_dir,
                        input_sha256=source_sha256,
                        input_bytes=7,
                        caps=MODULE.Caps(),
                        dpi=96,
                        print_mode=MODULE.PRINT_MODE_AUTHORED,
                        authored_scale_mode=scale_mode,
                    )
                    self.assertEqual(len(bundle.pages), expected_pages)

            for supplied_mode, bundle_mode in (
                (
                    MODULE.AUTHORED_SCALE_MODE_FIT,
                    MODULE.AUTHORED_SCALE_MODE_SCALE,
                ),
                (
                    MODULE.AUTHORED_SCALE_MODE_SCALE,
                    MODULE.AUTHORED_SCALE_MODE_FIT,
                ),
            ):
                with (
                    self.subTest(
                        supplied_mode=supplied_mode,
                        bundle_mode=bundle_mode,
                    ),
                    self.assertRaisesRegex(
                        MODULE.HarnessError,
                        "render_manifest_authored_",
                    ),
                ):
                    MODULE.validate_bundle(
                        bundles[bundle_mode],
                        input_sha256=source_sha256,
                        input_bytes=7,
                        caps=MODULE.Caps(),
                        dpi=96,
                        print_mode=MODULE.PRINT_MODE_AUTHORED,
                        authored_scale_mode=supplied_mode,
                    )

            for invalid_mode in (None, "automatic", ["scale"]):
                with (
                    self.subTest(invalid_mode=invalid_mode),
                    self.assertRaisesRegex(
                        MODULE.HarnessError,
                        "render_manifest_authored_scale_mode",
                    ),
                ):
                    MODULE.validate_bundle(
                        bundles[MODULE.AUTHORED_SCALE_MODE_SCALE],
                        input_sha256=source_sha256,
                        input_bytes=7,
                        caps=MODULE.Caps(),
                        dpi=96,
                        print_mode=MODULE.PRINT_MODE_AUTHORED,
                        authored_scale_mode=invalid_mode,
                    )

            with self.assertRaisesRegex(
                MODULE.HarnessError,
                "render_manifest_authored_scale_mode",
            ):
                MODULE.validate_bundle(
                    bundles[MODULE.AUTHORED_SCALE_MODE_SCALE],
                    input_sha256=source_sha256,
                    input_bytes=7,
                    caps=MODULE.Caps(),
                    dpi=96,
                    authored_scale_mode=MODULE.AUTHORED_SCALE_MODE_SCALE,
                )

    def test_authored_hidden_sheet_and_multipage_indices_remain_distinct(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            bundle_dir = root / "bundle"
            write_authored_bundle(
                bundle_dir,
                source,
                visibilities=("hidden", "visible"),
            )

            bundle = MODULE.validate_bundle(
                bundle_dir,
                input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                input_bytes=7,
                caps=MODULE.Caps(),
                dpi=96,
                print_mode=MODULE.PRINT_MODE_AUTHORED,
                authored_scale_mode=MODULE.AUTHORED_SCALE_MODE_SCALE,
            )

        self.assertEqual(len(bundle.pages), 8)
        self.assertEqual(
            [
                (
                    page.source_sheet_index,
                    page.source_pdf_page_index,
                    page.oracle_output_page_index,
                )
                for page in bundle.pages
            ],
            [
                (0, 0, None),
                (0, 1, None),
                (0, 2, None),
                (0, 3, None),
                (1, 0, 0),
                (1, 1, 1),
                (1, 2, 2),
                (1, 3, 3),
            ],
        )

    def test_bundle_retains_typed_warning_counts_without_workbook_content(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            bundle_dir = root / "bundle"
            write_bundle(bundle_dir, source)
            manifest_path = bundle_dir / "render-manifest.json"
            document = json.loads(manifest_path.read_text())
            document["sheets"][0]["report"]["warnings"] = [
                {
                    "code": "font_family_substituted",
                    "occurrences": 3,
                    "first_cell": {"row": 7, "col": 2},
                }
            ]
            manifest_path.write_text(json.dumps(document, sort_keys=True))
            bundle = MODULE.validate_bundle(
                bundle_dir,
                input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                input_bytes=7,
                caps=MODULE.Caps(),
                dpi=96,
            )

        self.assertEqual(
            bundle.pages[0].warnings,
            (("font_family_substituted", 3, {"row": 7, "col": 2}),),
        )

    def test_bundle_rejects_source_mismatch_and_unexpected_files(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            bundle_dir = root / "bundle"
            write_bundle(bundle_dir, source)
            with self.assertRaisesRegex(MODULE.HarnessError, "source_mismatch"):
                MODULE.validate_bundle(
                    bundle_dir,
                    input_sha256="0" * 64,
                    input_bytes=7,
                    caps=MODULE.Caps(),
                    dpi=96,
                )

            (bundle_dir / "unexpected.bin").write_bytes(b"x")
            with self.assertRaisesRegex(MODULE.HarnessError, "unexpected_artifact"):
                MODULE.validate_bundle(
                    bundle_dir,
                    input_sha256=hashlib.sha256(b"fixture").hexdigest(),
                    input_bytes=7,
                    caps=MODULE.Caps(),
                    dpi=96,
                )

    def test_svg_dimensions_use_exact_physical_unit_arithmetic(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "physical.svg"
            path.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" width="1in" height="72pt"/>',
                encoding="utf-8",
            )
            self.assertEqual(
                MODULE.inspect_svg(path, dpi=144, max_svg_bytes=1024),
                (144, 144),
            )

            path.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" width="2.54cm" height="25.4mm"/>',
                encoding="utf-8",
            )
            self.assertEqual(
                MODULE.inspect_svg(path, dpi=96, max_svg_bytes=1024),
                (96, 96),
            )

    def test_svg_rejects_external_references_and_doctypes(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "unsafe.svg"
            path.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">'
                '<image href="https://example.invalid/image.png"/></svg>',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(MODULE.HarnessError, "external_reference"):
                MODULE.inspect_svg(path, dpi=96, max_svg_bytes=1024)

            path.write_text(
                '<!DOCTYPE svg><svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(MODULE.HarnessError, "unsafe_markup"):
                MODULE.inspect_svg(path, dpi=96, max_svg_bytes=1024)

    def test_svg_allows_inert_external_anchor_navigation(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "anchor.svg"
            path.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" width="4" height="2">'
                '<a href="https://example.invalid/"><text>link</text></a></svg>',
                encoding="utf-8",
            )
            self.assertEqual(
                MODULE.inspect_svg(path, dpi=96, max_svg_bytes=1024), (4, 2)
            )

    def test_integer_metrics_are_exact_and_float_free(self) -> None:
        exact = MODULE.integer_image_metrics(bytes([1, 2, 3]), bytes([1, 2, 3]))
        self.assertEqual(exact["changed_pixels"], 0)
        self.assertEqual(exact["similarity_ppm"], 1_000_000)
        self.assertEqual(exact["root_mean_square_error_ppm"], 0)

        one_red_channel = MODULE.integer_image_metrics(
            bytes([0, 0, 0]), bytes([255, 0, 0])
        )
        self.assertEqual(one_red_channel["pixels"], 1)
        self.assertEqual(one_red_channel["changed_pixels"], 1)
        self.assertEqual(one_red_channel["mismatch_ppm"], 1_000_000)
        self.assertEqual(one_red_channel["absolute_error_sum"], 255)
        self.assertEqual(one_red_channel["mean_absolute_error_ppm"], 333_333)
        self.assertEqual(one_red_channel["root_mean_square_error_ppm"], 577_350)
        self.assertEqual(one_red_channel["similarity_ppm"], 666_667)

    def test_extended_metrics_are_exact_for_perfect_byte_buffers(self) -> None:
        image = rgb_buffer(5, 3, {(1, 1): (0, 0, 0)})
        metrics = MODULE.visual_image_metrics(image, image, 5, 3)

        self.assertEqual(metrics["foreground_rxls_pixels"], 1)
        self.assertEqual(metrics["foreground_libreoffice_pixels"], 1)
        self.assertEqual(metrics["foreground_precision_ppm"], 1_000_000)
        self.assertEqual(metrics["foreground_recall_ppm"], 1_000_000)
        self.assertEqual(metrics["foreground_f1_ppm"], 1_000_000)
        self.assertEqual(metrics["edge_f1_ppm"], 1_000_000)
        self.assertEqual(metrics["text_ink_f1_ppm"], 1_000_000)
        self.assertEqual(metrics["blurred_luma_similarity_ppm"], 1_000_000)
        self.assertEqual(metrics["foreground_matched_color_absolute_error_sum"], 0)
        self.assertEqual(
            metrics["foreground_rxls_bbox"],
            {"present": 1, "left": 1, "top": 1, "right": 1, "bottom": 1},
        )
        self.assertEqual(metrics["foreground_centroid_distance_millipixels"], 0)
        self.assertEqual(
            metrics["metric_work_units"], 15 * MODULE.METRIC_WORK_UNITS_PER_PIXEL
        )

        def assert_integer_tree(value) -> None:
            if isinstance(value, dict):
                for child in value.values():
                    assert_integer_tree(child)
            else:
                self.assertIsInstance(value, int)

        assert_integer_tree(metrics)
        with self.assertRaisesRegex(MODULE.HarnessError, "metric_work_limit"):
            MODULE.visual_image_metrics(
                image,
                image,
                5,
                3,
                max_metric_work_units=metrics["metric_work_units"] - 1,
            )

    @unittest.skipUnless(
        importlib.util.find_spec("numpy") is not None,
        "NumPy exact-equivalence path is unavailable",
    )
    def test_numpy_metrics_are_bit_exact_with_reference_for_bounded_shapes(self) -> None:
        generator = random.Random(0x52584C53)
        for width, height in ((1, 1), (1, 7), (8, 1), (2, 2), (7, 5), (31, 19)):
            for _ in range(12):
                rxls = generator.randbytes(width * height * 3)
                libreoffice = generator.randbytes(width * height * 3)
                self.assertEqual(
                    MODULE._visual_image_metrics_numpy(
                        rxls, libreoffice, width, height
                    ),
                    MODULE._visual_image_metrics_python(
                        rxls, libreoffice, width, height
                    ),
                )

    def test_one_pixel_shift_is_tolerated_but_alignment_is_reported(self) -> None:
        rxls = rgb_buffer(5, 3, {(1, 1): (0, 0, 0)})
        libreoffice = rgb_buffer(5, 3, {(2, 1): (0, 0, 0)})
        metrics = MODULE.visual_image_metrics(rxls, libreoffice, 5, 3)

        self.assertEqual(metrics["foreground_f1_ppm"], 1_000_000)
        self.assertEqual(metrics["edge_f1_ppm"], 1_000_000)
        self.assertEqual(metrics["text_ink_f1_ppm"], 1_000_000)
        self.assertEqual(metrics["foreground_centroid_delta_x_millipixels"], -1000)
        self.assertEqual(metrics["foreground_centroid_delta_y_millipixels"], 0)
        self.assertEqual(metrics["foreground_bbox_alignment_max_delta_pixels"], 1)
        self.assertEqual(metrics["blurred_luma_absolute_error_sum"], 280)
        self.assertEqual(metrics["blurred_luma_similarity_ppm"], 926_797)

    def test_matched_foreground_color_error_detects_color_change(self) -> None:
        rxls = rgb_buffer(5, 3, {(1, 1): (0, 0, 0)})
        libreoffice = rgb_buffer(5, 3, {(1, 1): (100, 100, 100)})
        metrics = MODULE.visual_image_metrics(rxls, libreoffice, 5, 3)

        self.assertEqual(metrics["foreground_f1_ppm"], 1_000_000)
        self.assertEqual(metrics["foreground_matched_color_samples"], 1)
        self.assertEqual(metrics["foreground_matched_color_absolute_error_sum"], 300)
        self.assertEqual(
            metrics["foreground_matched_color_mean_absolute_error_ppm"], 392_157
        )
        self.assertEqual(metrics["blurred_luma_absolute_error_sum"], 152)
        self.assertEqual(metrics["blurred_luma_similarity_ppm"], 960_261)

    def test_edge_and_text_ink_masks_report_missing_oracle_ink(self) -> None:
        rxls = rgb_buffer(5, 3, {(1, 1): (0, 0, 0)})
        libreoffice = rgb_buffer(5, 3)
        metrics = MODULE.visual_image_metrics(rxls, libreoffice, 5, 3)

        self.assertEqual(metrics["edge_rxls_pixels"], 5)
        self.assertEqual(metrics["edge_libreoffice_pixels"], 0)
        self.assertEqual(metrics["edge_precision_ppm"], 0)
        self.assertEqual(metrics["edge_recall_ppm"], 0)
        self.assertEqual(metrics["edge_f1_ppm"], 0)
        self.assertEqual(metrics["text_ink_rxls_pixels"], 1)
        self.assertEqual(metrics["text_ink_libreoffice_pixels"], 0)
        self.assertEqual(metrics["text_ink_f1_ppm"], 0)
        self.assertEqual(metrics["foreground_alignment_comparable"], 0)

    def test_empty_masks_have_exact_neutral_scores_and_explicit_bboxes(self) -> None:
        white = rgb_buffer(5, 3)
        metrics = MODULE.visual_image_metrics(white, white, 5, 3)

        empty_bbox = {
            "present": 0,
            "left": 0,
            "top": 0,
            "right": 0,
            "bottom": 0,
        }
        for prefix in ("foreground", "edge", "text_ink"):
            self.assertEqual(metrics[f"{prefix}_rxls_pixels"], 0)
            self.assertEqual(metrics[f"{prefix}_libreoffice_pixels"], 0)
            self.assertEqual(metrics[f"{prefix}_precision_ppm"], 1_000_000)
            self.assertEqual(metrics[f"{prefix}_recall_ppm"], 1_000_000)
            self.assertEqual(metrics[f"{prefix}_f1_ppm"], 1_000_000)
        self.assertEqual(metrics["foreground_rxls_bbox"], empty_bbox)
        self.assertEqual(metrics["foreground_libreoffice_bbox"], empty_bbox)
        self.assertEqual(metrics["text_ink_rxls_bbox"], empty_bbox)
        self.assertEqual(metrics["text_ink_libreoffice_bbox"], empty_bbox)
        self.assertEqual(metrics["foreground_alignment_comparable"], 1)

    def test_semantic_text_is_unicode_normalized_bounded_and_content_private(self) -> None:
        tokens = MODULE.normalize_semantic_tokens(
            "  A\u00ad  Cafe\u0301  \u2067한글\u2069  ",
            max_codepoints=32,
            max_tokens=4,
        )
        self.assertEqual(tokens, ("A", "Café", "한글"))
        with self.assertRaisesRegex(MODULE.HarnessError, "semantic_token_limit"):
            MODULE.normalize_semantic_tokens(
                "one two", max_codepoints=32, max_tokens=1
            )

        exact = MODULE.semantic_text_metrics(tokens, tokens)
        self.assertEqual(exact["semantic_exact"], 1)
        self.assertEqual(exact["semantic_token_f1_ppm"], 1_000_000)
        self.assertEqual(exact["semantic_codepoint_f1_ppm"], 1_000_000)
        self.assertNotIn("Café", json.dumps(exact, sort_keys=True))

        reordered = MODULE.semantic_text_metrics(tokens, tuple(reversed(tokens)))
        self.assertEqual(reordered["semantic_exact"], 0)
        self.assertEqual(reordered["semantic_token_f1_ppm"], 1_000_000)
        self.assertEqual(reordered["semantic_bigram_f1_ppm"], 0)

    def test_svg_labels_and_pdftotext_pages_share_semantic_contract(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            svg = Path(raw) / "page.svg"
            svg.write_text(
                """<svg xmlns="http://www.w3.org/2000/svg">
                <g role="text" aria-label="한글 &amp; 23"
                data-rxls-visible-label="한글 &amp; 23"><path/></g>
                <g role="text" aria-label="US"
                data-rxls-visible-label="US"><path/></g>
                </svg>""",
                encoding="utf-8",
            )
            tokens = MODULE.extract_svg_semantic_tokens(
                svg,
                max_svg_bytes=4096,
                max_codepoints=64,
                max_tokens=8,
            )
        self.assertEqual(tokens, ("한글", "&", "23", "US"))

        pages = MODULE.parse_pdftotext_pages(
            "한글  &  23\nUS\f두 번째\f".encode(),
            expected_pages=2,
            max_codepoints=64,
            max_tokens=8,
        )
        self.assertEqual(pages[0], tokens)
        self.assertEqual(pages[1], ("두", "번째"))
        with self.assertRaisesRegex(MODULE.HarnessError, "text_page_count"):
            MODULE.parse_pdftotext_pages(
                b"only one page",
                expected_pages=2,
                max_codepoints=64,
                max_tokens=8,
            )

    def test_svg_visible_labels_drive_clipped_semantic_tokens(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            svg = Path(raw) / "page.svg"
            svg.write_text(
                """<svg xmlns="http://www.w3.org/2000/svg">
                <g role="text" aria-label="prefix hidden"
                data-rxls-visible-label="prefix"><path/></g>
                <g role="text" aria-label="hidden suffix"
                data-rxls-visible-label="suffix"><path/></g>
                <g role="text" aria-label="fully hidden"
                data-rxls-visible-label=""><path/></g>
                <g role="text" aria-label="spreadsheet"
                data-rxls-visible-label="read"><path/></g>
                <g role="text" aria-label="甲中乙"
                data-rxls-visible-label="甲 乙"><path/></g>
                </svg>""",
                encoding="utf-8",
            )
            tokens = MODULE.extract_svg_semantic_tokens(
                svg,
                max_svg_bytes=4096,
                max_codepoints=64,
                max_tokens=8,
            )
        self.assertEqual(tokens, ("prefix", "suffix", "read", "甲", "乙"))

    def test_svg_visible_label_validation_fails_closed(self) -> None:
        fixtures = {
            "aria_missing": (
                '<svg><g role="text" data-rxls-visible-label="x"/></svg>',
                "label_missing",
                8,
            ),
            "visible_missing": (
                '<svg><g role="text" aria-label="x"/></svg>',
                "visible_label_missing",
                8,
            ),
            "unbounded": (
                '<svg><g role="text" aria-label="abcde" '
                'data-rxls-visible-label="abcde"/></svg>',
                "visible_label_unbounded",
                1,
            ),
            "control": (
                '<svg><g role="text" aria-label="a&#x85;b" '
                'data-rxls-visible-label="a&#x85;b"/></svg>',
                "visible_label_control",
                8,
            ),
            "longer_than_source": (
                '<svg><g role="text" aria-label="x" '
                'data-rxls-visible-label="xx"/></svg>',
                "visible_label_length",
                8,
            ),
            "injected_unicode": (
                '<svg><g role="text" aria-label="safe한글" '
                'data-rxls-visible-label="safe악"/></svg>',
                "visible_label_injection",
                8,
            ),
            "reordered": (
                '<svg><g role="text" aria-label="abc" '
                'data-rxls-visible-label="cba"/></svg>',
                "visible_label_injection",
                8,
            ),
        }
        with tempfile.TemporaryDirectory() as raw:
            for name, (payload, error, max_codepoints) in fixtures.items():
                path = Path(raw) / f"{name}.svg"
                path.write_text(payload, encoding="utf-8")
                with self.subTest(name=name), self.assertRaisesRegex(
                    MODULE.HarnessError, error
                ):
                    MODULE.extract_svg_semantic_tokens(
                        path,
                        max_svg_bytes=4096,
                        max_codepoints=max_codepoints,
                        max_tokens=8,
                    )

    def test_aggregate_semantic_scores_are_derived_from_raw_counts(self) -> None:
        first = {
            "pixels": 1,
            "changed_pixels": 0,
            "absolute_error_sum": 0,
            "squared_error_sum": 0,
            "max_channel_delta": 0,
            **MODULE.semantic_text_metrics(("A", "B"), ("A", "B")),
        }
        second = {
            "pixels": 1,
            "changed_pixels": 0,
            "absolute_error_sum": 0,
            "squared_error_sum": 0,
            "max_channel_delta": 0,
            **MODULE.semantic_text_metrics(("C",), ("D",)),
        }
        aggregate = MODULE.aggregate_page_metrics([first, second])
        self.assertEqual(aggregate["semantic_exact_pages"], 1)
        self.assertEqual(aggregate["semantic_page_mismatches"], 1)
        self.assertEqual(aggregate["semantic_token_matched_items"], 2)
        self.assertEqual(aggregate["semantic_token_f1_ppm"], 666_667)

    def test_pillow_comparison_routes_through_extended_metrics(self) -> None:
        try:
            from PIL import Image
        except ImportError as error:
            self.skipTest(str(error))
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            left = root / "left.png"
            right = root / "right.png"
            payload = rgb_buffer(4, 2, {(1, 1): (0, 0, 0)})
            Image.frombytes("RGB", (4, 2), payload).save(left)
            Image.frombytes("RGB", (4, 2), payload).save(right)
            metrics = MODULE.compare_pngs(
                left,
                right,
                max_page_pixels=8,
                max_metric_work_units=8 * MODULE.METRIC_WORK_UNITS_PER_PIXEL,
            )

        self.assertEqual(metrics["canvas_size"], {"width": 4, "height": 2})
        self.assertEqual(metrics["foreground_f1_ppm"], 1_000_000)
        self.assertEqual(metrics["blurred_luma_similarity_ppm"], 1_000_000)

    def test_aggregate_metrics_weight_pixels_instead_of_averaging_pages(self) -> None:
        pages = [
            {
                "pixels": 1,
                "changed_pixels": 1,
                "absolute_error_sum": 765,
                "squared_error_sum": 195075,
                "max_channel_delta": 255,
            },
            {
                "pixels": 3,
                "changed_pixels": 0,
                "absolute_error_sum": 0,
                "squared_error_sum": 0,
                "max_channel_delta": 0,
            },
        ]
        aggregate = MODULE.aggregate_page_metrics(pages)
        self.assertEqual(aggregate["pages"], 2)
        self.assertEqual(aggregate["pixels"], 4)
        self.assertEqual(aggregate["mismatch_ppm"], 250_000)
        self.assertEqual(aggregate["mean_absolute_error_ppm"], 250_000)
        self.assertEqual(aggregate["exact_pages"], 1)

    def test_extended_aggregate_metrics_are_derived_from_raw_counts(self) -> None:
        first = MODULE.visual_image_metrics(
            rgb_buffer(3, 3, {(1, 1): (0, 0, 0)}),
            rgb_buffer(3, 3),
            3,
            3,
        )
        first["canvas_size"] = {"width": 3, "height": 3}
        block = {
            (1, 1): (0, 0, 0),
            (2, 1): (0, 0, 0),
            (1, 2): (0, 0, 0),
            (2, 2): (0, 0, 0),
        }
        second = MODULE.visual_image_metrics(
            rgb_buffer(5, 5, block),
            rgb_buffer(5, 5, block),
            5,
            5,
        )
        second["canvas_size"] = {"width": 5, "height": 5}

        aggregate = MODULE.aggregate_page_metrics([first, second])

        self.assertEqual(aggregate["foreground_rxls_pixels"], 5)
        self.assertEqual(aggregate["foreground_libreoffice_pixels"], 4)
        self.assertEqual(aggregate["foreground_rxls_matched_1px"], 4)
        self.assertEqual(aggregate["foreground_libreoffice_matched_1px"], 4)
        self.assertEqual(aggregate["foreground_precision_ppm"], 800_000)
        self.assertEqual(aggregate["foreground_recall_ppm"], 1_000_000)
        self.assertEqual(aggregate["foreground_f1_ppm"], 888_889)
        self.assertEqual(aggregate["text_ink_f1_ppm"], 888_889)
        self.assertEqual(aggregate["edge_precision_ppm"], 705_882)
        self.assertEqual(aggregate["edge_recall_ppm"], 1_000_000)
        self.assertEqual(aggregate["edge_f1_ppm"], 827_586)
        self.assertEqual(
            aggregate["blurred_luma_absolute_error_sum"],
            first["blurred_luma_absolute_error_sum"]
            + second["blurred_luma_absolute_error_sum"],
        )
        self.assertEqual(aggregate["blurred_luma_similarity_ppm"], 948_328)
        self.assertEqual(aggregate["stacked_canvas_size"], {"width": 5, "height": 8})
        self.assertEqual(
            aggregate["foreground_rxls_bbox"],
            {"present": 1, "left": 1, "top": 1, "right": 2, "bottom": 5},
        )
        self.assertEqual(
            aggregate["foreground_libreoffice_bbox"],
            {"present": 1, "left": 1, "top": 4, "right": 2, "bottom": 5},
        )

    def test_page_dimension_deltas_are_explicit(self) -> None:
        page = MODULE.visual_image_metrics(
            rgb_buffer(2, 2), rgb_buffer(2, 2), 2, 2
        )
        page.update(
            {
                "canvas_size": {"width": 3, "height": 4},
                "rxls_size": {"width": 2, "height": 2},
                "libreoffice_size": {"width": 3, "height": 4},
            }
        )
        aggregate = MODULE.aggregate_page_metrics([page])
        self.assertEqual(aggregate["page_dimension_mismatches"], 1)
        self.assertEqual(aggregate["max_page_width_delta_pixels"], 1)
        self.assertEqual(aggregate["max_page_height_delta_pixels"], 2)

    def test_metric_cohorts_report_p10_means_and_worst_deltas(self) -> None:
        results = []
        for index in range(10):
            results.append(
                {
                    "features": ["korean-text", "wrapped-text"],
                    "format": "xlsx",
                    "status": "compared",
                    "metrics": {
                        "similarity_ppm": 100_000 + index * 10_000,
                        "max_page_width_delta_pixels": index,
                    },
                }
            )
        results.append(
            {
                "features": ["korean-text"],
                "format": "xls",
                "status": "skipped",
            }
        )
        results.append(
            {
                "features": ["korean-text"],
                "format": "xlsx",
                "status": "different",
                "metrics": {
                    "semantic_comparable": 0,
                    "similarity_ppm": 999_999,
                    "max_page_width_delta_pixels": 999,
                },
            }
        )
        results.append(
            {
                "features": ["empty-sheet"],
                "format": "xlsx",
                "status": "compared",
                "metrics": {
                    "semantic_comparable": 1,
                    "semantic_token_rxls_items": 0,
                    "semantic_token_libreoffice_items": 0,
                    "foreground_rxls_pixels": 0,
                    "foreground_libreoffice_pixels": 0,
                    "similarity_ppm": 1_000_000,
                    "max_page_width_delta_pixels": 500,
                },
            }
        )

        cohorts = MODULE.metric_cohorts(results)

        overall = cohorts["all"]
        self.assertEqual(overall["workbooks"], 13)
        self.assertEqual(overall["comparable_workbooks"], 10)
        self.assertEqual(overall["scores"]["similarity_ppm"]["p10"], 100_000)
        self.assertEqual(overall["scores"]["similarity_ppm"]["mean"], 145_000)
        self.assertEqual(
            overall["deltas"]["max_page_width_delta_pixels"]["p90"], 8
        )
        self.assertEqual(
            cohorts["by_feature"]["korean-text"]["workbooks"], 12
        )
        self.assertEqual(cohorts["by_format"]["xls"]["comparable_workbooks"], 0)

    def test_bounded_command_runner_classifies_combined_pipe_limit(self) -> None:
        runner = MODULE.BoundedCommandRunner()
        with tempfile.TemporaryDirectory() as raw:
            result = runner.run(
                [
                    sys.executable,
                    "-c",
                    "import sys;"
                    "sys.stdout.buffer.write(b'x'*50000);"
                    "sys.stderr.buffer.write(b'y'*50000)",
                ],
                cwd=Path(raw),
                env=os.environ.copy(),
                timeout_seconds=5,
                output_limit_bytes=1024,
            )
        self.assertEqual(result.status, "output_limit")
        self.assertLessEqual(len(result.stdout) + len(result.stderr), 1024)

    def test_bounded_command_runner_terminates_timeout(self) -> None:
        runner = MODULE.BoundedCommandRunner()
        with tempfile.TemporaryDirectory() as raw:
            result = runner.run(
                [sys.executable, "-c", "import time;time.sleep(5)"],
                cwd=Path(raw),
                env=os.environ.copy(),
                timeout_seconds=0.05,
                output_limit_bytes=1024,
            )
        self.assertEqual(result.status, "timeout")

    def test_bounded_command_runner_enforces_live_file_bytes_and_count(self) -> None:
        runner = MODULE.BoundedCommandRunner()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            output = root / "output"
            output.mkdir()
            result = runner.run(
                [
                    sys.executable,
                    "-c",
                    (
                        "from pathlib import Path;"
                        "Path('output/payload.bin').write_bytes(b'x'*4096)"
                    ),
                ],
                cwd=root,
                env=os.environ.copy(),
                timeout_seconds=5,
                output_limit_bytes=4096,
                file_output_roots=(output,),
                file_output_limit_bytes=1024,
                file_output_count_limit=4,
            )
            self.assertEqual(result.status, "file_output_limit")
            self.assertLessEqual((output / "payload.bin").stat().st_size, 1025)

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            output = root / "output"
            output.mkdir()
            result = runner.run(
                [
                    sys.executable,
                    "-c",
                    (
                        "from pathlib import Path;"
                        "root=Path('output');"
                        "[(root / f'{i}.bin').write_bytes(b'x') for i in range(3)]"
                    ),
                ],
                cwd=root,
                env=os.environ.copy(),
                timeout_seconds=5,
                output_limit_bytes=4096,
                file_output_roots=(output,),
                file_output_limit_bytes=4096,
                file_output_count_limit=2,
            )
            self.assertEqual(result.status, "file_output_limit")

    def test_live_output_scan_uses_one_race_safe_lstat_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            payload = root / "payload.bin"
            payload.write_bytes(b"bounded")
            with (
                mock.patch.object(
                    Path,
                    "is_symlink",
                    side_effect=AssertionError("must use lstat mode"),
                ),
                mock.patch.object(
                    Path,
                    "is_dir",
                    side_effect=AssertionError("must use lstat mode"),
                ),
                mock.patch.object(
                    Path,
                    "is_file",
                    side_effect=AssertionError("must use lstat mode"),
                ),
            ):
                self.assertEqual(
                    MODULE._live_output_usage((root,), count_limit=4),
                    (len(b"bounded"), 1),
                )

            link = root / "payload-link"
            link.symlink_to(payload)
            with self.assertRaisesRegex(
                MODULE.HarnessError, "live_output_symlink"
            ):
                MODULE._live_output_usage((link,), count_limit=4)

            with mock.patch.object(
                Path,
                "iterdir",
                side_effect=FileNotFoundError("atomic rename"),
            ):
                self.assertEqual(
                    MODULE._live_output_usage((root,), count_limit=4),
                    (0, 0),
                )

    def test_dry_run_is_path_neutral_and_executes_nothing(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "private" / "fixture.xlsx"
            source.parent.mkdir()
            source.write_bytes(b"fixture")
            case = MODULE.InputCase(source, "fixture.xlsx", 7)
            config = MODULE.HarnessConfig(
                rxls_command=("/private/tools/rxls-render",),
                libreoffice="/private/tools/soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=True,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
            )
            evidence, exit_code = MODULE.run_harness(
                [case],
                discovery={"candidate_count": 1, "selected_count": 1, "truncated": False},
                config=config,
                backends=MODULE.Backends(False, False, False),
                runner=NoCallRunner(),
            )
            rendered = json.dumps(evidence, sort_keys=True)

        self.assertEqual(exit_code, 0)
        self.assertEqual(evidence["files"][0]["status"], "dry_run")
        self.assertIsNone(evidence["configuration"]["font_pack"])
        self.assertEqual(
            evidence["preflight"]["font_pack"],
            {"attestation_required": False, "configured": False},
        )
        self.assertNotIn(str(root), rendered)
        self.assertNotIn("/private/tools", rendered)
        self.assertIn("SinglePageSheets", rendered)

    def test_mocked_execution_validates_both_outputs_then_classifies_missing_backends(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            runner = FakeRunner(source)
            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice="soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=False,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
            )
            evidence, exit_code = MODULE.run_harness(
                [MODULE.InputCase(source, "fixture.xlsx", 7)],
                discovery={"candidate_count": 1, "selected_count": 1, "truncated": False},
                config=config,
                backends=MODULE.Backends(False, False, False),
                runner=runner,
            )

        self.assertEqual(exit_code, 0)
        result = evidence["files"][0]
        self.assertEqual(result["status"], "skipped")
        self.assertEqual(result["classification"], "visual_dependencies_missing")
        self.assertEqual(result["missing_dependencies"], [
            "pillow",
            "pdfinfo",
            "pdftoppm",
            "pdftotext",
        ])
        self.assertEqual(len(runner.commands), 2)
        self.assertIn("bundle", runner.commands[0])
        self.assertIn("--single-page-sheets", runner.commands[0])
        self.assertIn(MODULE.PDF_FILTER, runner.commands[1])

    def test_common_poppler_pipeline_measures_both_native_pdfs(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            runner = CommonPdfRunner(source)
            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice="soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=False,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
            )
            evidence, exit_code = MODULE.run_harness(
                [MODULE.InputCase(source, "fixture.xlsx", 7)],
                discovery={
                    "candidate_count": 1,
                    "selected_count": 1,
                    "truncated": False,
                },
                config=config,
                backends=MODULE.Backends(
                    pillow=True,
                    pymupdf=True,
                    cairosvg=True,
                    pdftoppm=True,
                    pdfinfo=True,
                    pdftotext=True,
                ),
                runner=runner,
            )

        self.assertEqual(exit_code, 0)
        result = evidence["files"][0]
        self.assertEqual(result["status"], "compared")
        self.assertEqual(
            [fact["source"] for fact in result["raster_commands"]],
            ["rxls_pdf", "libreoffice_pdf"],
        )
        self.assertEqual(
            {fact["backend"] for fact in result["raster_commands"]},
            {"poppler_pdftoppm"},
        )
        self.assertEqual(result["metrics"]["text_box_match_coverage_ppm"], 1_000_000)
        self.assertEqual(
            result["metrics"]["text_line_box_match_coverage_ppm"],
            1_000_000,
        )
        self.assertEqual(
            len(
                [
                    command
                    for command in runner.commands
                    if Path(command[0]).name == "pdftoppm"
                ]
            ),
            2,
        )

    def test_custom_svg_rasterizer_is_rejected_before_execution(self) -> None:
        config = MODULE.HarnessConfig(
            rxls_command=("rxls-render",),
            libreoffice="soffice",
            svg_rasterizer_command=("svg-tool", "{input}", "{output}"),
            caps=MODULE.Caps(),
            dpi=96,
            locale="C.UTF-8",
            dry_run=True,
            min_similarity_ppm=None,
            fail_on_incomparable=False,
        )
        with self.assertRaisesRegex(
            MODULE.HarnessError,
            "svg_rasterizer_command_retired",
        ):
            MODULE.run_harness(
                [],
                discovery={
                    "candidate_count": 0,
                    "selected_count": 0,
                    "truncated": False,
                },
                config=config,
                runner=NoCallRunner(),
            )

    def test_libreoffice_source_rejection_is_an_incomparable_skip_not_renderer_error(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.ods"
            source.write_bytes(b"fixture")
            runner = RejectingLibreOfficeRunner(source)
            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice="soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=False,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
            )
            evidence, exit_code = MODULE.run_harness(
                [MODULE.InputCase(source, "fixture.ods", 7)],
                discovery={"candidate_count": 1, "selected_count": 1, "truncated": False},
                config=config,
                backends=MODULE.Backends(False, False, False),
                runner=runner,
            )

        self.assertEqual(exit_code, 0)
        result = evidence["files"][0]
        self.assertEqual(result["status"], "skipped")
        self.assertEqual(result["classification"], "libreoffice_oracle_rejected")

    def test_input_and_total_corpus_caps_are_classified_without_execution(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            first = root / "first.xlsx"
            second = root / "second.xlsx"
            first.write_bytes(b"1234")
            second.write_bytes(b"5678")
            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice="soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(max_input_bytes=3, max_total_input_bytes=5),
                dpi=96,
                locale="C.UTF-8",
                dry_run=True,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
            )
            evidence, _ = MODULE.run_harness(
                [
                    MODULE.InputCase(first, "first.xlsx", 4),
                    MODULE.InputCase(second, "second.xlsx", 4),
                ],
                discovery={"candidate_count": 2, "selected_count": 2, "truncated": False},
                config=config,
                backends=MODULE.Backends(False, False, False),
                runner=NoCallRunner(),
            )

        self.assertEqual(evidence["files"][0]["classification"], "input_limit")
        self.assertEqual(
            evidence["files"][1]["classification"], "corpus_input_budget_exceeded"
        )

    def test_corpus_discovery_is_sorted_bounded_and_relative(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "z.xlsx").write_bytes(b"z")
            (root / "A.xls").write_bytes(b"a")
            (root / "ignore.txt").write_bytes(b"x")
            cases, facts = MODULE.discover_corpus(
                root, max_candidates=10, max_files=1
            )
        self.assertEqual([case.label for case in cases], ["A.xls"])
        self.assertEqual(facts["candidate_count"], 2)
        self.assertTrue(facts["truncated"])

    def test_render_corpus_manifest_is_fail_closed_relative_and_deduplicated(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            payload = root / "payload" / "source" / "book.xlsx"
            payload.parent.mkdir(parents=True)
            payload.write_bytes(b"book")
            digest = hashlib.sha256(b"book").hexdigest()
            manifest = root / "manifest.json"
            manifest.write_text(
                json.dumps(
                    {
                        "schema": "rxls.render-corpus-manifest.v1",
                        "files": [
                            {
                                "source_id": "canonical",
                                "source_path": "fixtures/book.xlsx",
                                "local_path": "payload/source/book.xlsx",
                                "status": "ready",
                                "eligible": True,
                                "bytes": 4,
                                "sha256": digest,
                            },
                            {
                                "source_id": "duplicate",
                                "source_path": "other/book.xlsx",
                                "local_path": "payload/source/book.xlsx",
                                "status": "duplicate",
                                "eligible": True,
                                "bytes": 4,
                                "sha256": digest,
                            },
                            {
                                "source_id": "private",
                                "source_path": "private.xlsx",
                                "local_path": "payload/private.xlsx",
                                "status": "quarantined",
                                "eligible": False,
                            },
                        ],
                    }
                ),
                encoding="utf-8",
            )
            cases, facts = MODULE.discover_manifest(
                manifest,
                max_manifest_bytes=16_384,
                max_candidates=10,
                max_files=10,
            )

        self.assertEqual(len(cases), 1)
        self.assertEqual(cases[0].label, "canonical/fixtures/book.xlsx")
        self.assertEqual(cases[0].expected_sha256, digest)
        self.assertEqual(cases[0].expected_bytes, 4)
        self.assertTrue(cases[0].path.is_absolute())
        self.assertEqual(facts["candidate_count"], 2)

    def test_render_corpus_manifest_honors_explicit_post_dedup_selection(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            payload = root / "payload"
            payload.mkdir()
            selected = payload / "selected.xlsx"
            excluded = payload / "excluded.xlsx"
            selected.write_bytes(b"selected")
            excluded.write_bytes(b"excluded")
            manifest = root / "manifest.json"
            manifest.write_text(
                json.dumps(
                    {
                        "schema": "rxls.render-corpus-manifest.v1",
                        "files": [
                            {
                                "source_id": "source",
                                "source_path": "selected.xlsx",
                                "local_path": "payload/selected.xlsx",
                                "status": "ready",
                                "eligible": True,
                                "render_selected": True,
                            },
                            {
                                "source_id": "source",
                                "source_path": "excluded.xlsx",
                                "local_path": "payload/excluded.xlsx",
                                "status": "ready",
                                "eligible": True,
                                "render_selected": False,
                            },
                        ],
                    }
                ),
                encoding="utf-8",
            )
            cases, facts = MODULE.discover_manifest(
                manifest,
                max_manifest_bytes=16_384,
                max_candidates=10,
                max_files=10,
            )

        self.assertEqual([case.label for case in cases], ["source/selected.xlsx"])
        self.assertEqual(facts["candidate_count"], 1)

    def test_generated_manifest_retains_rights_features_and_exact_identity(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            payload = root / "payload" / "fixture.xlsx"
            payload.parent.mkdir()
            payload.write_bytes(b"generated fixture")
            manifest = root / "manifest.json"
            manifest.write_text(
                json.dumps(
                    {
                        "files": [
                            {
                                "byte_length": payload.stat().st_size,
                                "features": ["korean-text", "wrapped-text"],
                                "path": "payload/fixture.xlsx",
                                "rights_tier": "S",
                                "sha256": hashlib.sha256(payload.read_bytes()).hexdigest(),
                            }
                        ],
                        "schema_version": 1,
                    }
                ),
                encoding="utf-8",
            )

            cases, _ = MODULE.discover_manifest(
                manifest,
                max_manifest_bytes=16_384,
                max_candidates=10,
                max_files=10,
            )

        self.assertEqual(len(cases), 1)
        self.assertEqual(cases[0].rights_tier, "S")
        self.assertEqual(cases[0].features, ("korean-text", "wrapped-text"))
        self.assertEqual(cases[0].expected_bytes, len(b"generated fixture"))

    def test_deterministic_shards_are_disjoint_complete_and_capped(self) -> None:
        cases = [
            MODULE.InputCase(
                Path(f"case-{index}.xlsx"),
                f"case-{index}.xlsx",
                1,
                hashlib.sha256(str(index).encode()).hexdigest(),
                1,
            )
            for index in range(30)
        ]
        base = {"candidate_count": 30, "selected_count": 30, "truncated": False}
        shards = []
        for index in range(4):
            shard, facts = MODULE.select_shard(
                cases,
                base,
                shard_count=4,
                shard_index=index,
                max_files=30,
            )
            shards.append(shard)
            self.assertEqual(facts["shard_index"], index)
            self.assertEqual(facts["shard_count"], 4)
        labels = [case.label for shard in shards for case in shard]
        self.assertEqual(len(labels), len(set(labels)))
        self.assertEqual(set(labels), {case.label for case in cases})

        capped, facts = MODULE.select_shard(
            cases,
            base,
            shard_count=1,
            shard_index=0,
            max_files=7,
        )
        self.assertEqual(len(capped), 7)
        self.assertTrue(facts["truncated"])

    def test_render_corpus_manifest_rejects_absolute_and_escaping_paths(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            manifest = root / "manifest.json"
            base = {
                "schema": "rxls.render-corpus-manifest.v1",
                "files": [
                    {
                        "source_id": "source",
                        "source_path": "book.xlsx",
                        "local_path": str(root / "book.xlsx"),
                        "status": "ready",
                        "eligible": True,
                    }
                ],
            }
            manifest.write_text(json.dumps(base), encoding="utf-8")
            with self.assertRaisesRegex(MODULE.HarnessError, "absolute_local_path"):
                MODULE.discover_manifest(
                    manifest,
                    max_manifest_bytes=16_384,
                    max_candidates=10,
                    max_files=10,
                )

            base["files"][0]["local_path"] = "../book.xlsx"
            manifest.write_text(json.dumps(base), encoding="utf-8")
            with self.assertRaisesRegex(MODULE.HarnessError, "local_path_unsafe"):
                MODULE.discover_manifest(
                    manifest,
                    max_manifest_bytes=16_384,
                    max_candidates=10,
                    max_files=10,
                )

    def test_cli_dry_run_json_never_contains_corpus_absolute_path(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            corpus = root / "secret-corpus"
            corpus.mkdir()
            (corpus / "fixture.xlsx").write_bytes(b"fixture")
            process = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--corpus",
                    str(corpus),
                    "--dry-run",
                    "--rxls-command",
                    "/private/bin/rxls-render",
                    "--libreoffice",
                    "/private/bin/soffice",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            evidence = json.loads(process.stdout)

        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(evidence["schema"], MODULE.EVIDENCE_SCHEMA)
        self.assertNotIn(str(corpus), process.stdout)
        self.assertNotIn("/private/bin", process.stdout)
        self.assertEqual(evidence["files"][0]["path"], "fixture.xlsx")

    def test_unsafe_manifest_labels_are_hashed_not_exposed(self) -> None:
        native = "/redacted-origin/private/payroll.xlsx"
        label = MODULE.normalize_evidence_label(native, suffix=".xlsx")
        self.assertTrue(label.startswith("input-"))
        self.assertTrue(label.endswith(".xlsx"))
        self.assertNotIn("secret", label)
        windows_native = "".join(
            ("Q", ":", chr(92), "redacted-origin", chr(92), "private", chr(92), "payroll.xlsx")
        )
        windows = MODULE.normalize_evidence_label(windows_native, suffix=".xlsx")
        self.assertTrue(windows.startswith("input-"))

    def test_font_pack_identity_is_verified_and_path_neutral(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            manifest, expected_sha = write_font_pack(root / "font-pack")
            pack = MODULE.load_font_pack(manifest)

            self.assertEqual(pack.evidence["pack_sha256"], expected_sha)
            self.assertEqual(pack.evidence["font_count"], 1)
            self.assertNotIn("Fixture Sans", json.dumps(pack.evidence, sort_keys=True))
            self.assertNotIn(str(root), json.dumps(pack.evidence, sort_keys=True))
            self.assertEqual(pack.font_paths[0].name, "FixtureSans-Regular.ttf")

            pack.font_paths[0].write_bytes(b"tampered")
            with self.assertRaisesRegex(MODULE.HarnessError, "font_pack_font_identity"):
                MODULE.load_font_pack(manifest)

    def test_font_pack_derives_exact_pdf_identities_from_authenticated_sfnt_names(
        self,
    ) -> None:
        expected = frozenset(
            {
                "fixturesans",
                "fixturesansregular",
                "fixturesans-regular",
            }
        )
        identities, families, face_count = MODULE._sfnt_pdf_font_identities(
            minimal_sfnt_font()
        )
        self.assertEqual(identities, expected)
        self.assertEqual(families, frozenset({"fixturesans"}))
        self.assertEqual(face_count, 1)

        with tempfile.TemporaryDirectory() as raw:
            manifest, _ = write_font_pack(Path(raw) / "font-pack")
            pack = MODULE.load_font_pack(manifest)
            rendered_evidence = json.dumps(pack.evidence, sort_keys=True)
            full_name_records = MODULE.parse_pdffonts_output(
                pdffonts_payload(
                    [
                        (
                            "BAAAAA+FixtureSansRegular",
                            "TrueType",
                            "WinAnsi",
                            "yes",
                            "yes",
                            "yes",
                            17,
                            0,
                        )
                    ]
                ),
                max_bytes=4096,
            )
            punctuation_variant = MODULE.parse_pdffonts_output(
                pdffonts_payload(
                    [
                        (
                            "BAAAAA+FixtureSans,Regular",
                            "TrueType",
                            "WinAnsi",
                            "yes",
                            "yes",
                            "yes",
                            19,
                            0,
                        )
                    ]
                ),
                max_bytes=4096,
            )

        self.assertEqual(pack.pdf_identities, expected)
        self.assertEqual(pack.evidence["pdf_identity_count"], 3)
        self.assertEqual(
            pack.evidence["pdf_identities_sha256"],
            hashlib.sha256(
                b"fixturesans\nfixturesans-regular\nfixturesansregular\n"
            ).hexdigest(),
        )
        self.assertNotIn("Fixture", rendered_evidence)
        self.assertEqual(
            MODULE.attest_pdf_fonts(full_name_records, pack)[
                "matched_font_objects"
            ],
            1,
        )
        self.assertEqual(
            MODULE.attest_pdf_fonts(punctuation_variant, pack)[
                "matched_font_objects"
            ],
            0,
        )

    def test_sfnt_name_parser_accepts_bounded_encodings_and_ttc_faces(self) -> None:
        localized = minimal_sfnt_font(
            (
                (3, 1, 0x0409, 1, "Fixture Sans"),
                (3, 1, 0x0409, 4, "Fixture Sans Regular"),
                (3, 1, 0x0409, 6, "FixtureSans-Regular"),
                (3, 1, 0x0411, 1, "フィクスチャ サンズ"),
                (3, 1, 0x0411, 4, "フィクスチャ サンズ Regular"),
                (3, 3, 0x0409, 4, "I" * 600),
            )
        )
        identities, families, face_count = MODULE._sfnt_pdf_font_identities(
            localized
        )
        self.assertEqual(
            identities,
            frozenset(
                {
                    "fixturesans",
                    "fixturesansregular",
                    "fixturesans-regular",
                }
            ),
        )
        self.assertEqual(families, frozenset({"fixturesans"}))
        self.assertEqual(face_count, 1)

        mac_font = minimal_sfnt_font(
            (
                (1, 0, 0, 1, "Mac Fixture"),
                (1, 0, 0, 4, "Mac Fixture Regular"),
                (1, 0, 0, 6, "MacFixture-Regular"),
            )
        )
        mac_identities, _, _ = MODULE._sfnt_pdf_font_identities(mac_font)
        self.assertEqual(
            mac_identities,
            frozenset(
                {
                    "macfixture",
                    "macfixtureregular",
                    "macfixture-regular",
                }
            ),
        )

        for encoding_id in (2, 6):
            with self.subTest(windows_encoding_id=encoding_id):
                legacy_identities, _, _ = MODULE._sfnt_pdf_font_identities(
                    minimal_sfnt_font(
                        (
                            (3, encoding_id, 0x0409, 1, "Legacy Fixture"),
                            (
                                3,
                                encoding_id,
                                0x0409,
                                4,
                                "Legacy Fixture Regular",
                            ),
                            (
                                3,
                                encoding_id,
                                0x0409,
                                6,
                                "LegacyFixture-Regular",
                            ),
                        )
                    )
                )
                self.assertIn("legacyfixture-regular", legacy_identities)

        opaque_language_identities, _, _ = MODULE._sfnt_pdf_font_identities(
            minimal_sfnt_font(
                (
                    (3, 1, 0x8000, 1, "Tagged Fixture"),
                    (3, 1, 0x8000, 4, "Tagged Fixture Regular"),
                    (3, 1, 0x8000, 6, "TaggedFixture-Regular"),
                ),
                language_tags=("opaque language tag",),
            )
        )
        self.assertIn("taggedfixture-regular", opaque_language_identities)

        ttc_identities, ttc_families, ttc_face_count = (
            MODULE._sfnt_pdf_font_identities(minimal_ttc_font())
        )
        self.assertEqual(ttc_face_count, 2)
        self.assertEqual(
            ttc_families,
            frozenset({"fixturesans", "secondserif"}),
        )
        self.assertEqual(
            ttc_identities,
            frozenset(
                {
                    "fixturesans",
                    "fixturesansregular",
                    "fixturesans-regular",
                    "secondserif",
                    "secondserifregular",
                    "secondserif-regular",
                }
            ),
        )
        shared_identities, shared_families, shared_face_count = (
            MODULE._sfnt_pdf_font_identities(
                minimal_shared_table_ttc_font()
            )
        )
        self.assertEqual(shared_face_count, 2)
        self.assertEqual(shared_families, frozenset({"fixturesans"}))
        self.assertEqual(
            shared_identities,
            frozenset(
                {
                    "fixturesans",
                    "fixturesansregular",
                    "fixturesans-regular",
                }
            ),
        )

    def test_sfnt_name_parser_rejects_malformed_structures_and_truncation(
        self,
    ) -> None:
        valid = minimal_sfnt_font()

        def rejected(name: str, payload: bytes) -> None:
            with self.subTest(name=name), self.assertRaisesRegex(
                MODULE.HarnessError,
                "font_pack_font_identity",
            ):
                MODULE._sfnt_pdf_font_identities(payload)

        mutations: dict[str, bytes] = {}

        def mutated(start: int, end: int, replacement: bytes) -> bytes:
            payload = bytearray(valid)
            payload[start:end] = replacement
            return bytes(payload)

        mutations["unknown_magic"] = b"wOFF" + valid[4:]
        mutations["zero_tables"] = mutated(4, 6, b"\x00\x00")
        mutations["noncanonical_search"] = mutated(6, 8, b"\x00\x00")
        mutations["unaligned_name_table"] = mutated(
            20, 24, (29).to_bytes(4, "big")
        )
        mutations["unsupported_name_version"] = mutated(
            28, 30, b"\x00\x02"
        )
        mutations["excessive_name_records"] = mutated(
            30, 32, (1025).to_bytes(2, "big")
        )
        mutations["storage_inside_records"] = mutated(
            32, 34, b"\x00\x05"
        )
        mutations["duplicate_record_key"] = mutated(
            52, 54, b"\x00\x01"
        )
        mutations["string_interval_overflow"] = mutated(
            42, 44, b"\xff\xff"
        )
        mutations["odd_utf16"] = mutated(42, 44, b"\x00\x17")
        mutations["missing_postscript_name"] = mutated(
            64, 66, b"\x00\x05"
        )
        for name, payload in mutations.items():
            rejected(name, payload)

        inconsistent_postscript = minimal_sfnt_font(
            (
                (3, 1, 0x0409, 1, "Fixture Sans"),
                (3, 1, 0x0409, 4, "Fixture Sans Regular"),
                (3, 1, 0x0409, 6, "FixtureSans-Regular"),
                (3, 1, 0x040C, 6, "FixtureSans-Other"),
            )
        )
        rejected("inconsistent_postscript", inconsistent_postscript)
        for name, postscript_name in (
            ("postscript_space", "Fixture Sans-Regular"),
            ("postscript_too_long", "A" * 64),
            ("postscript_forbidden_delimiter", "Fixture/Sans-Regular"),
        ):
            rejected(
                name,
                minimal_sfnt_font(
                    (
                        (3, 1, 0x0409, 1, "Fixture Sans"),
                        (3, 1, 0x0409, 4, "Fixture Sans Regular"),
                        (3, 1, 0x0409, 6, postscript_name),
                    )
                ),
            )

        punctuation_identities, _, _ = MODULE._sfnt_pdf_font_identities(
            minimal_sfnt_font(
                (
                    (3, 1, 0x0409, 1, "Fixture Sans"),
                    (3, 1, 0x0409, 4, "Fixture Sans Regular"),
                    (3, 1, 0x0409, 6, "Fixture!Sans-Regular"),
                )
            )
        )
        self.assertIn("fixture!sans-regular", punctuation_identities)

        for name, language_tag in (
            ("empty_language_tag", ""),
            ("oversized_language_tag", "A" * 257),
            ("nonprintable_language_tag", "en\nUS"),
            ("padded_language_tag", " en-US"),
        ):
            rejected(
                name,
                minimal_sfnt_font(
                    (
                        (3, 1, 0x8000, 1, "Fixture Sans"),
                        (3, 1, 0x8000, 4, "Fixture Sans Regular"),
                        (3, 1, 0x8000, 6, "FixtureSans-Regular"),
                    ),
                    language_tags=(language_tag,),
                ),
            )
        rejected(
            "dangling_language_tag_reference",
            minimal_sfnt_font(
                (
                    (3, 1, 0x8001, 1, "Fixture Sans"),
                    (3, 1, 0x8001, 4, "Fixture Sans Regular"),
                    (3, 1, 0x8001, 6, "FixtureSans-Regular"),
                ),
                language_tags=("en-US",),
            ),
        )

        ttc_duplicate_face = bytearray(minimal_ttc_font())
        ttc_duplicate_face[16:20] = ttc_duplicate_face[12:16]
        rejected("ttc_duplicate_face", bytes(ttc_duplicate_face))

        required_bytes = (
            int.from_bytes(valid[20:24], "big")
            + int.from_bytes(valid[24:28], "big")
        )
        for length in range(required_bytes):
            rejected(f"truncated_at_{length}", valid[:length])

    def test_dry_run_records_verified_font_pack_without_host_paths(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            corpus = root / "corpus"
            corpus.mkdir()
            (corpus / "fixture.xlsx").write_bytes(b"fixture")
            manifest, expected_sha = write_font_pack(root / "font-pack")
            process = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--corpus",
                    str(corpus),
                    "--dry-run",
                    "--font-pack-manifest",
                    str(manifest),
                    "--require-font-pack",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            evidence = json.loads(process.stdout)

        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(
            evidence["configuration"]["font_pack"]["pack_sha256"], expected_sha
        )
        self.assertEqual(
            evidence["configuration"]["font_pack"],
            evidence["preflight"]["font_pack"],
        )
        self.assertIs(
            evidence["configuration"]["font_pack"]["attestation_required"],
            True,
        )
        self.assertIs(evidence["configuration"]["font_pack"]["configured"], True)
        self.assertTrue(evidence["preflight"]["font_pack"]["configured"])
        self.assertIn(
            "<font-pack-manifest>",
            evidence["files"][0]["planned_commands"]["rxls"],
        )
        self.assertNotIn(str(root), process.stdout)

    def test_required_font_pack_is_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            corpus = Path(raw)
            (corpus / "fixture.xlsx").write_bytes(b"fixture")
            process = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--corpus",
                    str(corpus),
                    "--dry-run",
                    "--require-font-pack",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(process.returncode, 2)
        self.assertIn("font_pack_required", process.stderr)

    def test_pdffonts_parser_and_attestation_are_bounded_and_content_private(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            manifest, _ = write_font_pack(Path(raw) / "font-pack")
            pack = MODULE.load_font_pack(manifest)
            payload = pdffonts_payload(
                [
                    (
                        "BAAAAA+FixtureSans-Regular",
                        "TrueType",
                        "WinAnsi",
                        "yes",
                        "yes",
                        "yes",
                        7,
                        0,
                    )
                ]
            )
            records = MODULE.parse_pdffonts_output(
                payload,
                max_bytes=len(payload),
                max_fonts=1,
            )
            evidence = MODULE.attest_pdf_fonts(records, pack)

        self.assertEqual(evidence["font_objects"], 1)
        self.assertEqual(evidence["matched_font_objects"], 1)
        self.assertEqual(evidence["embedded_font_objects"], 1)
        self.assertEqual(evidence["subset_font_objects"], 1)
        self.assertEqual(evidence["unicode_font_objects"], 1)
        self.assertNotIn("Fixture", json.dumps(evidence, sort_keys=True))
        with self.assertRaisesRegex(MODULE.HarnessError, "output_contract"):
            MODULE.parse_pdffonts_output(payload, max_bytes=len(payload) - 1)
        with self.assertRaisesRegex(MODULE.HarnessError, "output_contract"):
            MODULE.parse_pdffonts_output(payload, max_bytes=len(payload), max_fonts=0)

    def test_pdffonts_parser_attests_legal_plus_in_postscript_base_name(
        self,
    ) -> None:
        font = minimal_sfnt_font(
            (
                (3, 1, 0x0409, 1, "Fixture Plus"),
                (3, 1, 0x0409, 4, "Fixture Plus Regular"),
                (3, 1, 0x0409, 6, "Fixture+Sans-Regular"),
            )
        )
        with tempfile.TemporaryDirectory() as raw:
            manifest, _ = write_font_pack(
                Path(raw) / "font-pack",
                font=font,
                family="Fixture Plus",
                filename="FixturePlus-Regular.ttf",
            )
            pack = MODULE.load_font_pack(manifest)
            subset_payload = pdffonts_payload(
                [
                    (
                        "BAAAAA+Fixture+Sans-Regular",
                        "TrueType",
                        "WinAnsi",
                        "yes",
                        "yes",
                        "yes",
                        17,
                        0,
                    )
                ]
            )
            subset_records = MODULE.parse_pdffonts_output(
                subset_payload,
                max_bytes=len(subset_payload),
            )
            plain_payload = pdffonts_payload(
                [
                    (
                        "Fixture+Sans-Regular",
                        "TrueType",
                        "WinAnsi",
                        "yes",
                        "no",
                        "yes",
                        18,
                        0,
                    )
                ]
            )
            plain_records = MODULE.parse_pdffonts_output(
                plain_payload,
                max_bytes=len(plain_payload),
            )

        self.assertEqual(
            subset_records[0].normalized_identity,
            "fixture+sans-regular",
        )
        self.assertEqual(
            plain_records[0].normalized_identity,
            "fixture+sans-regular",
        )
        self.assertEqual(
            MODULE.attest_pdf_fonts(subset_records, pack)[
                "matched_font_objects"
            ],
            1,
        )

    def test_pdffonts_parser_rejects_adversarial_fixed_table_variants(self) -> None:
        valid = pdffonts_payload(
            [
                (
                    "BAAAAA+FixtureSans-Regular",
                    "TrueType",
                    "Identity-H",
                    "yes",
                    "yes",
                    "yes",
                    9,
                    0,
                )
            ]
        )
        invalid = {
            "invalid_utf8": valid[:-1] + b"\xff\n",
            "carriage_return": valid.replace(b"\n", b"\r\n", 1),
            "missing_newline": valid.rstrip(b"\n"),
            "header": valid.replace(b"name ", b"font ", 1),
            "column_shift": valid.replace(b" TrueType", b"  TrueType", 1),
            "unknown_type": valid.replace(b"TrueType", b"Unknown!", 1),
            "invalid_flag": valid.replace(b"yes yes yes", b"YES yes yes", 1),
            "missing_subset_prefix": valid.replace(b"BAAAAA+", b"       ", 1),
        }
        duplicate = valid + valid.splitlines(keepends=True)[2]
        invalid["duplicate_object"] = duplicate
        for name, payload in invalid.items():
            with self.subTest(name=name), self.assertRaises(MODULE.HarnessError):
                MODULE.parse_pdffonts_output(payload, max_bytes=4096)

        empty = pdffonts_payload()
        self.assertEqual(
            MODULE.parse_pdffonts_output(empty, max_bytes=len(empty), max_fonts=0),
            (),
        )

    def test_pdf_font_attestation_requires_embedded_subset_unicode_pack_fonts(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            manifest, _ = write_font_pack(Path(raw) / "font-pack")
            pack = MODULE.load_font_pack(manifest)
            cases = (
                ("no", "yes", "yes", "not_embedded", "BAAAAA+FixtureSans-Regular"),
                ("yes", "no", "yes", "not_subset", "FixtureSans-Regular"),
                ("yes", "yes", "no", "unicode_map_missing", "BAAAAA+FixtureSans-Regular"),
            )
            for embedded, subset, unicode_map, error, name in cases:
                payload = pdffonts_payload(
                    [
                        (
                            name,
                            "TrueType",
                            "WinAnsi",
                            embedded,
                            subset,
                            unicode_map,
                            11,
                            0,
                        )
                    ]
                )
                records = MODULE.parse_pdffonts_output(payload, max_bytes=4096)
                with self.subTest(error=error), self.assertRaisesRegex(
                    MODULE.HarnessError, error
                ):
                    MODULE.attest_pdf_fonts(records, pack)

    def test_required_font_attestation_classifies_macos_style_fallback_privately(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            manifest, pack_sha = write_font_pack(root / "font-pack")
            font_pack = MODULE.load_font_pack(manifest)
            runner = FontMismatchRunner(source, pack_sha)
            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice="soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=False,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
                require_font_pack=True,
                font_pack=font_pack,
            )
            with (
                mock.patch.dict(os.environ, {"PDFFONTS": "pdffonts"}),
                mock.patch.object(MODULE.sys, "platform", "linux"),
            ):
                evidence, exit_code = MODULE.run_harness(
                    [MODULE.InputCase(source, "fixture.xlsx", 7)],
                    discovery={
                        "candidate_count": 1,
                        "selected_count": 1,
                        "truncated": False,
                    },
                    config=config,
                    backends=MODULE.Backends(
                        False, False, False, pdffonts=True
                    ),
                    runner=runner,
                )
            rendered = json.dumps(evidence, sort_keys=True)

        self.assertEqual(exit_code, 1)
        result = evidence["files"][0]
        self.assertEqual(result["status"], "error")
        self.assertEqual(result["classification"], "libreoffice_font_pack_mismatch")
        self.assertEqual(result["font_attestation"]["font_objects"], 1)
        self.assertEqual(result["font_attestation"]["matched_font_objects"], 0)
        self.assertNotIn("LiberationSans", rendered)
        self.assertNotIn(str(root), rendered)

    def test_direct_non_linux_strict_font_activation_is_rejected_before_rendering(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "fixture.xlsx"
            source.write_bytes(b"fixture")
            manifest, _ = write_font_pack(root / "font-pack")
            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice="soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=False,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
                require_font_pack=True,
                font_pack=MODULE.load_font_pack(manifest),
            )
            with (
                mock.patch.object(MODULE.sys, "platform", "darwin"),
                self.assertRaisesRegex(
                    MODULE.HarnessError,
                    "font_pack_direct_activation_unsupported",
                ),
            ):
                MODULE.run_harness(
                    [MODULE.InputCase(source, "fixture.xlsx", 7)],
                    discovery={
                        "candidate_count": 1,
                        "selected_count": 1,
                        "truncated": False,
                    },
                    config=config,
                    runner=NoCallRunner(),
                )

    def test_oracle_lock_verifies_every_active_tool_and_remains_path_neutral(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            manifest, pack_sha = write_font_pack(root / "font-pack")
            font_pack = MODULE.load_font_pack(manifest)
            executables = []
            for name in (
                "lo-fixture",
                "pdfinfo-fixture",
                "pdftoppm-fixture",
                "pdftotext-fixture",
                "pdffonts-fixture",
            ):
                path = root / name
                path.write_text(f"fixture executable {name}\n")
                path.chmod(0o755)
                executables.append(path)
            lock = write_oracle_lock(
                root,
                libreoffice=executables[0],
                pdfinfo=executables[1],
                pdftoppm=executables[2],
                pdftotext=executables[3],
                pdffonts=executables[4],
                font_pack_sha256=pack_sha,
            )
            profile = MODULE.load_oracle_profile(lock, None)
            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice=str(executables[0]),
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=True,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
                font_pack=font_pack,
                oracle_profile=profile,
            )
            backends = MODULE.Backends(
                pillow=True,
                pymupdf=True,
                cairosvg=True,
                pdftoppm=True,
                pdfinfo=True,
                pdftotext=True,
                pdffonts=True,
            )
            with mock.patch.dict(
                os.environ,
                {
                    "PDFINFO": str(executables[1]),
                    "PDFTOPPM": str(executables[2]),
                    "PDFTOTEXT": str(executables[3]),
                    "PDFFONTS": str(executables[4]),
                },
            ):
                evidence = MODULE.verify_oracle_profile(
                    profile,
                    config=config,
                    backends=backends,
                    runner=OracleVersionRunner(),
                )
                executables[4].write_text("tampered pdffonts\n")
                with self.assertRaisesRegex(
                    MODULE.HarnessError, "oracle_pdffonts_identity"
                ):
                    MODULE.verify_oracle_profile(
                        profile,
                        config=config,
                        backends=backends,
                        runner=OracleVersionRunner(),
                    )
            rendered = json.dumps(evidence, sort_keys=True)

        self.assertEqual(evidence["profile"], "fixture-oracle")
        self.assertEqual(evidence["font_pack_sha256"], pack_sha)
        self.assertNotIn(str(root), rendered)
        self.assertEqual(evidence["pdf_rasterizer"]["kind"], "poppler")
        self.assertEqual(
            evidence["pdf_rasterizer"]["pdffonts_version"], "pdffonts fixture"
        )
        self.assertEqual(
            evidence["pdf_rasterizer"]["pdffonts_sha256"],
            profile.pdffonts_sha256,
        )

    def test_oracle_lock_is_fail_closed_for_drift_and_required_cli(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            corpus = root / "corpus"
            corpus.mkdir()
            (corpus / "fixture.xlsx").write_bytes(b"fixture")
            process = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--corpus",
                    str(corpus),
                    "--dry-run",
                    "--require-oracle-lock",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(process.returncode, 2)
        self.assertIn("oracle_lock_required", process.stderr)

        profile = MODULE.OracleProfile(
            name="fixture",
            system=platform.system().lower(),
            machine=platform.machine().lower(),
            locale="C.UTF-8",
            timezone="UTC",
            dpi=97,
            pdf_filter=MODULE.PDF_FILTER,
            profile_sha256="0" * 64,
            font_pack_sha256="0" * 64,
            libreoffice_version="fixture",
            libreoffice_sha256="0" * 64,
            python_version=platform.python_version(),
            python_executable_sha256="0" * 64,
            numpy_version=importlib.metadata.version("numpy"),
            pillow_version=importlib.metadata.version("Pillow"),
            cairosvg_version=importlib.metadata.version("CairoSVG"),
            pdfinfo_version="fixture",
            pdfinfo_sha256="0" * 64,
            pdftoppm_version="fixture",
            pdftoppm_sha256="0" * 64,
            pdftotext_version="fixture",
            pdftotext_sha256="0" * 64,
            pdffonts_version="fixture",
            pdffonts_sha256="0" * 64,
            source_evidence={},
        )
        config = MODULE.HarnessConfig(
            rxls_command=("rxls-render",),
            libreoffice="soffice",
            svg_rasterizer_command=None,
            caps=MODULE.Caps(),
            dpi=96,
            locale="C.UTF-8",
            dry_run=True,
            min_similarity_ppm=None,
            fail_on_incomparable=False,
            oracle_profile=profile,
        )
        with self.assertRaisesRegex(MODULE.HarnessError, "configuration_mismatch"):
            MODULE.verify_oracle_profile(
                profile,
                config=config,
                backends=MODULE.Backends(
                    True,
                    False,
                    True,
                    pdftoppm=True,
                    pdfinfo=True,
                    pdftotext=True,
                ),
                runner=NoCallRunner(),
            )

    def test_pdfinfo_parser_requires_every_page_size_for_bounded_rasterization(self) -> None:
        text = """Creator: Calc
Pages:           2
Page    1 size:  612 x 792 pts
Page    2 size:  841.89 x 595.276 pts
"""
        pages, sizes = MODULE.parse_pdfinfo(text, require_all_sizes=True)
        self.assertEqual(pages, 2)
        self.assertEqual(sizes[0], (MODULE.Fraction(612), MODULE.Fraction(792)))
        with self.assertRaisesRegex(MODULE.HarnessError, "page_sizes_missing"):
            MODULE.parse_pdfinfo(
                "Pages: 2\nPage size: 612 x 792 pts\n",
                require_all_sizes=True,
            )

    def test_pdfinfo_preserves_exact_numbered_media_and_crop_boxes(self) -> None:
        geometries = MODULE.parse_pdfinfo_page_geometries(
            """Pages: 2
Page    1 size:  612.125 x 792.25 pts
Page    1 MediaBox: 10.5 20.25 622.625 812.5
Page    1 CropBox: 11.125 21.5 621.375 811.75
Page    2 size:  841.89 x 595.276 pts
Page    2 MediaBox: -1.11 -2.222 840.78 593.054
Page    2 CropBox: 0 0 841.125 595.0625
""",
            expected_pages=2,
        )
        self.assertEqual(
            geometries[0],
            MODULE.PdfPageGeometry(
                MODULE.Fraction("612.125"),
                MODULE.Fraction("792.25"),
                MODULE.Fraction("612.125"),
                MODULE.Fraction("792.25"),
                MODULE.Fraction("610.25"),
                MODULE.Fraction("790.25"),
            ),
        )
        self.assertEqual(
            geometries[1],
            MODULE.PdfPageGeometry(
                MODULE.Fraction("841.89"),
                MODULE.Fraction("595.276"),
                MODULE.Fraction("841.89"),
                MODULE.Fraction("595.276"),
                MODULE.Fraction("841.125"),
                MODULE.Fraction("595.0625"),
            ),
        )

    def test_pdf_point_crosscheck_allows_only_poppler_display_precision(self) -> None:
        geometry = MODULE.PdfPageGeometry(
            MODULE.Fraction("841.89"),
            MODULE.Fraction("595.276"),
            MODULE.Fraction("841.89"),
            MODULE.Fraction("595.28"),
            MODULE.Fraction("841.89"),
            MODULE.Fraction("595.28"),
        )
        row = {"point_geometry": MODULE._pdf_geometry_evidence(geometry)}
        text_page = MODULE.PdfTextPage(
            MODULE.Fraction("841.889648"),
            MODULE.Fraction("595.275879"),
            (),
            (),
        )
        page = MODULE.pdf_point_geometry_metrics(
            row,
            row,
            text_page,
            text_page,
        )
        aggregate = MODULE._aggregate_pdf_point_geometry([page])
        self.assertEqual(aggregate["pdf_point_geometry_mismatches"], 0)
        self.assertEqual(
            aggregate["max_pdf_point_geometry_delta_millipoints"], 0
        )
        self.assertEqual(
            aggregate["max_pdf_xhtml_crosscheck_delta_micropoints"],
            352,
        )

        outside_precision = MODULE.PdfTextPage(
            MODULE.Fraction("841.888"),
            MODULE.Fraction("595.276"),
            (),
            (),
        )
        page = MODULE.pdf_point_geometry_metrics(
            row,
            row,
            outside_precision,
            outside_precision,
        )
        self.assertEqual(
            MODULE._aggregate_pdf_point_geometry([page])[
                "pdf_point_geometry_mismatches"
            ],
            1,
        )

    def test_svg_glyph_path_bounds_include_exact_curve_extrema(self) -> None:
        line, _ = MODULE._svg_path_bounds("M0 0 L10 0 L10 10 Z")
        quadratic, _ = MODULE._svg_path_bounds("M0 0 Q10 20 20 0")
        cubic, _ = MODULE._svg_path_bounds("M0 0 C0 30 30 30 30 0")
        relative, _ = MODULE._svg_path_bounds("m1 1 h9 v9 h-9 z")
        self.assertEqual(line, (0.0, 0.0, 10.0, 10.0))
        self.assertEqual(quadratic, (0.0, 0.0, 20.0, 10.0))
        self.assertEqual(cubic, (0.0, 0.0, 30.0, 22.5))
        self.assertEqual(relative, (1.0, 1.0, 10.0, 10.0))

    def test_svg_semantic_boxes_are_clipped_scaled_and_content_private(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            svg = Path(raw) / "page.svg"
            svg.write_text(
                """<svg xmlns="http://www.w3.org/2000/svg" width="200px"
                height="100px" viewBox="0 0 100 50">
                <defs><clipPath id="cell"><rect x="5" y="5" width="20"
                height="10"/></clipPath></defs>
                <g role="text" aria-label="private label hidden source"
                data-rxls-visible-label="private label" clip-path="url(#cell)">
                <path d="M0 0 L50 0 L50 30 L0 30 Z"/></g></svg>""",
                encoding="utf-8",
            )
            evidence = MODULE.extract_svg_semantic_evidence(
                svg,
                max_svg_bytes=4096,
                max_codepoints=64,
                max_tokens=8,
            )
        self.assertEqual(evidence.tokens, ("private", "label"))
        self.assertEqual(evidence.boxes[0].tokens, ("private", "label"))
        self.assertEqual(evidence.unbounded_items, 0)
        self.assertEqual(
            evidence.boxes[0].bbox_points,
            (7.5, 7.5, 37.5, 22.5),
        )

    def test_svg_semantic_box_parser_rejects_unsafe_or_unbounded_grammar(self) -> None:
        with self.assertRaisesRegex(MODULE.HarnessError, "path_command"):
            MODULE._svg_path_bounds("M0 0 A10 10 0 0 0 20 20")
        with self.assertRaisesRegex(MODULE.HarnessError, "path_syntax"):
            MODULE._svg_path_bounds("M0,,0 L1 1")
        with self.assertRaisesRegex(MODULE.HarnessError, "path_token_limit"):
            MODULE._svg_path_bounds("M0 0 L1 1", max_tokens=4)

        fixtures = {
            "transform": """<svg width="10" height="10" viewBox="0 0 10 10">
                <g role="text" aria-label="x" data-rxls-visible-label="x"
                transform="rotate(1)">
                <path d="M0 0 L1 1"/></g></svg>""",
            "entity": """<!DOCTYPE svg [<!ENTITY x "x">]><svg width="10"
                height="10" viewBox="0 0 10 10"/>""",
            "clip": """<svg width="10" height="10" viewBox="0 0 10 10">
                <g role="text" aria-label="x" data-rxls-visible-label="x"
                clip-path="url(#missing)">
                <path d="M0 0 L1 0 L1 1 Z"/></g></svg>""",
        }
        expected = {
            "transform": "text_transform",
            "entity": "unsafe_markup",
            "clip": "clip_reference",
        }
        with tempfile.TemporaryDirectory() as raw:
            for name, payload in fixtures.items():
                path = Path(raw) / f"{name}.svg"
                path.write_text(payload, encoding="utf-8")
                with self.subTest(name=name), self.assertRaisesRegex(
                    MODULE.HarnessError, expected[name]
                ):
                    MODULE.extract_svg_semantic_evidence(
                        path,
                        max_svg_bytes=4096,
                        max_codepoints=16,
                        max_tokens=4,
                    )

    def test_pdftotext_bbox_parser_is_bounded_and_fail_closed(self) -> None:
        payload = b"""<html xmlns="http://www.w3.org/1999/xhtml"><body><doc>
            <page width="200" height="100"><flow><block>
            <line xMin="10" yMin="20" xMax="60" yMax="40">
            <word xMin="10" yMin="20" xMax="30" yMax="40">A</word>
            <word xMin="31" yMin="20" xMax="60" yMax="40">B C</word>
            </line></block></flow></page></doc></body></html>"""
        pages = MODULE.parse_pdftotext_bbox_pages(
            payload,
            expected_pages=1,
            max_bytes=4096,
            max_codepoints=16,
            max_tokens=4,
        )
        self.assertEqual(pages[0].width_points, 200.0)
        self.assertEqual(pages[0].words[1].tokens, ("B", "C"))
        self.assertEqual(pages[0].lines[0].tokens, ("A", "B", "C"))

        invalid_payloads = (
            payload.replace(b'xMin="10"', b'xMin="40"'),
            b'<!DOCTYPE html [<!ENTITY x "boom">]><html/>',
            b'<!DOCTYPE html SYSTEM "local.dtd"><html/>',
        )
        for invalid in invalid_payloads:
            with self.subTest(payload=invalid[:40]), self.assertRaises(
                MODULE.HarnessError
            ):
                MODULE.parse_pdftotext_bbox_pages(
                    invalid,
                    expected_pages=1,
                    max_bytes=4096,
                    max_codepoints=16,
                    max_tokens=4,
                )
        with self.assertRaisesRegex(MODULE.HarnessError, "page_count"):
            MODULE.parse_pdftotext_bbox_pages(
                payload,
                expected_pages=2,
                max_bytes=4096,
                max_codepoints=16,
                max_tokens=4,
            )
        with self.assertRaisesRegex(MODULE.HarnessError, "output_limit"):
            MODULE.parse_pdftotext_bbox_pages(
                payload,
                expected_pages=1,
                max_bytes=16,
                max_codepoints=16,
                max_tokens=4,
            )

    def test_pdftotext_bbox_parser_clamps_overhang_larger_than_six_points(
        self,
    ) -> None:
        payload = b"""<html><body><doc><page width="200" height="100">
            <flow><block><line xMin="-125.5" yMin="-75.25"
            xMax="325.5" yMax="175.25"><word xMin="-125.5"
            yMin="-75.25" xMax="325.5" yMax="175.25">A</word>
            </line></block></flow></page></doc></body></html>"""
        pages = MODULE.parse_pdftotext_bbox_pages(
            payload,
            expected_pages=1,
            max_bytes=4096,
            max_codepoints=16,
            max_tokens=4,
        )
        self.assertEqual(pages[0].words[0].bbox_points, (0.0, 0.0, 200.0, 100.0))
        self.assertEqual(pages[0].lines[0].bbox_points, (0.0, 0.0, 200.0, 100.0))

    def test_pdftotext_bbox_parser_rejects_fully_outside_boxes(self) -> None:
        coordinates = (
            ('xMin="-30" yMin="20" xMax="0" yMax="40"'),
            ('xMin="200" yMin="20" xMax="230" yMax="40"'),
            ('xMin="10" yMin="-30" xMax="30" yMax="0"'),
            ('xMin="10" yMin="100" xMax="30" yMax="130"'),
        )
        for box in coordinates:
            payload = (
                '<html><body><doc><page width="200" height="100">'
                f"<flow><block><line {box}><word {box}>A</word></line>"
                "</block></flow></page></doc></body></html>"
            ).encode()
            with self.subTest(box=box), self.assertRaisesRegex(
                MODULE.HarnessError, "semantic_bbox_word_geometry"
            ):
                MODULE.parse_pdftotext_bbox_pages(
                    payload,
                    expected_pages=1,
                    max_bytes=4096,
                    max_codepoints=16,
                    max_tokens=4,
                )

    def test_pdftotext_bbox_parser_rejects_non_finite_coordinates(self) -> None:
        for coordinate in ("NaN", "Inf", "-Inf"):
            payload = (
                '<html><body><doc><page width="200" height="100">'
                '<flow><block><line xMin="10" yMin="20" xMax="30" yMax="40">'
                f'<word xMin="10" yMin="20" xMax="{coordinate}" '
                'yMax="40">A</word></line></block></flow></page></doc>'
                "</body></html>"
            ).encode()
            with self.subTest(coordinate=coordinate), self.assertRaisesRegex(
                MODULE.HarnessError, "semantic_bbox_word_geometry"
            ):
                MODULE.parse_pdftotext_bbox_pages(
                    payload,
                    expected_pages=1,
                    max_bytes=4096,
                    max_codepoints=16,
                    max_tokens=4,
                )

    def test_pdftotext_bbox_parser_rejects_absurd_coordinates(self) -> None:
        for x_min, x_max in (
            ("-1000000.001", "30"),
            ("10", "1000000.001"),
        ):
            payload = (
                '<html><body><doc><page width="200" height="100">'
                f'<flow><block><line xMin="{x_min}" yMin="20" '
                f'xMax="{x_max}" yMax="40"><word xMin="{x_min}" '
                f'yMin="20" xMax="{x_max}" yMax="40">A</word></line>'
                "</block></flow></page></doc></body></html>"
            ).encode()
            with self.subTest(x_min=x_min, x_max=x_max), self.assertRaisesRegex(
                MODULE.HarnessError, "semantic_bbox_word_geometry"
            ):
                MODULE.parse_pdftotext_bbox_pages(
                    payload,
                    expected_pages=1,
                    max_bytes=4096,
                    max_codepoints=16,
                    max_tokens=4,
                )

    def test_pdftotext_bbox_streaming_caps_elements_lines_and_words(self) -> None:
        empty_elements = (
            b'<html><body><doc><page width="10" height="10">'
            + b"<empty/>" * 8
            + b"</page></doc></body></html>"
        )
        with self.assertRaisesRegex(MODULE.HarnessError, "element_limit"):
            MODULE.parse_pdftotext_bbox_pages(
                empty_elements,
                expected_pages=1,
                max_bytes=4096,
                max_codepoints=64,
                max_tokens=16,
                max_elements=6,
                max_lines=16,
                max_words=16,
            )

        lines = b"""<html><body><page width="10" height="10">
            <line xMin="0" yMin="0" xMax="1" yMax="1"></line>
            <line xMin="1" yMin="1" xMax="2" yMax="2"></line>
            </page></body></html>"""
        with self.assertRaisesRegex(MODULE.HarnessError, "line_limit"):
            MODULE.parse_pdftotext_bbox_pages(
                lines,
                expected_pages=1,
                max_bytes=4096,
                max_codepoints=64,
                max_tokens=16,
                max_elements=64,
                max_lines=1,
                max_words=16,
            )

        words = b"""<html><body><page width="10" height="10">
            <line xMin="0" yMin="0" xMax="4" yMax="2">
            <word xMin="0" yMin="0" xMax="1" yMax="1">A</word>
            <word xMin="1" yMin="0" xMax="2" yMax="1">B</word>
            </line></page></body></html>"""
        with self.assertRaisesRegex(MODULE.HarnessError, "word_limit"):
            MODULE.parse_pdftotext_bbox_pages(
                words,
                expected_pages=1,
                max_bytes=4096,
                max_codepoints=64,
                max_tokens=16,
                max_elements=64,
                max_lines=4,
                max_words=1,
            )

    def test_text_box_matching_is_exact_unique_and_ambiguity_closed(self) -> None:
        libreoffice = MODULE.PdfTextPage(
            200.0,
            100.0,
            (
                MODULE.SemanticTextBox(("A",), (0.0, 0.0, 10.0, 10.0)),
                MODULE.SemanticTextBox(("A",), (100.0, 0.0, 110.0, 10.0)),
            ),
        )
        exact = MODULE.PdfTextPage(
            200.0,
            100.0,
            (
                MODULE.SemanticTextBox(("A",), (100.0, 0.0, 110.0, 10.0)),
                MODULE.SemanticTextBox(("A",), (0.0, 0.0, 10.0, 10.0)),
            ),
        )
        exact_metrics = MODULE.text_box_metrics(exact, libreoffice)
        self.assertEqual(exact_metrics["text_box_matched_items"], 2)
        self.assertEqual(exact_metrics["text_box_match_coverage_ppm"], 1_000_000)
        extra_reference = MODULE.PdfTextPage(
            200.0,
            100.0,
            (
                *libreoffice.words,
                MODULE.SemanticTextBox(("B",), (20.0, 0.0, 30.0, 10.0)),
            ),
        )
        extra_metrics = MODULE.text_box_metrics(exact, extra_reference)
        self.assertEqual(extra_metrics["text_box_precision_ppm"], 1_000_000)
        self.assertEqual(extra_metrics["text_box_recall_ppm"], 666_667)
        self.assertEqual(extra_metrics["text_box_f1_ppm"], 800_000)
        self.assertEqual(
            extra_metrics["text_box_libreoffice_unmatched_items"],
            1,
        )

        split = MODULE.PdfTextPage(
            200.0,
            100.0,
            (
                MODULE.SemanticTextBox(("A",), (0.0, 0.0, 10.0, 10.0)),
                MODULE.SemanticTextBox(("B",), (11.0, 0.0, 20.0, 10.0)),
            ),
        )
        joined = MODULE.PdfTextPage(
            200.0,
            100.0,
            (
                MODULE.SemanticTextBox(
                    ("A", "B"),
                    (0.0, 0.0, 20.0, 10.0),
                ),
            ),
        )
        split_metrics = MODULE.text_box_metrics(joined, split)
        self.assertEqual(split_metrics["text_box_matched_items"], 0)
        self.assertEqual(split_metrics["text_box_unmatched_items"], 1)

        ambiguous = MODULE.PdfTextPage(
            200.0,
            100.0,
            (
                MODULE.SemanticTextBox(("A",), (50.0, 0.0, 60.0, 10.0)),
                MODULE.SemanticTextBox(("B",), (50.0, 20.0, 60.0, 30.0)),
            ),
        )
        ambiguous_metrics = MODULE.text_box_metrics(ambiguous, libreoffice)
        self.assertEqual(ambiguous_metrics["text_box_ambiguous_items"], 1)
        self.assertEqual(ambiguous_metrics["text_box_unmatched_items"], 1)
        self.assertEqual(ambiguous_metrics["text_box_matched_items"], 0)
        with self.assertRaisesRegex(MODULE.HarnessError, "work_limit"):
            MODULE.text_box_metrics(exact, libreoffice, max_match_work=1)

        rxls_lines = MODULE.PdfTextPage(
            200.0,
            100.0,
            (),
            (MODULE.SemanticTextBox(("A",), (0.0, 0.0, 10.0, 10.0)),),
        )
        libreoffice_lines = MODULE.PdfTextPage(
            200.0,
            100.0,
            (),
            (MODULE.SemanticTextBox(("A",), (0.0, 0.0, 10.0, 10.0)),),
        )
        line_metrics = MODULE.text_line_box_metrics(
            rxls_lines, libreoffice_lines
        )
        self.assertEqual(
            line_metrics["text_line_box_match_coverage_ppm"],
            1_000_000,
        )

    def test_aggregate_text_box_histogram_derives_exact_quantiles(self) -> None:
        pages = []
        for error in (100, 2000):
            pages.append(
                {
                    "pixels": 1,
                    "changed_pixels": 0,
                    "absolute_error_sum": 0,
                    "squared_error_sum": 0,
                    "max_channel_delta": 0,
                    **MODULE._text_box_numeric_evidence(
                        1,
                        1,
                        1,
                        0,
                        0,
                        0,
                        MODULE.Counter({error: 1}),
                    ),
                }
            )
        aggregate = MODULE.aggregate_page_metrics(pages)
        self.assertEqual(aggregate["text_box_candidate_items"], 2)
        self.assertEqual(aggregate["text_box_median_error_millipoints"], 100)
        self.assertEqual(aggregate["text_box_p95_error_millipoints"], 2000)

    def test_manifest_binding_is_exact_for_input_set_and_feature_mapping(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            manifest = root / "manifest.json"
            manifest.write_text('{"files":[]}\n', encoding="utf-8")
            cases = [
                MODULE.InputCase(
                    root / "a.xlsx",
                    "a.xlsx",
                    1,
                    "1" * 64,
                    1,
                    features=("basic",),
                ),
                MODULE.InputCase(
                    root / "b.ods",
                    "b.ods",
                    1,
                    "2" * 64,
                    1,
                    features=("wrapped-text",),
                ),
            ]
            binding = MODULE.build_manifest_binding(
                manifest,
                cases,
                max_manifest_bytes=1024,
            )
            altered = MODULE.build_manifest_binding(
                manifest,
                [
                    cases[0],
                    MODULE.InputCase(
                        root / "b.ods",
                        "b.ods",
                        1,
                        "2" * 64,
                        1,
                        features=("chart",),
                    ),
                ],
                max_manifest_bytes=1024,
            )
        self.assertEqual(binding["selected_case_count"], 2)
        self.assertNotEqual(
            binding["feature_map_sha256"],
            altered["feature_map_sha256"],
        )
        self.assertEqual(
            binding["input_set_sha256"],
            altered["input_set_sha256"],
        )

    def test_ooxml_row_diagnostic_manifest_selects_one_unsharded_xlsx_lane(
        self,
    ) -> None:
        generator = __import__("runpy").run_path(
            str(ROOT / "scripts" / "generate-ooxml-row-oracle.py")
        )
        (ROOT / "local").mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(
            prefix="ooxml-row-harness-", dir=ROOT / "local"
        ) as raw:
            output = Path(raw) / "corpus"
            generator["generate"](output)
            manifest = output / "manifest.json"
            cases, discovery = MODULE.discover_manifest(
                manifest,
                max_manifest_bytes=256 * 1024,
                max_candidates=12,
                max_files=12,
            )
            filtered, discovery = MODULE.filter_cases(
                cases,
                discovery,
                formats=("xlsx",),
                required_features=("ooxml-implicit-row",),
            )
            selected, discovery = MODULE.select_shard(
                filtered,
                discovery,
                shard_count=1,
                shard_index=0,
                max_files=12,
            )
            binding = MODULE.build_manifest_binding(
                manifest,
                selected,
                max_manifest_bytes=256 * 1024,
            )
        self.assertEqual(len(selected), 12)
        self.assertTrue(
            all(
                case.path.suffix == ".xlsx"
                and "ooxml-implicit-row" in case.features
                for case in selected
            )
        )
        self.assertEqual(
            discovery,
            {
                "candidate_count": 12,
                "pre_shard_selected_count": 12,
                "selected_count": 12,
                "shard_candidate_count": 12,
                "shard_count": 1,
                "shard_index": 0,
                "truncated": False,
            },
        )
        self.assertEqual(binding["selected_case_count"], 12)
        self.assertEqual(
            binding["manifest_sha256"],
            "c94f37252d4f78e5352299b831d2620be39178c676b145cda7d076f7d3d09e8a",
        )

    def test_each_case_workspace_is_removed_before_the_next_case(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            sources = []
            for index in range(3):
                source = root / f"{index}.xlsx"
                source.write_bytes(b"x")
                sources.append(MODULE.InputCase(source, source.name, 1))
            observed: list[Path] = []

            def fake_evaluate(
                case,
                *,
                index,
                work_root,
                config,
                backends,
                runner,
            ):
                self.assertTrue(work_root.is_dir())
                self.assertTrue(all(not path.exists() for path in observed))
                (work_root / "bounded-artifact").write_bytes(b"x")
                observed.append(work_root)
                return {
                    "classification": "within_threshold",
                    "format": "xlsx",
                    "path": case.label,
                    "status": "compared",
                }

            config = MODULE.HarnessConfig(
                rxls_command=("rxls-render",),
                libreoffice="soffice",
                svg_rasterizer_command=None,
                caps=MODULE.Caps(),
                dpi=96,
                locale="C.UTF-8",
                dry_run=False,
                min_similarity_ppm=None,
                fail_on_incomparable=False,
            )
            with mock.patch.object(MODULE, "evaluate_case", side_effect=fake_evaluate):
                MODULE.run_harness(
                    sources,
                    discovery={
                        "candidate_count": 3,
                        "selected_count": 3,
                        "truncated": False,
                    },
                    config=config,
                    backends=MODULE.Backends(False, False, False),
                    runner=NoCallRunner(),
                )
            self.assertTrue(all(not path.exists() for path in observed))


if __name__ == "__main__":
    unittest.main()
