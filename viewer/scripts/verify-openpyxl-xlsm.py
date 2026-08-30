"""Reopen a browser-edited XLSM with a pinned external spreadsheet library."""

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
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)

    workbook = None
    try:
        workbook = openpyxl.load_workbook(
            BytesIO(args.workbook.read_bytes()), keep_vba=True, data_only=False
        )
        value = workbook.active[args.cell].value
        if value != args.expected:
            raise ValueError(f"{args.cell} is {value!r}, expected {args.expected!r}")
        if workbook.vba_archive is None:
            raise ValueError("openpyxl did not retain the VBA package")
        vba = workbook.vba_archive.read("xl/vbaProject.bin")
        if not vba.startswith(bytes.fromhex("d0cf11e0a1b11ae1")):
            raise ValueError("xl/vbaProject.bin is not an OLE compound document")
        report = {
            "schema": "rxls.viewer-openpyxl-reopen.v1",
            "openpyxl": openpyxl.__version__,
            "sheets": workbook.sheetnames,
            "cell": args.cell,
            "value": value,
            "vba_bytes": len(vba),
        }
    except (InvalidFileException, KeyError, OSError, ValueError, zipfile.BadZipFile) as error:
        print(f"openpyxl XLSM reopen: {error}", file=sys.stderr)
        return 1
    finally:
        if workbook is not None:
            workbook.close()

    print(json.dumps(report, ensure_ascii=True, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
