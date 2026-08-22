#!/usr/bin/env python3
"""Create a deterministic release artifact manifest with SHA-256 checksums."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path


SCHEMA = "rxls.release-manifest.v1"
HYGIENE_SCHEMA = "rxls.public-hygiene-audit.v1"
MANIFEST_NAME = "rxls-release-manifest.json"
PRERELEASE_IDENTIFIER = r"(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
BUILD_IDENTIFIER = r"[0-9A-Za-z-]+"
SEMVER = re.compile(
    rf"(?:0|[1-9]\d*)[.](?:0|[1-9]\d*)[.](?:0|[1-9]\d*)"
    rf"(?:-{PRERELEASE_IDENTIFIER}(?:[.]{PRERELEASE_IDENTIFIER})*)?"
    rf"(?:[+]{BUILD_IDENTIFIER}(?:[.]{BUILD_IDENTIFIER})*)?"
)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def artifact_record(path: Path) -> dict[str, object]:
    return {
        "name": path.name,
        "path": path.as_posix(),
        "bytes": path.stat().st_size,
        "sha256": sha256_file(path),
    }


def hygiene_record(path: Path) -> dict[str, object]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("schema") != HYGIENE_SCHEMA:
        raise ValueError("unexpected public hygiene report schema")
    if payload.get("passed") is not True or payload.get("findings") != []:
        raise ValueError("public hygiene report did not pass")
    return artifact_record(path)


def release_manifest(
    artifacts: list[Path], version: str, git_rev: str, hygiene_report: Path
) -> dict[str, object]:
    if SEMVER.fullmatch(version) is None:
        raise ValueError("version is not valid SemVer")
    if not re.fullmatch(r"[0-9a-fA-F]{40}|[0-9a-fA-F]{64}", git_rev):
        raise ValueError("git revision must be a full hexadecimal object ID")
    if not artifacts:
        raise ValueError("at least one artifact is required")
    paths = [Path(path) for path in artifacts]
    if len(set(paths)) != len(paths):
        raise ValueError("artifact paths must be unique")
    missing = [path.as_posix() for path in paths + [hygiene_report] if not path.is_file()]
    if missing:
        raise FileNotFoundError("missing release input: " + ", ".join(missing))
    return {
        "schema": SCHEMA,
        "version": version,
        "git_rev": git_rev.lower(),
        "artifacts": [artifact_record(path) for path in sorted(paths, key=lambda item: item.as_posix())],
        "evidence": {"public_hygiene": hygiene_record(hygiene_report)},
    }


def _verify_artifact_record(
    record: object, files: dict[str, Path], label: str
) -> str:
    if not isinstance(record, dict):
        raise ValueError(f"{label} record must be an object")
    name = record.get("name")
    if (
        not isinstance(name, str)
        or not name
        or name in {".", ".."}
        or "/" in name
        or "\\" in name
        or Path(name).name != name
    ):
        raise ValueError(f"{label} record has an invalid name")
    path = files.get(name)
    if path is None:
        raise ValueError(f"release bundle is missing {name}")
    if record.get("bytes") != path.stat().st_size:
        raise ValueError(f"release bundle size differs for {name}")
    digest = record.get("sha256")
    if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise ValueError(f"{label} record has an invalid SHA-256 for {name}")
    if digest != sha256_file(path):
        raise ValueError(f"release bundle SHA-256 differs for {name}")
    return name


def verify_release_bundle(
    root: Path,
    *,
    expected_files: int | None = None,
    version: str | None = None,
    git_rev: str | None = None,
) -> dict[str, object]:
    """Verify flat bundle coverage and every manifest size and SHA-256 record."""
    if not root.is_dir():
        raise ValueError(f"release bundle is not a directory: {root}")
    entries = list(root.iterdir())
    non_files = sorted(path.name for path in entries if not path.is_file())
    if non_files:
        raise ValueError(f"release bundle must be flat; non-files found: {non_files}")
    files = {path.name: path for path in entries}
    if expected_files is not None:
        if expected_files <= 0:
            raise ValueError("expected release bundle file count must be positive")
        if len(files) != expected_files:
            raise ValueError(
                f"release bundle has {len(files)} files; expected {expected_files}"
            )
    manifest_path = files.get(MANIFEST_NAME)
    if manifest_path is None:
        raise ValueError(f"release bundle is missing {MANIFEST_NAME}")
    payload = json.loads(manifest_path.read_text(encoding="utf-8"))
    if payload.get("schema") != SCHEMA:
        raise ValueError("unexpected release manifest schema")
    if version is not None and payload.get("version") != version:
        raise ValueError("release manifest version differs from the expected version")
    if git_rev is not None and payload.get("git_rev") != git_rev.lower():
        raise ValueError("release manifest revision differs from the expected revision")

    artifacts = payload.get("artifacts")
    if not isinstance(artifacts, list) or not artifacts:
        raise ValueError("release manifest artifacts must be a non-empty array")
    names = [
        _verify_artifact_record(record, files, "artifact") for record in artifacts
    ]
    evidence = payload.get("evidence")
    if not isinstance(evidence, dict):
        raise ValueError("release manifest evidence must be an object")
    hygiene = evidence.get("public_hygiene")
    hygiene_name = _verify_artifact_record(hygiene, files, "public hygiene")
    if len(set(names + [hygiene_name])) != len(names) + 1:
        raise ValueError("release manifest repeats an artifact name")
    covered = set(names) | {hygiene_name, MANIFEST_NAME}
    if covered != set(files):
        raise ValueError(
            "release manifest coverage differs: "
            f"missing={sorted(set(files) - covered)} "
            f"extra={sorted(covered - set(files))}"
        )
    hygiene_payload = json.loads(files[hygiene_name].read_text(encoding="utf-8"))
    if (
        hygiene_payload.get("schema") != HYGIENE_SCHEMA
        or hygiene_payload.get("passed") is not True
        or hygiene_payload.get("findings") != []
    ):
        raise ValueError("public hygiene evidence did not pass")
    return payload


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", nargs="*", type=Path)
    parser.add_argument("--version")
    parser.add_argument("--git-rev")
    parser.add_argument("--hygiene-report", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--verify-bundle", type=Path)
    parser.add_argument("--expected-files", type=int)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        if args.verify_bundle is not None:
            if args.artifacts or args.hygiene_report is not None or args.output is not None:
                raise ValueError(
                    "bundle verification cannot be combined with manifest generation"
                )
            verify_release_bundle(
                args.verify_bundle,
                expected_files=args.expected_files,
                version=args.version,
                git_rev=args.git_rev,
            )
            print(
                f"release bundle: files={len(list(args.verify_bundle.iterdir()))} "
                "coverage=ok checksums=ok"
            )
            return 0
        if args.expected_files is not None:
            raise ValueError("--expected-files requires --verify-bundle")
        if not args.artifacts or not args.version or not args.git_rev:
            raise ValueError(
                "manifest generation requires artifacts, --version, and --git-rev"
            )
        if args.hygiene_report is None:
            raise ValueError("manifest generation requires --hygiene-report")
        payload = release_manifest(
            args.artifacts, args.version, args.git_rev, args.hygiene_report
        )
        rendered = json.dumps(payload, indent=2, sort_keys=True) + "\n"
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(rendered, encoding="utf-8")
        else:
            sys.stdout.write(rendered)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"release_manifest: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
