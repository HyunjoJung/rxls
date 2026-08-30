#!/usr/bin/env python3
"""Rewrite a VSIX with deterministic entry order, timestamps, and permissions."""

from __future__ import annotations

import argparse
import pathlib
import zipfile

FIXED_TIME = (1980, 1, 1, 0, 0, 0)


def normalized_name(value: str) -> str:
    path = pathlib.PurePosixPath(value)
    if value.startswith("/") or "\\" in value or ".." in path.parts:
        raise ValueError(f"unsafe VSIX entry: {value}")
    return value


def normalize(source: pathlib.Path, destination: pathlib.Path) -> None:
    with zipfile.ZipFile(source, "r") as archive:
        names = archive.namelist()
        if len(names) != len(set(names)):
            raise ValueError("VSIX contains duplicate entries")
        entries = [(normalized_name(name), archive.read(name)) for name in names]

    destination.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(
        destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9
    ) as archive:
        for name, payload in sorted(entries):
            info = zipfile.ZipInfo(name, FIXED_TIME)
            info.create_system = 3
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = ((0o755 if name.endswith("/") else 0o644) & 0xFFFF) << 16
            archive.writestr(info, payload, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("source", type=pathlib.Path)
    parser.add_argument("destination", type=pathlib.Path)
    args = parser.parse_args()
    normalize(args.source.resolve(), args.destination.resolve())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
