#!/usr/bin/env python3
"""Validate generated rxls-wasm exports, metadata, and bundle budgets."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys
import tarfile


MAX_WASM_BYTES = 2 * 1024 * 1024
MAX_JS_BYTES = 128 * 1024
MAX_ARCHIVE_BYTES = 2 * 1024 * 1024
REQUIRED_FILES = (
    "package.json",
    "README.md",
    "LICENSE",
    "demo/index.html",
    "demo/app.js",
    "node/rxls_wasm.js",
    "node/rxls_wasm.d.ts",
    "node/rxls_wasm_bg.wasm",
    "node/rxls_wasm_bg.wasm.d.ts",
    "web/rxls_wasm.js",
    "web/rxls_wasm.d.ts",
    "web/rxls_wasm_bg.wasm",
    "web/rxls_wasm_bg.wasm.d.ts",
    "web/package.json",
)
EXPECTED_FILES = frozenset(REQUIRED_FILES)
REQUIRED_TYPES = (
    "RxlsErrorObject",
    "extractText",
    "maxInputBytes",
    "reportJson",
    "toCsv",
    "toHtml",
)


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate(package_dir: Path, archive: Path | None = None) -> tuple[list[str], dict]:
    errors: list[str] = []
    files: dict[str, dict[str, int | str]] = {}
    actual_files = {
        path.relative_to(package_dir).as_posix()
        for path in package_dir.rglob("*")
        if path.is_file() or path.is_symlink()
    }
    missing_files = EXPECTED_FILES - actual_files
    unexpected_files = actual_files - EXPECTED_FILES
    for relative in sorted(missing_files):
        errors.append(f"missing package file: {relative}")
    for relative in sorted(unexpected_files):
        errors.append(f"unexpected package file: {relative}")
    for relative in REQUIRED_FILES:
        path = package_dir / relative
        if not path.is_file():
            continue
        files[relative] = {"bytes": path.stat().st_size, "sha256": _sha256(path)}

    metadata_path = package_dir / "package.json"
    metadata: dict = {}
    if metadata_path.is_file():
        try:
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            errors.append(f"invalid package.json: {error}")
    if metadata.get("name") != "rxls-wasm":
        errors.append("package.json name must be rxls-wasm")
    if metadata.get("private") is True:
        errors.append("package.json must be publishable")
    if metadata.get("main") != "./node/rxls_wasm.js":
        errors.append("package.json main must select the Node binding")
    if "module" in metadata or "browser" in metadata:
        errors.append("package.json must use conditional exports for browser selection")
    if metadata.get("types") != "./node/rxls_wasm.d.ts":
        errors.append("package.json types must select Node declarations")
    expected_exports = {
        "browser": {
            "types": "./web/rxls_wasm.d.ts",
            "default": "./web/rxls_wasm.js",
        },
        "node": {
            "types": "./node/rxls_wasm.d.ts",
            "default": "./node/rxls_wasm.js",
        },
        "types": "./node/rxls_wasm.d.ts",
        "default": "./node/rxls_wasm.js",
    }
    if metadata.get("exports", {}).get(".") != expected_exports:
        errors.append("package.json exports must map condition-correct web and Node bindings")
    required_package_files = {
        "node",
        "web",
        "demo",
        "README.md",
        "LICENSE",
    }
    if set(metadata.get("files", [])) != required_package_files:
        errors.append("package.json files must list exactly the npm release assets")

    web_metadata_path = package_dir / "web" / "package.json"
    if web_metadata_path.is_file():
        try:
            web_metadata = json.loads(web_metadata_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            errors.append(f"invalid web/package.json: {error}")
        else:
            if web_metadata != {"type": "module"}:
                errors.append("web/package.json must mark the browser binding as ESM")

    for target in ("node", "web"):
        declarations = package_dir / target / "rxls_wasm.d.ts"
        if declarations.is_file():
            text = declarations.read_text(encoding="utf-8")
            for symbol in REQUIRED_TYPES:
                if symbol not in text:
                    errors.append(f"{target} declarations omit {symbol}")
            has_default_init = "export default function" in text
            if target == "node" and has_default_init:
                errors.append("node declarations must not advertise browser initialization")
            if target == "web" and not has_default_init:
                errors.append("web declarations must advertise browser initialization")
        wasm = package_dir / target / "rxls_wasm_bg.wasm"
        if wasm.is_file():
            size = wasm.stat().st_size
            if wasm.read_bytes()[:4] != b"\0asm":
                errors.append(f"{target} output is not a WebAssembly module")
            if size > MAX_WASM_BYTES:
                errors.append(
                    f"{target} wasm bundle is {size} bytes; budget is {MAX_WASM_BYTES}"
                )
        glue = package_dir / target / "rxls_wasm.js"
        if glue.is_file() and glue.stat().st_size > MAX_JS_BYTES:
            errors.append(
                f"{target} JavaScript glue is {glue.stat().st_size} bytes; "
                f"budget is {MAX_JS_BYTES}"
            )

    archive_data = None
    if archive is not None:
        if not archive.is_file():
            errors.append(f"missing npm archive: {archive}")
        else:
            archive_data = {
                "name": archive.name,
                "bytes": archive.stat().st_size,
                "sha256": _sha256(archive),
            }
            if archive.stat().st_size > MAX_ARCHIVE_BYTES:
                errors.append(
                    f"npm archive is {archive.stat().st_size} bytes; "
                    f"budget is {MAX_ARCHIVE_BYTES}"
                )
            try:
                with tarfile.open(archive, "r:gz") as package:
                    archive_files = [
                        member.name.removeprefix("package/")
                        for member in package.getmembers()
                        if member.isfile() or member.issym() or member.islnk()
                    ]
            except (OSError, tarfile.TarError) as error:
                errors.append(f"invalid npm archive: {error}")
            else:
                archive_file_set = set(archive_files)
                if len(archive_files) != len(archive_file_set):
                    errors.append("npm archive contains duplicate files")
                for relative in sorted(EXPECTED_FILES - archive_file_set):
                    errors.append(f"npm archive missing file: {relative}")
                for relative in sorted(archive_file_set - EXPECTED_FILES):
                    errors.append(f"npm archive contains unexpected file: {relative}")

    report = {
        "schema": "rxls.wasm-bundle-budget.v1",
        "package": {"name": metadata.get("name"), "version": metadata.get("version")},
        "budgets": {
            "wasm_bytes_per_target": MAX_WASM_BYTES,
            "javascript_bytes_per_target": MAX_JS_BYTES,
            "npm_archive_bytes": MAX_ARCHIVE_BYTES,
        },
        "files": dict(sorted(files.items())),
        "archive": archive_data,
        "passed": not errors,
    }
    return errors, report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package_dir", type=Path)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--write-report", type=Path)
    args = parser.parse_args()

    errors, report = validate(args.package_dir, args.archive)
    if args.write_report:
        args.write_report.write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    if errors:
        for error in errors:
            print(f"WASM package: {error}", file=sys.stderr)
        return 1
    print(
        "WASM package: "
        f"version={report['package']['version']} files={len(report['files'])} budgets=ok"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
