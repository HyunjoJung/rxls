#!/usr/bin/env python3
"""Verify the unpublished core/MCP archives together without publishing either.

Cargo 1.96.1 supplies the interdependent-package overlay; rustc and rustdoc
remain pinned to the MCP MSRV. Only an exact, clean Git snapshot is staged.
The temporary virtual workspace does not alter either source manifest/lock.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from collections.abc import Iterator


ROOT = Path(__file__).resolve().parents[3]
PACKAGE_CARGO = "1.96.1"
MCP_MSRV = "1.88.0"
MAX_FILES = 10_000
MAX_BYTES = 128 * 1024 * 1024
MAX_FILE_BYTES = 32 * 1024 * 1024
COMMAND_TIMEOUT = 60
PACKAGE_TIMEOUT = 30 * 60
GENERATED = {"Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json"}


def run(arguments: list[str], *, cwd: Path, env: dict[str, str] | None = None) -> str:
    result = subprocess.run(arguments, cwd=cwd, env=env, check=True,
                            stdout=subprocess.PIPE, text=True, timeout=COMMAND_TIMEOUT)
    return result.stdout.strip()


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def source_revision(root: Path, expected: str) -> str:
    if not re.fullmatch(r"[0-9a-f]{40}", expected):
        raise ValueError("expected SHA must be a full lowercase Git revision")
    actual = run(["git", "rev-parse", "HEAD"], cwd=root)
    if actual != expected:
        raise ValueError("source HEAD does not match expected SHA")
    if run(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=root):
        raise ValueError("source must be clean, including non-ignored untracked files")
    return run(["git", "rev-parse", "HEAD^{tree}"], cwd=root)


def safe_members(archive: tarfile.TarFile) -> Iterator[tarfile.TarInfo]:
    total = 0
    names: set[str] = set()
    for count, member in enumerate(archive, start=1):
        if count > MAX_FILES:
            raise ValueError("snapshot/package exceeds file-count budget")
        logical = member.name.rstrip("/") if member.isdir() else member.name
        parts = logical.split("/")
        if (any(part in {"", ".", ".."} or ":" in part for part in parts) or "\\" in logical
                or logical in names or not (member.isfile() or member.isdir())):
            raise ValueError("snapshot/package contains unsafe or duplicate paths")
        names.add(logical)
        if member.size > MAX_FILE_BYTES:
            raise ValueError("snapshot/package exceeds individual-file budget")
        total += member.size
        if total > MAX_BYTES:
            raise ValueError("snapshot/package exceeds expanded-byte budget")
        yield member


def snapshot(root: Path, revision: str, destination: Path) -> str:
    # Bound Git objects before archive creation; symlinks/submodules are rejected.
    listing = subprocess.check_output(
        ["git", "ls-tree", "-r", "-l", "-z", revision], cwd=root, timeout=COMMAND_TIMEOUT)
    entries = [entry for entry in listing.split(b"\0") if entry]
    if len(entries) > MAX_FILES:
        raise ValueError("Git snapshot exceeds file-count budget")
    total = 0
    expected: dict[str, str] = {}
    for entry in entries:
        metadata, filename = entry.split(b"\t", 1)
        mode, kind, blob, size = metadata.split()
        if mode not in {b"100644", b"100755"} or kind != b"blob":
            raise ValueError("Git snapshot must contain only regular tracked files")
        length = int(size)
        if length > MAX_FILE_BYTES:
            raise ValueError("Git snapshot exceeds individual-file budget")
        total += length
        expected[filename.decode("utf-8")] = blob.decode("ascii")
    if total > MAX_BYTES:
        raise ValueError("Git snapshot exceeds expanded-byte budget")
    archive_path = destination.parent / "source.tar"
    subprocess.run(["git", "archive", "--format=tar", "--output", str(archive_path),
                    revision], cwd=root, check=True, timeout=COMMAND_TIMEOUT)
    digest = sha256(archive_path.read_bytes())
    with tarfile.open(archive_path, "r:") as archive:
        extracted: set[str] = set()
        for member in safe_members(archive):
            target = destination.joinpath(*PurePosixPath(member.name).parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                stream = archive.extractfile(member)
                assert stream is not None
                data = stream.read()
                blob = hashlib.sha1(b"blob " + str(len(data)).encode("ascii") + b"\0" + data).hexdigest()
                if expected.get(member.name) != blob:
                    raise ValueError("Git archive altered or added a tracked source blob")
                extracted.add(member.name)
                target.write_bytes(data)
                target.chmod(member.mode & 0o777)
        if extracted != set(expected):
            raise ValueError("Git archive omitted tracked source files")
    return digest


def identities(source: Path) -> tuple[str, str]:
    core = tomllib.loads((source / "Cargo.toml").read_text(encoding="utf-8"))
    mcp = tomllib.loads((source / "bindings/mcp/Cargo.toml").read_text(encoding="utf-8"))
    version = core["package"]["version"]
    dependency = mcp["dependencies"]["rxls"]
    if (core["package"]["name"] != "rxls" or mcp["package"]["name"] != "rxls-mcp"
            or mcp["package"].get("publish") is not False
            or mcp["package"].get("rust-version") != "1.88"
            or dependency.get("version") != version or dependency.get("path") != "../.."):
        raise ValueError("core/MCP source identity or MSRV policy drift")
    lock = tomllib.loads((source / "bindings/mcp/Cargo.lock").read_text(encoding="utf-8"))
    for name, expected in (("rxls", version), ("rxls-mcp", mcp["package"]["version"])):
        packages = [package for package in lock["package"] if package["name"] == name]
        if len(packages) != 1 or packages[0]["version"] != expected or "source" in packages[0]:
            raise ValueError("MCP locked local package identity drift")
    return version, mcp["package"]["version"]


def verify_archive(path: Path, source: Path, name: str, version: str) -> dict:
    root = f"{name}-{version}"
    inventory = []
    manifest = None
    with tarfile.open(path, "r:gz") as archive:
        for member in safe_members(archive):
            if not member.isfile():
                continue
            parts = PurePosixPath(member.name).parts
            if len(parts) < 2 or parts[0] != root:
                raise ValueError("packaged archive root identity drift")
            relative = str(PurePosixPath(*parts[1:]))
            stream = archive.extractfile(member)
            assert stream is not None
            data = stream.read()
            if relative == "Cargo.toml":
                manifest = tomllib.loads(data.decode("utf-8"))
            elif relative == "Cargo.toml.orig":
                if data != (source / "Cargo.toml").read_bytes():
                    raise ValueError("packaged original manifest differs from exact source")
            elif relative not in GENERATED:
                original = source.joinpath(*PurePosixPath(relative).parts)
                if not original.is_file() or data != original.read_bytes():
                    raise ValueError(f"packaged file differs from exact source: {relative}")
            inventory.append({"path": relative, "bytes": len(data), "sha256": sha256(data)})
    if manifest is None or manifest["package"]["name"] != name or manifest["package"]["version"] != version:
        raise ValueError("packaged manifest identity drift")
    if name == "rxls-mcp":
        dependency = manifest["dependencies"]["rxls"]
        source_version = tomllib.loads((source / "Cargo.toml").read_text(encoding="utf-8"))["dependencies"]["rxls"]["version"]
        if (manifest["package"].get("publish") is not False
                or dependency.get("version") not in {source_version, f"^{source_version}"}
                or "path" in dependency or "registry" in dependency
                or manifest["package"].get("rust-version") != "1.88"):
            raise ValueError("normalized MCP publication/dependency/MSRV identity drift")
    if not {"Cargo.toml", "Cargo.toml.orig", "Cargo.lock"}.issubset(item["path"] for item in inventory):
        raise ValueError("packaged manifest/lock evidence is incomplete")
    return {"name": name, "version": version, "archive": path.name,
            "sha256": sha256(path.read_bytes()), "files": sorted(inventory, key=lambda item: item["path"])}


def verify_packaged_lock(path: Path, source_lock: bytes, core_version: str,
                         core_checksum: str, *, mcp: bool) -> None:
    """Bind Cargo's normalized registry lock to the exact paired core archive."""
    with tarfile.open(path, "r:gz") as archive:
        locks = [member for member in safe_members(archive)
                 if member.isfile() and PurePosixPath(member.name).name == "Cargo.lock"
                 and len(PurePosixPath(member.name).parts) == 2]
        if len(locks) != 1:
            raise ValueError("archive must contain exactly one root Cargo.lock")
        stream = archive.extractfile(locks[0])
        assert stream is not None
        packages = tomllib.loads(stream.read().decode("utf-8"))["package"]
    original = tomllib.loads(source_lock.decode("utf-8"))["package"]
    canonical = "registry+https://github.com/rust-lang/crates.io-index"
    core = [package for package in packages if package["name"] == "rxls"]
    if len(core) != 1 or core[0]["version"] != core_version:
        raise ValueError("archived lock core version drift")
    if mcp:
        if core[0].get("source") != canonical or core[0].get("checksum") != core_checksum:
            raise ValueError("archived MCP lock does not bind the exact paired core archive")
    elif "source" in core[0] or "checksum" in core[0]:
        raise ValueError("archived core lock must retain its own local package identity")
    def registry_entries(entries: list[dict]) -> set[tuple[str, str, str, str]]:
        result = set()
        for entry in entries:
            if entry["name"] == "rxls" or "source" not in entry:
                continue
            if entry["source"] != canonical or not re.fullmatch(r"[0-9a-f]{64}", entry.get("checksum", "")):
                raise ValueError("archived third-party lock source/checksum is not canonical")
            result.add((entry["name"], entry["version"], entry["source"], entry["checksum"]))
        return result
    expected = registry_entries(original)
    actual = registry_entries(packages)
    if (mcp and actual != expected) or not actual.issubset(expected):
        raise ValueError("archived lock changed third-party registry package identities")


def package(root: Path, expected: str, output: Path) -> dict:
    tree = source_revision(root, expected)
    cargo = run(["rustup", "which", "--toolchain", PACKAGE_CARGO, "cargo"], cwd=root)
    rustc = run(["rustup", "which", "--toolchain", MCP_MSRV, "rustc"], cwd=root)
    rustdoc = run(["rustup", "which", "--toolchain", MCP_MSRV, "rustdoc"], cwd=root)
    versions = {tool: run([path, "--version"], cwd=root)
                for tool, path in (("cargo", cargo), ("rustc", rustc), ("rustdoc", rustdoc))}
    for tool, version in (("cargo", PACKAGE_CARGO), ("rustc", MCP_MSRV), ("rustdoc", MCP_MSRV)):
        if not versions[tool].startswith(f"{tool} {version} "):
            raise ValueError(f"unexpected {tool} packaging toolchain")
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rxls-mcp-package-") as directory:
        workspace = Path(directory)
        git_probe = subprocess.run(["git", "rev-parse", "--is-inside-work-tree"], cwd=workspace,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=COMMAND_TIMEOUT)
        if git_probe.returncode == 0:
            raise ValueError("temporary packaging workspace must be outside any Git checkout")
        source = workspace / "source"
        digest = snapshot(root, expected, source)
        core_version, mcp_version = identities(source)
        (workspace / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["source", "source/bindings/mcp"]\nresolver = "2"\n', encoding="utf-8")
        lock = (source / "bindings/mcp/Cargo.lock").read_bytes()
        (workspace / "Cargo.lock").write_bytes(lock)
        environment = os.environ.copy()
        for variable in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                         "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"):
            environment.pop(variable, None)
        environment.update({"RUSTC": rustc, "RUSTDOC": rustdoc,
                            "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "",
                            "CARGO_TARGET_DIR": str(workspace / "target")})
        # Never add --no-verify/--allow-dirty or invoke publish. Explicit -p keeps
        # the intentionally publish=false MCP package in the verification set.
        command = [cargo, "package", "--manifest-path", str(workspace / "Cargo.toml"),
                   "-p", "rxls", "-p", "rxls-mcp", "--locked"]
        subprocess.run(command, cwd=workspace, env=environment, check=True, timeout=PACKAGE_TIMEOUT)
        if (workspace / "Cargo.lock").read_bytes() != lock:
            raise ValueError("packaging altered the seeded locked source graph")
        artifacts = []
        for name, version, original in (("rxls", core_version, source),
                                        ("rxls-mcp", mcp_version, source / "bindings/mcp")):
            archive = workspace / "target/package" / f"{name}-{version}.crate"
            artifacts.append(verify_archive(archive, original, name, version))
            verify_packaged_lock(archive, lock, core_version, artifacts[0]["sha256"], mcp=name == "rxls-mcp")
            shutil.copyfile(archive, output / archive.name)
        source_revision(root, expected)
        report = {"schema_version": 1, "source_sha": expected, "source_tree": tree,
                  "source_snapshot_sha256": digest, "mcp_source_lock_sha256": sha256(lock),
                  "toolchains": versions, "verified": True, "published": False,
                  "packages": artifacts}
        (output / "mcp-package-candidate.json").write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected-sha", required=True)
    parser.add_argument("--source-root", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path, default=ROOT / "bindings/mcp/target/package-candidate")
    arguments = parser.parse_args()
    try:
        report = package(arguments.source_root.resolve(), arguments.expected_sha, arguments.output.resolve())
    except (ValueError, OSError, subprocess.SubprocessError, tarfile.TarError, KeyError) as error:
        print(f"MCP package candidate verification failed: {error}", file=sys.stderr)
        return 1
    print(f"Verified core and MCP archives at {report['source_sha']} with rustc {MCP_MSRV}; nothing published")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
