#!/usr/bin/env python3
"""Build and run the pinned Linux LibreOffice rendering oracle.

The host wrapper uses only the Python standard library.  Rendering happens in
an ephemeral Docker or Podman container with a read-only root filesystem,
read-only inputs, no network or capabilities, and size-capped tmpfs mounts.
The container streams a bounded tar archive to stdout before its evidence
tmpfs is destroyed; only path-neutral, verified artifacts are committed to the
requested host evidence directory.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import secrets
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
from typing import Any, Protocol, Sequence
from urllib.parse import quote


ROOT = Path(__file__).resolve().parents[1]
CONTAINER_DIR = ROOT / "scripts" / "render-oracle-container"
DEFAULT_LOCK = CONTAINER_DIR / "lock.json"
CONTAINERFILE = CONTAINER_DIR / "Containerfile"
LOCK_SCHEMA = "rxls.render-oracle-container-lock.v2"
OUTPUT_SCHEMA = "rxls.render-oracle-container-output.v2"
EXECUTION_SCHEMA = "rxls.render-oracle-container-execution.v2"
PLAN_SCHEMA = "rxls.render-oracle-container-plan.v1"
SOURCE_DATE_EPOCH = 1_783_900_800
FONT_PACK_SCHEMA = "rxls.render-font-pack.v1"
SUPPORTED_EXTENSIONS = {".xls", ".xlsx", ".xlsm", ".xlsb", ".ods"}
PRINT_MODES = {"authored", "single-page-sheets"}
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
IMAGE_ID_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")
RUN_ID_RE = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?\Z")
IMAGE_RE = re.compile(r"[^\s\x00-\x1f\x7f]{1,256}\Z")
MAX_LOCK_BYTES = 256 * 1024
MAX_FONT_PACK_BYTES = 128 * 1024 * 1024
MAX_FONT_PACK_FILES = 128
MAX_EVIDENCE_FILES = 16
MAX_ENGINE_DIAGNOSTIC_BYTES = 1024 * 1024
LIBREOFFICE_ARTIFACT_SHA256 = (
    "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5"
)
EXPECTED_IMAGE_LABELS = {
    "org.opencontainers.image.version": "26.2.3.2",
    "org.rxls.render-oracle.architecture": "linux/amd64",
    "org.rxls.render-oracle.libreoffice-artifact-sha256": (
        LIBREOFFICE_ARTIFACT_SHA256
    ),
}


class OracleContainerError(RuntimeError):
    """A stable container-oracle contract failed."""


@dataclass(frozen=True)
class ResourceLimits:
    timeout_seconds: float = 180.0
    cpus: float = 2.0
    memory_mib: int = 2048
    pids: int = 128
    nofile: int = 256
    evidence_mib: int = 256
    runtime_mib: int = 256
    tmp_mib: int = 256
    max_source_mib: int = 64

    def validate(self) -> "ResourceLimits":
        if not 1.0 <= self.timeout_seconds <= 3600.0:
            raise OracleContainerError("limit_timeout")
        if not 0.25 <= self.cpus <= 16.0:
            raise OracleContainerError("limit_cpus")
        if not 256 <= self.memory_mib <= 16384:
            raise OracleContainerError("limit_memory")
        if not 16 <= self.pids <= 1024:
            raise OracleContainerError("limit_pids")
        if not 64 <= self.nofile <= 4096:
            raise OracleContainerError("limit_nofile")
        if not 16 <= self.evidence_mib <= 1024:
            raise OracleContainerError("limit_evidence")
        if not 64 <= self.runtime_mib <= 2048:
            raise OracleContainerError("limit_runtime")
        if not 64 <= self.tmp_mib <= 2048:
            raise OracleContainerError("limit_tmp")
        if not 1 <= self.max_source_mib <= 1024:
            raise OracleContainerError("limit_source")
        return self

    @property
    def evidence_bytes(self) -> int:
        return self.evidence_mib * 1024 * 1024

    @property
    def max_source_bytes(self) -> int:
        return self.max_source_mib * 1024 * 1024


@dataclass(frozen=True)
class RenderConfig:
    source: Path
    font_pack: Path
    corpus: Path | None
    evidence_dir: Path
    run_id: str
    limits: ResourceLimits
    print_mode: str = "single-page-sheets"


@dataclass(frozen=True)
class FontPackIdentity:
    root: Path
    pack_sha256: str


@dataclass(frozen=True)
class CommandResult:
    status: str
    returncode: int | None
    stdout: bytes = b""
    stderr: bytes = b""


class CommandRunner(Protocol):
    def run(
        self,
        command: Sequence[str],
        *,
        timeout_seconds: float,
        output_limit_bytes: int,
        stdout_path: Path | None = None,
    ) -> CommandResult: ...


class BoundedProcessRunner:
    """Execute a command with process-group timeout and output bounds."""

    def run(
        self,
        command: Sequence[str],
        *,
        timeout_seconds: float,
        output_limit_bytes: int,
        stdout_path: Path | None = None,
    ) -> CommandResult:
        if not command:
            return CommandResult("not_found", None)
        try:
            process = subprocess.Popen(
                list(command),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=(os.name != "nt"),
            )
        except (FileNotFoundError, PermissionError, OSError):
            return CommandResult("not_found", None)

        output_limit_bytes = max(1, output_limit_bytes)
        stdout = bytearray()
        stderr = bytearray()
        total_read = 0
        over_limit = threading.Event()
        lock = threading.Lock()
        output_file = stdout_path.open("wb") if stdout_path is not None else None

        def drain(stream: Any, destination: bytearray | None) -> None:
            nonlocal total_read
            try:
                while True:
                    chunk = stream.read(64 * 1024)
                    if not chunk:
                        break
                    with lock:
                        remaining = max(0, output_limit_bytes - total_read)
                        retained = chunk[:remaining]
                        total_read += len(chunk)
                        if destination is not None:
                            destination.extend(retained)
                        elif output_file is not None and retained:
                            output_file.write(retained)
                        if total_read > output_limit_bytes:
                            over_limit.set()
            finally:
                stream.close()

        assert process.stdout is not None and process.stderr is not None
        threads = [
            threading.Thread(
                target=drain,
                args=(process.stdout, None if stdout_path else stdout),
                daemon=True,
            ),
            threading.Thread(target=drain, args=(process.stderr, stderr), daemon=True),
        ]
        for thread in threads:
            thread.start()

        deadline = time.monotonic() + timeout_seconds
        status: str | None = None
        while process.poll() is None:
            if over_limit.is_set():
                status = "output_limit"
                _terminate_process_group(process)
                break
            if time.monotonic() >= deadline:
                status = "timeout"
                _terminate_process_group(process)
                break
            time.sleep(0.01)

        try:
            returncode = process.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            _kill_process_group(process)
            returncode = process.wait()
        for thread in threads:
            thread.join(timeout=2.0)
        if output_file is not None:
            output_file.flush()
            output_file.close()

        if status is None and over_limit.is_set():
            status = "output_limit"
        if status is None:
            status = "ok" if returncode == 0 else "nonzero"
        return CommandResult(status, returncode, bytes(stdout), bytes(stderr))


def _terminate_process_group(process: subprocess.Popen[bytes]) -> None:
    try:
        if os.name == "nt":
            process.terminate()
        else:
            os.killpg(process.pid, signal.SIGTERM)
    except (ProcessLookupError, OSError):
        pass


def _kill_process_group(process: subprocess.Popen[bytes]) -> None:
    try:
        if os.name == "nt":
            process.kill()
        else:
            os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, OSError):
        pass


def canonical_json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha256_file(path: Path, limit: int) -> str:
    digest = hashlib.sha256()
    total = 0
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            total += len(chunk)
            if total > limit:
                raise OracleContainerError("file_limit")
            digest.update(chunk)
    return digest.hexdigest()


def load_lock(path: Path = DEFAULT_LOCK) -> tuple[dict[str, Any], bytes, str]:
    try:
        payload = path.read_bytes()
        if len(payload) > MAX_LOCK_BYTES:
            raise OracleContainerError("lock_limit")
        document = json.loads(payload)
    except (OSError, json.JSONDecodeError) as error:
        raise OracleContainerError("lock_unreadable") from error
    validate_lock(document)
    verify_locked_files(document, path.parent)
    return document, payload, build_contract_sha256(document)


def build_contract_sha256(document: dict[str, Any]) -> str:
    """Hash build inputs while excluding the optional post-build image pin.

    The OCI image config contains this digest as a label. Excluding only the
    expected image ID avoids a recursive digest while preserving a stable,
    reviewable contract for every byte that affects the build.
    """
    normalized = json.loads(json.dumps(document))
    normalized["built_image"]["expected_id"] = None
    return sha256_bytes(canonical_json_bytes(normalized))


def validate_lock(document: object) -> dict[str, Any]:
    if not isinstance(document, dict) or document.get("schema") != LOCK_SCHEMA:
        raise OracleContainerError("lock_schema")
    if set(document) != {
        "base_image",
        "built_image",
        "debian_snapshot",
        "files",
        "libreoffice",
        "runtime_defaults",
        "schema",
    }:
        raise OracleContainerError("lock_keys")
    base = document.get("base_image")
    if not isinstance(base, dict):
        raise OracleContainerError("lock_base")
    if base.get("platform") != "linux/amd64":
        raise OracleContainerError("lock_platform")
    reference = base.get("reference")
    digest = base.get("manifest_sha256")
    if not isinstance(digest, str) or not SHA256_RE.fullmatch(digest):
        raise OracleContainerError("lock_base_digest")
    if not isinstance(reference, str) or not reference.endswith(f"@sha256:{digest}"):
        raise OracleContainerError("lock_base_reference")

    snapshot = document.get("debian_snapshot")
    if not isinstance(snapshot, dict):
        raise OracleContainerError("lock_snapshot")
    if not re.fullmatch(r"[0-9]{8}T[0-9]{6}Z", str(snapshot.get("timestamp", ""))):
        raise OracleContainerError("lock_snapshot_timestamp")
    if snapshot.get("timestamp") not in str(snapshot.get("url", "")):
        raise OracleContainerError("lock_snapshot_url")

    libreoffice = document.get("libreoffice")
    if not isinstance(libreoffice, dict):
        raise OracleContainerError("lock_libreoffice")
    if libreoffice.get("version") != "26.2.3.2":
        raise OracleContainerError("lock_libreoffice_version")
    if libreoffice.get("platform") != "linux/x86_64":
        raise OracleContainerError("lock_libreoffice_platform")
    artifact = libreoffice.get("artifact")
    if not isinstance(artifact, dict):
        raise OracleContainerError("lock_artifact")
    if artifact.get("sha256") != LIBREOFFICE_ARTIFACT_SHA256:
        raise OracleContainerError("lock_artifact_sha256")
    if artifact.get("bytes") != 216_816_909:
        raise OracleContainerError("lock_artifact_bytes")
    if artifact.get("url") != (
        "https://download.documentfoundation.org/libreoffice/stable/26.2.3/"
        "deb/x86_64/LibreOffice_26.2.3_Linux_x86-64_deb.tar.gz"
    ):
        raise OracleContainerError("lock_artifact_url")

    files = document.get("files")
    if not isinstance(files, list) or not files:
        raise OracleContainerError("lock_files")
    paths = [row.get("path") for row in files if isinstance(row, dict)]
    if paths != sorted(set(paths)) or len(paths) != len(files):
        raise OracleContainerError("lock_file_order")
    required = {
        "Containerfile",
        "oracle-entrypoint.sh",
        "profile/registrymodifications.xcu",
    }
    if set(paths) != required:
        raise OracleContainerError("lock_file_set")
    for row in files:
        if not isinstance(row, dict):
            raise OracleContainerError("lock_file_row")
        safe_relative(row.get("path"))
        digest_value = row.get("sha256")
        size = row.get("bytes")
        if not isinstance(digest_value, str) or not SHA256_RE.fullmatch(digest_value):
            raise OracleContainerError("lock_file_sha256")
        if not isinstance(size, int) or not 0 < size <= 1024 * 1024:
            raise OracleContainerError("lock_file_bytes")
    built_image = document.get("built_image")
    if not isinstance(built_image, dict) or set(built_image) != {
        "expected_id",
        "identity_kind",
        "source_date_epoch",
        "unpinned_verification",
    }:
        raise OracleContainerError("lock_built_image")
    if built_image.get("identity_kind") != "oci_image_config_digest":
        raise OracleContainerError("lock_built_image_kind")
    if built_image.get("source_date_epoch") != SOURCE_DATE_EPOCH:
        raise OracleContainerError("lock_built_image_epoch")
    if built_image.get("unpinned_verification") != (
        "bootstrap_only_runtime_image_id_plus_exact_build_contract_and_labels"
    ):
        raise OracleContainerError("lock_built_image_verification")
    expected_id = built_image.get("expected_id")
    if expected_id is not None and (
        not isinstance(expected_id, str) or not IMAGE_ID_RE.fullmatch(expected_id)
    ):
        raise OracleContainerError("lock_built_image_id")
    if "built_image_digest" in document or "image_digest" in document:
        raise OracleContainerError("lock_ambiguous_image_claim")
    if document.get("runtime_defaults") != {
        "capabilities": "none",
        "cpus": "2.00",
        "evidence_mib": 256,
        "memory_mib": 2048,
        "network": "none",
        "nofile": 256,
        "pids": 128,
        "root_filesystem": "read_only",
        "timeout_seconds": 180,
    }:
        raise OracleContainerError("lock_runtime_defaults")
    return document


def safe_relative(value: object) -> str:
    if not isinstance(value, str) or not value or "\0" in value or "\\" in value:
        raise OracleContainerError("unsafe_relative_path")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or path.as_posix() != value:
        raise OracleContainerError("unsafe_relative_path")
    return value


def verify_locked_files(document: dict[str, Any], root: Path) -> None:
    for row in document["files"]:
        path = root / row["path"]
        try:
            metadata = path.lstat()
        except OSError as error:
            raise OracleContainerError("locked_file_missing") from error
        if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
            raise OracleContainerError("locked_file_type")
        if metadata.st_size != row["bytes"]:
            raise OracleContainerError("locked_file_size")
        if sha256_file(path, 1024 * 1024) != row["sha256"]:
            raise OracleContainerError("locked_file_hash")


def validate_run_id(value: str) -> str:
    if not RUN_ID_RE.fullmatch(value):
        raise OracleContainerError("invalid_run_id")
    return value


def validate_image_reference(value: str) -> str:
    if not IMAGE_RE.fullmatch(value) or value.startswith("-"):
        raise OracleContainerError("invalid_image_reference")
    return value


def validate_source(path: Path, maximum: int) -> tuple[Path, int, str, str]:
    try:
        resolved = path.resolve(strict=True)
        metadata = resolved.stat()
    except OSError as error:
        raise OracleContainerError("source_unreadable") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise OracleContainerError("source_type")
    extension = resolved.suffix.lower()
    if extension not in SUPPORTED_EXTENSIONS:
        raise OracleContainerError("source_extension")
    if not 0 < metadata.st_size <= maximum:
        raise OracleContainerError("source_size")
    return resolved, metadata.st_size, sha256_file(resolved, maximum), extension


def validate_directory(path: Path, code: str) -> Path:
    try:
        if path.is_symlink():
            raise OracleContainerError(f"{code}_symlink")
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise OracleContainerError(f"{code}_unreadable") from error
    if not resolved.is_dir():
        raise OracleContainerError(f"{code}_type")
    if "," in str(resolved) or "\0" in str(resolved):
        raise OracleContainerError(f"{code}_mount_path")
    return resolved


def validate_font_pack(path: Path) -> FontPackIdentity:
    root = validate_directory(path, "font_pack")
    manifest_path = root / "manifest.json"
    config_path = root / "fonts.conf"
    fonts_dir = root / "fonts"
    try:
        document = json.loads(manifest_path.read_bytes())
    except (OSError, json.JSONDecodeError) as error:
        raise OracleContainerError("font_pack_manifest") from error
    if not isinstance(document, dict) or document.get("schema") != FONT_PACK_SCHEMA:
        raise OracleContainerError("font_pack_schema")
    if not config_path.is_file() or config_path.is_symlink() or not fonts_dir.is_dir():
        raise OracleContainerError("font_pack_layout")

    file_count = 0
    total = 0
    actual_paths: set[str] = set()
    for item in sorted(root.rglob("*")):
        metadata = item.lstat()
        if item.is_symlink():
            raise OracleContainerError("font_pack_symlink")
        if item.is_dir():
            continue
        if not stat.S_ISREG(metadata.st_mode):
            raise OracleContainerError("font_pack_file_type")
        file_count += 1
        total += metadata.st_size
        actual_paths.add(item.relative_to(root).as_posix())
        if file_count > MAX_FONT_PACK_FILES or total > MAX_FONT_PACK_BYTES:
            raise OracleContainerError("font_pack_limit")

    expected_config_sha = document.get("fonts_conf_sha256")
    if not isinstance(expected_config_sha, str) or not SHA256_RE.fullmatch(
        expected_config_sha
    ):
        raise OracleContainerError("font_pack_config_hash")
    if sha256_file(config_path, 1024 * 1024) != expected_config_sha:
        raise OracleContainerError("font_pack_config_mismatch")
    fonts = document.get("fonts")
    if not isinstance(fonts, list) or not fonts:
        raise OracleContainerError("font_pack_fonts")
    expected_paths = {"fonts.conf", "manifest.json"}
    for row in fonts:
        _verify_font_pack_row(root, row, "font")
        expected_paths.add(safe_relative(row.get("output")))
    licenses = document.get("licenses")
    if not isinstance(licenses, list) or not licenses:
        raise OracleContainerError("font_pack_licenses")
    for row in licenses:
        _verify_font_pack_row(root, row, "license")
        expected_paths.add(safe_relative(row.get("output")))
    if actual_paths != expected_paths:
        raise OracleContainerError("font_pack_file_set")
    content_bytes = total - manifest_path.stat().st_size
    if document.get("total_bytes") != content_bytes:
        raise OracleContainerError("font_pack_total")
    identity = {
        "fonts": fonts,
        "fonts_conf_sha256": expected_config_sha,
        "licenses": licenses,
    }
    aliases = document.get("aliases")
    if aliases is not None:
        if not isinstance(aliases, list) or len(aliases) > 128:
            raise OracleContainerError("font_pack_aliases")
        available_families = {
            row.get("family", "").strip().lower()
            for row in fonts
            if isinstance(row, dict) and isinstance(row.get("family"), str)
        }
        normalized_aliases = []
        for alias in aliases:
            if not isinstance(alias, dict) or set(alias) != {"family", "substitute"}:
                raise OracleContainerError("font_pack_alias")
            family = alias.get("family")
            substitute = alias.get("substitute")
            if (
                not isinstance(family, str)
                or not 0 < len(family) <= 128
                or family != family.strip()
                or not family.isascii()
                or not family.isprintable()
                or not isinstance(substitute, str)
                or not 0 < len(substitute) <= 128
                or substitute != substitute.strip()
                or not substitute.isascii()
                or not substitute.isprintable()
                or substitute.lower() not in available_families
            ):
                raise OracleContainerError("font_pack_alias")
            normalized_aliases.append(family.lower())
        if normalized_aliases != sorted(set(normalized_aliases)):
            raise OracleContainerError("font_pack_alias_order")
        identity["aliases"] = aliases
    expected_pack_sha = document.get("pack_sha256")
    if (
        not isinstance(expected_pack_sha, str)
        or not SHA256_RE.fullmatch(expected_pack_sha)
        or sha256_bytes(canonical_json_bytes(identity)) != expected_pack_sha
    ):
        raise OracleContainerError("font_pack_identity")
    return FontPackIdentity(root, expected_pack_sha)


def _verify_font_pack_row(root: Path, row: object, kind: str) -> None:
    if not isinstance(row, dict):
        raise OracleContainerError(f"font_pack_{kind}_row")
    relative = safe_relative(row.get("output"))
    expected_sha = row.get("sha256")
    expected_bytes = row.get("bytes")
    if not isinstance(expected_sha, str) or not SHA256_RE.fullmatch(expected_sha):
        raise OracleContainerError(f"font_pack_{kind}_sha256")
    if not isinstance(expected_bytes, int) or not 0 < expected_bytes <= MAX_FONT_PACK_BYTES:
        raise OracleContainerError(f"font_pack_{kind}_bytes")
    path = root / relative
    if not path.is_file() or path.is_symlink() or path.stat().st_size != expected_bytes:
        raise OracleContainerError(f"font_pack_{kind}_missing")
    if sha256_file(path, MAX_FONT_PACK_BYTES) != expected_sha:
        raise OracleContainerError(f"font_pack_{kind}_mismatch")


def mount_spec(source: Path, target: str) -> str:
    if "," in str(source):
        raise OracleContainerError("mount_path_comma")
    return f"type=bind,source={source},target={target},readonly"


def build_create_command(
    engine: str,
    image: str,
    config: RenderConfig,
    *,
    source_mount: Path,
    font_mount: Path,
    corpus_mount: Path,
    source_bytes: int,
    source_sha256: str,
    extension: str,
    lock_sha256: str,
    font_pack_sha256: str,
) -> list[str]:
    limits = config.limits.validate()
    validate_run_id(config.run_id)
    if config.print_mode not in PRINT_MODES:
        raise OracleContainerError("print_mode")
    name = f"rxls-lo-{config.run_id}"
    memory = f"{limits.memory_mib}m"
    command = [
        engine,
        "create",
        "--name",
        name,
        "--hostname",
        "rxls-oracle",
        "--platform",
        "linux/amd64",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges=true" if engine == "docker" else "no-new-privileges",
        "--pids-limit",
        str(limits.pids),
        "--cpus",
        format(limits.cpus, ".2f"),
        "--memory",
        memory,
        "--memory-swap",
        memory,
        "--ulimit",
        f"nofile={limits.nofile}:{limits.nofile}",
        "--ulimit",
        f"fsize={limits.evidence_bytes}:{limits.evidence_bytes}",
        "--stop-timeout",
        "10",
        "--init",
        "--ipc",
        "private",
        "--shm-size",
        "64m",
        "--user",
        "65534:65534",
        "--workdir",
        "/oracle",
        "--tmpfs",
        (
            "/oracle/evidence:rw,noexec,nosuid,nodev,"
            f"size={limits.evidence_bytes},mode=0700,uid=65534,gid=65534"
        ),
        "--tmpfs",
        (
            "/oracle/runtime:rw,noexec,nosuid,nodev,"
            f"size={limits.runtime_mib * 1024 * 1024},"
            "mode=0700,uid=65534,gid=65534"
        ),
        "--tmpfs",
        (
            "/tmp:rw,noexec,nosuid,nodev,"
            f"size={limits.tmp_mib * 1024 * 1024},mode=1777"
        ),
        "--mount",
        mount_spec(source_mount, f"/oracle/source/input{extension}"),
        "--mount",
        mount_spec(font_mount, "/oracle/fonts"),
        "--mount",
        mount_spec(corpus_mount, "/oracle/corpus"),
    ]
    environment = {
        "HOME": f"/oracle/runtime/{config.run_id}/home",
        "XDG_CACHE_HOME": f"/oracle/runtime/{config.run_id}/cache",
        "XDG_CONFIG_HOME": f"/oracle/runtime/{config.run_id}/config",
        "XDG_DATA_HOME": f"/oracle/runtime/{config.run_id}/data",
        "TMPDIR": f"/oracle/runtime/{config.run_id}/tmp",
        "RXLS_EVIDENCE_MAX_BYTES": str(limits.evidence_bytes),
        "RXLS_FONT_PACK_SHA256": font_pack_sha256,
        "RXLS_LOCK_SHA256": lock_sha256,
        "RXLS_PRINT_MODE": config.print_mode,
        "RXLS_RUN_ID": config.run_id,
        "RXLS_SOURCE_BYTES": str(source_bytes),
        "RXLS_SOURCE_EXTENSION": extension,
        "RXLS_SOURCE_SHA256": source_sha256,
    }
    for key in sorted(environment):
        command.extend(["--env", f"{key}={environment[key]}"])
    command.append(validate_image_reference(image))
    return command


def build_build_command(
    engine: str, image: str, lock_sha256: str
) -> list[str]:
    validate_image_reference(image)
    if not SHA256_RE.fullmatch(lock_sha256):
        raise OracleContainerError("invalid_lock_sha256")
    return [
        engine,
        "build",
        "--platform",
        "linux/amd64",
        "--pull=false",
        "--build-arg",
        f"ORACLE_LOCK_SHA256={lock_sha256}",
        "--build-arg",
        f"SOURCE_DATE_EPOCH={SOURCE_DATE_EPOCH}",
        "--tag",
        image,
        "--file",
        str(CONTAINERFILE),
        str(CONTAINER_DIR),
    ]


def path_neutral_command(
    command: Sequence[str], replacements: Sequence[tuple[Path, str]]
) -> list[str]:
    """Redact host paths from a printable dry-run command plan."""
    rendered = []
    ordered = sorted(
        ((str(path), label) for path, label in replacements),
        key=lambda item: len(item[0]),
        reverse=True,
    )
    for token in command:
        for host_path, label in ordered:
            token = token.replace(host_path, label)
        rendered.append(token)
    return rendered


def inspect_image(
    runner: CommandRunner,
    engine: str,
    image: str,
    lock_sha256: str,
    expected_image_id: str | None = None,
) -> str:
    result = runner.run(
        [engine, "image", "inspect", image],
        timeout_seconds=30.0,
        output_limit_bytes=4 * 1024 * 1024,
    )
    if result.status != "ok":
        raise OracleContainerError("image_inspect_failed")
    try:
        document = json.loads(result.stdout)
        row = document[0]
        image_id = row["Id"]
        architecture = row["Architecture"]
        labels = row["Config"]["Labels"]
    except (json.JSONDecodeError, KeyError, IndexError, TypeError) as error:
        raise OracleContainerError("image_inspect_schema") from error
    if isinstance(image_id, str) and re.fullmatch(r"[0-9a-f]{64}", image_id):
        image_id = f"sha256:{image_id}"
    if not isinstance(image_id, str) or not IMAGE_ID_RE.fullmatch(image_id):
        raise OracleContainerError("image_id")
    if expected_image_id is not None and image_id != expected_image_id:
        raise OracleContainerError("image_id_mismatch")
    if architecture not in {"amd64", "x86_64"}:
        raise OracleContainerError("image_architecture")
    if not isinstance(labels, dict):
        raise OracleContainerError("image_labels")
    expected = {**EXPECTED_IMAGE_LABELS, "org.rxls.render-oracle.lock-sha256": lock_sha256}
    for key, value in expected.items():
        if labels.get(key) != value:
            raise OracleContainerError("image_label_mismatch")
    return image_id


def resolve_engine(requested: str, *, execute: bool) -> str:
    if requested not in {"auto", "docker", "podman"}:
        raise OracleContainerError("engine_value")
    if requested != "auto":
        if execute and shutil.which(requested) is None:
            raise OracleContainerError("engine_not_found")
        return requested
    for candidate in ("docker", "podman"):
        if shutil.which(candidate) is not None:
            return candidate
    if execute:
        raise OracleContainerError("engine_not_found")
    return "docker"


def validate_render_config(
    config: RenderConfig,
) -> tuple[Path, int, str, str, FontPackIdentity, Path | None]:
    limits = config.limits.validate()
    validate_run_id(config.run_id)
    if config.print_mode not in PRINT_MODES:
        raise OracleContainerError("print_mode")
    source, source_bytes, source_sha, extension = validate_source(
        config.source, limits.max_source_bytes
    )
    font_pack = validate_font_pack(config.font_pack)
    corpus = validate_directory(config.corpus, "corpus") if config.corpus else None
    evidence = config.evidence_dir.resolve(strict=False)
    if evidence.exists():
        if not evidence.is_dir() or evidence.is_symlink():
            raise OracleContainerError("evidence_type")
        try:
            if next(evidence.iterdir(), None) is not None:
                raise OracleContainerError("evidence_not_empty")
        except OSError as error:
            raise OracleContainerError("evidence_unreadable") from error
    for protected in (source, font_pack, corpus):
        protected_path = protected.root if isinstance(protected, FontPackIdentity) else protected
        if protected_path is not None and (
            evidence == protected_path or protected_path in evidence.parents
        ):
            raise OracleContainerError("evidence_overlap")
    return source, source_bytes, source_sha, extension, font_pack, corpus


def prepare_staging_inputs(
    temporary: Path,
    source: Path,
    extension: str,
    font_pack: Path,
    corpus: Path | None,
) -> tuple[Path, Path, Path]:
    source_root = temporary / "source"
    source_root.mkdir(mode=0o755)
    source_copy = source_root / f"input{extension}"
    shutil.copyfile(source, source_copy)
    source_copy.chmod(0o444)
    source_root.chmod(0o555)

    font_copy = temporary / "font-pack"
    shutil.copytree(font_pack, font_copy, symlinks=False)
    for item in sorted(font_copy.rglob("*"), reverse=True):
        item.chmod(0o555 if item.is_dir() else 0o444)
    font_copy.chmod(0o555)

    if corpus is None:
        corpus_mount = temporary / "corpus"
        corpus_mount.mkdir(mode=0o555)
    else:
        corpus_mount = corpus
    return source_copy, font_copy, corpus_mount


def render_plan(
    config: RenderConfig,
    engine: str,
    image: str,
    lock_sha256: str,
    expected_image_id: str | None = None,
) -> dict[str, Any]:
    source, source_bytes, source_sha, extension, font_pack, corpus = (
        validate_render_config(config)
    )
    # The execute path creates an empty staged corpus directory. Reuse the
    # already validated, non-sensitive font directory as the dry-run stand-in
    # so the printed plan never exposes the source's sibling files.
    corpus_mount = corpus if corpus is not None else font_pack.root
    create = build_create_command(
        engine,
        image,
        config,
        source_mount=source,
        font_mount=font_pack.root,
        corpus_mount=corpus_mount,
        source_bytes=source_bytes,
        source_sha256=source_sha,
        extension=extension,
        lock_sha256=lock_sha256,
        font_pack_sha256=font_pack.pack_sha256,
    )
    create = path_neutral_command(
        create,
        [
            (source, "<source>"),
            (font_pack.root, "<font-pack>"),
            (corpus_mount, "<corpus>"),
            (config.evidence_dir.resolve(strict=False), "<evidence-dir>"),
        ],
    )
    name = f"rxls-lo-{config.run_id}"
    return {
        "commands": {
            "cleanup": [engine, "rm", "--force", name],
            "create": create,
            "start": [engine, "start", "--attach", name],
        },
        "dry_run": True,
        "evidence_contract": {
            "contains_host_paths": False,
            "schema": EXECUTION_SCHEMA,
        },
        "image_verified": False,
        "expected_image_id": expected_image_id,
        "schema": PLAN_SCHEMA,
    }


def execute_render(
    config: RenderConfig,
    engine: str,
    image: str,
    lock_sha256: str,
    expected_image_id: str | None = None,
    lock_file_sha256: str | None = None,
    *,
    runner: CommandRunner | None = None,
) -> dict[str, Any]:
    runner = runner or BoundedProcessRunner()
    if lock_file_sha256 is None:
        lock_file_sha256 = lock_sha256
    if not SHA256_RE.fullmatch(lock_file_sha256):
        raise OracleContainerError("invalid_lock_file_sha256")
    source, source_bytes, source_sha, extension, font_pack, corpus = (
        validate_render_config(config)
    )
    image_id = inspect_image(
        runner, engine, image, lock_sha256, expected_image_id
    )
    name = f"rxls-lo-{config.run_id}"
    destination = config.evidence_dir.resolve(strict=False)
    parent = destination.parent
    parent.mkdir(parents=True, exist_ok=True)
    atomic_stage = Path(tempfile.mkdtemp(prefix=".rxls-oracle-evidence-", dir=parent))
    completed = False
    try:
        with tempfile.TemporaryDirectory(prefix="rxls-render-oracle-") as raw:
            temporary = Path(raw)
            source_mount, font_mount, corpus_mount = prepare_staging_inputs(
                temporary, source, extension, font_pack.root, corpus
            )
            staged_font_pack = validate_font_pack(font_mount)
            if staged_font_pack.pack_sha256 != font_pack.pack_sha256:
                raise OracleContainerError("font_pack_staging_identity")
            archive = temporary / "evidence.tar"
            create = build_create_command(
                engine,
                image_id,
                config,
                source_mount=source_mount,
                font_mount=font_mount,
                corpus_mount=corpus_mount,
                source_bytes=source_bytes,
                source_sha256=source_sha,
                extension=extension,
                lock_sha256=lock_sha256,
                font_pack_sha256=font_pack.pack_sha256,
            )
            created = runner.run(
                create,
                timeout_seconds=30.0,
                output_limit_bytes=MAX_ENGINE_DIAGNOSTIC_BYTES,
            )
            if created.status != "ok":
                raise OracleContainerError("container_create_failed")
            try:
                started = runner.run(
                    [engine, "start", "--attach", name],
                    timeout_seconds=config.limits.timeout_seconds,
                    output_limit_bytes=config.limits.evidence_bytes + 4 * 1024 * 1024,
                    stdout_path=archive,
                )
                if started.status != "ok":
                    raise OracleContainerError(f"container_start_{started.status}")
            finally:
                runner.run(
                    [engine, "rm", "--force", name],
                    timeout_seconds=30.0,
                    output_limit_bytes=MAX_ENGINE_DIAGNOSTIC_BYTES,
                )

            extract_evidence_archive(
                archive,
                atomic_stage,
                maximum_bytes=config.limits.evidence_bytes,
            )
            output = validate_output_evidence(
                atomic_stage,
                source_sha256=source_sha,
                source_bytes=source_bytes,
                extension=extension,
                lock_sha256=lock_sha256,
                font_pack_sha256=font_pack.pack_sha256,
                print_mode=config.print_mode,
            )
            reject_host_paths(
                atomic_stage,
                [source, font_pack.root, corpus, destination],
                maximum_bytes=config.limits.evidence_bytes,
            )
            execution = build_execution_evidence(
                engine=engine,
                image_id=image_id,
                lock_sha256=lock_sha256,
                source_sha256=source_sha,
                source_bytes=source_bytes,
                extension=extension,
                limits=config.limits,
                output=output,
                font_pack_sha256=font_pack.pack_sha256,
                expected_image_id=expected_image_id,
                lock_file_sha256=lock_file_sha256,
            )
            (atomic_stage / "execution.json").write_bytes(
                canonical_json_bytes(execution)
            )
            reject_absolute_strings(execution)

        if destination.exists():
            destination.rmdir()
        os.replace(atomic_stage, destination)
        completed = True
        return execution
    finally:
        if not completed:
            shutil.rmtree(atomic_stage, ignore_errors=True)


def extract_evidence_archive(
    archive: Path, destination: Path, *, maximum_bytes: int
) -> None:
    try:
        archive_size = archive.stat().st_size
    except OSError as error:
        raise OracleContainerError("evidence_archive_missing") from error
    if not 0 < archive_size <= maximum_bytes + 4 * 1024 * 1024:
        raise OracleContainerError("evidence_archive_limit")
    total = 0
    count = 0
    names: list[str] = []
    try:
        with tarfile.open(archive, mode="r:*") as bundle:
            for member in bundle:
                name = safe_relative(member.name.removeprefix("./"))
                if not member.isfile() or member.issym() or member.islnk():
                    raise OracleContainerError("evidence_member_type")
                if member.size < 0:
                    raise OracleContainerError("evidence_member_size")
                count += 1
                total += member.size
                if count > MAX_EVIDENCE_FILES or total > maximum_bytes:
                    raise OracleContainerError("evidence_member_limit")
                if name in names:
                    raise OracleContainerError("evidence_member_duplicate")
                names.append(name)
                source = bundle.extractfile(member)
                if source is None:
                    raise OracleContainerError("evidence_member_unreadable")
                target = destination / name
                target.parent.mkdir(parents=True, exist_ok=True)
                written = 0
                with target.open("wb") as output:
                    while True:
                        chunk = source.read(1024 * 1024)
                        if not chunk:
                            break
                        written += len(chunk)
                        if written > member.size:
                            raise OracleContainerError("evidence_member_overflow")
                        output.write(chunk)
                if written != member.size:
                    raise OracleContainerError("evidence_member_truncated")
                target.chmod(0o444)
    except (OSError, tarfile.TarError) as error:
        raise OracleContainerError("evidence_archive_invalid") from error
    if sorted(names) != ["oracle-manifest.json", "oracle.pdf"]:
        raise OracleContainerError("evidence_member_set")


def validate_output_evidence(
    root: Path,
    *,
    source_sha256: str,
    source_bytes: int,
    extension: str,
    lock_sha256: str,
    font_pack_sha256: str,
    print_mode: str = "single-page-sheets",
) -> dict[str, Any]:
    manifest_path = root / "oracle-manifest.json"
    pdf_path = root / "oracle.pdf"
    try:
        manifest = json.loads(manifest_path.read_bytes())
    except (OSError, json.JSONDecodeError) as error:
        raise OracleContainerError("output_manifest_unreadable") from error
    if not isinstance(manifest, dict) or manifest.get("schema") != OUTPUT_SCHEMA:
        raise OracleContainerError("output_manifest_schema")
    if manifest.get("lock_sha256") != lock_sha256:
        raise OracleContainerError("output_lock_mismatch")
    if manifest.get("font_pack_sha256") != font_pack_sha256:
        raise OracleContainerError("output_font_pack_mismatch")
    if manifest.get("oracle") != {
        "artifact_sha256": LIBREOFFICE_ARTIFACT_SHA256,
        "name": "LibreOffice",
        "version": "26.2.3.2",
    }:
        raise OracleContainerError("output_oracle_identity")
    if manifest.get("source") != {
        "bytes": source_bytes,
        "path": f"source/input{extension}",
        "sha256": source_sha256,
    }:
        raise OracleContainerError("output_source_identity")
    if print_mode not in PRINT_MODES:
        raise OracleContainerError("print_mode")
    if manifest.get("export") != {
        "filter": "calc_pdf_Export",
        "single_page_sheets": print_mode == "single-page-sheets",
    }:
        raise OracleContainerError("output_export_contract")
    artifact = manifest.get("artifact")
    if not isinstance(artifact, dict) or artifact.get("path") != "oracle/oracle.pdf":
        raise OracleContainerError("output_artifact_contract")
    try:
        pdf_size = pdf_path.stat().st_size
        if pdf_path.read_bytes()[:5] != b"%PDF-":
            raise OracleContainerError("output_pdf_header")
    except OSError as error:
        raise OracleContainerError("output_pdf_unreadable") from error
    if artifact.get("bytes") != pdf_size:
        raise OracleContainerError("output_pdf_size")
    digest = sha256_file(pdf_path, max(pdf_size, 1))
    if artifact.get("sha256") != digest:
        raise OracleContainerError("output_pdf_hash")
    reject_absolute_strings(manifest)
    return manifest


def reject_absolute_strings(value: object) -> None:
    if isinstance(value, dict):
        for item in value.values():
            reject_absolute_strings(item)
    elif isinstance(value, list):
        for item in value:
            reject_absolute_strings(item)
    elif isinstance(value, str):
        lowered = value.lower()
        if (
            value.startswith("/")
            or re.match(r"[a-zA-Z]:[\\/]", value)
            or lowered.startswith("file://")
        ):
            raise OracleContainerError("evidence_absolute_path")


def reject_host_paths(
    root: Path,
    paths: Sequence[Path | None],
    *,
    maximum_bytes: int,
) -> None:
    needles: set[bytes] = set()
    for path in paths:
        if path is None:
            continue
        for text in {str(path), str(path.resolve(strict=False))}:
            for candidate in (text, quote(text), f"file://{text}"):
                needles.add(candidate.encode("utf-8"))
    total = 0
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.is_symlink():
            continue
        payload = path.read_bytes()
        total += len(payload)
        if total > maximum_bytes:
            raise OracleContainerError("evidence_scan_limit")
        if any(needle and needle in payload for needle in needles):
            raise OracleContainerError("evidence_host_path")


def build_execution_evidence(
    *,
    engine: str,
    image_id: str,
    lock_sha256: str,
    source_sha256: str,
    source_bytes: int,
    extension: str,
    limits: ResourceLimits,
    output: dict[str, Any],
    font_pack_sha256: str,
    expected_image_id: str | None,
    lock_file_sha256: str,
) -> dict[str, Any]:
    return {
        "artifacts": {
            "manifest": "oracle/oracle-manifest.json",
            "pdf": output["artifact"],
        },
        "image": {
            "architecture": "linux/amd64",
            "expected_id": expected_image_id,
            "id": image_id,
            "identity_status": (
                "pinned_match" if expected_image_id is not None else "runtime_verified"
            ),
            "lock_sha256": lock_sha256,
        },
        "font_pack_sha256": font_pack_sha256,
        "isolation": {
            "capabilities": "none",
            "corpus_mount": "read_only",
            "evidence_mount": "size_capped_tmpfs",
            "external_links": "network_and_filesystem_isolated",
            "font_mount": "read_only",
            "macro_execution": "disabled",
            "network": "none",
            "no_new_privileges": True,
            "root_filesystem": "read_only",
            "source_mount": "read_only",
            "unique_home_xdg_profile": True,
        },
        "limits": {
            "cpus": format(limits.cpus, ".2f"),
            "evidence_bytes": limits.evidence_bytes,
            "memory_bytes": limits.memory_mib * 1024 * 1024,
            "nofile": limits.nofile,
            "pids": limits.pids,
            "timeout_milliseconds": int(limits.timeout_seconds * 1000),
        },
        "lock_file_sha256": lock_file_sha256,
        "runtime": engine,
        "schema": EXECUTION_SCHEMA,
        "source": {
            "bytes": source_bytes,
            "path": f"source/input{extension}",
            "sha256": source_sha256,
        },
    }


def execute_build(
    engine: str,
    image: str,
    lock_sha256: str,
    expected_image_id: str | None = None,
    *,
    runner: CommandRunner | None = None,
) -> str:
    runner = runner or BoundedProcessRunner()
    result = runner.run(
        build_build_command(engine, image, lock_sha256),
        timeout_seconds=1800.0,
        output_limit_bytes=16 * 1024 * 1024,
    )
    if result.status != "ok":
        raise OracleContainerError(f"image_build_{result.status}")
    return inspect_image(
        runner, engine, image, lock_sha256, expected_image_id
    )


def add_mode_flags(parser: argparse.ArgumentParser) -> None:
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--execute", action="store_true")


def pin_image_from_evidence(
    lock: dict[str, Any],
    lock_payload: bytes,
    lock_sha256: str,
    evidence_path: Path,
) -> dict[str, Any]:
    """Validate one hosted bootstrap build and return a pinned lock document."""
    if lock["built_image"]["expected_id"] is not None:
        raise OracleContainerError("image_lock_already_pinned")
    try:
        payload = evidence_path.read_bytes()
        if len(payload) > MAX_LOCK_BYTES:
            raise OracleContainerError("bootstrap_build_limit")
        evidence = json.loads(payload)
    except (OSError, json.JSONDecodeError) as error:
        raise OracleContainerError("bootstrap_build_unreadable") from error
    if not isinstance(evidence, dict) or set(evidence) != {
        "build_contract_sha256",
        "built_image_id",
        "expected_image_id",
        "image_identity_status",
        "lock_file_sha256",
        "platform",
        "status",
    }:
        raise OracleContainerError("bootstrap_build_schema")
    image_id = evidence.get("built_image_id")
    if (
        evidence.get("build_contract_sha256") != lock_sha256
        or evidence.get("expected_image_id") is not None
        or evidence.get("image_identity_status") != "bootstrap_capture_required"
        or evidence.get("lock_file_sha256") != sha256_bytes(lock_payload)
        or evidence.get("platform") != "linux/amd64"
        or evidence.get("status") != "ok"
        or not isinstance(image_id, str)
        or IMAGE_ID_RE.fullmatch(image_id) is None
    ):
        raise OracleContainerError("bootstrap_build_identity")
    pinned = json.loads(json.dumps(lock))
    pinned["built_image"]["expected_id"] = image_id
    validate_lock(pinned)
    return pinned


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    subparsers = parser.add_subparsers(dest="action", required=True)

    verify = subparsers.add_parser("verify-lock", help="verify pins and local assets")
    verify.add_argument("--bootstrap-identities", action="store_true")
    verify.set_defaults(action="verify-lock")

    pin = subparsers.add_parser(
        "pin-image", help="validate hosted bootstrap evidence and emit a pinned lock"
    )
    pin.add_argument("--build-evidence", required=True, type=Path)

    build = subparsers.add_parser("build", help="build the locked linux/amd64 image")
    build.add_argument("--engine", choices=("auto", "docker", "podman"), default="auto")
    build.add_argument("--image", default="rxls-render-oracle:lo-26.2.3")
    build.add_argument("--bootstrap-identities", action="store_true")
    add_mode_flags(build)

    render = subparsers.add_parser("render", help="render one workbook in isolation")
    render.add_argument("--engine", choices=("auto", "docker", "podman"), default="auto")
    render.add_argument("--image", default="rxls-render-oracle:lo-26.2.3")
    render.add_argument("--source", required=True, type=Path)
    render.add_argument("--font-pack", required=True, type=Path)
    render.add_argument("--corpus", type=Path)
    render.add_argument("--evidence-dir", required=True, type=Path)
    render.add_argument("--run-id", default=None)
    render.add_argument(
        "--print-mode",
        choices=tuple(sorted(PRINT_MODES)),
        default="single-page-sheets",
        help="use one-page-per-sheet export or retain authored pagination",
    )
    render.add_argument("--timeout-seconds", type=float, default=180.0)
    render.add_argument("--cpus", type=float, default=2.0)
    render.add_argument("--memory-mib", type=int, default=2048)
    render.add_argument("--pids", type=int, default=128)
    render.add_argument("--nofile", type=int, default=256)
    render.add_argument("--evidence-mib", type=int, default=256)
    render.add_argument("--runtime-mib", type=int, default=256)
    render.add_argument("--tmp-mib", type=int, default=256)
    render.add_argument("--max-source-mib", type=int, default=64)
    add_mode_flags(render)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        lock, payload, lock_sha256 = load_lock(args.lock)
        lock_file_sha256 = sha256_bytes(payload)
        expected_image_id = lock["built_image"]["expected_id"]
        if args.action == "verify-lock":
            if expected_image_id is None and not args.bootstrap_identities:
                raise OracleContainerError("image_pin_required")
            print(
                canonical_json_bytes(
                    {
                        "build_contract_sha256": lock_sha256,
                        "expected_image_id": expected_image_id,
                        "lock_file_sha256": lock_file_sha256,
                        "schema": LOCK_SCHEMA,
                        "status": "ok",
                    }
                ).decode("utf-8"),
                end="",
            )
            return 0

        if args.action == "pin-image":
            pinned = pin_image_from_evidence(
                lock, payload, lock_sha256, args.build_evidence
            )
            print(canonical_json_bytes(pinned).decode("utf-8"), end="")
            return 0

        engine = resolve_engine(args.engine, execute=args.execute)
        image = validate_image_reference(args.image)
        if args.action == "build":
            if args.dry_run:
                document = {
                    "commands": {
                        "build": path_neutral_command(
                            build_build_command(engine, image, lock_sha256),
                            [
                                (CONTAINERFILE, "<container-context>/Containerfile"),
                                (CONTAINER_DIR, "<container-context>"),
                            ],
                        )
                    },
                    "dry_run": True,
                    "expected_image_id": expected_image_id,
                    "image_verified": False,
                    "schema": PLAN_SCHEMA,
                }
            else:
                if expected_image_id is None and not args.bootstrap_identities:
                    raise OracleContainerError("image_pin_required")
                image_id = execute_build(
                    engine, image, lock_sha256, expected_image_id
                )
                document = {
                    "build_contract_sha256": lock_sha256,
                    "built_image_id": image_id,
                    "expected_image_id": expected_image_id,
                    "image_identity_status": (
                        "pinned_match"
                        if expected_image_id is not None
                        else "bootstrap_capture_required"
                    ),
                    "lock_file_sha256": lock_file_sha256,
                    "platform": "linux/amd64",
                    "status": "ok",
                }
            print(canonical_json_bytes(document).decode("utf-8"), end="")
            return 0

        limits = ResourceLimits(
            timeout_seconds=args.timeout_seconds,
            cpus=args.cpus,
            memory_mib=args.memory_mib,
            pids=args.pids,
            nofile=args.nofile,
            evidence_mib=args.evidence_mib,
            runtime_mib=args.runtime_mib,
            tmp_mib=args.tmp_mib,
            max_source_mib=args.max_source_mib,
        )
        config = RenderConfig(
            source=args.source,
            font_pack=args.font_pack,
            corpus=args.corpus,
            evidence_dir=args.evidence_dir,
            run_id=validate_run_id(args.run_id or secrets.token_hex(8)),
            limits=limits,
            print_mode=args.print_mode,
        )
        if args.dry_run:
            document = render_plan(
                config, engine, image, lock_sha256, expected_image_id
            )
        else:
            if expected_image_id is None:
                raise OracleContainerError("image_pin_required")
            document = execute_render(
                config,
                engine,
                image,
                lock_sha256,
                expected_image_id,
                lock_file_sha256,
            )
        print(canonical_json_bytes(document).decode("utf-8"), end="")
        return 0
    except OracleContainerError as error:
        print(f"render_oracle_error:{error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
