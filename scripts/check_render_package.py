#!/usr/bin/env python3
"""Validate the publishable @rxls/render-worker npm artifact."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import sys
import tarfile
import tomllib


PACKAGE_NAME = "@rxls/render-worker"
CRATE_NAME = "rxls-render-wasm"
REPORT_SCHEMA = "rxls.render-worker-package.v2"
REPOSITORY_URL = "git+https://github.com/HyunjoJung/rxls.git"
HOMEPAGE_URL = "https://hyunjojung.github.io/rxls/"
EXPECTED_AUTHOR = {
    "name": "Hyunjo Jung",
    "url": "https://github.com/HyunjoJung",
}
EXPECTED_KEYWORDS = [
    "excel",
    "spreadsheet",
    "rust",
    "wasm",
    "worker",
    "rendering",
]
MAX_ARCHIVE_BYTES = 2 * 1024 * 1024
MAX_UNPACKED_BYTES = 5 * 1024 * 1024
MAX_WASM_BYTES = 4 * 1024 * 1024
MAX_JAVASCRIPT_BYTES = 128 * 1024
MAX_DECLARATION_BYTES = 128 * 1024
MAX_NOTICE_BYTES = 512 * 1024
MAX_ARCHIVE_MEMBERS = 64
NOTICE_NAME = "THIRD_PARTY_NOTICES.txt"
DECLARATION_MODULES = {
    "js/client.mjs": "js/client.d.mts",
    "js/protocol.mjs": "js/protocol.d.mts",
    "js/worker-runtime.mjs": "js/worker-runtime.d.mts",
    "js/worker.mjs": "js/worker.d.mts",
}
EXPECTED_FILES = frozenset(
    {
        "LICENSE",
        "README.md",
        NOTICE_NAME,
        "js/client.d.mts",
        "js/client.mjs",
        "js/protocol.d.mts",
        "js/protocol.mjs",
        "js/worker-runtime.d.mts",
        "js/worker-runtime.mjs",
        "js/worker.d.mts",
        "js/worker.mjs",
        "package.json",
        "pkg/rxls_render_wasm.d.ts",
        "pkg/rxls_render_wasm.js",
        "pkg/rxls_render_wasm_bg.wasm",
        "pkg/rxls_render_wasm_bg.wasm.d.ts",
    }
)
EXPECTED_EXPORTS = {
    ".": {
        "types": "./js/client.d.mts",
        "import": "./js/client.mjs",
        "default": "./js/client.mjs",
    },
    "./protocol": {
        "types": "./js/protocol.d.mts",
        "import": "./js/protocol.mjs",
        "default": "./js/protocol.mjs",
    },
    "./worker-runtime": {
        "types": "./js/worker-runtime.d.mts",
        "import": "./js/worker-runtime.mjs",
        "default": "./js/worker-runtime.mjs",
    },
    "./worker": {
        "types": "./js/worker.d.mts",
        "import": "./js/worker.mjs",
        "default": "./js/worker.mjs",
    },
}
SEMVER_RE = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)


def _sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _file_digest(path: Path, algorithm: str) -> bytes:
    digest = hashlib.new(algorithm)
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.digest()


def _read_json(path: Path, errors: list[str], label: str) -> dict:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        errors.append(f"invalid {label}: {error}")
        return {}
    if not isinstance(document, dict):
        errors.append(f"{label} must contain a JSON object")
        return {}
    return document


def _metadata_errors(metadata: dict, crate_version: str | None) -> list[str]:
    errors: list[str] = []
    version = metadata.get("version")
    if metadata.get("name") != PACKAGE_NAME:
        errors.append(f"package name must be {PACKAGE_NAME}")
    if not isinstance(version, str) or SEMVER_RE.fullmatch(version) is None:
        errors.append("package version must be valid SemVer")
    if crate_version is not None and version != crate_version:
        errors.append("npm and render-WASM crate versions must match")
    if metadata.get("private") is True:
        errors.append("package must remain publishable")
    if metadata.get("type") != "module":
        errors.append("package must remain an ES module")
    if metadata.get("license") != "MIT":
        errors.append("package license must be MIT")
    if metadata.get("author") != EXPECTED_AUTHOR:
        errors.append("package author must identify the public maintainer")
    if metadata.get("repository") != {
        "type": "git",
        "url": REPOSITORY_URL,
        "directory": "bindings/render-wasm",
    }:
        errors.append("package repository metadata must identify its public source directory")
    if metadata.get("homepage") != HOMEPAGE_URL:
        errors.append("package homepage must identify the public viewer")
    if metadata.get("keywords") != EXPECTED_KEYWORDS:
        errors.append("package keywords must identify its public rendering role")
    if metadata.get("publishConfig") != {"access": "public", "provenance": True}:
        errors.append("package must require public access and npm provenance")
    if metadata.get("files") != [
        "js/",
        "pkg/",
        "README.md",
        "LICENSE",
        NOTICE_NAME,
    ]:
        errors.append(
            "package files must list exactly the worker, WASM, README, and legal assets"
        )
    if metadata.get("types") != "./js/client.d.mts":
        errors.append("package root types must resolve to the client declaration")
    if metadata.get("exports") != EXPECTED_EXPORTS:
        errors.append(
            "package exports must expose the bounded worker API with exact type conditions"
        )
    else:
        for subpath, conditions in metadata["exports"].items():
            if list(conditions) != ["types", "import", "default"]:
                errors.append(
                    f"package export {subpath} conditions must order types before runtime"
                )
    if metadata.get("engines") != {"node": ">=20"}:
        errors.append("package Node floor must remain explicit")
    for field in ("dependencies", "optionalDependencies", "peerDependencies"):
        if metadata.get(field) not in (None, {}):
            errors.append(f"package must remain free of {field}")
    return errors


def _module_exports(payload: bytes, label: str, errors: list[str]) -> dict[str, str]:
    try:
        text = payload.decode("utf-8")
    except UnicodeDecodeError as error:
        errors.append(f"{label} must be UTF-8: {error}")
        return {}
    declaration = label.endswith(".d.mts")
    marker = r"(?:declare\s+)?" if declaration else ""
    exports: dict[str, str] = {}
    for kind, name in re.findall(
        rf"^export\s+{marker}(const|class|function)\s+([A-Za-z_$][A-Za-z0-9_$]*)",
        text,
        re.MULTILINE,
    ):
        previous = exports.setdefault(name, kind)
        if previous != kind:
            errors.append(f"{label} declares {name} with conflicting runtime kinds")
    return exports


def _declaration_errors(payloads: dict[str, bytes], label: str) -> list[str]:
    errors: list[str] = []
    for runtime_name, declaration_name in DECLARATION_MODULES.items():
        runtime = payloads.get(runtime_name)
        declaration = payloads.get(declaration_name)
        if runtime is None:
            errors.append(f"{label} is missing JavaScript module: {runtime_name}")
            continue
        if declaration is None:
            errors.append(f"{label} is missing declaration module: {declaration_name}")
            continue
        if len(declaration) > MAX_DECLARATION_BYTES:
            errors.append(
                f"{label} declaration is {len(declaration)} bytes; budget is "
                f"{MAX_DECLARATION_BYTES}: {declaration_name}"
            )
        try:
            text = declaration.decode("utf-8")
        except UnicodeDecodeError as error:
            errors.append(f"{label} declaration must be UTF-8: {declaration_name}: {error}")
            continue
        if declaration.startswith(b"\xef\xbb\xbf") or "\r" in text:
            errors.append(
                f"{label} declaration must use deterministic UTF-8/LF: {declaration_name}"
            )
        if re.search(r"\bany\b", text):
            errors.append(f"{label} declaration must not expose any: {declaration_name}")
        for forbidden in ("/// <reference", "declare module", "sourceMappingURL"):
            if forbidden in text:
                errors.append(
                    f"{label} declaration contains forbidden metadata: "
                    f"{declaration_name}: {forbidden}"
                )
        for specifier in re.findall(r"\bfrom\s+[\"']([^\"']+)[\"']", text):
            if not specifier.startswith("./") or not specifier.endswith(".mjs"):
                errors.append(
                    f"{label} declaration import must be a relative .mjs target: "
                    f"{declaration_name}: {specifier}"
                )
                continue
            target = (
                PurePosixPath(declaration_name).parent
                / PurePosixPath(specifier.removeprefix("./"))
            ).as_posix()
            if target not in payloads:
                errors.append(
                    f"{label} declaration import does not resolve: "
                    f"{declaration_name}: {specifier}"
                )
        runtime_exports = _module_exports(runtime, runtime_name, errors)
        declaration_exports = _module_exports(declaration, declaration_name, errors)
        if declaration_exports != runtime_exports:
            errors.append(
                f"{label} runtime declarations differ for {runtime_name}: "
                f"expected {sorted(runtime_exports.items())}, "
                f"got {sorted(declaration_exports.items())}"
            )
    return errors


def _safe_archive_name(name: str) -> str | None:
    if "\\" in name:
        return None
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or not path.parts:
        return None
    if path.parts[0] != "package" or len(path.parts) < 2:
        return None
    return PurePosixPath(*path.parts[1:]).as_posix()


def _notice_errors(payload: bytes) -> tuple[list[str], dict[str, object] | None]:
    errors: list[str] = []
    if len(payload) > MAX_NOTICE_BYTES:
        errors.append(
            f"packed third-party notice is {len(payload)} bytes; budget is "
            f"{MAX_NOTICE_BYTES}"
        )
    try:
        text = payload.decode("utf-8")
    except UnicodeDecodeError as error:
        return [f"packed third-party notice must be UTF-8: {error}"], None
    if not text.startswith("RXLS RENDER WORKER THIRD-PARTY NOTICES\n"):
        errors.append("packed third-party notice has an invalid title")
    for required in (
        "Generated by scripts/render_supply_chain.py. Do not edit manually.",
        "- Manifest: bindings/render-wasm/Cargo.toml",
        "- Target: wasm32-unknown-unknown",
        "- Dependency edges: Cargo normal edges for the production target",
    ):
        if required not in text:
            errors.append(f"packed third-party notice is missing: {required}")
    if "file://" in text or re.search(r"/(?:Users|home)/[^/\s]+/", text):
        errors.append("packed third-party notice contains a host path")

    lock_match = re.search(r"^- Cargo lock SHA-256: ([0-9a-f]{64})$", text, re.MULTILINE)
    package_count_match = re.search(
        r"^- Third-party packages: ([1-9][0-9]*)$", text, re.MULTILINE
    )
    legal_count_match = re.search(
        r"^- Unique legal texts: ([1-9][0-9]*)$", text, re.MULTILINE
    )
    if lock_match is None:
        errors.append("packed third-party notice lacks a locked Cargo SHA-256")
    if package_count_match is None:
        errors.append("packed third-party notice lacks a positive package count")
    if legal_count_match is None:
        errors.append("packed third-party notice lacks a positive legal-text count")

    package_ids = re.findall(
        r"^PACKAGE: ([A-Za-z0-9_.-]+) ([^\s]+)$", text, re.MULTILINE
    )
    legal_digests = re.findall(
        r"^LEGAL TEXT SHA-256: ([0-9a-f]{64})$", text, re.MULTILINE
    )
    referenced_digests = re.findall(
        r"^- [A-Za-z0-9_.+/-]+: ([0-9a-f]{64})$", text, re.MULTILINE
    )
    if len(set(package_ids)) != len(package_ids):
        errors.append("packed third-party notice repeats a package identity")
    if len(set(legal_digests)) != len(legal_digests):
        errors.append("packed third-party notice repeats a legal text identity")
    if package_count_match is not None and int(package_count_match.group(1)) != len(
        package_ids
    ):
        errors.append("packed third-party notice package count differs from its index")
    if legal_count_match is not None and int(legal_count_match.group(1)) != len(
        legal_digests
    ):
        errors.append("packed third-party notice legal-text count differs from its index")
    for label in (
        "Cargo source:",
        "Declared license expression:",
        "Registry archive SHA-256:",
        "Legal files:",
    ):
        if text.count(label) != len(package_ids):
            errors.append(f"packed third-party notice has an incomplete {label} index")
    if set(referenced_digests) != set(legal_digests):
        errors.append("packed third-party notice legal-file references are incomplete")
    if text.count("----- BEGIN LEGAL TEXT -----") != len(legal_digests) or text.count(
        "----- END LEGAL TEXT -----"
    ) != len(legal_digests):
        errors.append("packed third-party notice legal-text framing is incomplete")

    summary: dict[str, object] | None = None
    if lock_match is not None and package_count_match is not None and legal_count_match is not None:
        summary = {
            "cargo_lock_sha256": lock_match.group(1),
            "packages": int(package_count_match.group(1)),
            "legal_texts": int(legal_count_match.group(1)),
            "sha256": _sha256_bytes(payload),
        }
    return errors, summary


def validate(
    package_root: Path,
    archive: Path,
    *,
    git_rev: str | None = None,
    compare_source_files: bool = True,
    npm_pack: Path | None = None,
) -> tuple[list[str], dict]:
    errors: list[str] = []
    package_root = package_root.resolve()
    archive = archive.resolve()

    crate_version: str | None = None
    manifest_path = package_root / "Cargo.toml"
    try:
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        errors.append(f"invalid render-WASM Cargo.toml: {error}")
    else:
        package = manifest.get("package", {})
        if package.get("name") != CRATE_NAME:
            errors.append(f"render-WASM crate name must be {CRATE_NAME}")
        version = package.get("version")
        if isinstance(version, str):
            crate_version = version
        else:
            errors.append("render-WASM crate version must be a string")
        if package.get("publish") is not False:
            errors.append("the implementation crate must remain excluded from crates.io")

    source_metadata = _read_json(package_root / "package.json", errors, "package.json")
    errors.extend(_metadata_errors(source_metadata, crate_version))
    source_modules: dict[str, bytes] = {}
    for relative in (*DECLARATION_MODULES.keys(), *DECLARATION_MODULES.values()):
        path = package_root / relative
        try:
            source_modules[relative] = path.read_bytes()
        except OSError as error:
            errors.append(f"cannot read package source module {relative}: {error}")
    errors.extend(_declaration_errors(source_modules, "package source"))
    repository_license = package_root.parents[1] / "LICENSE"
    package_license = package_root / "LICENSE"
    if repository_license.is_file() and (
        not package_license.is_file()
        or package_license.read_bytes() != repository_license.read_bytes()
    ):
        errors.append("packed license must match the repository MIT license")

    archive_record: dict[str, object] | None = None
    files: dict[str, dict[str, object]] = {}
    archive_metadata: dict = {}
    notice_summary: dict[str, object] | None = None
    archive_modules: dict[str, bytes] = {}
    if not archive.is_file():
        errors.append(f"missing npm archive: {archive}")
    else:
        archive_bytes = archive.stat().st_size
        archive_record = {
            "name": archive.name,
            "bytes": archive_bytes,
            "sha256": _sha256(archive),
            "shasum": _file_digest(archive, "sha1").hex(),
            "integrity": "sha512-"
            + base64.b64encode(_file_digest(archive, "sha512")).decode("ascii"),
        }
        if archive_bytes > MAX_ARCHIVE_BYTES:
            errors.append(
                f"npm archive is {archive_bytes} bytes; budget is {MAX_ARCHIVE_BYTES}"
            )
        try:
            with tarfile.open(archive, "r|gz") as package:
                archive_files: list[str] = []
                total_unpacked = 0
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
                    total_unpacked += member.size
                    if total_unpacked > MAX_UNPACKED_BYTES:
                        errors.append(
                            f"npm archive expands to more than {MAX_UNPACKED_BYTES} bytes"
                        )
                        break
                    relative = _safe_archive_name(member.name)
                    if relative is None:
                        errors.append(f"npm archive contains an unsafe member: {member.name}")
                        continue
                    if not member.isfile():
                        errors.append(f"npm archive contains a non-file member: {relative}")
                        continue
                    archive_files.append(relative)
                    if relative not in EXPECTED_FILES or archive_files.count(relative) > 1:
                        continue
                    source_path = package_root / relative
                    if compare_source_files:
                        if not source_path.is_file():
                            errors.append(f"missing packed source file: {relative}")
                            continue
                        if member.size != source_path.stat().st_size:
                            errors.append(f"npm archive file size differs: {relative}")
                            continue
                    extracted = package.extractfile(member)
                    if extracted is None:
                        errors.append(f"npm archive file cannot be read: {relative}")
                        continue
                    data = extracted.read()
                    if len(data) != member.size:
                        errors.append(f"npm archive file size differs: {relative}")
                        continue
                    files[relative] = {
                        "bytes": len(data),
                        "sha256": _sha256_bytes(data),
                    }
                    if compare_source_files:
                        if source_path.read_bytes() != data:
                            errors.append(f"packed file differs from source: {relative}")
                    if relative == "package.json":
                        try:
                            archive_metadata = json.loads(data.decode("utf-8"))
                        except (UnicodeDecodeError, json.JSONDecodeError) as error:
                            errors.append(f"invalid packed package.json: {error}")
                    if relative == NOTICE_NAME:
                        notice_errors, notice_summary = _notice_errors(data)
                        errors.extend(notice_errors)
                    if relative in DECLARATION_MODULES or relative in DECLARATION_MODULES.values():
                        archive_modules[relative] = data
                    if relative.endswith(".wasm"):
                        if not data.startswith(b"\0asm"):
                            errors.append(f"packed WebAssembly magic is invalid: {relative}")
                        if len(data) > MAX_WASM_BYTES:
                            errors.append(
                                f"packed WebAssembly is {len(data)} bytes; budget is "
                                f"{MAX_WASM_BYTES}"
                            )
                    if relative.endswith((".mjs", ".js")) and len(data) > MAX_JAVASCRIPT_BYTES:
                        errors.append(
                            f"packed JavaScript is {len(data)} bytes; budget is "
                            f"{MAX_JAVASCRIPT_BYTES}: {relative}"
                        )
                    if relative.endswith(".d.mts") and len(data) > MAX_DECLARATION_BYTES:
                        errors.append(
                            f"packed declaration is {len(data)} bytes; budget is "
                            f"{MAX_DECLARATION_BYTES}: {relative}"
                        )
                archive_record["unpacked_bytes"] = total_unpacked
                seen = set(archive_files)
                if len(archive_files) != len(seen):
                    errors.append("npm archive contains duplicate files")
                for relative in sorted(EXPECTED_FILES - seen):
                    errors.append(f"npm archive is missing file: {relative}")
                for relative in sorted(seen - EXPECTED_FILES):
                    errors.append(f"npm archive contains unexpected file: {relative}")
        except (OSError, tarfile.TarError) as error:
            errors.append(f"invalid npm archive: {error}")
    if archive.is_file():
        errors.extend(_declaration_errors(archive_modules, "npm archive"))

    if archive_metadata:
        errors.extend(_metadata_errors(archive_metadata, crate_version))
        if archive_metadata != source_metadata:
            errors.append("packed package metadata differs from the reviewed source metadata")
        version = archive_metadata.get("version")
        expected_name = f"rxls-render-worker-{version}.tgz"
        if archive.name != expected_name:
            errors.append(f"npm archive name must be {expected_name}")

    npm_pack_record: dict[str, object] | None = None
    if npm_pack is not None:
        if archive_record is None:
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
                        "name": source_metadata.get("name"),
                        "version": source_metadata.get("version"),
                        "filename": archive_record.get("name"),
                        "size": archive_record.get("bytes"),
                        "unpackedSize": archive_record.get("unpacked_bytes"),
                        "shasum": archive_record.get("shasum"),
                        "integrity": archive_record.get("integrity"),
                        "entryCount": len(EXPECTED_FILES),
                    }
                    for field, expected in expected_receipt.items():
                        if row.get(field) != expected:
                            errors.append(f"npm pack receipt differs: {field}")
                    npm_pack_record = {
                        "bytes": npm_pack.stat().st_size,
                        "sha256": _sha256(npm_pack),
                    }

    if git_rev is not None and re.fullmatch(r"[0-9a-f]{40}", git_rev) is None:
        errors.append("git revision must be a lowercase 40-character SHA")

    report = {
        "schema": REPORT_SCHEMA,
        "package": {
            "name": source_metadata.get("name"),
            "version": source_metadata.get("version"),
        },
        "git_rev": git_rev,
        "budgets": {
            "archive_bytes": MAX_ARCHIVE_BYTES,
            "unpacked_bytes": MAX_UNPACKED_BYTES,
            "wasm_bytes": MAX_WASM_BYTES,
            "javascript_bytes_per_file": MAX_JAVASCRIPT_BYTES,
            "declaration_bytes_per_file": MAX_DECLARATION_BYTES,
            "third_party_notice_bytes": MAX_NOTICE_BYTES,
        },
        "third_party_notice": notice_summary,
        "files": dict(sorted(files.items())),
        "archive": archive_record,
        "npm_pack": npm_pack_record,
        "passed": not errors,
    }
    return errors, report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package_root", type=Path)
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--npm-pack", type=Path)
    parser.add_argument("--git-rev")
    parser.add_argument("--archive-only", action="store_true")
    parser.add_argument("--write-report", type=Path)
    args = parser.parse_args()

    errors, report = validate(
        args.package_root,
        args.archive,
        git_rev=args.git_rev,
        compare_source_files=not args.archive_only,
        npm_pack=args.npm_pack,
    )
    if args.write_report is not None:
        args.write_report.parent.mkdir(parents=True, exist_ok=True)
        args.write_report.write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    if errors:
        for error in errors:
            print(f"render package: {error}", file=sys.stderr)
        return 1
    print(
        "render package: "
        f"name={report['package']['name']} version={report['package']['version']} "
        f"files={len(report['files'])} budgets=ok"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
