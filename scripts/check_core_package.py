#!/usr/bin/env python3
"""Fail closed when the published core crate absorbs rendering weight."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import tarfile
import tomllib


SCHEMA = "rxls.core-package-gate.v1"
MAX_ARCHIVE_BYTES = 1 << 20
MAX_UNPACKED_BYTES = 5 << 20
MAX_FILES = 160
ALLOWED_DEPENDENCIES = {
    "cfb",
    "chrono",
    "encoding_rs",
    "quick-xml",
    "serde",
    "thiserror",
    "zip",
}
EXPECTED_FEATURES = {
    "chrono": ["dep:chrono"],
    "cli": [],
    "default": ["xlsx", "cli"],
    "full": ["xlsx", "xlsb", "ods", "serde", "chrono"],
    "ods": ["dep:zip", "dep:quick-xml"],
    "serde": ["dep:serde"],
    "xlsb": ["xlsx"],
    "xlsx": ["dep:zip", "dep:quick-xml"],
}
FORBIDDEN_TOP_LEVEL = {
    ".github",
    "bindings",
    "fuzz",
    "local",
    "oss-fuzz",
    "render",
    "target",
}
FORBIDDEN_RENDER_SCRIPTS = {
    "check-authored-print-parity.py",
    "check-render-fidelity-targets.py",
    "check-render-parity-baseline.py",
    "check_render_oracle_release_evidence.py",
    "check_render_package.py",
    "compare-render-parity-runs.py",
    "fetch-render-corpus.py",
    "fetch-render-fonts.py",
    "generate-render-corpus.py",
    "libreoffice-render-parity.py",
    "merge-render-parity-reports.py",
    "render-corpus-sources.json",
    "render-fonts-lock.json",
    "render-oracle-host-requirements.txt",
    "render-oracle-host-profile.xcu",
    "render-oracle-host-tools-lock.json",
    "render-oracle-host-tools.py",
    "render-oracle-lock.json",
    "render-parity-baseline-full.json",
    "render-performance-gates.py",
    "render_supply_chain.py",
    "run-render-oracle-container.py",
    "test_check_render_parity_baseline.py",
    "test_check_authored_print_parity.py",
    "test_check_render_fidelity_targets.py",
    "test_check_render_oracle_release_evidence.py",
    "test_check_render_package.py",
    "test_compare_render_parity_runs.py",
    "test_fetch_render_corpus.py",
    "test_fetch_render_fonts.py",
    "test_generate_render_corpus.py",
    "test_libreoffice_render_parity.py",
    "test_merge_render_parity_reports.py",
    "test_render_fuzz_assets.py",
    "test_render_oracle_host_tools.py",
    "test_render_oracle_container.py",
    "test_render_performance_gates.py",
    "test_render_supply_chain.py",
}


def _safe_member_name(name: str) -> PurePosixPath | None:
    path = PurePosixPath(name)
    if not name or path.is_absolute() or ".." in path.parts or "\\" in name:
        return None
    return path


def validate(crate: Path) -> tuple[list[str], dict[str, object]]:
    errors: list[str] = []
    archive_bytes = crate.stat().st_size if crate.is_file() else 0
    archive_sha256 = ""
    file_count = 0
    unpacked_bytes = 0
    package_name = None
    package_version = None
    dependencies: list[str] = []
    features: dict[str, list[str]] = {}

    if not crate.is_file():
        errors.append("crate archive is missing")
    else:
        digest = hashlib.sha256()
        with crate.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1 << 20), b""):
                digest.update(chunk)
        archive_sha256 = digest.hexdigest()
        if archive_bytes > MAX_ARCHIVE_BYTES:
            errors.append(
                f"archive bytes {archive_bytes} exceed {MAX_ARCHIVE_BYTES}"
            )
        try:
            with tarfile.open(crate, "r:gz") as package:
                members = package.getmembers()
                roots: set[str] = set()
                regular: dict[PurePosixPath, tarfile.TarInfo] = {}
                for member in members:
                    path = _safe_member_name(member.name)
                    if path is None:
                        errors.append("archive contains an unsafe member path")
                        continue
                    if path.parts:
                        roots.add(path.parts[0])
                    if member.isdir():
                        continue
                    if not member.isfile():
                        errors.append("archive contains a non-regular member")
                        continue
                    if path in regular:
                        errors.append("archive contains a duplicate member path")
                        continue
                    regular[path] = member
                    file_count += 1
                    unpacked_bytes += member.size
                if len(roots) != 1:
                    errors.append("archive must contain exactly one package root")
                    root = None
                else:
                    root = next(iter(roots))
                if file_count > MAX_FILES:
                    errors.append(f"file count {file_count} exceeds {MAX_FILES}")
                if unpacked_bytes > MAX_UNPACKED_BYTES:
                    errors.append(
                        f"unpacked bytes {unpacked_bytes} exceed {MAX_UNPACKED_BYTES}"
                    )
                if root is not None:
                    for path in regular:
                        relative = path.parts[1:]
                        if not relative:
                            errors.append("archive contains a file at its package root")
                            continue
                        if relative[0] in FORBIDDEN_TOP_LEVEL:
                            errors.append(f"forbidden package subtree: {relative[0]}")
                        if (
                            len(relative) == 2
                            and relative[0] == "scripts"
                            and relative[1] in FORBIDDEN_RENDER_SCRIPTS
                        ):
                            errors.append(
                                f"render-only script entered the core package: {relative[1]}"
                            )
                        if (
                            len(relative) >= 2
                            and relative[0] == "scripts"
                            and relative[1] == "render-oracle-container"
                        ):
                            errors.append(
                                "render-only script subtree entered the core package: "
                                "render-oracle-container"
                            )
                        basename = relative[-1].upper()
                        if basename.startswith("ROADMAP-") or basename.startswith("MIGRATION-"):
                            errors.append("internal planning document entered the package")
                    required = {
                        PurePosixPath(root, "Cargo.lock"),
                        PurePosixPath(root, "Cargo.toml"),
                        PurePosixPath(root, "Cargo.toml.orig"),
                        PurePosixPath(root, "LICENSE"),
                        PurePosixPath(root, "README.md"),
                        PurePosixPath(root, "src", "lib.rs"),
                    }
                    missing = sorted(str(path) for path in required - regular.keys())
                    if missing:
                        errors.append("required package files are missing")
                    manifest_member = regular.get(PurePosixPath(root, "Cargo.toml"))
                    if manifest_member is not None:
                        stream = package.extractfile(manifest_member)
                        manifest_bytes = stream.read() if stream is not None else b""
                        try:
                            manifest = tomllib.loads(manifest_bytes.decode("utf-8"))
                        except (UnicodeDecodeError, tomllib.TOMLDecodeError):
                            errors.append("packaged Cargo.toml is invalid")
                        else:
                            metadata = manifest.get("package", {})
                            package_name = metadata.get("name")
                            package_version = metadata.get("version")
                            if package_name != "rxls" or package_version != "0.1.2":
                                errors.append("package identity is not rxls 0.1.2")
                            if metadata.get("rust-version") != "1.85":
                                errors.append("packaged core MSRV is not 1.85")
                            dependencies = sorted(manifest.get("dependencies", {}))
                            if set(dependencies) != ALLOWED_DEPENDENCIES:
                                errors.append("core dependency profile changed")
                            for dependency in manifest.get("dependencies", {}).values():
                                if isinstance(dependency, dict) and any(
                                    key in dependency for key in ("git", "path")
                                ):
                                    errors.append("package contains a git or path dependency")
                                    break
                            raw_features = manifest.get("features", {})
                            features = {
                                str(name): [str(value) for value in values]
                                for name, values in raw_features.items()
                                if isinstance(values, list)
                            }
                            if features != EXPECTED_FEATURES:
                                errors.append("core feature semantics changed")
        except (tarfile.TarError, OSError):
            errors.append("crate archive is not a readable gzip tar package")

    report: dict[str, object] = {
        "schema": SCHEMA,
        "passed": not errors,
        "package": {"name": package_name, "version": package_version},
        "archive_sha256": archive_sha256,
        "actual": {
            "archive_bytes": archive_bytes,
            "unpacked_bytes": unpacked_bytes,
            "files": file_count,
        },
        "limits": {
            "archive_bytes": MAX_ARCHIVE_BYTES,
            "unpacked_bytes": MAX_UNPACKED_BYTES,
            "files": MAX_FILES,
        },
        "dependencies": dependencies,
        "features": features,
        "errors": errors,
    }
    return errors, report


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("crate", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    errors, report = validate(args.crate)
    rendered = json.dumps(report, ensure_ascii=True, sort_keys=True, separators=(",", ":"))
    print(rendered)
    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
