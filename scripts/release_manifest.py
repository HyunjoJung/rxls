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


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", nargs="+", type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--git-rev", required=True)
    parser.add_argument("--hygiene-report", required=True, type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
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
