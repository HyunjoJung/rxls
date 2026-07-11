#!/usr/bin/env python3
"""Audit public release inputs for secrets, local paths, and internal traces."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import zipfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath


REPO = Path(__file__).resolve().parents[1]
SCHEMA = "rxls.public-hygiene-audit.v1"
MAX_TEXT_BYTES = 8 * 1024 * 1024
MAX_OFFICE_TEXT_BYTES = 4 * 1024 * 1024
OFFICE_SUFFIXES = {".ods", ".xlsb", ".xlsm", ".xlsx"}
BINARY_SUFFIXES = {
    ".bin",
    ".gif",
    ".ico",
    ".jpeg",
    ".jpg",
    ".pdf",
    ".png",
    ".ttf",
    ".wasm",
    ".xls",
    ".zip",
}
SKIP_DIRS = {".git", ".pytest_cache", "__pycache__", "target"}

SECRET_PATTERNS = (
    ("openai_api_key", re.compile(r"\bsk-(?:proj-)?[A-Za-z0-9_-]{20,}\b")),
    ("github_token", re.compile(r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9_]{30,}\b")),
    ("github_pat", re.compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b")),
    ("slack_token", re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{20,}\b")),
    ("aws_access_key", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    (
        "private_key",
        re.compile(r"-----BEGIN (?:RSA |DSA |EC |OPENSSH |ENCRYPTED )?PRIVATE KEY-----"),
    ),
)
LOCAL_PATH_PATTERNS = (
    ("mac_home_path", re.compile(r"(?<![A-Za-z]:)/Users/[A-Za-z0-9._-]+/")),
    ("linux_home_path", re.compile(r"/home/[A-Za-z0-9._-]+/")),
    ("windows_home_path", re.compile(r"[A-Za-z]:[/\\]Users[/\\][^/\\\s]+[/\\]")),
    (
        "windows_drive_path",
        re.compile(r"(?<![A-Za-z])[A-Za-z]:[/\\](?!Users[/\\])", re.IGNORECASE),
    ),
    ("windows_unc_path", re.compile(r"(?<!\\)\\\\[A-Za-z0-9._-]{2,}\\[^\\\s]{2,}\\")),
)
INTERNAL_TRACE_PATTERNS = (
    (
        "internal_docs_trace",
        re.compile("rxls" + r"[-_]internal[-_]docs", re.IGNORECASE),
    ),
    (
        "private_project_trace",
        re.compile("pps" + r"[-_]procurement[-_]ai[-_]kr[-_]bid", re.IGNORECASE),
    ),
    ("claude_project_trace", re.compile(r"[.]claude[/\\]projects", re.IGNORECASE)),
    ("private_workspace_trace", re.compile("cong" + "mo", re.IGNORECASE)),
)


@dataclass(frozen=True)
class Finding:
    path: str
    line: int | None
    kind: str
    detail: str

    def as_dict(self) -> dict[str, object]:
        return {
            "path": self.path,
            "line": self.line,
            "kind": self.kind,
            "detail": self.detail,
        }


def git_files(repo: Path = REPO) -> list[Path]:
    result = subprocess.run(
        ["git", "ls-files", "-co", "--exclude-standard", "-z"],
        cwd=repo,
        check=True,
        capture_output=True,
    )
    return sorted(
        repo / raw.decode("utf-8", "surrogateescape")
        for raw in result.stdout.split(b"\0")
        if raw
    )


def scan_text(path: str, text: str) -> list[Finding]:
    findings: list[Finding] = []
    patterns = SECRET_PATTERNS + LOCAL_PATH_PATTERNS + INTERNAL_TRACE_PATTERNS
    for line_number, line in enumerate(text.splitlines(), start=1):
        for kind, pattern in patterns:
            if pattern.search(line):
                findings.append(Finding(path, line_number, kind, "public release blocker"))
    return findings


def decode_office_text(data: bytes) -> str | None:
    for encoding in ("utf-8-sig", "utf-16"):
        try:
            return data.decode(encoding)
        except UnicodeDecodeError:
            pass
    return None


def scan_office_package(path: Path, display_path: str) -> list[Finding]:
    findings: list[Finding] = []
    try:
        with zipfile.ZipFile(path) as archive:
            for info in archive.infolist():
                normalized_name = info.filename.replace("\\", "/")
                member = PurePosixPath(normalized_name)
                member_path = f"{display_path}::{info.filename}"
                findings.extend(scan_text(member_path, info.filename))
                if (
                    member.is_absolute()
                    or ".." in member.parts
                    or re.match(r"^[A-Za-z]:/", normalized_name)
                ):
                    findings.append(
                        Finding(member_path, None, "unsafe_office_member", "unsafe ZIP member path")
                    )
                lowered = info.filename.lower()
                if not (lowered.endswith(".xml") or lowered.endswith(".rels")):
                    continue
                if info.file_size > MAX_OFFICE_TEXT_BYTES:
                    findings.append(
                        Finding(member_path, None, "office_text_too_large", "Office text part exceeds audit limit")
                    )
                    continue
                text = decode_office_text(archive.read(info))
                if text is None:
                    findings.append(
                        Finding(member_path, None, "invalid_office_text", "Office text part is not UTF-8 or UTF-16")
                    )
                else:
                    findings.extend(scan_text(member_path, text))
    except zipfile.BadZipFile:
        findings.append(
            Finding(display_path, None, "invalid_office_package", "Office package is not a ZIP archive")
        )
    return findings


def audit_file(path: Path, repo: Path = REPO) -> list[Finding]:
    relative = path.relative_to(repo).as_posix()
    findings = scan_text(relative, relative)
    if any(part in SKIP_DIRS for part in path.relative_to(repo).parts) or not path.is_file():
        return findings
    suffix = path.suffix.lower()
    if suffix in OFFICE_SUFFIXES:
        return findings + scan_office_package(path, relative)
    if suffix in BINARY_SUFFIXES:
        return findings
    if path.stat().st_size > MAX_TEXT_BYTES:
        return findings + [
            Finding(relative, None, "text_file_too_large", "text file exceeds audit limit")
        ]
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return findings + [
            Finding(relative, None, "non_utf8_text", "non-binary file is not UTF-8")
        ]
    return findings + scan_text(relative, text)


def audit(repo: Path = REPO) -> list[Finding]:
    findings = [finding for path in git_files(repo) for finding in audit_file(path, repo)]
    return sorted(findings, key=lambda item: (item.path, item.line or 0, item.kind))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    findings = audit()
    if args.json:
        print(
            json.dumps(
                {
                    "schema": SCHEMA,
                    "passed": not findings,
                    "findings": [finding.as_dict() for finding in findings],
                },
                indent=2,
                sort_keys=True,
            )
        )
    elif findings:
        for finding in findings:
            location = finding.path + (f":{finding.line}" if finding.line else "")
            print(f"{location}: {finding.kind}: {finding.detail}", file=sys.stderr)
    return 1 if findings else 0


if __name__ == "__main__":
    raise SystemExit(main())
