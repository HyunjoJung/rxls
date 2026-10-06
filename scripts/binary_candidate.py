#!/usr/bin/env python3
"""Prepare and verify exact-source CLI/MCP binary candidates; never publish."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import stat
import struct
import subprocess
import sys
import threading
import zipfile

from release_manifest import SEMVER, sha256_file
from smoke_crate_distribution import SmokeError, _safe_member_target
from binary_process import OwnedCommand

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = "rxls.binary-candidate.v1"
RECEIPT_SCHEMA = "rxls.binary-candidate-verification.v1"
PLATFORMS = {
    "windows-x64": {"system": "Windows", "machines": {"amd64", "x86_64"}, "target": "x86_64-pc-windows-msvc", "runner": "windows-2022", "suffix": ".exe"},
    "linux-x64": {"system": "Linux", "machines": {"x86_64", "amd64"}, "target": "x86_64-unknown-linux-gnu", "runner": "ubuntu-22.04", "suffix": ""},
    "macos-arm64": {"system": "Darwin", "machines": {"arm64", "aarch64"}, "target": "aarch64-apple-darwin", "runner": "macos-15", "suffix": ""},
}
MAX_ARCHIVE = 64 * 1024 * 1024
MAX_EXPANDED = 96 * 1024 * 1024
MAX_BINARY = 32 * 1024 * 1024
MAX_NOTICE = 512 * 1024
MAX_METADATA = 32 * 1024
MAX_MEMBERS = 16
MAX_COMMAND_OUTPUT = 16 * 1024 * 1024
SOURCE_FILES = ("Cargo.toml", "Cargo.lock", "bindings/mcp/Cargo.toml", "bindings/mcp/Cargo.lock")
LEGAL_FILES = {"LICENSE": "LICENSE", "MCP-LICENSE": "bindings/mcp/LICENSE", "THIRD_PARTY_LICENSES.md": "THIRD_PARTY_LICENSES.md", "THIRD_PARTY_NOTICES.txt": "bindings/mcp/THIRD_PARTY_NOTICES.txt"}


def load_helper(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise ValueError("source helper unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def encoded(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def safe_target(name: str, destination: Path):
    try:
        return _safe_member_target(name, destination)
    except SmokeError as error:
        raise ValueError(str(error)) from error


def candidate_name(core: str, mcp: str, revision: str, selected: str) -> str:
    if any(not isinstance(value, str) for value in (core, mcp, revision, selected)) or SEMVER.fullmatch(core) is None or SEMVER.fullmatch(mcp) is None or re.fullmatch(r"[0-9a-f]{40}", revision) is None or selected not in PLATFORMS:
        raise ValueError("invalid candidate identity")
    return f"rxls-{core}-mcp-{mcp}-candidate-{revision}-{selected}"


def binary_paths(selected: str) -> tuple[str, str]:
    suffix = PLATFORMS[selected]["suffix"]
    return f"bin/rxls{suffix}", f"bin/rxls-mcp{suffix}"


def verify_header(data: bytes, selected: str) -> None:
    valid = False
    if selected == "windows-x64" and len(data) >= 64 and data[:2] == b"MZ":
        offset = struct.unpack_from("<I", data, 60)[0]
        valid = 64 <= offset <= 128 * 1024 and offset + 6 <= len(data) and data[offset:offset + 4] == b"PE\0\0" and struct.unpack_from("<H", data, offset + 4)[0] == 0x8664
    elif selected == "linux-x64":
        valid = len(data) >= 64 and data[:6] == b"\x7fELF\x02\x01" and struct.unpack_from("<H", data, 18)[0] == 62
    elif selected == "macos-arm64":
        valid = len(data) >= 32 and data[:4] == b"\xcf\xfa\xed\xfe" and struct.unpack_from("<I", data, 4)[0] == 0x0100000C
    if not valid:
        raise ValueError("binary native architecture/header mismatch")


def run(command: list[str], cwd: Path, log: Path, *, timeout: int = 60, env: dict | None = None) -> bytes:
    """Bound output while it is produced; own and terminate only this child."""
    try:
        owner = OwnedCommand(command, cwd, env)
    except Exception as error:
        message = f"command start: {type(error).__name__}: {str(error)[:1024]}"
        record = {"stage": "start", "status": "failed", "command": command, "exit_code": getattr(error, "owned_exit_code", None), "errors": [message], "output_bytes": 0, "ownership": "windows-job" if os.name == "nt" else "posix-session", "ownership_observations": getattr(error, "owned_observations", [])}
        if len(encoded(record)) > MAX_METADATA:
            record["command"] = {"arguments": len(command), "sha256": digest(encoded(command))}
            record["ownership_observations"] = record["ownership_observations"][:8]
            record["receipt_truncated"] = True
        log.parent.mkdir(parents=True, exist_ok=True)
        log.write_bytes(b"")
        log.with_suffix(".exit.json").write_bytes(encoded(record))
        raise ValueError(f"candidate command could not start; {message}; log={log.name}") from error
    process = owner.process
    output = bytearray()
    errors = []

    def reader():
        try:
            while block := process.stdout.read(4096):
                if len(output) + len(block) > MAX_COMMAND_OUTPUT:
                    errors.append("command output limit")
                    owner.kill()
                    return
                output.extend(block)
        except OSError as error:
            errors.append(str(error))

    thread = threading.Thread(target=reader, daemon=True)
    code = None
    try:
        thread.start()
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            errors.append("command timeout")
            owner.kill()
            code = process.wait(timeout=10)
        thread.join(timeout=10)
        if thread.is_alive():
            errors.append("command reader did not finish")
            owner.kill()
    except Exception as error:
        errors.append(f"command stage: {type(error).__name__}: {str(error)[:1024]}")
    finally:
        try:
            owner.close()
        except Exception as error:
            errors.append(f"owned command cleanup: {type(error).__name__}: {str(error)[:1024]}")
        if thread.ident is not None:
            thread.join(timeout=10)
        if thread.is_alive():
            errors.append("owned command reader remained active")
        if code is None:
            code = process.poll()
    log.parent.mkdir(parents=True, exist_ok=True)
    log.write_bytes(output)
    log.with_suffix(".exit.json").write_bytes(encoded({"stage": "command", "status": "passed" if code == 0 and not errors else "failed", "command": command, "exit_code": code, "errors": errors, "output_bytes": len(output), "ownership": "windows-job" if os.name == "nt" else "posix-session", "ownership_observations": owner.observations}))
    if code is None or code or errors:
        raise ValueError(f"candidate command failed ({code}, {errors}); log={log.name}")
    return bytes(output)


def native_host(selected: str, compilers: dict) -> dict:
    rule = PLATFORMS[selected]
    if platform.system() != rule["system"] or platform.machine().lower() not in rule["machines"]:
        raise ValueError("native runner OS/CPU mismatch; cross compilation is not accepted")
    for name, version in (("cli", "1.85.0"), ("mcp", "1.88.0")):
        text = compilers[name]["rustc_verbose"]
        if not text.startswith(f"rustc {version} ") or f"\nhost: {rule['target']}\n" not in text:
            raise ValueError("compiler version/native host mismatch")
    return {"system": platform.system(), "release": platform.release(), "version": platform.version(), "machine": platform.machine(), "libc": list(platform.libc_ver()), "python": platform.python_version(), "runner_os": os.environ.get("RUNNER_OS"), "runner_arch": os.environ.get("RUNNER_ARCH"), "image_os": os.environ.get("ImageOS"), "image_version": os.environ.get("ImageVersion")}


def archive_envelope(path: Path) -> None:
    # Check the small, non-ZIP64 directory envelope before ZipFile allocates
    # its member index. Our writer has no archive comment or trailing bytes.
    size = path.stat().st_size
    if size < 22 or size > MAX_ARCHIVE:
        raise ValueError("archive size limit")
    with path.open("rb") as stream:
        stream.seek(-22, 2)
        end = stream.read(22)
    signature, disk, directory_disk, disk_count, count, directory_size, offset, comment = struct.unpack("<4s4H2LH", end)
    if signature != b"PK\x05\x06" or disk or directory_disk or disk_count != count or not 1 <= count <= MAX_MEMBERS or comment or directory_size > 16 * 1024 or offset + directory_size != size - 22:
        raise ValueError("archive directory envelope/count limit")


def validate_manifest(manifest: object, revision: str, selected: str) -> dict:
    if not isinstance(manifest, dict) or manifest.get("schema") != SCHEMA or manifest.get("stage") != "prepared" or manifest.get("source_revision") != revision or manifest.get("platform") != selected or manifest.get("target") != PLATFORMS[selected]["target"] or manifest.get("planned_runner") != PLATFORMS[selected]["runner"] or manifest.get("notice_cli_subset") is not True:
        raise ValueError("candidate metadata source/platform identity mismatch")
    for name, length in (("source_tree", 40), ("source_archive_sha256", 64)):
        if not isinstance(manifest.get(name), str) or re.fullmatch(rf"[0-9a-f]{{{length}}}", manifest[name]) is None:
            raise ValueError("candidate source identity missing/malformed")
    source_files = manifest.get("source_files")
    if not isinstance(source_files, dict) or set(source_files) != set(SOURCE_FILES) or any(not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None for value in source_files.values()):
        raise ValueError("candidate source manifest/lock identity malformed")
    versions, compilers = manifest.get("versions"), manifest.get("compilers")
    if not isinstance(versions, dict) or set(versions) != {"cli", "mcp"} or not isinstance(compilers, dict) or set(compilers) != {"cli", "mcp"} or not isinstance(manifest.get("host"), dict):
        raise ValueError("candidate version/compiler/host metadata malformed")
    candidate_name(versions["cli"], versions["mcp"], revision, selected)
    for name, version in (("cli", "1.85.0"), ("mcp", "1.88.0")):
        compiler = compilers[name]
        if not isinstance(compiler, dict) or compiler.get("toolchain") != version or not isinstance(compiler.get("rustc_verbose"), str) or not compiler["rustc_verbose"].startswith(f"rustc {version} ") or f"\nhost: {PLATFORMS[selected]['target']}\n" not in compiler["rustc_verbose"] or not isinstance(compiler.get("cargo_version"), str) or not compiler["cargo_version"].startswith(f"cargo {version} "):
            raise ValueError("candidate native compiler metadata mismatch")
    return manifest


def read_archive(path: Path, expected_revision: str, selected: str, expected_hash: str) -> tuple[dict, dict[str, bytes]]:
    """Validate everything before writing or launching an unpacked member."""
    candidate_name("0.0.0", "0.0.0", expected_revision, selected)
    archive_envelope(path)
    if not isinstance(expected_hash, str) or re.fullmatch(r"[0-9a-f]{64}", expected_hash) is None or sha256_file(path) != expected_hash:
        raise ValueError("archive size/checksum mismatch")
    payloads = {}
    modes = {}
    total = 0
    folded = set()
    with zipfile.ZipFile(path) as archive:
        infos = archive.infolist()
        if len(infos) > MAX_MEMBERS:
            raise ValueError("archive member count limit")
        for info in infos:
            raw = info.orig_filename
            if any(part in {"", ".", ".."} for part in raw.split("/")) or len(raw) > 240 or raw.casefold() in folded:
                raise ValueError("unsafe/duplicate/case-colliding archive member")
            safe_target(raw, Path.cwd() / "unused-validation-root")
            folded.add(raw.casefold())
            mode = info.external_attr >> 16
            if info.create_system != 3 or not stat.S_ISREG(mode) or info.flag_bits & 1 or info.is_dir() or info.extract_version > 20 or info.compress_type not in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED):
                raise ValueError("archive member must be an unencrypted regular file")
            total += info.file_size
            if info.file_size > MAX_BINARY or total > MAX_EXPANDED:
                raise ValueError("archive expanded-byte limit")
            data = archive.read(info)
            if len(data) != info.file_size:
                raise ValueError("archive member truncated")
            payloads[raw] = data
            modes[raw] = stat.S_IMODE(mode)
    manifests = [name for name in payloads if name.endswith("/candidate.json")]
    if len(manifests) != 1 or len(payloads[manifests[0]]) > MAX_METADATA:
        raise ValueError("candidate metadata absent/ambiguous/oversized")
    manifest = validate_manifest(json.loads(payloads[manifests[0]]), expected_revision, selected)
    stem = candidate_name(manifest["versions"]["cli"], manifest["versions"]["mcp"], expected_revision, selected)
    if path.name != stem + ".zip" or manifest.get("candidate") != stem or manifests[0] != stem + "/candidate.json":
        raise ValueError("candidate archive/root filename identity mismatch")
    expected = set(binary_paths(selected)) | set(LEGAL_FILES) | {"README.txt"}
    records = manifest.get("files")
    if not isinstance(records, dict) or set(records) != expected or set(payloads) != {f"{stem}/{name}" for name in expected | {"candidate.json"}}:
        raise ValueError("candidate inventory mismatch")
    selected_payloads = {}
    for name, record in records.items():
        data = payloads[f"{stem}/{name}"]
        expected_mode = 0o755 if name in binary_paths(selected) else 0o644
        limit = MAX_BINARY if name in binary_paths(selected) else MAX_NOTICE
        if not isinstance(record, dict) or type(record.get("bytes")) is not int or type(record.get("mode")) is not int or len(data) > limit or record != {"bytes": len(data), "sha256": digest(data), "mode": expected_mode} or modes[f"{stem}/{name}"] != expected_mode:
            raise ValueError("candidate member hash/size/mode mismatch")
        selected_payloads[name] = data
    if modes[manifests[0]] != 0o644:
        raise ValueError("metadata mode mismatch")
    for name in binary_paths(selected):
        verify_header(selected_payloads[name], selected)
    notice = selected_payloads["THIRD_PARTY_NOTICES.txt"].decode("utf-8")
    if not notice.startswith("RXLS MCP THIRD-PARTY NOTICES\n") or f"- Cargo lock SHA-256: {manifest['source_files']['bindings/mcp/Cargo.lock']}\n" not in notice:
        raise ValueError("notice/locked source identity mismatch")
    return manifest, selected_payloads


def extract_verified(destination: Path, manifest: dict, payloads: dict[str, bytes]) -> Path:
    if destination.exists():
        raise ValueError("unpack directory must be fresh")
    destination.mkdir(parents=True)
    for name, data in payloads.items():
        _, target = safe_target(name, destination)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        target.chmod(manifest["files"][name]["mode"])
    return destination


def build(root: Path, revision: str, selected: str, out: Path) -> dict:
    helper = load_helper(root / "bindings/mcp/scripts/package_candidate.py", "mcp_source_candidate")
    tree = helper.source_revision(root, revision)
    out = out.resolve()
    if out.exists() or not any(out.is_relative_to((root / item).resolve()) for item in ("target", "local")):
        raise ValueError("candidate output must be a fresh owned target/local directory")
    for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET", "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"):
        if os.environ.get(key):
            raise ValueError(f"unset inherited {key}; candidate build records explicit defaults")
    source = out / "work/source"
    source.mkdir(parents=True)
    source_archive_hash = helper.snapshot(root, revision, source)
    versions = dict(zip(("cli", "mcp"), helper.identities(source)))
    source_hashes = {name: sha256_file(source / name) for name in SOURCE_FILES}
    logs = out / "logs"
    compilers = {}
    for name, version in (("cli", "1.85.0"), ("mcp", "1.88.0")):
        compilers[name] = {"toolchain": version, "rustc_verbose": run(["rustc", f"+{version}", "-vV"], source, logs / f"{name}-rustc.log").decode("utf-8"), "cargo_version": run(["cargo", f"+{version}", "-V"], source, logs / f"{name}-cargo.log").decode("utf-8").strip()}
    host = native_host(selected, compilers)
    target = PLATFORMS[selected]["target"]
    for name, version in (("cli", "1.85.0"), ("mcp", "1.88.0")):
        command = ["cargo", f"+{version}", "build", "--release", "--locked", "--target", target, "--target-dir", str(out / f"work/target-{name}")]
        command += ["--features", "full", "--bin", "rxls"] if name == "cli" else ["--manifest-path", "bindings/mcp/Cargo.toml", "--bin", "rxls-mcp"]
        run(command, source, logs / f"{name}-build.log", timeout=1800)
    # Reuse the existing legal renderer instead of maintaining another license list.
    notice_env = dict(os.environ, RUSTUP_TOOLCHAIN="1.88.0")
    run([sys.executable, "scripts/render_supply_chain.py", "notice", "--profile", "mcp", "--check", "bindings/mcp/THIRD_PARTY_NOTICES.txt"], source, logs / "notice-check.log", timeout=300, env=notice_env)
    supply = load_helper(source / "scripts/render_supply_chain.py", "native_supply_chain")
    closures = {}
    for name, version, path in (("cli", "1.85.0", "Cargo.toml"), ("mcp", "1.88.0", "bindings/mcp/Cargo.toml")):
        command = ["cargo", f"+{version}", "metadata", "--format-version", "1", "--locked", "--filter-platform", target, "--manifest-path", path]
        if name == "cli":
            command += ["--features", "full"]
        metadata = json.loads(run(command, source, logs / f"{name}-metadata.log"))
        _, closure, _ = supply.production_closure(metadata, crate_name="rxls" if name == "cli" else "rxls-mcp")
        closures[name] = {(item["name"], item["version"], item.get("source")) for item in closure.values() if item.get("source")}
    if not closures["cli"] <= closures["mcp"]:
        raise ValueError("full CLI third-party closure not covered by native MCP notice")
    if source_hashes != {name: sha256_file(source / name) for name in SOURCE_FILES}:
        raise ValueError("build/notice metadata changed source manifest or lock")
    stem = candidate_name(versions["cli"], versions["mcp"], revision, selected)
    payloads = {name: (source / original).read_bytes() for name, original in LEGAL_FILES.items()}
    for name, relative in zip(("cli", "mcp"), binary_paths(selected)):
        artifact = out / f"work/target-{name}" / target / "release" / Path(relative).name
        if not artifact.is_file() or artifact.stat().st_size > MAX_BINARY:
            raise ValueError("expected bounded native binary not built")
        payloads[relative] = artifact.read_bytes()
        verify_header(payloads[relative], selected)
    payloads["README.txt"] = f"rxls CLI {versions['cli']} and MCP {versions['mcp']}\nCandidate source {revision}; not the published v{versions['cli']} artifact.\nTarget {target}.\nRun bin/rxls{PLATFORMS[selected]['suffix']} --help for CLI commands.\nRun bin/rxls-mcp{PLATFORMS[selected]['suffix']} --root <owned workbook directory> for newline-delimited MCP.\nKeep the included licenses and notices with both binaries.\nUnsigned verification candidate; hosted verification and maintainer publication are separate.\n".encode("utf-8")
    manifest = {"schema": SCHEMA, "stage": "prepared", "candidate": stem, "source_revision": revision, "source_tree": tree, "source_archive_sha256": source_archive_hash, "source_files": source_hashes, "versions": versions, "platform": selected, "target": target, "planned_runner": PLATFORMS[selected]["runner"], "compilers": compilers, "host": host, "notice_cli_subset": True, "files": {name: {"bytes": len(data), "sha256": digest(data), "mode": 0o755 if name in binary_paths(selected) else 0o644} for name, data in sorted(payloads.items())}}
    if len(encoded(manifest)) > MAX_METADATA:
        raise ValueError("candidate metadata limit")
    distribution = out / "dist"
    distribution.mkdir()
    archive_path = distribution / f"{stem}.zip"
    with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9, allowZip64=False) as archive:
        for name, data in sorted((payloads | {"candidate.json": encoded(manifest)}).items()):
            info = zipfile.ZipInfo(f"{stem}/{name}", date_time=(1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (stat.S_IFREG | (0o755 if name in binary_paths(selected) else 0o644)) << 16
            archive.writestr(info, data)
    archive_hash = sha256_file(archive_path)
    checked, checked_payloads = read_archive(archive_path, revision, selected, archive_hash)
    unpacked = extract_verified(out / "unpacked", checked, checked_payloads)
    smoke = load_helper(source / "scripts/binary_smoke.py", "binary_candidate_smoke")
    smoke_report = smoke.smoke(unpacked, checked, out / "runtime")
    if helper.source_revision(root, revision) != tree:
        raise ValueError("source checkout changed during candidate verification")
    context = {name: os.environ.get(name) for name in ("GITHUB_ACTIONS", "GITHUB_REPOSITORY", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA")}
    stage = "hosted_verified" if context["GITHUB_ACTIONS"] == "true" else "locally_verified"
    receipt = {"schema": RECEIPT_SCHEMA, "stage": stage, "publication": "not_published", "source_revision": revision, "source_tree": tree, "platform": selected, "archive": {"name": archive_path.name, "bytes": archive_path.stat().st_size, "sha256": archive_hash}, "context": context, "smoke": smoke_report}
    (distribution / f"{stem}.verification.json").write_bytes(encoded(receipt))
    (distribution / f"{stem}.zip.sha256").write_text(f"{archive_hash}  {archive_path.name}\n", encoding="ascii", newline="\n")
    return receipt


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build_args = commands.add_parser("build")
    build_args.add_argument("--expected-sha", required=True)
    build_args.add_argument("--platform", choices=PLATFORMS, required=True)
    build_args.add_argument("--output-dir", type=Path, required=True)
    verify_args = commands.add_parser("verify")
    verify_args.add_argument("archive", type=Path)
    verify_args.add_argument("--expected-sha", required=True)
    verify_args.add_argument("--platform", choices=PLATFORMS, required=True)
    verify_args.add_argument("--expected-sha256", required=True)
    args = parser.parse_args()
    try:
        if args.command == "build":
            result = build(ROOT, args.expected_sha, args.platform, args.output_dir)
        else:
            result, _ = read_archive(args.archive, args.expected_sha, args.platform, args.expected_sha256)
        print(json.dumps(result, indent=2, sort_keys=True))
    except (OSError, ValueError, zipfile.BadZipFile, subprocess.SubprocessError) as error:
        print(f"binary candidate: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
