#!/usr/bin/env python3
"""Validate and record exact release/fuzz tool versions as release evidence."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Callable, Sequence


SCHEMA = "rxls.release-toolchain-versions.v1"


def _version(
    command: list[str], runner: Callable[..., subprocess.CompletedProcess[str]]
) -> str:
    completed = runner(
        command,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return completed.stdout.strip()


def record_versions(
    release_rust: str,
    fuzz_nightly: str,
    cargo_fuzz: str,
    output: Path,
    runner: Callable[..., subprocess.CompletedProcess[str]] = subprocess.run,
) -> dict[str, object]:
    release_rustc = _version(["rustc", f"+{release_rust}", "--version"], runner)
    fuzz_rustc = _version(["rustc", f"+{fuzz_nightly}", "--version"], runner)
    cargo_fuzz_output = _version(["cargo", "fuzz", "--version"], runner)
    if not release_rustc.startswith(f"rustc {release_rust} "):
        raise ValueError(f"release rustc differs from {release_rust}: {release_rustc}")
    if "-nightly " not in fuzz_rustc:
        raise ValueError(f"fuzz rustc is not a nightly: {fuzz_rustc}")
    if cargo_fuzz_output != f"cargo-fuzz {cargo_fuzz}":
        raise ValueError(
            f"cargo-fuzz differs from {cargo_fuzz}: {cargo_fuzz_output}"
        )

    payload: dict[str, object] = {
        "schema": SCHEMA,
        "release_rust": {"toolchain": release_rust, "rustc": release_rustc},
        "fuzz_nightly": {"toolchain": fuzz_nightly, "rustc": fuzz_rustc},
        "cargo_fuzz": cargo_fuzz_output,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return payload


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release-rust", required=True)
    parser.add_argument("--fuzz-nightly", required=True)
    parser.add_argument("--cargo-fuzz", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        record_versions(
            args.release_rust,
            args.fuzz_nightly,
            args.cargo_fuzz,
            args.output,
        )
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"toolchain evidence error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
