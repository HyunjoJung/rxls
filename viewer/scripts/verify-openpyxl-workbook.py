"""Reopen a browser-edited OOXML workbook with a pinned external library."""

from __future__ import annotations

import argparse
from io import BytesIO
import json
from pathlib import Path
import sys
import zipfile

import openpyxl
from openpyxl.utils.exceptions import InvalidFileException


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("workbook", type=Path)
    parser.add_argument("--cell", default="A1")
    parser.add_argument("--expected", required=True)
    parser.add_argument("--expected-title")
    parser.add_argument("--require-vba", action="store_true")
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)

    workbook = None
    try:
        workbook = openpyxl.load_workbook(
            BytesIO(args.workbook.read_bytes()),
            keep_vba=args.require_vba,
            data_only=False,
        )
        value = workbook.active[args.cell].value
        if value != args.expected:
            raise ValueError(f"{args.cell} is {value!r}, expected {args.expected!r}")
        title = workbook.properties.title
        if args.expected_title is not None and title != args.expected_title:
            raise ValueError(f"document title is {title!r}, expected {args.expected_title!r}")
        vba_bytes = None
        if args.require_vba:
            if workbook.vba_archive is None:
                raise ValueError("openpyxl did not retain the VBA package")
            vba = workbook.vba_archive.read("xl/vbaProject.bin")
            if not vba.startswith(bytes.fromhex("d0cf11e0a1b11ae1")):
                raise ValueError("xl/vbaProject.bin is not an OLE compound document")
            vba_bytes = len(vba)
        report = {
            "schema": "rxls.viewer-openpyxl-reopen.v2",
            "openpyxl": openpyxl.__version__,
            "sheets": workbook.sheetnames,
            "cell": args.cell,
            "value": value,
            "title": title,
            "vba_bytes": vba_bytes,
        }
    except (InvalidFileException, KeyError, OSError, ValueError, zipfile.BadZipFile) as error:
        print(f"openpyxl workbook reopen: {error}", file=sys.stderr)
        return 1
    finally:
        if workbook is not None:
            workbook.close()

    print(json.dumps(report, ensure_ascii=True, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
