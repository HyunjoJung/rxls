#!/usr/bin/env python3
"""Smoke the exact packaged or published rxls crate outside its checkout."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from pathlib import Path, PurePosixPath


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_FIXTURE = ROOT / "tests" / "fixtures" / "xlsx" / "reader-structural.xlsx"
WINDOWS_DEVICE_NAMES = {
    "AUX",
    "CLOCK$",
    "CON",
    "NUL",
    "PRN",
    *(f"COM{index}" for index in range(1, 10)),
    *(f"LPT{index}" for index in range(1, 10)),
}


class SmokeError(RuntimeError):
    """A distribution smoke contract failed."""


def _safe_member_target(member_name: str, destination: Path) -> tuple[PurePosixPath, Path]:
    if "\\" in member_name or "\0" in member_name:
        raise SmokeError(f"unsafe crate member path: {member_name!r}")

    path = PurePosixPath(member_name)
    if path.is_absolute() or ".." in path.parts or not path.parts:
        raise SmokeError(f"unsafe crate member path: {member_name!r}")
    for part in path.parts:
        if ":" in part or part != part.rstrip(" ."):
            raise SmokeError(f"unsafe Windows crate member path: {member_name!r}")
        device_name = part.split(".", 1)[0].rstrip(" ").upper()
        if device_name in WINDOWS_DEVICE_NAMES:
            raise SmokeError(f"unsafe Windows device path: {member_name!r}")

    destination = destination.resolve()
    target = destination.joinpath(*path.parts).resolve(strict=False)
    try:
        target.relative_to(destination)
    except ValueError as error:
        raise SmokeError(f"crate member escapes extraction root: {member_name!r}") from error
    return path, target


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Build an external Rust consumer and install/run the rxls CLI from "
            "an exact local .crate or crates.io version."
        )
    )
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--crate", type=Path, help="exact local .crate archive")
    source.add_argument(
        "--registry-version",
        help="exact published crates.io version, for example 0.1.2",
    )
    parser.add_argument(
        "--fixture",
        type=Path,
        default=DEFAULT_FIXTURE,
        help="workbook copied into the isolated smoke directory",
    )
    parser.add_argument(
        "--cargo",
        default=os.environ.get("CARGO", "cargo"),
        help="cargo executable (default: CARGO or cargo)",
    )
    parser.add_argument("--write-report", type=Path, help="optional JSON report path")
    return parser.parse_args(argv)


def _safe_extract_crate(crate: Path, destination: Path) -> Path:
    if not crate.is_file():
        raise SmokeError(f"crate archive does not exist: {crate}")

    with tarfile.open(crate, "r:gz") as archive:
        members = archive.getmembers()
        if not members:
            raise SmokeError("crate archive is empty")
        roots: set[str] = set()
        validated: list[tuple[tarfile.TarInfo, Path]] = []
        for member in members:
            path, target = _safe_member_target(member.name, destination)
            if member.issym() or member.islnk():
                raise SmokeError(f"crate archive contains a link: {member.name!r}")
            if not member.isfile() and not member.isdir():
                raise SmokeError(f"crate archive contains a special entry: {member.name!r}")
            roots.add(path.parts[0])
            validated.append((member, target))
        if len(roots) != 1:
            raise SmokeError("crate archive must contain exactly one package root")
        for member, target in validated:
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            source = archive.extractfile(member)
            if source is None:
                raise SmokeError(f"crate archive member has no file data: {member.name!r}")
            with source, target.open("wb") as output:
                shutil.copyfileobj(source, output)

    package_root = destination / next(iter(roots))
    if not (package_root / "Cargo.toml").is_file():
        raise SmokeError("crate archive package root has no Cargo.toml")
    return package_root


def _package_version(package_root: Path) -> str:
    manifest = tomllib.loads((package_root / "Cargo.toml").read_text(encoding="utf-8"))
    package = manifest.get("package", {})
    if package.get("name") != "rxls":
        raise SmokeError("crate archive package name is not rxls")
    version = package.get("version")
    if not isinstance(version, str) or not version:
        raise SmokeError("crate archive has no package version")
    return version


def _toml_path(path: Path) -> str:
    return path.as_posix().replace('"', '\\"')


def _write_consumer(consumer: Path, dependency: str) -> None:
    (consumer / "src").mkdir(parents=True)
    (consumer / "Cargo.toml").write_text(
        """[package]
name = "rxls-distribution-smoke"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
rxls = { DEPENDENCY, default-features = false, features = ["xlsx"] }
""".replace("DEPENDENCY", dependency),
        encoding="utf-8",
    )
    (consumer / "src" / "main.rs").write_text(
        r'''use rxls::{Cell, Workbook};

fn main() {
    let path = std::env::args().nth(1).expect("fixture path");
    let bytes = std::fs::read(path).expect("read fixture");
    let workbook = Workbook::open(&bytes).expect("open fixture through packaged rxls");
    assert!(!workbook.sheets.is_empty(), "fixture has no sheets");

    let mut authored = Workbook::new();
    authored.add_sheet("Smoke").write(0, 0, "packaged");
    let encoded = authored.to_xlsx_checked().expect("author xlsx");
    let reopened = Workbook::open(&encoded).expect("reopen authored xlsx");
    assert_eq!(
        reopened.sheets[0].cell(0, 0),
        Some(&Cell::Text("packaged".into()))
    );
    println!("rxls external consumer ok: sheets={}", workbook.sheets.len());
}
''',
        encoding="utf-8",
    )


def _run(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    expected_code: int = 0,
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != expected_code:
        rendered = " ".join(command)
        raise SmokeError(
            f"command returned {result.returncode}, expected {expected_code}: {rendered}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def _installed_binary(install_root: Path) -> Path:
    for name in ("rxls", "rxls.exe", "rxls.cmd"):
        candidate = install_root / "bin" / name
        if candidate.is_file():
            return candidate
    raise SmokeError("cargo install did not produce an rxls executable")


def smoke(args: argparse.Namespace) -> dict[str, object]:
    fixture = args.fixture.resolve()
    if not fixture.is_file():
        raise SmokeError(f"fixture does not exist: {fixture}")

    with tempfile.TemporaryDirectory(prefix="rxls-distribution-smoke-") as temporary:
        work = Path(temporary).resolve()
        source_root = work / "source"
        source_root.mkdir()

        if args.crate is not None:
            package_root = _safe_extract_crate(args.crate.resolve(), source_root)
            version = _package_version(package_root)
            dependency = f'path = "{_toml_path(package_root)}"'
            install_command = [
                args.cargo,
                "install",
                "--path",
                str(package_root),
                "--root",
                str(work / "install"),
                "--locked",
                "--force",
            ]
            mode = "local-crate"
            source_name = args.crate.name
        else:
            version = args.registry_version
            if not version or version.startswith(("<", ">", "=", "~", "^")):
                raise SmokeError("--registry-version must be a plain exact version")
            dependency = f'version = "={version}"'
            install_command = [
                args.cargo,
                "install",
                "rxls",
                "--version",
                f"={version}",
                "--root",
                str(work / "install"),
                "--locked",
                "--force",
            ]
            mode = "registry"
            source_name = f"rxls-{version}"

        isolated_fixture = work / f"fixture{fixture.suffix}"
        shutil.copyfile(fixture, isolated_fixture)
        consumer = work / "consumer"
        _write_consumer(consumer, dependency)

        env = os.environ.copy()
        env["CARGO_TARGET_DIR"] = str(work / "cargo-target")
        env["CARGO_TERM_COLOR"] = "never"

        _run(
            [args.cargo, "generate-lockfile", "--manifest-path", str(consumer / "Cargo.toml")],
            cwd=work,
            env=env,
        )
        consumer_result = _run(
            [
                args.cargo,
                "run",
                "--quiet",
                "--locked",
                "--manifest-path",
                str(consumer / "Cargo.toml"),
                "--",
                str(isolated_fixture),
            ],
            cwd=work,
            env=env,
        )
        if "rxls external consumer ok:" not in consumer_result.stdout:
            raise SmokeError("external consumer did not emit its success marker")

        _run(install_command, cwd=work, env=env)
        binary = _installed_binary(work / "install")
        runtime = work / "runtime"
        runtime.mkdir()

        version_result = _run([str(binary), "--version"], cwd=runtime, env=env)
        if version_result.stdout.strip() != f"rxls {version}" or version_result.stderr:
            raise SmokeError("installed rxls --version output does not match the package")

        help_result = _run([str(binary), "--help"], cwd=runtime, env=env)
        if not help_result.stdout.startswith("usage: ") or help_result.stderr:
            raise SmokeError("installed rxls --help is not a stdout-only success")

        diagnose_result = _run(
            [str(binary), "diagnose", str(isolated_fixture)], cwd=runtime, env=env
        )
        if diagnose_result.stderr:
            raise SmokeError("installed rxls diagnose wrote to stderr on success")
        try:
            report = json.loads(diagnose_result.stdout)
        except json.JSONDecodeError as error:
            raise SmokeError(f"installed rxls diagnose emitted invalid JSON: {error}") from error
        if report.get("schema_version") != 1:
            raise SmokeError("installed rxls diagnose did not emit schema version 1")

        error_result = _run(
            [str(binary), "diagnose"], cwd=runtime, env=env, expected_code=64
        )
        if error_result.stdout or not error_result.stderr.startswith("usage: "):
            raise SmokeError("installed rxls invalid usage is not stderr-only")

    return {
        "schema": "rxls.crate-distribution-smoke.v1",
        "mode": mode,
        "source": source_name,
        "version": version,
        "external_consumer": "passed",
        "cargo_install": "passed",
        "cli": {
            "version": "passed",
            "help_stdout": "passed",
            "diagnose_schema_v1": "passed",
            "invalid_usage_stderr": "passed",
        },
    }


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        report = smoke(args)
    except (OSError, SmokeError, tarfile.TarError, tomllib.TOMLDecodeError) as error:
        print(f"crate distribution smoke failed: {error}", file=sys.stderr)
        return 1

    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.write_report is not None:
        args.write_report.parent.mkdir(parents=True, exist_ok=True)
        args.write_report.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
