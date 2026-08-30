#!/usr/bin/env python3
"""Validate generated rxls-wasm exports, metadata, and bundle budgets."""

from __future__ import annotations

import argparse
import base64
import hashlib
from html.parser import HTMLParser
import json
from pathlib import Path, PurePosixPath
import re
import sys
import tarfile


MAX_WASM_BYTES = 2 * 1024 * 1024
MAX_JS_BYTES = 128 * 1024
MAX_ARCHIVE_BYTES = 2 * 1024 * 1024
MAX_UNPACKED_BYTES = 5 * 1024 * 1024
MAX_NOTICE_BYTES = 512 * 1024
MAX_ARCHIVE_MEMBERS = 64
REQUIRED_FILES = (
    "package.json",
    "README.md",
    "LICENSE",
    "THIRD_PARTY_LICENSES.md",
    "THIRD_PARTY_NOTICES.txt",
    "demo/index.html",
    "demo/app.js",
    "demo/style.css",
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
    "maxExportOutputBytes",
    "maxInputBytes",
    "reportJson",
    "toCsv",
    "toHtml",
    "toMarkdown",
)
GIT_SHA_RE = re.compile(r"[0-9a-f]{40}")
SEMVER_RE = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
EXPECTED_AUTHOR = {
    "name": "Hyunjo Jung",
    "url": "https://github.com/HyunjoJung",
}
EXPECTED_KEYWORDS = [
    "excel",
    "spreadsheet",
    "rust",
    "wasm",
    "xls",
    "xlsx",
    "xlsb",
    "ods",
]


class _DemoHtmlPolicy(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.inline_style = False
        self.inline_script = False
        self.module_script = False
        self.stylesheet = False

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attributes = dict(attrs)
        if tag == "style" or "style" in attributes:
            self.inline_style = True
        if tag == "script":
            source = attributes.get("src")
            if source is None:
                self.inline_script = True
            if source == "./app.js" and attributes.get("type") == "module":
                self.module_script = True
        if (
            tag == "link"
            and attributes.get("rel") == "stylesheet"
            and attributes.get("href") == "./style.css"
        ):
            self.stylesheet = True


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _file_digest(path: Path, algorithm: str) -> bytes:
    digest = hashlib.new(algorithm)
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.digest()


def _safe_archive_name(name: str) -> str | None:
    if "\\" in name:
        return None
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or not path.parts:
        return None
    if path.parts[0] != "package" or len(path.parts) < 2:
        return None
    return PurePosixPath(*path.parts[1:]).as_posix()


def validate(
    package_dir: Path,
    archive: Path | None = None,
    git_rev: str | None = None,
    npm_pack: Path | None = None,
) -> tuple[list[str], dict]:
    errors: list[str] = []
    files: dict[str, dict[str, int | str]] = {}
    actual_files: set[str] = set()
    for path in package_dir.rglob("*"):
        relative = path.relative_to(package_dir).as_posix()
        if path.is_symlink():
            errors.append(f"package contains symbolic link: {relative}")
        elif path.is_file():
            actual_files.add(relative)
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
            decoded = json.loads(metadata_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            errors.append(f"invalid package.json: {error}")
        else:
            if isinstance(decoded, dict):
                metadata = decoded
            else:
                errors.append("package.json must contain a JSON object")
    if metadata.get("name") != "rxls-wasm":
        errors.append("package.json name must be rxls-wasm")
    version = metadata.get("version")
    if not isinstance(version, str) or SEMVER_RE.fullmatch(version) is None:
        errors.append("package.json version must be valid SemVer")
    if metadata.get("license") != "MIT":
        errors.append("package.json license must be MIT")
    if metadata.get("author") != EXPECTED_AUTHOR:
        errors.append("package.json author must identify the public maintainer")
    if metadata.get("repository") != {
        "type": "git",
        "url": "git+https://github.com/HyunjoJung/rxls.git",
        "directory": "bindings/wasm",
    }:
        errors.append("package.json repository must identify the public binding source")
    if metadata.get("homepage") != "https://hyunjojung.github.io/rxls/":
        errors.append("package.json homepage must identify the public viewer")
    if metadata.get("keywords") != EXPECTED_KEYWORDS:
        errors.append("package.json keywords must identify the supported formats")
    if metadata.get("engines") != {"node": ">=20"}:
        errors.append("package.json engines must require Node.js 20 or newer")
    if metadata.get("publishConfig") != {"access": "public", "provenance": True}:
        errors.append("package.json publishConfig must require public provenance")
    if metadata.get("private") is True:
        errors.append("package.json must be publishable")
    if metadata.get("main") != "./node/rxls_wasm.js":
        errors.append("package.json main must select the Node binding")
    if "module" in metadata or "browser" in metadata:
        errors.append("package.json must use conditional exports for browser selection")
    if metadata.get("types") != "./node/rxls_wasm.d.ts":
        errors.append("package.json types must select Node declarations")
    if metadata.get("type") != "commonjs":
        errors.append("package.json type must preserve the Node CommonJS entry point")
    if metadata.get("sideEffects") is not False:
        errors.append("package.json sideEffects must be false")
    for field in ("dependencies", "optionalDependencies", "peerDependencies"):
        if metadata.get(field) not in (None, {}):
            errors.append(f"package.json must remain free of {field}")
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
    exports = metadata.get("exports")
    if not isinstance(exports, dict) or exports.get(".") != expected_exports:
        errors.append("package.json exports must map condition-correct web and Node bindings")
    required_package_files = [
        "node",
        "web",
        "demo",
        "README.md",
        "LICENSE",
        "THIRD_PARTY_LICENSES.md",
        "THIRD_PARTY_NOTICES.txt",
    ]
    if metadata.get("files") != required_package_files:
        errors.append("package.json files must list exactly the npm release assets")

    license_summary_path = package_dir / "THIRD_PARTY_LICENSES.md"
    license_summary: dict[str, int | str] | None = None
    if license_summary_path.is_file():
        notice_text = license_summary_path.read_text(encoding="utf-8")
        if "wasm-bindgen" not in notice_text or "Third-Party Licenses" not in notice_text:
            errors.append("third-party license notice must identify the WASM dependency")
        license_summary = {
            "bytes": license_summary_path.stat().st_size,
            "sha256": _sha256(license_summary_path),
        }

    generated_notice_path = package_dir / "THIRD_PARTY_NOTICES.txt"
    generated_notice: dict[str, int | str] | None = None
    if generated_notice_path.is_file():
        payload = generated_notice_path.read_bytes()
        try:
            generated_text = payload.decode("utf-8")
        except UnicodeDecodeError as error:
            errors.append(f"generated third-party notice must be UTF-8: {error}")
        else:
            if len(payload) > MAX_NOTICE_BYTES:
                errors.append("generated third-party notice exceeds its size budget")
            if b"\r" in payload:
                errors.append("generated third-party notice must use LF line endings")
            required_notice_text = (
                "RXLS WASM THIRD-PARTY NOTICES\n",
                "Generated by scripts/render_supply_chain.py. Do not edit manually.",
                "- Manifest: bindings/wasm/Cargo.toml",
                "- Target: wasm32-unknown-unknown",
                "PACKAGE: wasm-bindgen ",
                "DEDUPLICATED LEGAL TEXTS",
                "----- BEGIN LEGAL TEXT -----",
                "----- END LEGAL TEXT -----",
            )
            for required in required_notice_text:
                if required not in generated_text:
                    errors.append(
                        f"generated third-party notice is missing: {required.strip()}"
                    )
            lock_match = re.search(
                r"^- Cargo lock SHA-256: ([0-9a-f]{64})$",
                generated_text,
                re.MULTILINE,
            )
            package_match = re.search(
                r"^- Third-party packages: ([1-9][0-9]*)$",
                generated_text,
                re.MULTILINE,
            )
            legal_match = re.search(
                r"^- Unique legal texts: ([1-9][0-9]*)$",
                generated_text,
                re.MULTILINE,
            )
            if lock_match is None:
                errors.append("generated third-party notice lacks a locked Cargo SHA-256")
            if package_match is None or int(package_match.group(1)) != generated_text.count(
                "\nPACKAGE: "
            ):
                errors.append("generated third-party notice package count differs")
            if legal_match is None or int(legal_match.group(1)) != generated_text.count(
                "\nLEGAL TEXT SHA-256: "
            ):
                errors.append("generated third-party notice legal-text count differs")
            if generated_text.count("----- BEGIN LEGAL TEXT -----") != generated_text.count(
                "----- END LEGAL TEXT -----"
            ):
                errors.append("generated third-party notice legal-text framing differs")
            if "file://" in generated_text or re.search(
                r"(?:[A-Za-z]:\\|/(?:Users|home)/[^/\s]+/)", generated_text
            ):
                errors.append("generated third-party notice contains a host path")
            generated_notice = {
                "bytes": len(payload),
                "sha256": _sha256_bytes(payload),
            }

    demo_html_path = package_dir / "demo" / "index.html"
    if demo_html_path.is_file():
        try:
            demo_html = demo_html_path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            errors.append(f"invalid demo HTML: {error}")
        else:
            demo_policy = _DemoHtmlPolicy()
            demo_policy.feed(demo_html)
            demo_policy.close()
            if demo_policy.inline_style or demo_policy.inline_script:
                errors.append("demo HTML must not require unsafe-inline CSP")
            if not demo_policy.module_script or not demo_policy.stylesheet:
                errors.append("demo HTML must load its external module and stylesheet")
    demo_style_path = package_dir / "demo" / "style.css"
    if demo_style_path.is_file() and not demo_style_path.read_bytes().strip():
        errors.append("demo stylesheet must not be empty")

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
            if isinstance(version, str) and SEMVER_RE.fullmatch(version):
                expected_name = f"rxls-wasm-{version}.tgz"
                if archive.name != expected_name:
                    errors.append(f"npm archive name must be {expected_name}")
            archive_data = {
                "name": archive.name,
                "bytes": archive.stat().st_size,
                "sha256": _sha256(archive),
                "shasum": _file_digest(archive, "sha1").hex(),
                "integrity": "sha512-"
                + base64.b64encode(_file_digest(archive, "sha512")).decode("ascii"),
            }
            if archive.stat().st_size > MAX_ARCHIVE_BYTES:
                errors.append(
                    f"npm archive is {archive.stat().st_size} bytes; "
                    f"budget is {MAX_ARCHIVE_BYTES}"
                )
            try:
                with tarfile.open(archive, "r|gz") as package:
                    archive_files: list[str] = []
                    archive_hashes: dict[str, str] = {}
                    unpacked_bytes = 0
                    for member_index, member in enumerate(package, start=1):
                        if member_index > MAX_ARCHIVE_MEMBERS:
                            errors.append(
                                "npm archive contains more than "
                                f"{MAX_ARCHIVE_MEMBERS} members"
                            )
                            break
                        if member.size < 0:
                            errors.append(
                                f"npm archive member has invalid size: {member.name}"
                            )
                            break
                        unpacked_bytes += member.size
                        if unpacked_bytes > MAX_UNPACKED_BYTES:
                            errors.append(
                                f"npm archive expands to more than {MAX_UNPACKED_BYTES} bytes"
                            )
                            break
                        relative = _safe_archive_name(member.name)
                        if relative is None:
                            errors.append(f"npm archive contains unsafe member: {member.name}")
                            continue
                        if not member.isfile():
                            errors.append(f"npm archive contains non-file member: {relative}")
                            continue
                        archive_files.append(relative)
                        source = files.get(relative)
                        if source is None or archive_files.count(relative) > 1:
                            continue
                        if member.size != source["bytes"]:
                            errors.append(f"npm archive member size differs: {relative}")
                            continue
                        extracted = package.extractfile(member)
                        if extracted is None:
                            errors.append(f"npm archive member is unreadable: {relative}")
                            continue
                        payload = extracted.read()
                        if len(payload) != member.size:
                            errors.append(f"npm archive member size differs: {relative}")
                            continue
                        archive_hashes[relative] = _sha256_bytes(payload)
            except (OSError, tarfile.TarError) as error:
                errors.append(f"invalid npm archive: {error}")
            else:
                archive_data["unpacked_bytes"] = unpacked_bytes
                archive_file_set = set(archive_files)
                if len(archive_files) != len(archive_file_set):
                    errors.append("npm archive contains duplicate files")
                for relative in sorted(EXPECTED_FILES - archive_file_set):
                    errors.append(f"npm archive missing file: {relative}")
                for relative in sorted(archive_file_set - EXPECTED_FILES):
                    errors.append(f"npm archive contains unexpected file: {relative}")
                for relative in sorted(EXPECTED_FILES & archive_file_set):
                    source = files.get(relative)
                    packed_sha = archive_hashes.get(relative)
                    if source is not None and packed_sha != source["sha256"]:
                        errors.append(
                            f"npm archive member differs from package directory: {relative}"
                        )

    npm_pack_data: dict[str, int | str] | None = None
    if npm_pack is not None:
        if archive is None or archive_data is None:
            errors.append("npm pack receipt requires a validated archive")
        elif not npm_pack.is_file():
            errors.append(f"missing npm pack receipt: {npm_pack}")
        else:
            try:
                receipt = json.loads(npm_pack.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as error:
                errors.append(f"invalid npm pack receipt: {error}")
            else:
                if (
                    not isinstance(receipt, list)
                    or len(receipt) != 1
                    or not isinstance(receipt[0], dict)
                ):
                    errors.append("npm pack receipt must contain exactly one package")
                else:
                    row = receipt[0]
                    expected_receipt = {
                        "name": metadata.get("name"),
                        "version": metadata.get("version"),
                        "filename": archive_data.get("name"),
                        "size": archive_data.get("bytes"),
                        "unpackedSize": archive_data.get("unpacked_bytes"),
                        "shasum": archive_data.get("shasum"),
                        "integrity": archive_data.get("integrity"),
                        "entryCount": len(EXPECTED_FILES),
                    }
                    for field, expected in expected_receipt.items():
                        if row.get(field) != expected:
                            errors.append(f"npm pack receipt differs: {field}")
                    npm_pack_data = {
                        "bytes": npm_pack.stat().st_size,
                        "sha256": _sha256(npm_pack),
                    }

    normalized_git_rev = git_rev.lower() if git_rev is not None else None
    if normalized_git_rev is not None and GIT_SHA_RE.fullmatch(normalized_git_rev) is None:
        errors.append("git revision must be a full lowercase SHA-1")

    report = {
        "schema": "rxls.wasm-bundle-budget.v2",
        "git_rev": normalized_git_rev,
        "package": {"name": metadata.get("name"), "version": metadata.get("version")},
        "budgets": {
            "wasm_bytes_per_target": MAX_WASM_BYTES,
            "javascript_bytes_per_target": MAX_JS_BYTES,
            "npm_archive_bytes": MAX_ARCHIVE_BYTES,
            "npm_unpacked_bytes": MAX_UNPACKED_BYTES,
        },
        "files": dict(sorted(files.items())),
        "license_summary": license_summary,
        "third_party_notice": generated_notice,
        "archive": archive_data,
        "npm_pack": npm_pack_data,
        "passed": not errors,
    }
    return errors, report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package_dir", type=Path)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--git-rev")
    parser.add_argument("--npm-pack", type=Path)
    parser.add_argument("--write-report", type=Path)
    args = parser.parse_args()

    errors, report = validate(
        args.package_dir,
        args.archive,
        args.git_rev,
        args.npm_pack,
    )
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
