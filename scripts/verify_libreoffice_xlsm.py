#!/usr/bin/env python3
"""Verify macro preservation around the package-edit and LibreOffice XLSM smoke."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import sys
import xml.etree.ElementTree as ET
import zipfile


MACRO_WORKBOOK_CONTENT_TYPE = "application/vnd.ms-excel.sheet.macroEnabled.main+xml"
VBA_CONTENT_TYPE = "application/vnd.ms-office.vbaProject"
EXPECTED_WARNINGS = ["MacrosPresentNotExecuted"]


@dataclass(frozen=True)
class PackageFacts:
    vba_part: str
    vba_bytes: bytes
    workbook_content_type: str | None
    vba_content_type: str | None


def _normalized_part_name(value: str) -> str:
    return value.replace("\\", "/").lstrip("/").lower()


def inspect_package(path: Path) -> PackageFacts:
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        vba_parts = [
            name
            for name in names
            if _normalized_part_name(name) == "xl/vbaproject.bin"
        ]
        if len(vba_parts) != 1:
            raise ValueError(
                f"{path}: expected exactly one xl/vbaProject.bin, found {len(vba_parts)}"
            )
        try:
            content_types = archive.read("[Content_Types].xml")
        except KeyError as error:
            raise ValueError(f"{path}: missing [Content_Types].xml") from error
        vba_part = vba_parts[0]
        vba_bytes = archive.read(vba_part)

    try:
        root = ET.fromstring(content_types)
    except ET.ParseError as error:
        raise ValueError(f"{path}: malformed [Content_Types].xml: {error}") from error

    defaults: dict[str, str] = {}
    overrides: dict[str, str] = {}
    for element in root:
        local_name = element.tag.rsplit("}", 1)[-1]
        if local_name == "Default":
            extension = element.attrib.get("Extension")
            content_type = element.attrib.get("ContentType")
            if extension and content_type:
                defaults[extension.lower()] = content_type
        elif local_name == "Override":
            part_name = element.attrib.get("PartName")
            content_type = element.attrib.get("ContentType")
            if part_name and content_type:
                overrides[_normalized_part_name(part_name)] = content_type

    def content_type(part_name: str) -> str | None:
        normalized = _normalized_part_name(part_name)
        override = overrides.get(normalized)
        if override is not None:
            return override
        extension = normalized.rsplit(".", 1)[-1] if "." in normalized else ""
        return defaults.get(extension)

    return PackageFacts(
        vba_part=vba_part,
        vba_bytes=vba_bytes,
        workbook_content_type=content_type("xl/workbook.xml"),
        vba_content_type=content_type(vba_part),
    )


def _sha256(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def verify(
    source_path: Path,
    edited_path: Path,
    libreoffice_path: Path,
    report_path: Path,
    version_path: Path,
) -> dict[str, object]:
    source = inspect_package(source_path)
    edited = inspect_package(edited_path)
    libreoffice = inspect_package(libreoffice_path)

    if source.vba_part != edited.vba_part:
        raise ValueError(
            "package edit renamed vbaProject.bin: "
            f"{source.vba_part!r} -> {edited.vba_part!r}"
        )
    if source.vba_bytes != edited.vba_bytes:
        raise ValueError("package edit changed xl/vbaProject.bin bytes")

    for label, facts in (
        ("source", source),
        ("package-edited", edited),
        ("LibreOffice", libreoffice),
    ):
        if facts.workbook_content_type != MACRO_WORKBOOK_CONTENT_TYPE:
            raise ValueError(
                f"{label} workbook content type is {facts.workbook_content_type!r}, "
                f"expected {MACRO_WORKBOOK_CONTENT_TYPE!r}"
            )
        if facts.vba_content_type != VBA_CONTENT_TYPE:
            raise ValueError(
                f"{label} VBA content type is {facts.vba_content_type!r}, "
                f"expected {VBA_CONTENT_TYPE!r}"
            )

    try:
        report = json.loads(report_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"{report_path}: invalid diagnose report: {error}") from error
    warnings = report.get("warnings")
    if warnings != EXPECTED_WARNINGS:
        raise ValueError(
            f"{report_path}: expected warnings {EXPECTED_WARNINGS!r}, got {warnings!r}"
        )
    if report.get("format") != "xlsm":
        raise ValueError(f"{report_path}: expected format 'xlsm'")
    features = report.get("features")
    if not isinstance(features, dict) or features.get("vba_project") is not True:
        raise ValueError(f"{report_path}: expected features.vba_project=true")

    try:
        version_lines = [
            line.strip()
            for line in version_path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
    except OSError as error:
        raise ValueError(f"{version_path}: cannot read LibreOffice version: {error}") from error
    if len(version_lines) != 1 or not version_lines[0].startswith("LibreOffice"):
        raise ValueError(
            f"{version_path}: expected one captured LibreOffice version line"
        )

    return {
        "schema": "rxls.libreoffice-xlsm-preservation.v1",
        "libreoffice_version": version_lines[0],
        "diagnose_warnings": warnings,
        "content_types": {
            "expected_macro_workbook": MACRO_WORKBOOK_CONTENT_TYPE,
            "expected_vba_project": VBA_CONTENT_TYPE,
            "source": {
                "macro_workbook": source.workbook_content_type,
                "vba_project": source.vba_content_type,
            },
            "package_edited": {
                "macro_workbook": edited.workbook_content_type,
                "vba_project": edited.vba_content_type,
            },
            "libreoffice": {
                "macro_workbook": libreoffice.workbook_content_type,
                "vba_project": libreoffice.vba_content_type,
            },
        },
        "vba_project": {
            "part": source.vba_part,
            "bytes": len(source.vba_bytes),
            "source_sha256": _sha256(source.vba_bytes),
            "package_edited_sha256": _sha256(edited.vba_bytes),
            "package_edit_byte_preserved": True,
            "present_after_libreoffice": True,
        },
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="original pinned-corpus XLSM")
    parser.add_argument("edited", type=Path, help="rxls package-edited XLSM")
    parser.add_argument("libreoffice", type=Path, help="LibreOffice-saved XLSM")
    parser.add_argument("report", type=Path, help="rxls diagnose JSON for LibreOffice output")
    parser.add_argument("--version-file", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)

    try:
        evidence = verify(
            args.source,
            args.edited,
            args.libreoffice,
            args.report,
            args.version_file,
        )
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(evidence, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
    except (OSError, ValueError, zipfile.BadZipFile) as error:
        print(f"LibreOffice XLSM smoke: {error}", file=sys.stderr)
        return 1

    print(
        "LibreOffice XLSM smoke: verified exact rxls VBA preservation, "
        "macro content types, LibreOffice VBA retention, and expected warning"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
