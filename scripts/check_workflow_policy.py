#!/usr/bin/env python3
"""Enforce immutable GitHub Actions and reproducible release tool versions."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


ACTION_RE = re.compile(
    r"^\s*(?:-\s*)?uses:\s*[\"']?(?P<spec>[^\s\"'#]+)[\"']?"
    r"(?:\s+#\s*(?P<comment>.+?))?\s*$"
)
FULL_SHA_RE = re.compile(r"[0-9a-f]{40}")
REMOTE_ACTION_RE = re.compile(r"[^/\s]+/[^@\s]+@.+")
RELEASE_VERSIONS = {
    "RELEASE_RUST_VERSION": "1.96.1",
    "FUZZ_NIGHTLY_VERSION": "nightly-2026-07-10",
    "CARGO_FUZZ_VERSION": "0.13.2",
}


def audit_action_pins(path: Path, text: str) -> list[str]:
    """Return policy violations for remote action references in one workflow."""

    errors: list[str] = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        if "uses:" not in line:
            continue
        match = ACTION_RE.match(line)
        if match is None:
            errors.append(f"{path}:{line_number}: unreadable uses entry")
            continue
        spec = match.group("spec")
        if spec.startswith("./"):
            continue
        if not REMOTE_ACTION_RE.fullmatch(spec):
            errors.append(f"{path}:{line_number}: invalid remote action {spec!r}")
            continue
        action, ref = spec.rsplit("@", 1)
        if FULL_SHA_RE.fullmatch(ref) is None:
            errors.append(
                f"{path}:{line_number}: {action} must use a full immutable commit SHA"
            )
        comment = match.group("comment")
        if comment is None or not comment.strip():
            errors.append(
                f"{path}:{line_number}: pinned action {action} needs a version comment"
            )
    return errors


def _cargo_fuzz_commands(text: str) -> list[tuple[int, str]]:
    lines = text.splitlines()
    commands: list[tuple[int, str]] = []
    index = 0
    while index < len(lines):
        line = lines[index]
        if re.search(r"\bcargo\s+install\s+cargo-fuzz\b", line):
            start = index + 1
            command = line.strip()
            while command.rstrip().endswith("\\") and index + 1 < len(lines):
                index += 1
                command = command.rstrip()[:-1] + " " + lines[index].strip()
            commands.append((start, command))
        index += 1
    return commands


def _audit_exact_assignments(
    path: Path, text: str, names: tuple[str, ...]
) -> list[str]:
    errors: list[str] = []
    for name in names:
        expected = RELEASE_VERSIONS[name]
        assignment = re.compile(
            rf"^\s*{re.escape(name)}:\s*[\"']?{re.escape(expected)}[\"']?\s*$",
            re.MULTILINE,
        )
        if assignment.search(text) is None:
            errors.append(f"{path}: expected exact {name}={expected}")
    return errors


def audit_fuzz_tools(
    path: Path, text: str, required_assignments: tuple[str, ...]
) -> list[str]:
    """Return violations for a workflow that installs and invokes cargo-fuzz."""

    errors = _audit_exact_assignments(path, text, required_assignments)

    commands = _cargo_fuzz_commands(text)
    if not commands:
        errors.append(f"{path}: fuzzing workflow must install cargo-fuzz")
    errors.extend(audit_tool_commands(path, text))

    return errors


def audit_tool_commands(path: Path, text: str) -> list[str]:
    """Reject mutable nightly/cargo-fuzz commands in any hosted workflow."""

    errors: list[str] = []
    for line_number, command in _cargo_fuzz_commands(text):
        if not re.search(
            r"--version(?:=|\s+)(?:[\"']?\$\{?CARGO_FUZZ_VERSION\}?|"
            + re.escape(RELEASE_VERSIONS["CARGO_FUZZ_VERSION"])
            + r")[\"']?(?:\s|$)",
            command,
        ):
            errors.append(
                f"{path}:{line_number}: cargo-fuzz install must use exact version "
                f'{RELEASE_VERSIONS["CARGO_FUZZ_VERSION"]}'
            )

    if re.search(r"rustup\s+toolchain\s+install\s+nightly(?:\s|$)", text):
        errors.append(f"{path}: workflow must not install mutable nightly")
    if re.search(r"cargo\s+\+nightly(?:\s|$)", text):
        errors.append(f"{path}: workflow must not invoke mutable nightly")
    return errors


def audit_release_versions(path: Path, text: str) -> list[str]:
    """Return violations for release toolchain and cargo-fuzz version pins."""

    return audit_fuzz_tools(path, text, tuple(RELEASE_VERSIONS))


def audit_fuzz_workflow(path: Path, text: str) -> list[str]:
    """Return violations for the standalone hosted fuzz workflow."""

    return audit_fuzz_tools(
        path, text, ("FUZZ_NIGHTLY_VERSION", "CARGO_FUZZ_VERSION")
    )


def audit_repository(root: Path) -> list[str]:
    workflow_root = root / ".github" / "workflows"
    workflows = sorted((*workflow_root.glob("*.yml"), *workflow_root.glob("*.yaml")))
    if not workflows:
        return [f"{workflow_root}: no workflows found"]

    errors: list[str] = []
    for path in workflows:
        text = path.read_text(encoding="utf-8")
        errors.extend(audit_action_pins(path.relative_to(root), text))
        relative = path.relative_to(root)
        if path.name == "fuzz.yml":
            errors.extend(audit_fuzz_workflow(relative, text))
        elif path.name != "release.yml":
            errors.extend(audit_tool_commands(relative, text))

    release = workflow_root / "release.yml"
    if not release.is_file():
        errors.append(f"{release.relative_to(root)}: missing release workflow")
    else:
        errors.extend(
            audit_release_versions(
                release.relative_to(root), release.read_text(encoding="utf-8")
            )
        )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root", type=Path, default=Path(__file__).resolve().parents[1]
    )
    args = parser.parse_args()

    errors = audit_repository(args.root.resolve())
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print("workflow policy passed: immutable action SHAs and exact release tools")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
