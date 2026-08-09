#!/usr/bin/env python3
"""Run and verify the exact crates.io publication dry-run evidence contract."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any, Callable, Sequence


SCHEMA = "rxls.cargo-publish-dry-run.v1"
REGISTRY = "crates-io"
PACKAGE_NAME = "rxls"
RECEIPT_NAME = "release-cargo-publish-dry-run.json"
PUBLISH_ARGV = (
    "cargo",
    "publish",
    "--dry-run",
    "--locked",
    "--registry",
    REGISTRY,
)
SHA_RE = re.compile(r"[0-9a-f]{40}")
DIGEST_RE = re.compile(r"[0-9a-f]{64}")
VERSION_RE = re.compile(
    r"(?:0|[1-9][0-9]*)"
    r"\.(?:0|[1-9][0-9]*)"
    r"\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
RELEASE_RE = re.compile(r"[0-9A-Za-z][0-9A-Za-z.+-]*")
DATE_RE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}")
HOST_RE = re.compile(r"[0-9A-Za-z][0-9A-Za-z_.-]*")


class EvidenceError(ValueError):
    """Raised when dry-run evidence is absent, malformed, or inconsistent."""


Runner = Callable[..., subprocess.CompletedProcess[str]]


def _require(condition: bool, label: str) -> None:
    if not condition:
        raise EvidenceError(label)


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise EvidenceError("receipt_duplicate_key")
        result[key] = value
    return result


def _reject_json_constant(_value: str) -> None:
    raise EvidenceError("receipt_non_finite_number")


def _load_receipt(path: Path) -> object:
    try:
        payload = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise EvidenceError("receipt_read") from error
    try:
        return json.loads(
            payload,
            object_pairs_hook=_strict_object,
            parse_constant=_reject_json_constant,
        )
    except (json.JSONDecodeError, TypeError) as error:
        raise EvidenceError("receipt_json") from error


def _manifest_version(manifest_path: Path) -> str:
    try:
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise EvidenceError("manifest_read") from error
    package = manifest.get("package")
    _require(isinstance(package, dict), "manifest_package")
    _require(package.get("name") == PACKAGE_NAME, "manifest_package_name")
    version = package.get("version")
    _require(
        isinstance(version, str) and VERSION_RE.fullmatch(version) is not None,
        "manifest_version",
    )
    return version


def _validate_git_sha(git_sha: str) -> None:
    _require(SHA_RE.fullmatch(git_sha) is not None, "git_rev")


def _crate_record(crate_path: Path, version: str) -> dict[str, object]:
    expected_name = f"{PACKAGE_NAME}-{version}.crate"
    _require(crate_path.name == expected_name, "crate_name")
    try:
        payload = crate_path.read_bytes()
    except OSError as error:
        raise EvidenceError("crate_read") from error
    _require(bool(payload), "crate_bytes")
    return {
        "name": expected_name,
        "bytes": len(payload),
        "sha256": hashlib.sha256(payload).hexdigest(),
    }


def _command_stdout(
    argv: Sequence[str],
    *,
    cwd: Path,
    runner: Runner,
    failure_label: str,
) -> str:
    try:
        completed = runner(
            list(argv),
            cwd=cwd,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except OSError as error:
        raise EvidenceError(failure_label) from error
    _require(completed.returncode == 0, failure_label)
    _require(isinstance(completed.stdout, str), failure_label)
    return completed.stdout


def _tool_identity(tool: str, *, cwd: Path, runner: Runner) -> dict[str, str]:
    output = _command_stdout(
        [tool, "--version", "--verbose"],
        cwd=cwd,
        runner=runner,
        failure_label=f"{tool}_identity",
    )
    fields: dict[str, str] = {}
    for line in output.splitlines()[1:]:
        key, separator, value = line.partition(":")
        if not separator:
            continue
        normalized = key.strip().lower().replace("-", "_")
        if normalized in {"release", "commit_hash", "commit_date", "host"}:
            _require(normalized not in fields, f"{tool}_identity")
            fields[normalized] = value.strip()
    _require(
        set(fields) == {"release", "commit_hash", "commit_date", "host"},
        f"{tool}_identity",
    )
    _require(RELEASE_RE.fullmatch(fields["release"]) is not None, f"{tool}_release")
    _require(SHA_RE.fullmatch(fields["commit_hash"]) is not None, f"{tool}_commit")
    _require(DATE_RE.fullmatch(fields["commit_date"]) is not None, f"{tool}_date")
    _require(HOST_RE.fullmatch(fields["host"]) is not None, f"{tool}_host")
    return fields


def _toolchain_identity(*, cwd: Path, runner: Runner) -> dict[str, object]:
    cargo = _tool_identity("cargo", cwd=cwd, runner=runner)
    rustc = _tool_identity("rustc", cwd=cwd, runner=runner)
    _require(cargo["release"] == rustc["release"], "toolchain_release")
    _require(cargo["host"] == rustc["host"], "toolchain_host")
    return {"cargo": cargo, "rustc": rustc}


def _validate_string_map(
    value: object, expected: dict[str, str], label: str
) -> None:
    _require(isinstance(value, dict), label)
    _require(set(value) == set(expected), f"{label}.keys")
    for key, expected_value in expected.items():
        _require(type(value.get(key)) is str, f"{label}.{key}.type")
        _require(value[key] == expected_value, f"{label}.{key}")


def validate_receipt(
    receipt: object,
    *,
    version: str,
    git_sha: str,
    crate: dict[str, object],
    toolchain: dict[str, object],
) -> None:
    """Validate strict receipt types and every immutable release binding."""

    _validate_git_sha(git_sha)
    _require(isinstance(receipt, dict), "receipt_type")
    _require(
        set(receipt)
        == {
            "schema",
            "version",
            "git_rev",
            "registry",
            "argv",
            "crate",
            "toolchain",
            "passed",
        },
        "receipt_keys",
    )
    for key, expected in (
        ("schema", SCHEMA),
        ("version", version),
        ("git_rev", git_sha),
        ("registry", REGISTRY),
    ):
        _require(type(receipt.get(key)) is str, f"receipt_{key}_type")
        _require(receipt[key] == expected, f"receipt_{key}")
    _require(
        isinstance(receipt.get("argv"), list)
        and all(type(item) is str for item in receipt["argv"]),
        "receipt_argv_type",
    )
    _require(receipt["argv"] == list(PUBLISH_ARGV), "receipt_argv")
    _require(type(receipt.get("passed")) is bool, "receipt_passed_type")
    _require(receipt["passed"] is True, "receipt_passed")

    receipt_crate = receipt.get("crate")
    _require(isinstance(receipt_crate, dict), "receipt_crate")
    _require(set(receipt_crate) == {"name", "bytes", "sha256"}, "receipt_crate_keys")
    _require(type(receipt_crate.get("name")) is str, "receipt_crate_name_type")
    _require(type(receipt_crate.get("bytes")) is int, "receipt_crate_bytes_type")
    _require(type(receipt_crate.get("sha256")) is str, "receipt_crate_sha256_type")
    _require(receipt_crate == crate, "receipt_crate_binding")
    _require(
        DIGEST_RE.fullmatch(receipt_crate["sha256"]) is not None,
        "receipt_crate_sha256",
    )

    receipt_toolchain = receipt.get("toolchain")
    _require(isinstance(receipt_toolchain, dict), "receipt_toolchain")
    _require(set(receipt_toolchain) == {"cargo", "rustc"}, "receipt_toolchain_keys")
    expected_cargo = toolchain.get("cargo")
    expected_rustc = toolchain.get("rustc")
    _require(isinstance(expected_cargo, dict), "toolchain_cargo")
    _require(isinstance(expected_rustc, dict), "toolchain_rustc")
    _validate_string_map(receipt_toolchain.get("cargo"), expected_cargo, "receipt_cargo")
    _validate_string_map(receipt_toolchain.get("rustc"), expected_rustc, "receipt_rustc")


def run_and_write(
    manifest_path: Path,
    git_sha: str,
    output_path: Path,
    *,
    runner: Runner = subprocess.run,
) -> dict[str, object]:
    """Run the one authorized Cargo argv and atomically write its receipt."""

    _validate_git_sha(git_sha)
    version = _manifest_version(manifest_path)
    root = manifest_path.parent
    crate_path = root / "target" / "package" / f"{PACKAGE_NAME}-{version}.crate"
    expected_output = crate_path.parent / RECEIPT_NAME
    _require(
        output_path.resolve(strict=False) == expected_output.resolve(strict=False),
        "receipt_output_location",
    )
    try:
        output_path.unlink(missing_ok=True)
    except OSError as error:
        raise EvidenceError("receipt_output_reset") from error

    toolchain = _toolchain_identity(cwd=root, runner=runner)
    _command_stdout(
        PUBLISH_ARGV,
        cwd=root,
        runner=runner,
        failure_label="cargo_publish_dry_run",
    )
    crate = _crate_record(crate_path, version)
    receipt: dict[str, object] = {
        "schema": SCHEMA,
        "version": version,
        "git_rev": git_sha,
        "registry": REGISTRY,
        "argv": list(PUBLISH_ARGV),
        "crate": crate,
        "toolchain": toolchain,
        "passed": True,
    }
    validate_receipt(
        receipt,
        version=version,
        git_sha=git_sha,
        crate=crate,
        toolchain=toolchain,
    )
    encoded = json.dumps(receipt, indent=2, sort_keys=True) + "\n"
    temporary = output_path.with_name(f".{output_path.name}.tmp")
    try:
        temporary.write_text(encoded, encoding="utf-8")
        temporary.replace(output_path)
    except OSError as error:
        try:
            temporary.unlink(missing_ok=True)
        except OSError:
            pass
        raise EvidenceError("receipt_write") from error
    return receipt


def verify_file(
    manifest_path: Path,
    git_sha: str,
    receipt_path: Path,
    *,
    runner: Runner = subprocess.run,
) -> dict[str, object]:
    """Verify a receipt and the crate archive adjacent to it."""

    _validate_git_sha(git_sha)
    _require(receipt_path.name == RECEIPT_NAME, "receipt_name")
    version = _manifest_version(manifest_path)
    crate_path = receipt_path.parent / f"{PACKAGE_NAME}-{version}.crate"
    crate = _crate_record(crate_path, version)
    toolchain = _toolchain_identity(cwd=manifest_path.parent, runner=runner)
    receipt = _load_receipt(receipt_path)
    validate_receipt(
        receipt,
        version=version,
        git_sha=git_sha,
        crate=crate,
        toolchain=toolchain,
    )
    assert isinstance(receipt, dict)
    return receipt


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="operation", required=True)
    run_parser = subparsers.add_parser("run")
    run_parser.add_argument("--manifest", type=Path, required=True)
    run_parser.add_argument("--git-sha", required=True)
    run_parser.add_argument("--output", type=Path, required=True)
    verify_parser = subparsers.add_parser("verify")
    verify_parser.add_argument("--manifest", type=Path, required=True)
    verify_parser.add_argument("--git-sha", required=True)
    verify_parser.add_argument("--receipt", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        if args.operation == "run":
            run_and_write(args.manifest, args.git_sha, args.output)
        else:
            verify_file(args.manifest, args.git_sha, args.receipt)
    except EvidenceError as error:
        print(f"cargo publish evidence error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
