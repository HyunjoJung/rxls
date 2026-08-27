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
from datetime import datetime, timezone
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
import zipfile


WRAPPER_PATH = Path(__file__).resolve()
ROOT = WRAPPER_PATH.parents[1]
CONTAINER_DIR = ROOT / "scripts" / "render-oracle-container"
DEFAULT_LOCK = CONTAINER_DIR / "lock.json"
PINNED_LOCK_OUTPUT = CONTAINER_DIR / "lock.pinned.json"
CONTAINERFILE = CONTAINER_DIR / "Containerfile"
WRAPPER_RELATIVE_PATH = "scripts/run-render-oracle-container.py"
LOCK_RELATIVE_PATH = "scripts/render-oracle-container/lock.json"
LOCK_SCHEMA = "rxls.render-oracle-container-lock.v3"
OUTPUT_SCHEMA = "rxls.render-oracle-container-output.v2"
EXECUTION_SCHEMA = "rxls.render-oracle-container-execution.v3"
PLAN_SCHEMA = "rxls.render-oracle-container-plan.v1"
BUILD_EVIDENCE_SCHEMA = "rxls.render-oracle-container-build.v3"
BOOTSTRAP_RECEIPT_SCHEMA = "rxls.render-oracle-bootstrap-receipt.v1"
SOURCE_DATE_EPOCH = 1_783_900_800
SOURCE_DATE_EPOCH_RFC3339 = "2026-07-13T00:00:00Z"
FONT_PACK_SCHEMA = "rxls.render-font-pack.v1"
SUPPORTED_EXTENSIONS = {".xls", ".xlsx", ".xlsm", ".xlsb", ".ods"}
PRINT_MODES = {"authored", "single-page-sheets"}
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
GIT_COMMIT_RE = re.compile(r"[0-9a-f]{40}\Z")
IMAGE_ID_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")
RUN_ID_RE = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?\Z")
BUILDER_NAME_RE = re.compile(r"[a-z0-9](?:[a-z0-9_.-]{0,62})\Z")
IMAGE_RE = re.compile(r"[^\s\x00-\x1f\x7f]{1,256}\Z")
ENTRYPOINT_ERROR_RE = re.compile(
    rb"oracle_error:([a-z][a-z0-9_]{0,63})\n?\Z"
)
REVIEWED_ENTRYPOINT_ERROR_CODES = frozenset(
    {
        "font_runtime_closure_empty",
        "font_runtime_closure_failed",
        "font_runtime_closure_mismatch",
    }
)
DOCKER_V2_MANIFEST_MEDIA_TYPE = (
    "application/vnd.docker.distribution.manifest.v2+json"
)
IMAGE_MANIFEST_MEDIA_TYPES = {DOCKER_V2_MANIFEST_MEDIA_TYPE}
MAX_LOCK_BYTES = 256 * 1024
MAX_WRAPPER_BYTES = 512 * 1024
MAX_FONT_PACK_BYTES = 128 * 1024 * 1024
MAX_FONT_PACK_FILES = 128
MAX_EVIDENCE_FILES = 16
MAX_ENGINE_DIAGNOSTIC_BYTES = 1024 * 1024
MAX_BUILD_DIAGNOSTIC_BYTES = 64 * 1024
MAX_BUILD_ARCHIVE_BYTES = 4 * 1024 * 1024 * 1024
MAX_BUILD_ARCHIVE_MEMBERS = 4096
MAX_BUILD_ARCHIVE_MANIFEST_BYTES = 1024 * 1024
MAX_IMAGE_CONFIG_BYTES = 16 * 1024 * 1024
MAX_BUILD_STDERR_BYTES = 16 * 1024 * 1024
MAX_GITHUB_API_BYTES = 512 * 1024
MAX_BOOTSTRAP_ARTIFACT_ZIP_BYTES = 1024 * 1024
MAX_GITHUB_ID = (1 << 63) - 1
GITHUB_REPOSITORY = "HyunjoJung/rxls"
GITHUB_REPOSITORY_ID = 1_297_467_060
GITHUB_WORKFLOW_PATH = ".github/workflows/render-hardening.yml"
GITHUB_WORKFLOW_NAME = "render-hardening"
GITHUB_BOOTSTRAP_EVENT = "pull_request"
GITHUB_BOOTSTRAP_JOB_NAME = "locked LibreOffice oracle image"
GITHUB_BOOTSTRAP_BUILD_STEP = "Build and verify the locked oracle image"
GITHUB_BOOTSTRAP_UPLOAD_STEP = "Upload oracle image identity evidence"
GITHUB_BOOTSTRAP_EVIDENCE_MEMBER = "render-oracle-image-build.json"
BUILDX_VERSION = "v0.35.0"
BUILDX_COMMIT = "a319e5b15052cf6557ceb666eb8ff6e32380b782"
BUILDX_SETUP_ACTION = (
    "docker/setup-buildx-action@bb05f3f5519dd87d3ba754cc423b652a5edd6d2c"
)
BUILDKIT_VERSION = "v0.31.2"
BUILDKIT_COMMIT = "e42e1bfd389af7203238cce77b1f7dad447285e9"
BUILDKIT_INDEX_SHA256 = (
    "2f5adac4ecd194d9f8c10b7b5d7bceb5186853db1b26e5abd3a657af0b7e26ec"
)
BUILDKIT_AMD64_MANIFEST_SHA256 = (
    "63db51c9b30208a7c2b1c40392c7ebb9ce2f85ba238a18a85420f8f5ea2d4684"
)
BUILDKIT_IMAGE = (
    "docker.io/moby/buildkit:v0.31.2@sha256:"
    + BUILDKIT_INDEX_SHA256
)
# The hosted runner's overlayfs/containerd implementation changes the image
# layer identity across runner generations.  BuildKit's native snapshotter
# keeps the Docker-schema2 archive independent of that host overlay stack.
BUILDKIT_SNAPSHOTTER = "native"
# Stock Buildx v0.35.0 leaves SolveOpt.CompatibilityVersion unset. The pinned
# BuildKit v0.31.2 daemon maps that zero value to its immutable default, 30.
BUILDKIT_DEFAULT_COMPATIBILITY_VERSION = 30
BUILDKIT_COMPATIBILITY_SOURCE = "pinned-buildkit-default"
REPRODUCIBILITY_BUILD_COUNT = 2
LIBREOFFICE_ARTIFACT_URLS = (
    "https://mirrors.ibiblio.org/pub/mirrors/libreoffice/stable/26.2.3/"
    "deb/x86_64/LibreOffice_26.2.3_Linux_x86-64_deb.tar.gz",
    "https://download.documentfoundation.org/libreoffice/stable/26.2.3/"
    "deb/x86_64/LibreOffice_26.2.3_Linux_x86-64_deb.tar.gz",
)
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


@dataclass(frozen=True)
class SourceIdentity:
    commit: str
    wrapper_sha256: str


@dataclass(frozen=True)
class BuildMetadataIdentity:
    config_digest: str
    manifest_digest: str
    descriptor_digest: str
    descriptor_media_type: str
    descriptor_size: int
    descriptor_annotations: tuple[tuple[str, str], ...]
    descriptor_platform: tuple[tuple[str, str], ...] | None


@dataclass(frozen=True)
class ImageIdentity:
    image_id: str
    platform: str
    created: str
    diff_ids: tuple[str, ...]
    labels: tuple[tuple[str, str], ...]
    manifest_digest: str | None = None
    descriptor_digest: str | None = None
    descriptor_media_type: str | None = None
    descriptor_size: int | None = None
    descriptor_annotations: tuple[tuple[str, str], ...] = ()
    descriptor_platform: tuple[tuple[str, str], ...] | None = None

    @property
    def diff_ids_sha256(self) -> str:
        return sha256_bytes(canonical_json_bytes(list(self.diff_ids)))

    def normalized_document(self) -> dict[str, Any]:
        descriptor_values = (
            self.descriptor_digest,
            self.descriptor_media_type,
            self.descriptor_size,
        )
        if all(value is None for value in descriptor_values):
            if self.descriptor_annotations or self.descriptor_platform is not None:
                raise OracleContainerError("image_descriptor_identity")
            descriptor: dict[str, Any] | None = None
        elif any(value is None for value in descriptor_values):
            raise OracleContainerError("image_descriptor_identity")
        else:
            descriptor = {
                "annotations": dict(self.descriptor_annotations),
                "digest": self.descriptor_digest,
                "mediaType": self.descriptor_media_type,
                "size": self.descriptor_size,
            }
            if self.descriptor_platform is not None:
                descriptor["platform"] = dict(self.descriptor_platform)
        return {
            "config_id": self.image_id,
            "created": self.created,
            "descriptor": descriptor,
            "labels": dict(self.labels),
            "manifest_digest": self.manifest_digest,
            "platform": self.platform,
            "rootfs_diff_ids": list(self.diff_ids),
        }

    @property
    def identity_sha256(self) -> str:
        return sha256_bytes(canonical_json_bytes(self.normalized_document()))

    def evidence_row(self) -> dict[str, Any]:
        document = self.normalized_document()
        if document["manifest_digest"] is None or document["descriptor"] is None:
            raise OracleContainerError("image_identity_incomplete")
        document["identity_sha256"] = self.identity_sha256
        document["rootfs_diff_ids_sha256"] = self.diff_ids_sha256
        return document


@dataclass(frozen=True)
class ReproducibleBuild:
    identities: tuple[ImageIdentity, ...]

    def __post_init__(self) -> None:
        if (
            len(self.identities) != REPRODUCIBILITY_BUILD_COUNT
            or any(
                identity != self.identities[0]
                for identity in self.identities[1:]
            )
        ):
            raise OracleContainerError("image_reproducibility_mismatch")
        for identity in self.identities:
            identity.evidence_row()

    @property
    def image_id(self) -> str:
        return self.identities[-1].image_id

    @property
    def manifest_digest(self) -> str:
        value = self.identities[-1].manifest_digest
        if value is None:
            raise OracleContainerError("build_manifest_digest")
        return value

    def evidence(self) -> dict[str, Any]:
        return {
            "build_count": len(self.identities),
            "buildkit_compatibility": {
                "explicit": False,
                "source": BUILDKIT_COMPATIBILITY_SOURCE,
                "version": BUILDKIT_DEFAULT_COMPATIBILITY_VERSION,
            },
            "buildkit_commit": BUILDKIT_COMMIT,
            "buildkit_image": BUILDKIT_IMAGE,
            "buildkit_version": BUILDKIT_VERSION,
            "buildx_commit": BUILDX_COMMIT,
            "buildx_version": BUILDX_VERSION,
            "config_ids": [identity.image_id for identity in self.identities],
            "descriptor_digests": [
                identity.descriptor_digest for identity in self.identities
            ],
            "descriptor_media_types": [
                identity.descriptor_media_type for identity in self.identities
            ],
            "descriptor_sizes": [
                identity.descriptor_size for identity in self.identities
            ],
            "driver": "docker-container",
            "export_archive_max_bytes": MAX_BUILD_ARCHIVE_BYTES,
            "export_destination": "stdout",
            "export_media_type": DOCKER_V2_MANIFEST_MEDIA_TYPE,
            "export_tar": True,
            "identities": [
                identity.evidence_row() for identity in self.identities
            ],
            "identity_sha256": [
                identity.identity_sha256 for identity in self.identities
            ],
            "manifest_digests": [
                identity.manifest_digest for identity in self.identities
            ],
            "no_cache": True,
            "provenance": False,
            "rewrite_timestamp": True,
            "rootfs_diff_ids_sha256": [
                identity.diff_ids_sha256 for identity in self.identities
            ],
            "sbom": False,
            "snapshotter": BUILDKIT_SNAPSHOTTER,
            "source_date_epoch": SOURCE_DATE_EPOCH,
            "status": "matched",
        }


class CommandRunner(Protocol):
    def run(
        self,
        command: Sequence[str],
        *,
        timeout_seconds: float,
        output_limit_bytes: int,
        stdout_path: Path | None = None,
        stdout_limit_bytes: int | None = None,
        stderr_limit_bytes: int | None = None,
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
        stdout_limit_bytes: int | None = None,
        stderr_limit_bytes: int | None = None,
    ) -> CommandResult:
        if not command:
            return CommandResult("not_found", None)
        output_file = None
        if stdout_path is not None:
            try:
                output_file = stdout_path.open("xb")
            except OSError:
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
            if output_file is not None:
                output_file.close()
            return CommandResult("not_found", None)

        output_limit_bytes = max(1, output_limit_bytes)
        stdout_limit_bytes = max(
            1,
            output_limit_bytes
            if stdout_limit_bytes is None
            else stdout_limit_bytes,
        )
        stderr_limit_bytes = max(
            1,
            output_limit_bytes
            if stderr_limit_bytes is None
            else stderr_limit_bytes,
        )
        stdout = bytearray()
        stderr = bytearray()
        total_read = 0
        stream_totals = {"stdout": 0, "stderr": 0}
        over_limit = threading.Event()
        lock = threading.Lock()

        def drain(
            stream: Any,
            destination: bytearray | None,
            stream_name: str,
            stream_limit: int,
        ) -> None:
            nonlocal total_read
            try:
                while True:
                    chunk = stream.read(64 * 1024)
                    if not chunk:
                        break
                    with lock:
                        remaining_total = max(
                            0, output_limit_bytes - total_read
                        )
                        remaining_stream = max(
                            0,
                            stream_limit - stream_totals[stream_name],
                        )
                        retained = chunk[
                            : min(remaining_total, remaining_stream)
                        ]
                        total_read += len(chunk)
                        stream_totals[stream_name] += len(chunk)
                        if destination is not None:
                            destination.extend(retained)
                        elif output_file is not None and retained:
                            output_file.write(retained)
                        if (
                            total_read > output_limit_bytes
                            or stream_totals[stream_name] > stream_limit
                        ):
                            over_limit.set()
            except OSError:
                over_limit.set()
            finally:
                stream.close()

        assert process.stdout is not None and process.stderr is not None
        threads = [
            threading.Thread(
                target=drain,
                args=(
                    process.stdout,
                    None if stdout_path else stdout,
                    "stdout",
                    stdout_limit_bytes,
                ),
                daemon=True,
            ),
            threading.Thread(
                target=drain,
                args=(
                    process.stderr,
                    stderr,
                    "stderr",
                    stderr_limit_bytes,
                ),
                daemon=True,
            ),
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


def reviewed_entrypoint_error(stderr: object) -> str | None:
    """Return only a fixed path/content-neutral entrypoint failure code."""
    if not isinstance(stderr, bytes) or len(stderr) > 256:
        return None
    match = ENTRYPOINT_ERROR_RE.fullmatch(stderr)
    if match is None:
        return None
    code = match.group(1).decode("ascii")
    return code if code in REVIEWED_ENTRYPOINT_ERROR_CODES else None


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


def build_failure_diagnostic(result: CommandResult) -> str:
    """Return a bounded, path-neutral Docker/Podman build failure tail."""
    header = (
        "render_oracle_build_diagnostic "
        f"status={result.status} returncode={result.returncode} "
        f"stdout_sha256={sha256_bytes(result.stdout)} "
        f"stderr_sha256={sha256_bytes(result.stderr)}"
    )
    replacements = {
        str(ROOT): "<repo>",
        str(ROOT.resolve(strict=False)): "<repo>",
        str(Path.home()): "<home>",
        tempfile.gettempdir(): "<tmp>",
    }
    sections = [header]
    for label, payload in (("stderr", result.stderr), ("stdout", result.stdout)):
        if not payload:
            continue
        tail = payload[-MAX_BUILD_DIAGNOSTIC_BYTES:]
        text = tail.decode("utf-8", errors="replace").replace("\r\n", "\n").replace("\r", "\n")
        text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)
        text = "".join(
            character
            if character in {"\n", "\t"} or (character.isprintable() and character != "\x7f")
            else "\ufffd"
            for character in text
        )
        for source, replacement in sorted(
            replacements.items(), key=lambda item: len(item[0]), reverse=True
        ):
            if source:
                text = text.replace(source, replacement)
        sections.extend((f"--- {label} tail ---", text.rstrip("\n")))
    return "\n".join(sections).rstrip() + "\n"


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
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
            raise OracleContainerError("lock_type")
        if not 0 < metadata.st_size <= MAX_LOCK_BYTES:
            raise OracleContainerError("lock_limit")
        payload = path.read_bytes()
        if len(payload) != metadata.st_size:
            raise OracleContainerError("lock_changed")
        document = json.loads(payload)
    except (OSError, json.JSONDecodeError) as error:
        raise OracleContainerError("lock_unreadable") from error
    validate_lock(document)
    verify_locked_files(document, path.parent)
    verify_wrapper_identity(document)
    return document, payload, build_contract_sha256(document)


def build_contract_sha256(document: dict[str, Any]) -> str:
    """Hash build inputs while excluding optional post-build image pins.

    The OCI image config contains this digest as a label. Excluding the
    post-build config/manifest pins and their hosted bootstrap receipt avoids
    recursion while preserving a stable, reviewable contract for every input
    that affects the build.
    """
    normalized = json.loads(json.dumps(document))
    normalized["built_image"]["expected_id"] = None
    normalized["built_image"]["expected_manifest_digest"] = None
    normalized["built_image"]["bootstrap_receipt"] = None
    return sha256_bytes(canonical_json_bytes(normalized))


def _is_github_id(value: object) -> bool:
    return (
        not isinstance(value, bool)
        and isinstance(value, int)
        and 0 < value <= MAX_GITHUB_ID
    )


def bootstrap_artifact_name(
    source_commit: str, run_id: int, run_attempt: int
) -> str:
    if (
        not isinstance(source_commit, str)
        or GIT_COMMIT_RE.fullmatch(source_commit) is None
    ):
        raise OracleContainerError("bootstrap_receipt_source")
    if not _is_github_id(run_id) or not _is_github_id(run_attempt):
        raise OracleContainerError("bootstrap_receipt_run")
    return (
        f"render-oracle-image-{source_commit}-{run_id}-{run_attempt}"
    )


def validate_bootstrap_receipt(
    receipt: object,
    *,
    source_commit: str | None = None,
    evidence_payload: bytes | None = None,
) -> dict[str, Any]:
    """Validate the path-neutral GitHub bootstrap provenance receipt."""
    if not isinstance(receipt, dict) or set(receipt) != {
        "artifact",
        "evidence",
        "job",
        "repository",
        "run",
        "schema",
    }:
        raise OracleContainerError("bootstrap_receipt_schema")
    if receipt.get("schema") != BOOTSTRAP_RECEIPT_SCHEMA:
        raise OracleContainerError("bootstrap_receipt_schema")

    repository = receipt.get("repository")
    if repository != {
        "full_name": GITHUB_REPOSITORY,
        "id": GITHUB_REPOSITORY_ID,
    }:
        raise OracleContainerError("bootstrap_receipt_repository")

    run = receipt.get("run")
    if not isinstance(run, dict) or set(run) != {
        "conclusion",
        "event",
        "head_sha",
        "id",
        "run_attempt",
        "workflow",
    }:
        raise OracleContainerError("bootstrap_receipt_run")
    run_id = run.get("id")
    run_attempt = run.get("run_attempt")
    head_sha = run.get("head_sha")
    if (
        not _is_github_id(run_id)
        or not _is_github_id(run_attempt)
        or not isinstance(head_sha, str)
        or GIT_COMMIT_RE.fullmatch(head_sha) is None
        or run.get("event") != GITHUB_BOOTSTRAP_EVENT
        or run.get("workflow") != GITHUB_WORKFLOW_PATH
        or run.get("conclusion") != "failure"
    ):
        raise OracleContainerError("bootstrap_receipt_run")
    if source_commit is not None and head_sha != source_commit:
        raise OracleContainerError("bootstrap_receipt_source")

    job = receipt.get("job")
    if not isinstance(job, dict) or set(job) != {
        "conclusion",
        "id",
        "name",
        "run_attempt",
        "run_id",
    }:
        raise OracleContainerError("bootstrap_receipt_job")
    if (
        not _is_github_id(job.get("id"))
        or job.get("run_id") != run_id
        or not _is_github_id(job.get("run_id"))
        or job.get("run_attempt") != run_attempt
        or not _is_github_id(job.get("run_attempt"))
        or job.get("name") != GITHUB_BOOTSTRAP_JOB_NAME
        or job.get("conclusion") != "failure"
    ):
        raise OracleContainerError("bootstrap_receipt_job")

    artifact = receipt.get("artifact")
    if not isinstance(artifact, dict) or set(artifact) != {
        "digest",
        "id",
        "name",
        "size_in_bytes",
    }:
        raise OracleContainerError("bootstrap_receipt_artifact")
    artifact_size = artifact.get("size_in_bytes")
    if (
        not _is_github_id(artifact.get("id"))
        or artifact.get("name")
        != bootstrap_artifact_name(head_sha, run_id, run_attempt)
        or not isinstance(artifact.get("digest"), str)
        or IMAGE_ID_RE.fullmatch(artifact["digest"]) is None
        or isinstance(artifact_size, bool)
        or not isinstance(artifact_size, int)
        or not 0 < artifact_size <= MAX_BOOTSTRAP_ARTIFACT_ZIP_BYTES
    ):
        raise OracleContainerError("bootstrap_receipt_artifact")

    evidence = receipt.get("evidence")
    if not isinstance(evidence, dict) or set(evidence) != {
        "bytes",
        "member",
        "sha256",
    }:
        raise OracleContainerError("bootstrap_receipt_evidence")
    evidence_size = evidence.get("bytes")
    evidence_sha256 = evidence.get("sha256")
    if (
        evidence.get("member") != GITHUB_BOOTSTRAP_EVIDENCE_MEMBER
        or isinstance(evidence_size, bool)
        or not isinstance(evidence_size, int)
        or not 0 < evidence_size <= MAX_LOCK_BYTES
        or not isinstance(evidence_sha256, str)
        or SHA256_RE.fullmatch(evidence_sha256) is None
    ):
        raise OracleContainerError("bootstrap_receipt_evidence")
    if evidence_payload is not None and (
        len(evidence_payload) != evidence_size
        or sha256_bytes(evidence_payload) != evidence_sha256
    ):
        raise OracleContainerError("bootstrap_receipt_evidence")
    return receipt


def validate_lock(document: object) -> dict[str, Any]:
    if not isinstance(document, dict) or document.get("schema") != LOCK_SCHEMA:
        raise OracleContainerError("lock_schema")
    if set(document) != {
        "base_image",
        "builder",
        "built_image",
        "debian_snapshot",
        "files",
        "libreoffice",
        "runtime_defaults",
        "schema",
        "wrapper",
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

    builder = document.get("builder")
    if builder != {
        "buildkit": {
            "commit": BUILDKIT_COMMIT,
            "compatibility": {
                "explicit": False,
                "source": BUILDKIT_COMPATIBILITY_SOURCE,
                "version": BUILDKIT_DEFAULT_COMPATIBILITY_VERSION,
            },
            "image": BUILDKIT_IMAGE,
            "index_sha256": BUILDKIT_INDEX_SHA256,
            "linux_amd64_manifest_sha256": BUILDKIT_AMD64_MANIFEST_SHA256,
            "version": BUILDKIT_VERSION,
        },
        "buildx": {
            "commit": BUILDX_COMMIT,
            "setup_action": BUILDX_SETUP_ACTION,
            "version": BUILDX_VERSION,
        },
        "driver": "docker-container",
        "driver_options": {
            "provenance_add_gha": False,
        },
        "exporter": {
            "archive_max_bytes": MAX_BUILD_ARCHIVE_BYTES,
            "destination": "stdout",
            "media_type": DOCKER_V2_MANIFEST_MEDIA_TYPE,
            "oci_mediatypes": False,
            "provenance": False,
            "rewrite_timestamp": True,
            "sbom": False,
            "tar": True,
            "type": "docker",
        },
        "platform": "linux/amd64",
        "reproducibility_builds": REPRODUCIBILITY_BUILD_COUNT,
        "snapshotter": BUILDKIT_SNAPSHOTTER,
    }:
        raise OracleContainerError("lock_builder")

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
    if not isinstance(artifact, dict) or set(artifact) != {
        "bytes",
        "fallback_url",
        "sha256",
        "url",
    }:
        raise OracleContainerError("lock_artifact")
    if artifact.get("sha256") != LIBREOFFICE_ARTIFACT_SHA256:
        raise OracleContainerError("lock_artifact_sha256")
    if artifact.get("bytes") != 216_816_909:
        raise OracleContainerError("lock_artifact_bytes")
    if (
        artifact.get("url"),
        artifact.get("fallback_url"),
    ) != LIBREOFFICE_ARTIFACT_URLS:
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
        "bootstrap_receipt",
        "expected_id",
        "expected_manifest_digest",
        "identity_kind",
        "source_date_epoch",
        "unpinned_verification",
    }:
        raise OracleContainerError("lock_built_image")
    if built_image.get("identity_kind") != (
        "docker_schema2_manifest_digest_plus_oci_image_config_digest"
    ):
        raise OracleContainerError("lock_built_image_kind")
    if built_image.get("source_date_epoch") != SOURCE_DATE_EPOCH:
        raise OracleContainerError("lock_built_image_epoch")
    if built_image.get("unpinned_verification") != (
        "bootstrap_only_two_isolated_no_cache_builds_plus_exact_config_"
        "manifest_descriptor_rootfs_contract_and_labels"
    ):
        raise OracleContainerError("lock_built_image_verification")
    expected_id = built_image.get("expected_id")
    if expected_id is not None and (
        not isinstance(expected_id, str) or not IMAGE_ID_RE.fullmatch(expected_id)
    ):
        raise OracleContainerError("lock_built_image_id")
    expected_manifest_digest = built_image.get("expected_manifest_digest")
    if expected_manifest_digest is not None and (
        not isinstance(expected_manifest_digest, str)
        or IMAGE_ID_RE.fullmatch(expected_manifest_digest) is None
    ):
        raise OracleContainerError("lock_built_image_manifest_digest")
    if (expected_id is None) != (expected_manifest_digest is None):
        raise OracleContainerError("lock_built_image_pin_pair")
    bootstrap_receipt = built_image.get("bootstrap_receipt")
    if bootstrap_receipt is not None:
        validate_bootstrap_receipt(bootstrap_receipt)
    if (expected_id is None) != (bootstrap_receipt is None):
        raise OracleContainerError("lock_bootstrap_receipt_pair")
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
    wrapper = document.get("wrapper")
    if not isinstance(wrapper, dict) or set(wrapper) != {
        "bytes",
        "path",
        "sha256",
    }:
        raise OracleContainerError("lock_wrapper")
    if wrapper.get("path") != WRAPPER_RELATIVE_PATH:
        raise OracleContainerError("lock_wrapper_path")
    wrapper_bytes = wrapper.get("bytes")
    wrapper_sha256 = wrapper.get("sha256")
    if (
        isinstance(wrapper_bytes, bool)
        or not isinstance(wrapper_bytes, int)
        or not 0 < wrapper_bytes <= MAX_WRAPPER_BYTES
    ):
        raise OracleContainerError("lock_wrapper_bytes")
    if (
        not isinstance(wrapper_sha256, str)
        or SHA256_RE.fullmatch(wrapper_sha256) is None
    ):
        raise OracleContainerError("lock_wrapper_sha256")
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


def verify_wrapper_identity(
    document: dict[str, Any], path: Path = WRAPPER_PATH
) -> str:
    row = document["wrapper"]
    try:
        metadata = path.lstat()
    except OSError as error:
        raise OracleContainerError("wrapper_missing") from error
    if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
        raise OracleContainerError("wrapper_type")
    if metadata.st_size != row["bytes"]:
        raise OracleContainerError("wrapper_size")
    digest = sha256_file(path, MAX_WRAPPER_BYTES)
    if digest != row["sha256"]:
        raise OracleContainerError("wrapper_hash")
    return digest


def require_canonical_build_lock(path: Path) -> None:
    try:
        resolved = path.resolve(strict=True)
        canonical = DEFAULT_LOCK.resolve(strict=True)
    except OSError as error:
        raise OracleContainerError("canonical_build_lock") from error
    if resolved != canonical:
        raise OracleContainerError("canonical_build_lock")


def _run_git(
    args: Sequence[str],
    *,
    root: Path = ROOT,
    limit: int = 1024 * 1024,
) -> bytes:
    result = BoundedProcessRunner().run(
        ["git", "-C", str(root), *args],
        timeout_seconds=15.0,
        output_limit_bytes=limit,
    )
    if result.status != "ok":
        raise OracleContainerError("source_git_state")
    return result.stdout


def require_clean_source(
    document: dict[str, Any],
    *,
    root: Path = ROOT,
    wrapper_path: Path = WRAPPER_PATH,
    lock_path: Path = DEFAULT_LOCK,
) -> SourceIdentity:
    raw_commit = _run_git(
        ["rev-parse", "--verify", "HEAD^{commit}"],
        root=root,
        limit=1024,
    ).decode("ascii", errors="strict").strip()
    if GIT_COMMIT_RE.fullmatch(raw_commit) is None:
        raise OracleContainerError("source_commit")

    status = _run_git(
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        root=root,
    )
    if status:
        raise OracleContainerError("source_tree_dirty")
    locked_paths = [
        (
            f"scripts/render-oracle-container/{row['path']}",
            lock_path.parent / row["path"],
            1024 * 1024,
        )
        for row in document["files"]
    ]
    tracked_paths = [
        (WRAPPER_RELATIVE_PATH, wrapper_path, MAX_WRAPPER_BYTES),
        (LOCK_RELATIVE_PATH, lock_path, MAX_LOCK_BYTES),
        *locked_paths,
    ]
    _run_git(
        [
            "ls-files",
            "--error-unmatch",
            "--",
            *(relative for relative, _, _ in tracked_paths),
        ],
        root=root,
        limit=16 * 1024,
    )
    for relative, live_path, maximum in tracked_paths:
        head_payload = _run_git(
            ["cat-file", "blob", f"HEAD:{relative}"],
            root=root,
            limit=maximum,
        )
        try:
            live_payload = live_path.read_bytes()
        except OSError as error:
            raise OracleContainerError("source_tracked_file") from error
        if head_payload != live_payload:
            raise OracleContainerError("source_tracked_file")

    wrapper_sha256 = verify_wrapper_identity(document, wrapper_path)
    return SourceIdentity(
        commit=raw_commit,
        wrapper_sha256=wrapper_sha256,
    )


def read_bootstrap_evidence_payload(evidence_path: Path) -> bytes:
    try:
        metadata = evidence_path.lstat()
        if not stat.S_ISREG(metadata.st_mode) or evidence_path.is_symlink():
            raise OracleContainerError("bootstrap_build_type")
        if not 0 < metadata.st_size <= MAX_LOCK_BYTES:
            raise OracleContainerError("bootstrap_build_limit")
        payload = evidence_path.read_bytes()
        if len(payload) != metadata.st_size:
            raise OracleContainerError("bootstrap_build_changed")
        return payload
    except OracleContainerError:
        raise
    except OSError as error:
        raise OracleContainerError("bootstrap_build_unreadable") from error


def github_api_command(endpoint: str) -> list[str]:
    if (
        not isinstance(endpoint, str)
        or not endpoint.startswith(f"/repos/{GITHUB_REPOSITORY}/")
        or any(character in endpoint for character in ("\0", "\r", "\n"))
    ):
        raise OracleContainerError("bootstrap_receipt_endpoint")
    return [
        "gh",
        "api",
        "--hostname",
        "github.com",
        "--method",
        "GET",
        "--header",
        "Accept: application/vnd.github+json",
        "--header",
        "X-GitHub-Api-Version: 2022-11-28",
        endpoint,
    ]


def github_api_json(
    runner: CommandRunner, endpoint: str, error_code: str
) -> object:
    result = runner.run(
        github_api_command(endpoint),
        timeout_seconds=30.0,
        output_limit_bytes=MAX_GITHUB_API_BYTES,
    )
    if result.status != "ok":
        raise OracleContainerError(error_code)
    try:
        return json.loads(result.stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise OracleContainerError(error_code) from error


def _github_repository_matches(value: object) -> bool:
    return (
        isinstance(value, dict)
        and value.get("id") == GITHUB_REPOSITORY_ID
        and value.get("full_name") == GITHUB_REPOSITORY
    )


def validate_hosted_run(
    document: object,
    *,
    run_id: int,
    run_attempt: int,
    source_commit: str,
) -> None:
    if not isinstance(document, dict):
        raise OracleContainerError("bootstrap_receipt_run")
    if (
        document.get("id") != run_id
        or not _is_github_id(document.get("id"))
        or document.get("run_attempt") != run_attempt
        or not _is_github_id(document.get("run_attempt"))
        or document.get("event") != GITHUB_BOOTSTRAP_EVENT
        or document.get("head_sha") != source_commit
        or document.get("path") != GITHUB_WORKFLOW_PATH
        or document.get("status") != "completed"
        or document.get("conclusion") != "failure"
        or not _github_repository_matches(document.get("repository"))
        or not _github_repository_matches(document.get("head_repository"))
    ):
        raise OracleContainerError("bootstrap_receipt_run")


def validate_hosted_job(
    document: object,
    *,
    run_id: int,
    run_attempt: int,
    job_id: int,
    source_commit: str,
) -> None:
    if not isinstance(document, dict):
        raise OracleContainerError("bootstrap_receipt_job")
    if (
        document.get("id") != job_id
        or not _is_github_id(document.get("id"))
        or document.get("run_id") != run_id
        or not _is_github_id(document.get("run_id"))
        or document.get("run_attempt") != run_attempt
        or not _is_github_id(document.get("run_attempt"))
        or document.get("workflow_name") != GITHUB_WORKFLOW_NAME
        or document.get("head_sha") != source_commit
        or document.get("name") != GITHUB_BOOTSTRAP_JOB_NAME
        or document.get("status") != "completed"
        or document.get("conclusion") != "failure"
    ):
        raise OracleContainerError("bootstrap_receipt_job")
    steps = document.get("steps")
    if not isinstance(steps, list):
        raise OracleContainerError("bootstrap_receipt_job")
    required_steps = {
        GITHUB_BOOTSTRAP_BUILD_STEP: "failure",
        GITHUB_BOOTSTRAP_UPLOAD_STEP: "success",
    }
    for name, conclusion in required_steps.items():
        matching = [
            step
            for step in steps
            if isinstance(step, dict) and step.get("name") == name
        ]
        if (
            len(matching) != 1
            or matching[0].get("status") != "completed"
            or matching[0].get("conclusion") != conclusion
        ):
            raise OracleContainerError("bootstrap_receipt_job")


def validate_hosted_artifact(
    document: object,
    *,
    run_id: int,
    run_attempt: int,
    artifact_id: int,
    source_commit: str,
) -> tuple[str, int, str]:
    if not isinstance(document, dict):
        raise OracleContainerError("bootstrap_receipt_artifact")
    expected_name = bootstrap_artifact_name(
        source_commit, run_id, run_attempt
    )
    digest = document.get("digest")
    size = document.get("size_in_bytes")
    workflow_run = document.get("workflow_run")
    if (
        document.get("id") != artifact_id
        or not _is_github_id(document.get("id"))
        or document.get("name") != expected_name
        or not isinstance(digest, str)
        or IMAGE_ID_RE.fullmatch(digest) is None
        or isinstance(size, bool)
        or not isinstance(size, int)
        or not 0 < size <= MAX_BOOTSTRAP_ARTIFACT_ZIP_BYTES
        or document.get("expired") is not False
        or not isinstance(workflow_run, dict)
        or workflow_run.get("id") != run_id
        or not _is_github_id(workflow_run.get("id"))
        or workflow_run.get("repository_id") != GITHUB_REPOSITORY_ID
        or workflow_run.get("head_repository_id")
        != GITHUB_REPOSITORY_ID
        or workflow_run.get("head_sha") != source_commit
    ):
        raise OracleContainerError("bootstrap_receipt_artifact")
    return digest, size, expected_name


def read_bootstrap_artifact_member(archive: Path) -> bytes:
    try:
        with zipfile.ZipFile(archive, mode="r") as bundle:
            members = bundle.infolist()
            if len(members) != 1:
                raise OracleContainerError("bootstrap_receipt_zip")
            member = members[0]
            mode = (member.external_attr >> 16) & 0xFFFF
            file_type = stat.S_IFMT(mode)
            if (
                member.filename != GITHUB_BOOTSTRAP_EVIDENCE_MEMBER
                or member.is_dir()
                or member.flag_bits & 0x1
                or file_type not in (0, stat.S_IFREG)
                or member.compress_type
                not in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED)
                or not 0 < member.file_size <= MAX_LOCK_BYTES
                or not 0 <= member.compress_size
                <= MAX_BOOTSTRAP_ARTIFACT_ZIP_BYTES
            ):
                raise OracleContainerError("bootstrap_receipt_zip")
            with bundle.open(member, mode="r") as stream:
                payload = stream.read(MAX_LOCK_BYTES + 1)
            if len(payload) != member.file_size:
                raise OracleContainerError("bootstrap_receipt_zip")
            return payload
    except OracleContainerError:
        raise
    except (OSError, RuntimeError, zipfile.BadZipFile) as error:
        raise OracleContainerError("bootstrap_receipt_zip") from error


def fetch_hosted_bootstrap_receipt(
    evidence_path: Path,
    source_identity: SourceIdentity,
    *,
    run_id: int,
    run_attempt: int,
    job_id: int,
    artifact_id: int,
    runner: CommandRunner | None = None,
) -> dict[str, Any]:
    """Authenticate exact live GitHub run/job/artifact evidence."""
    if (
        not isinstance(source_identity, SourceIdentity)
        or GIT_COMMIT_RE.fullmatch(source_identity.commit) is None
        or SHA256_RE.fullmatch(source_identity.wrapper_sha256) is None
        or not all(
            _is_github_id(value)
            for value in (run_id, run_attempt, job_id, artifact_id)
        )
    ):
        raise OracleContainerError("bootstrap_receipt_identity")
    runner = runner or BoundedProcessRunner()
    local_evidence = read_bootstrap_evidence_payload(evidence_path)

    run_endpoint = (
        f"/repos/{GITHUB_REPOSITORY}/actions/runs/{run_id}"
    )
    run_document = github_api_json(
        runner, run_endpoint, "bootstrap_receipt_run"
    )
    validate_hosted_run(
        run_document,
        run_id=run_id,
        run_attempt=run_attempt,
        source_commit=source_identity.commit,
    )

    job_endpoint = (
        f"/repos/{GITHUB_REPOSITORY}/actions/jobs/{job_id}"
    )
    job_document = github_api_json(
        runner, job_endpoint, "bootstrap_receipt_job"
    )
    validate_hosted_job(
        job_document,
        run_id=run_id,
        run_attempt=run_attempt,
        job_id=job_id,
        source_commit=source_identity.commit,
    )

    artifact_endpoint = (
        f"/repos/{GITHUB_REPOSITORY}/actions/artifacts/{artifact_id}"
    )
    artifact_document = github_api_json(
        runner, artifact_endpoint, "bootstrap_receipt_artifact"
    )
    artifact_digest, artifact_size, artifact_name = (
        validate_hosted_artifact(
            artifact_document,
            run_id=run_id,
            run_attempt=run_attempt,
            artifact_id=artifact_id,
            source_commit=source_identity.commit,
        )
    )

    with tempfile.TemporaryDirectory(
        prefix="rxls-bootstrap-artifact-"
    ) as raw:
        archive = Path(raw) / "artifact.zip"
        download_endpoint = f"{artifact_endpoint}/zip"
        downloaded = runner.run(
            github_api_command(download_endpoint),
            timeout_seconds=60.0,
            output_limit_bytes=(
                MAX_BOOTSTRAP_ARTIFACT_ZIP_BYTES
                + MAX_BUILD_DIAGNOSTIC_BYTES
            ),
            stdout_path=archive,
            stdout_limit_bytes=MAX_BOOTSTRAP_ARTIFACT_ZIP_BYTES,
            stderr_limit_bytes=MAX_BUILD_DIAGNOSTIC_BYTES,
        )
        if downloaded.status != "ok":
            raise OracleContainerError("bootstrap_receipt_download")
        try:
            metadata = archive.lstat()
        except OSError as error:
            raise OracleContainerError(
                "bootstrap_receipt_download"
            ) from error
        if (
            not stat.S_ISREG(metadata.st_mode)
            or archive.is_symlink()
            or metadata.st_size != artifact_size
            or not 0 < metadata.st_size
            <= MAX_BOOTSTRAP_ARTIFACT_ZIP_BYTES
            or sha256_file(
                archive, MAX_BOOTSTRAP_ARTIFACT_ZIP_BYTES
            )
            != artifact_digest.removeprefix("sha256:")
        ):
            raise OracleContainerError("bootstrap_receipt_download")
        hosted_evidence = read_bootstrap_artifact_member(archive)
    if hosted_evidence != local_evidence:
        raise OracleContainerError("bootstrap_receipt_evidence")

    receipt = {
        "artifact": {
            "digest": artifact_digest,
            "id": artifact_id,
            "name": artifact_name,
            "size_in_bytes": artifact_size,
        },
        "evidence": {
            "bytes": len(local_evidence),
            "member": GITHUB_BOOTSTRAP_EVIDENCE_MEMBER,
            "sha256": sha256_bytes(local_evidence),
        },
        "job": {
            "conclusion": "failure",
            "id": job_id,
            "name": GITHUB_BOOTSTRAP_JOB_NAME,
            "run_attempt": run_attempt,
            "run_id": run_id,
        },
        "repository": {
            "full_name": GITHUB_REPOSITORY,
            "id": GITHUB_REPOSITORY_ID,
        },
        "run": {
            "conclusion": "failure",
            "event": GITHUB_BOOTSTRAP_EVENT,
            "head_sha": source_identity.commit,
            "id": run_id,
            "run_attempt": run_attempt,
            "workflow": GITHUB_WORKFLOW_PATH,
        },
        "schema": BOOTSTRAP_RECEIPT_SCHEMA,
    }
    validate_bootstrap_receipt(
        receipt,
        source_commit=source_identity.commit,
        evidence_payload=local_evidence,
    )
    return receipt


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


def validate_builder_name(value: str) -> str:
    if not isinstance(value, str) or BUILDER_NAME_RE.fullmatch(value) is None:
        raise OracleContainerError("invalid_builder_name")
    return value


def require_docker_build_engine(engine: str) -> str:
    if engine != "docker":
        raise OracleContainerError("build_engine_docker_required")
    return engine


def buildx_create_command(engine: str, builder_name: str) -> list[str]:
    require_docker_build_engine(engine)
    builder_name = validate_builder_name(builder_name)
    return [
        engine,
        "buildx",
        "create",
        "--name",
        builder_name,
        "--driver",
        "docker-container",
        "--driver-opt",
        f"image={BUILDKIT_IMAGE}",
        "--driver-opt",
        "provenance-add-gha=false",
        "--buildkitd-flags",
        f"--oci-worker-snapshotter={BUILDKIT_SNAPSHOTTER}",
        "--platform",
        "linux/amd64",
        "--bootstrap",
    ]


def buildx_inspect_command(engine: str, builder_name: str) -> list[str]:
    require_docker_build_engine(engine)
    return [
        engine,
        "buildx",
        "inspect",
        "--builder",
        validate_builder_name(builder_name),
        "--bootstrap",
    ]


def buildx_remove_command(engine: str, builder_name: str) -> list[str]:
    require_docker_build_engine(engine)
    return [
        engine,
        "buildx",
        "rm",
        "--force",
        validate_builder_name(builder_name),
    ]


def build_build_command(
    engine: str,
    image: str,
    lock_sha256: str,
    *,
    builder_name: str = "rxls-oracle-repro",
    metadata_file: Path | None = None,
) -> list[str]:
    require_docker_build_engine(engine)
    validate_image_reference(image)
    builder_name = validate_builder_name(builder_name)
    if not SHA256_RE.fullmatch(lock_sha256):
        raise OracleContainerError("invalid_lock_sha256")
    if metadata_file is None:
        metadata_file = Path("render-oracle-build-metadata.json")
    metadata_value = str(metadata_file)
    if not metadata_value or "\0" in metadata_value:
        raise OracleContainerError("invalid_build_metadata_path")
    return [
        engine,
        "buildx",
        "build",
        "--builder",
        builder_name,
        "--platform",
        "linux/amd64",
        "--pull=false",
        "--no-cache",
        "--provenance=false",
        "--sbom=false",
        "--progress",
        "plain",
        "--build-arg",
        f"ORACLE_LOCK_SHA256={lock_sha256}",
        "--build-arg",
        f"SOURCE_DATE_EPOCH={SOURCE_DATE_EPOCH}",
        "--output",
        (
            "type=docker,dest=-,tar=true,rewrite-timestamp=true,"
            "oci-mediatypes=false"
        ),
        "--metadata-file",
        metadata_value,
        "--tag",
        image,
        "--file",
        str(CONTAINERFILE),
        str(CONTAINER_DIR),
    ]


def image_load_command(engine: str, archive: Path) -> list[str]:
    require_docker_build_engine(engine)
    value = str(archive)
    if not value or "\0" in value:
        raise OracleContainerError("invalid_build_archive_path")
    return [engine, "image", "load", "--input", value]


def validate_build_archive(path: Path) -> None:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise OracleContainerError("build_archive_missing") from error
    if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
        raise OracleContainerError("build_archive_type")
    if not 0 < metadata.st_size <= MAX_BUILD_ARCHIVE_BYTES:
        raise OracleContainerError("build_archive_limit")


def verify_docker_archive_config(
    path: Path, expected_config_digest: str
) -> None:
    if IMAGE_ID_RE.fullmatch(expected_config_digest) is None:
        raise OracleContainerError("build_archive_config_digest")
    try:
        with tarfile.open(path, mode="r:") as archive:
            members: dict[str, tarfile.TarInfo] = {}
            for member in archive:
                if len(members) >= MAX_BUILD_ARCHIVE_MEMBERS:
                    raise OracleContainerError("build_archive_members")
                if member.name in members:
                    raise OracleContainerError("build_archive_duplicate")
                members[member.name] = member

            manifest_member = members.get("manifest.json")
            if (
                manifest_member is None
                or not manifest_member.isreg()
                or not 0
                < manifest_member.size
                <= MAX_BUILD_ARCHIVE_MANIFEST_BYTES
            ):
                raise OracleContainerError("build_archive_manifest")
            manifest_stream = archive.extractfile(manifest_member)
            if manifest_stream is None:
                raise OracleContainerError("build_archive_manifest")
            manifest_payload = manifest_stream.read(
                MAX_BUILD_ARCHIVE_MANIFEST_BYTES + 1
            )
            if len(manifest_payload) != manifest_member.size:
                raise OracleContainerError("build_archive_manifest")
            manifest = json.loads(manifest_payload)
            if not isinstance(manifest, list) or len(manifest) != 1:
                raise OracleContainerError("build_archive_manifest")
            row = manifest[0]
            if not isinstance(row, dict):
                raise OracleContainerError("build_archive_manifest")
            config_name = safe_relative(row.get("Config"))
            config_member = members.get(config_name)
            if (
                config_member is None
                or not config_member.isreg()
                or not 0 < config_member.size <= MAX_IMAGE_CONFIG_BYTES
            ):
                raise OracleContainerError("build_archive_config")
            config_stream = archive.extractfile(config_member)
            if config_stream is None:
                raise OracleContainerError("build_archive_config")
            config_payload = config_stream.read(MAX_IMAGE_CONFIG_BYTES + 1)
            if len(config_payload) != config_member.size:
                raise OracleContainerError("build_archive_config")
    except (
        OSError,
        tarfile.TarError,
        UnicodeDecodeError,
        json.JSONDecodeError,
    ) as error:
        raise OracleContainerError("build_archive_unreadable") from error
    if f"sha256:{sha256_bytes(config_payload)}" != expected_config_digest:
        raise OracleContainerError("build_archive_config_digest")


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


def normalize_image_created(value: object) -> str:
    if not isinstance(value, str):
        raise OracleContainerError("image_created")
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as error:
        raise OracleContainerError("image_created") from error
    if parsed.tzinfo is None:
        raise OracleContainerError("image_created")
    if parsed.astimezone(timezone.utc).timestamp() != SOURCE_DATE_EPOCH:
        raise OracleContainerError("image_created")
    return SOURCE_DATE_EPOCH_RFC3339


def normalize_diff_id(value: object) -> str:
    if isinstance(value, str) and SHA256_RE.fullmatch(value):
        return f"sha256:{value}"
    if not isinstance(value, str) or IMAGE_ID_RE.fullmatch(value) is None:
        raise OracleContainerError("image_rootfs")
    return value


def normalize_descriptor_annotations(
    value: object,
) -> tuple[tuple[str, str], ...]:
    if not isinstance(value, dict) or any(
        not isinstance(key, str) or not isinstance(annotation, str)
        for key, annotation in value.items()
    ):
        raise OracleContainerError("build_metadata_descriptor_annotations")
    if set(value) != {"org.opencontainers.image.created"}:
        raise OracleContainerError("build_metadata_descriptor_annotations")
    created = value.get("org.opencontainers.image.created")
    try:
        normalized_created = normalize_image_created(created)
    except OracleContainerError as error:
        raise OracleContainerError(
            "build_metadata_descriptor_created"
        ) from error
    normalized = dict(value)
    normalized["org.opencontainers.image.created"] = normalized_created
    return tuple(sorted(normalized.items()))


def normalize_descriptor_platform(
    value: object,
) -> tuple[tuple[str, str], ...] | None:
    if value is None:
        return None
    if value != {"architecture": "amd64", "os": "linux"}:
        raise OracleContainerError("build_metadata_descriptor_platform")
    return (("architecture", "amd64"), ("os", "linux"))


def identity_diagnostic_row(identity: ImageIdentity) -> dict[str, Any]:
    return {
        "created": identity.created,
        "descriptor_digest": identity.descriptor_digest,
        "descriptor_media_type": identity.descriptor_media_type,
        "descriptor_size": identity.descriptor_size,
        "image_id": identity.image_id,
        "identity_sha256": identity.identity_sha256,
        "manifest_digest": identity.manifest_digest,
        "platform": identity.platform,
        "rootfs_diff_ids": len(identity.diff_ids),
        "rootfs_diff_ids_sha256": identity.diff_ids_sha256,
    }


def emit_image_identity_mismatch(
    reason: str,
    *,
    expected_image_id: str | None = None,
    expected_manifest_digest: str | None = None,
    identities: Sequence[ImageIdentity] = (),
) -> None:
    payload = {
        "expected_image_id": expected_image_id,
        "expected_manifest_digest": expected_manifest_digest,
        "observed": [identity_diagnostic_row(identity) for identity in identities],
        "reason": reason,
    }
    rendered = json.dumps(payload, sort_keys=True, separators=(",", ":"))
    if len(rendered.encode("utf-8")) > MAX_BUILD_DIAGNOSTIC_BYTES:
        rendered = json.dumps(
            {
                "expected_image_id": expected_image_id,
                "expected_manifest_digest": expected_manifest_digest,
                "observed_identity_sha256": [
                    identity.identity_sha256 for identity in identities
                ],
                "reason": reason,
            },
            sort_keys=True,
            separators=(",", ":"),
        )
    print(f"render_oracle_image_identity_diagnostic {rendered}", file=sys.stderr)


def inspect_image_identity(
    runner: CommandRunner,
    engine: str,
    image: str,
    lock_sha256: str,
    expected_image_id: str | None = None,
    expected_manifest_digest: str | None = None,
) -> ImageIdentity:
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
        loaded_store_id = row["Id"]
        architecture = row["Architecture"]
        operating_system = row["Os"]
        created = row["Created"]
        labels = row["Config"]["Labels"]
        rootfs_type = row["RootFS"]["Type"]
        rootfs_layers = row["RootFS"]["Layers"]
    except (json.JSONDecodeError, KeyError, IndexError, TypeError) as error:
        raise OracleContainerError("image_inspect_schema") from error
    if isinstance(loaded_store_id, str) and re.fullmatch(
        r"[0-9a-f]{64}", loaded_store_id
    ):
        loaded_store_id = f"sha256:{loaded_store_id}"
    if (
        not isinstance(loaded_store_id, str)
        or not IMAGE_ID_RE.fullmatch(loaded_store_id)
    ):
        raise OracleContainerError("image_id")
    if architecture not in {"amd64", "x86_64"}:
        raise OracleContainerError("image_architecture")
    if operating_system != "linux":
        raise OracleContainerError("image_operating_system")
    if rootfs_type != "layers" or not isinstance(rootfs_layers, list) or not rootfs_layers:
        raise OracleContainerError("image_rootfs")
    diff_ids = tuple(normalize_diff_id(value) for value in rootfs_layers)
    if not isinstance(labels, dict) or any(
        not isinstance(key, str) or not isinstance(value, str)
        for key, value in labels.items()
    ):
        raise OracleContainerError("image_labels")
    expected = {**EXPECTED_IMAGE_LABELS, "org.rxls.render-oracle.lock-sha256": lock_sha256}
    for key, value in expected.items():
        if labels.get(key) != value:
            raise OracleContainerError("image_label_mismatch")
    observed_identity = ImageIdentity(
        image_id=loaded_store_id,
        platform="linux/amd64",
        created=normalize_image_created(created),
        diff_ids=diff_ids,
        labels=tuple(sorted(labels.items())),
    )
    if expected_manifest_digest is not None and (
        not isinstance(expected_manifest_digest, str)
        or IMAGE_ID_RE.fullmatch(expected_manifest_digest) is None
        or expected_image_id is None
    ):
        raise OracleContainerError("image_manifest_digest")
    if expected_image_id is not None and loaded_store_id not in {
        expected_image_id,
        expected_manifest_digest,
    }:
        emit_image_identity_mismatch(
            "expected_loaded_store_id_mismatch",
            expected_image_id=expected_image_id,
            expected_manifest_digest=expected_manifest_digest,
            identities=(observed_identity,),
        )
        raise OracleContainerError("image_id_mismatch")
    if expected_image_id is None:
        return observed_identity
    return ImageIdentity(
        image_id=expected_image_id,
        platform=observed_identity.platform,
        created=observed_identity.created,
        diff_ids=observed_identity.diff_ids,
        labels=observed_identity.labels,
    )


def inspect_image(
    runner: CommandRunner,
    engine: str,
    image: str,
    lock_sha256: str,
    expected_image_id: str | None = None,
    expected_manifest_digest: str | None = None,
) -> str:
    return inspect_image_identity(
        runner,
        engine,
        image,
        lock_sha256,
        expected_image_id,
        expected_manifest_digest,
    ).image_id


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
    expected_manifest_digest: str | None = None,
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
        "expected_manifest_digest": expected_manifest_digest,
        "schema": PLAN_SCHEMA,
    }


def execute_render(
    config: RenderConfig,
    engine: str,
    image: str,
    lock_sha256: str,
    expected_image_id: str | None = None,
    expected_manifest_digest: str | None = None,
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
        runner,
        engine,
        image,
        lock_sha256,
        expected_image_id,
        expected_manifest_digest,
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
                    entrypoint_error = (
                        reviewed_entrypoint_error(started.stderr)
                        if started.status == "nonzero"
                        else None
                    )
                    if entrypoint_error is not None:
                        raise OracleContainerError(entrypoint_error)
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
                expected_manifest_digest=expected_manifest_digest,
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
    expected_manifest_digest: str | None,
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
            "expected_manifest_digest": expected_manifest_digest,
            "id": image_id,
            "identity_status": (
                "pinned_match" if expected_image_id is not None else "runtime_verified"
            ),
            "lock_sha256": lock_sha256,
            "manifest_digest": expected_manifest_digest,
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


def verify_buildx_client(runner: CommandRunner, engine: str) -> None:
    require_docker_build_engine(engine)
    result = runner.run(
        [engine, "buildx", "version"],
        timeout_seconds=30.0,
        output_limit_bytes=1024 * 1024,
    )
    if result.status != "ok":
        print(build_failure_diagnostic(result), file=sys.stderr, end="")
        raise OracleContainerError(f"buildx_client_{result.status}")
    output = (result.stdout + b"\n" + result.stderr).decode(
        "utf-8", errors="replace"
    )
    if (
        re.search(rf"(?<![0-9A-Za-z]){re.escape(BUILDX_VERSION)}(?![0-9A-Za-z])", output)
        is None
        or BUILDX_COMMIT not in output
    ):
        raise OracleContainerError("buildx_client_identity")


def verify_buildx_builder_description(result: CommandResult) -> None:
    if result.status != "ok":
        print(build_failure_diagnostic(result), file=sys.stderr, end="")
        raise OracleContainerError(f"buildx_builder_inspect_{result.status}")
    output = (result.stdout + b"\n" + result.stderr).decode(
        "utf-8", errors="replace"
    )
    if (
        re.search(
            rf"BuildKit\s+version:\s*{re.escape(BUILDKIT_VERSION)}(?:\s|$)",
            output,
            re.IGNORECASE,
        )
        is None
    ):
        raise OracleContainerError("buildkit_version")
    if (
        re.search(
            (
                r"org\.mobyproject\.buildkit\.worker\.snapshotter:\s*"
                + re.escape(BUILDKIT_SNAPSHOTTER)
                + r"(?:\s|$)"
            ),
            output,
            re.IGNORECASE,
        )
        is None
    ):
        raise OracleContainerError("buildkit_snapshotter")


def read_build_metadata(path: Path) -> BuildMetadataIdentity:
    try:
        metadata = path.lstat()
        if (
            not stat.S_ISREG(metadata.st_mode)
            or path.is_symlink()
        ):
            raise OracleContainerError("build_metadata_symlink")
        if not 0 < metadata.st_size <= MAX_LOCK_BYTES:
            raise OracleContainerError("build_metadata_limit")
        payload = path.read_bytes()
        if len(payload) != metadata.st_size:
            raise OracleContainerError("build_metadata_changed")
        document = json.loads(payload)
    except (OSError, json.JSONDecodeError) as error:
        raise OracleContainerError("build_metadata_unreadable") from error
    if not isinstance(document, dict):
        raise OracleContainerError("build_metadata_unreadable")
    config_digest = document.get("containerimage.config.digest")
    if not isinstance(config_digest, str) or IMAGE_ID_RE.fullmatch(config_digest) is None:
        raise OracleContainerError("build_metadata_config_digest")
    manifest_digest = document.get("containerimage.digest")
    if (
        not isinstance(manifest_digest, str)
        or IMAGE_ID_RE.fullmatch(manifest_digest) is None
    ):
        raise OracleContainerError("build_metadata_manifest_digest")

    descriptor = document.get("containerimage.descriptor")
    descriptor_keys = set(descriptor) if isinstance(descriptor, dict) else set()
    if not isinstance(descriptor, dict) or descriptor_keys not in ({
        "annotations",
        "digest",
        "mediaType",
        "size",
    }, {
        "annotations",
        "digest",
        "mediaType",
        "platform",
        "size",
    }):
        raise OracleContainerError("build_metadata_descriptor")
    descriptor_digest = descriptor.get("digest")
    if (
        not isinstance(descriptor_digest, str)
        or IMAGE_ID_RE.fullmatch(descriptor_digest) is None
    ):
        raise OracleContainerError("build_metadata_descriptor_digest")
    if descriptor_digest != manifest_digest:
        raise OracleContainerError(
            "build_metadata_descriptor_digest_mismatch"
        )
    descriptor_media_type = descriptor.get("mediaType")
    if (
        not isinstance(descriptor_media_type, str)
        or descriptor_media_type not in IMAGE_MANIFEST_MEDIA_TYPES
    ):
        raise OracleContainerError(
            "build_metadata_descriptor_media_type"
        )
    descriptor_size = descriptor.get("size")
    if (
        isinstance(descriptor_size, bool)
        or not isinstance(descriptor_size, int)
        or descriptor_size <= 0
    ):
        raise OracleContainerError("build_metadata_descriptor_size")
    descriptor_annotations = normalize_descriptor_annotations(
        descriptor.get("annotations")
    )
    descriptor_platform = normalize_descriptor_platform(
        descriptor.get("platform")
    )
    return BuildMetadataIdentity(
        config_digest=config_digest,
        manifest_digest=manifest_digest,
        descriptor_digest=descriptor_digest,
        descriptor_media_type=descriptor_media_type,
        descriptor_size=descriptor_size,
        descriptor_annotations=descriptor_annotations,
        descriptor_platform=descriptor_platform,
    )


def execute_isolated_build(
    runner: CommandRunner,
    engine: str,
    image: str,
    lock_sha256: str,
    builder_name: str,
    metadata_file: Path,
    archive_file: Path,
) -> ImageIdentity:
    create_result: CommandResult | None = None
    completed = False
    try:
        create_result = runner.run(
            buildx_create_command(engine, builder_name),
            timeout_seconds=300.0,
            output_limit_bytes=4 * 1024 * 1024,
        )
        if create_result.status != "ok":
            print(build_failure_diagnostic(create_result), file=sys.stderr, end="")
            raise OracleContainerError(
                f"buildx_builder_create_{create_result.status}"
            )
        description = runner.run(
            buildx_inspect_command(engine, builder_name),
            timeout_seconds=120.0,
            output_limit_bytes=4 * 1024 * 1024,
        )
        verify_buildx_builder_description(description)
        result = runner.run(
            build_build_command(
                engine,
                image,
                lock_sha256,
                builder_name=builder_name,
                metadata_file=metadata_file,
            ),
            timeout_seconds=1800.0,
            output_limit_bytes=(
                MAX_BUILD_ARCHIVE_BYTES + MAX_BUILD_STDERR_BYTES
            ),
            stdout_path=archive_file,
            stdout_limit_bytes=MAX_BUILD_ARCHIVE_BYTES,
            stderr_limit_bytes=MAX_BUILD_STDERR_BYTES,
        )
        if result.status != "ok":
            print(build_failure_diagnostic(result), file=sys.stderr, end="")
            raise OracleContainerError(f"image_build_{result.status}")
        validate_build_archive(archive_file)
        metadata_identity = read_build_metadata(metadata_file)
        verify_docker_archive_config(
            archive_file, metadata_identity.config_digest
        )
        loaded = runner.run(
            image_load_command(engine, archive_file),
            timeout_seconds=300.0,
            output_limit_bytes=4 * 1024 * 1024,
        )
        if loaded.status != "ok":
            print(build_failure_diagnostic(loaded), file=sys.stderr, end="")
            raise OracleContainerError(f"image_load_{loaded.status}")
        inspected_identity = inspect_image_identity(
            runner,
            engine,
            image,
            lock_sha256,
            metadata_identity.config_digest,
            metadata_identity.manifest_digest,
        )
        identity = ImageIdentity(
            image_id=inspected_identity.image_id,
            platform=inspected_identity.platform,
            created=inspected_identity.created,
            diff_ids=inspected_identity.diff_ids,
            labels=inspected_identity.labels,
            manifest_digest=metadata_identity.manifest_digest,
            descriptor_digest=metadata_identity.descriptor_digest,
            descriptor_media_type=metadata_identity.descriptor_media_type,
            descriptor_size=metadata_identity.descriptor_size,
            descriptor_annotations=metadata_identity.descriptor_annotations,
            descriptor_platform=metadata_identity.descriptor_platform,
        )
        completed = True
        return identity
    finally:
        archive_cleanup_failed = False
        try:
            archive_file.unlink(missing_ok=True)
        except OSError:
            archive_cleanup_failed = True
        cleanup = runner.run(
            buildx_remove_command(engine, builder_name),
            timeout_seconds=120.0,
            output_limit_bytes=4 * 1024 * 1024,
        )
        if completed and cleanup.status != "ok":
            print(build_failure_diagnostic(cleanup), file=sys.stderr, end="")
            raise OracleContainerError(
                f"buildx_builder_cleanup_{cleanup.status}"
            )
        if completed and archive_cleanup_failed:
            raise OracleContainerError("build_archive_cleanup")


def execute_build(
    engine: str,
    image: str,
    lock_sha256: str,
    expected_image_id: str | None = None,
    expected_manifest_digest: str | None = None,
    *,
    runner: CommandRunner | None = None,
) -> ReproducibleBuild:
    runner = runner or BoundedProcessRunner()
    require_docker_build_engine(engine)
    verify_buildx_client(runner, engine)
    identities: list[ImageIdentity] = []
    with tempfile.TemporaryDirectory(prefix="rxls-oracle-build-") as raw:
        root = Path(raw)
        nonce = secrets.token_hex(8)
        for index in range(REPRODUCIBILITY_BUILD_COUNT):
            identities.append(
                execute_isolated_build(
                    runner,
                    engine,
                    image,
                    lock_sha256,
                    f"rxls-oracle-{nonce}-{index + 1}",
                    root / f"metadata-{index + 1}.json",
                    root / f"image-{index + 1}.tar",
                )
            )
    if (
        len(identities) != REPRODUCIBILITY_BUILD_COUNT
        or any(identity != identities[0] for identity in identities[1:])
    ):
        emit_image_identity_mismatch(
            "isolated_build_identity_mismatch",
            identities=identities,
        )
        raise OracleContainerError("image_reproducibility_mismatch")
    if expected_image_id is not None and identities[0].image_id != expected_image_id:
        emit_image_identity_mismatch(
            "expected_config_id_mismatch",
            expected_image_id=expected_image_id,
            identities=identities,
        )
        raise OracleContainerError("image_id_mismatch")
    if (
        expected_manifest_digest is not None
        and identities[0].manifest_digest != expected_manifest_digest
    ):
        emit_image_identity_mismatch(
            "expected_manifest_digest_mismatch",
            expected_image_id=expected_image_id,
            expected_manifest_digest=expected_manifest_digest,
            identities=identities,
        )
        raise OracleContainerError("image_manifest_digest_mismatch")
    return ReproducibleBuild(tuple(identities))


def image_identity_from_evidence_row(
    row: object, lock_sha256: str
) -> ImageIdentity:
    """Reconstruct and authenticate one normalized hosted-build identity."""
    try:
        if not isinstance(row, dict) or set(row) != {
            "config_id",
            "created",
            "descriptor",
            "identity_sha256",
            "labels",
            "manifest_digest",
            "platform",
            "rootfs_diff_ids",
            "rootfs_diff_ids_sha256",
        }:
            raise OracleContainerError("identity_row_schema")
        config_id = row.get("config_id")
        manifest_digest = row.get("manifest_digest")
        if (
            not isinstance(config_id, str)
            or IMAGE_ID_RE.fullmatch(config_id) is None
            or not isinstance(manifest_digest, str)
            or IMAGE_ID_RE.fullmatch(manifest_digest) is None
            or row.get("platform") != "linux/amd64"
        ):
            raise OracleContainerError("identity_row_core")
        created = normalize_image_created(row.get("created"))
        rootfs_diff_ids = row.get("rootfs_diff_ids")
        if not isinstance(rootfs_diff_ids, list) or not rootfs_diff_ids:
            raise OracleContainerError("identity_row_rootfs")
        diff_ids = tuple(
            normalize_diff_id(value) for value in rootfs_diff_ids
        )
        labels = row.get("labels")
        if not isinstance(labels, dict) or any(
            not isinstance(key, str) or not isinstance(value, str)
            for key, value in labels.items()
        ):
            raise OracleContainerError("identity_row_labels")
        expected_labels = {
            **EXPECTED_IMAGE_LABELS,
            "org.rxls.render-oracle.lock-sha256": lock_sha256,
        }
        if any(labels.get(key) != value for key, value in expected_labels.items()):
            raise OracleContainerError("identity_row_labels")

        descriptor = row.get("descriptor")
        descriptor_keys = (
            set(descriptor) if isinstance(descriptor, dict) else set()
        )
        if not isinstance(descriptor, dict) or descriptor_keys not in ({
            "annotations",
            "digest",
            "mediaType",
            "size",
        }, {
            "annotations",
            "digest",
            "mediaType",
            "platform",
            "size",
        }):
            raise OracleContainerError("identity_row_descriptor")
        descriptor_digest = descriptor.get("digest")
        descriptor_media_type = descriptor.get("mediaType")
        descriptor_size = descriptor.get("size")
        if (
            not isinstance(descriptor_digest, str)
            or IMAGE_ID_RE.fullmatch(descriptor_digest) is None
            or descriptor_digest != manifest_digest
            or descriptor_media_type != DOCKER_V2_MANIFEST_MEDIA_TYPE
            or isinstance(descriptor_size, bool)
            or not isinstance(descriptor_size, int)
            or descriptor_size <= 0
        ):
            raise OracleContainerError("identity_row_descriptor")
        descriptor_annotations = normalize_descriptor_annotations(
            descriptor.get("annotations")
        )
        descriptor_platform = normalize_descriptor_platform(
            descriptor.get("platform")
        )
        identity = ImageIdentity(
            image_id=config_id,
            platform="linux/amd64",
            created=created,
            diff_ids=diff_ids,
            labels=tuple(sorted(labels.items())),
            manifest_digest=manifest_digest,
            descriptor_digest=descriptor_digest,
            descriptor_media_type=descriptor_media_type,
            descriptor_size=descriptor_size,
            descriptor_annotations=descriptor_annotations,
            descriptor_platform=descriptor_platform,
        )
        if identity.evidence_row() != row:
            raise OracleContainerError("identity_row_authentication")
        return identity
    except (OracleContainerError, TypeError, ValueError) as error:
        raise OracleContainerError(
            "bootstrap_build_reproducibility"
        ) from error


def add_mode_flags(parser: argparse.ArgumentParser) -> None:
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--execute", action="store_true")


def github_id_argument(value: str) -> int:
    try:
        parsed = int(value, 10)
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            "expected a positive GitHub numeric ID"
        ) from error
    if not _is_github_id(parsed):
        raise argparse.ArgumentTypeError(
            "expected a positive GitHub numeric ID"
        )
    return parsed


def reject_bootstrap_after_pin(
    expected_image_id: str | None,
    expected_manifest_digest: str | None,
    bootstrap_identities: bool,
) -> None:
    if bootstrap_identities and (
        expected_image_id is not None
        or expected_manifest_digest is not None
    ):
        raise OracleContainerError("bootstrap_identities_after_pin")


def pin_image_from_evidence(
    lock: dict[str, Any],
    lock_payload: bytes,
    lock_sha256: str,
    evidence_path: Path,
    source_identity: SourceIdentity,
    bootstrap_receipt: object,
) -> dict[str, Any]:
    """Validate hosted two-build bootstrap evidence and return a pinned lock."""
    if (
        not isinstance(source_identity, SourceIdentity)
        or GIT_COMMIT_RE.fullmatch(source_identity.commit) is None
        or SHA256_RE.fullmatch(source_identity.wrapper_sha256) is None
    ):
        raise OracleContainerError("bootstrap_source_identity")
    if (
        lock["built_image"]["expected_id"] is not None
        or lock["built_image"]["expected_manifest_digest"] is not None
        or lock["built_image"]["bootstrap_receipt"] is not None
    ):
        raise OracleContainerError("image_lock_already_pinned")
    try:
        payload = read_bootstrap_evidence_payload(evidence_path)
        evidence = json.loads(payload)
    except json.JSONDecodeError as error:
        raise OracleContainerError("bootstrap_build_unreadable") from error
    validate_bootstrap_receipt(
        bootstrap_receipt,
        source_commit=source_identity.commit,
        evidence_payload=payload,
    )
    if not isinstance(evidence, dict) or set(evidence) != {
        "build_contract_sha256",
        "built_image_id",
        "built_manifest_digest",
        "expected_image_id",
        "expected_manifest_digest",
        "image_identity_status",
        "lock_file_sha256",
        "platform",
        "reproducibility",
        "schema",
        "source_commit",
        "status",
        "wrapper_sha256",
    }:
        raise OracleContainerError("bootstrap_build_schema")
    image_id = evidence.get("built_image_id")
    manifest_digest = evidence.get("built_manifest_digest")
    reproducibility = evidence.get("reproducibility")
    if (
        evidence.get("schema") != BUILD_EVIDENCE_SCHEMA
        or evidence.get("build_contract_sha256") != lock_sha256
        or evidence.get("expected_image_id") is not None
        or evidence.get("expected_manifest_digest") is not None
        or evidence.get("image_identity_status") != "bootstrap_capture_required"
        or evidence.get("lock_file_sha256") != sha256_bytes(lock_payload)
        or evidence.get("platform") != "linux/amd64"
        or evidence.get("source_commit") != source_identity.commit
        or evidence.get("status") != "ok"
        or evidence.get("wrapper_sha256")
        != source_identity.wrapper_sha256
        or evidence.get("wrapper_sha256")
        != lock["wrapper"]["sha256"]
        or not isinstance(image_id, str)
        or IMAGE_ID_RE.fullmatch(image_id) is None
        or not isinstance(manifest_digest, str)
        or IMAGE_ID_RE.fullmatch(manifest_digest) is None
    ):
        raise OracleContainerError("bootstrap_build_identity")
    if not isinstance(reproducibility, dict):
        raise OracleContainerError("bootstrap_build_reproducibility")
    identity_rows = reproducibility.get("identities")
    if (
        not isinstance(identity_rows, list)
        or len(identity_rows) != REPRODUCIBILITY_BUILD_COUNT
    ):
        raise OracleContainerError("bootstrap_build_reproducibility")
    identities = tuple(
        image_identity_from_evidence_row(row, lock_sha256)
        for row in identity_rows
    )
    try:
        reproducible_build = ReproducibleBuild(identities)
    except OracleContainerError as error:
        raise OracleContainerError(
            "bootstrap_build_reproducibility"
        ) from error
    if reproducibility != reproducible_build.evidence():
        raise OracleContainerError("bootstrap_build_reproducibility")
    if (
        reproducible_build.image_id != image_id
        or reproducible_build.manifest_digest != manifest_digest
    ):
        raise OracleContainerError("bootstrap_build_identity")
    pinned = json.loads(json.dumps(lock))
    pinned["built_image"]["expected_id"] = image_id
    pinned["built_image"]["expected_manifest_digest"] = manifest_digest
    pinned["built_image"]["bootstrap_receipt"] = json.loads(
        json.dumps(bootstrap_receipt)
    )
    validate_lock(pinned)
    return pinned


def write_pinned_lock(
    document: dict[str, Any],
    output: Path,
    *,
    expected_output: Path = PINNED_LOCK_OUTPUT,
) -> tuple[int, str]:
    candidate = output if output.is_absolute() else ROOT / output
    if candidate.resolve(strict=False) != expected_output.resolve(
        strict=False
    ):
        raise OracleContainerError("pinned_lock_output")
    payload = canonical_json_bytes(document)
    descriptor: int | None = None
    created = False
    try:
        descriptor = os.open(
            candidate,
            os.O_CREAT | os.O_EXCL | os.O_WRONLY,
            0o600,
        )
        created = True
        with os.fdopen(descriptor, "wb") as stream:
            descriptor = None
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
    except OSError as error:
        if descriptor is not None:
            os.close(descriptor)
        if created:
            try:
                candidate.unlink(missing_ok=True)
            except OSError:
                pass
        raise OracleContainerError("pinned_lock_write") from error
    return len(payload), sha256_bytes(payload)


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
    pin.add_argument(
        "--github-run-id", required=True, type=github_id_argument
    )
    pin.add_argument(
        "--github-run-attempt", required=True, type=github_id_argument
    )
    pin.add_argument(
        "--github-job-id", required=True, type=github_id_argument
    )
    pin.add_argument(
        "--github-artifact-id", required=True, type=github_id_argument
    )
    pin.add_argument("--output-lock", required=True, type=Path)

    build = subparsers.add_parser("build", help="build the locked linux/amd64 image")
    build.add_argument("--engine", choices=("auto", "docker"), default="auto")
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
        expected_manifest_digest = lock["built_image"][
            "expected_manifest_digest"
        ]
        if args.action in {"verify-lock", "build"}:
            reject_bootstrap_after_pin(
                expected_image_id,
                expected_manifest_digest,
                args.bootstrap_identities,
            )
        if args.action in {"build", "pin-image"}:
            require_canonical_build_lock(args.lock)
        if args.action == "verify-lock":
            if expected_image_id is None and not args.bootstrap_identities:
                raise OracleContainerError("image_pin_required")
            print(
                canonical_json_bytes(
                    {
                        "build_contract_sha256": lock_sha256,
                        "expected_image_id": expected_image_id,
                        "expected_manifest_digest": expected_manifest_digest,
                        "lock_file_sha256": lock_file_sha256,
                        "schema": LOCK_SCHEMA,
                        "status": "ok",
                        "wrapper_sha256": lock["wrapper"]["sha256"],
                    }
                ).decode("utf-8"),
                end="",
            )
            return 0

        if args.action == "pin-image":
            source_identity = require_clean_source(lock)
            bootstrap_receipt = fetch_hosted_bootstrap_receipt(
                args.build_evidence,
                source_identity,
                run_id=args.github_run_id,
                run_attempt=args.github_run_attempt,
                job_id=args.github_job_id,
                artifact_id=args.github_artifact_id,
            )
            pinned = pin_image_from_evidence(
                lock,
                payload,
                lock_sha256,
                args.build_evidence,
                source_identity,
                bootstrap_receipt,
            )
            output_bytes, output_sha256 = write_pinned_lock(
                pinned, args.output_lock
            )
            print(
                canonical_json_bytes(
                    {
                        "bytes": output_bytes,
                        "output": (
                            "scripts/render-oracle-container/"
                            "lock.pinned.json"
                        ),
                        "schema": LOCK_SCHEMA,
                        "sha256": output_sha256,
                        "status": "ok",
                    }
                ).decode("utf-8"),
                end="",
            )
            return 0

        engine = resolve_engine(args.engine, execute=args.execute)
        image = validate_image_reference(args.image)
        if args.action == "build":
            require_docker_build_engine(engine)
            if args.dry_run:
                isolated_builds = []
                for index in range(REPRODUCIBILITY_BUILD_COUNT):
                    builder_name = f"rxls-oracle-dry-run-{index + 1}"
                    archive_placeholder = Path(
                        f"<build-archive-{index + 1}>"
                    )
                    isolated_builds.append(
                        {
                            "archive_stdout": {
                                "max_bytes": MAX_BUILD_ARCHIVE_BYTES,
                                "path": str(archive_placeholder),
                            },
                            "build": path_neutral_command(
                                build_build_command(
                                    engine,
                                    image,
                                    lock_sha256,
                                    builder_name=builder_name,
                                    metadata_file=Path(
                                        f"<build-metadata-{index + 1}>"
                                    ),
                                ),
                                [
                                    (
                                        CONTAINERFILE,
                                        "<container-context>/Containerfile",
                                    ),
                                    (CONTAINER_DIR, "<container-context>"),
                                ],
                            ),
                            "cleanup": buildx_remove_command(
                                engine, builder_name
                            ),
                            "create": buildx_create_command(
                                engine, builder_name
                            ),
                            "inspect": buildx_inspect_command(
                                engine, builder_name
                            ),
                            "load": image_load_command(
                                engine, archive_placeholder
                            ),
                        }
                    )
                document = {
                    "commands": {
                        "buildx_client_version": [
                            engine,
                            "buildx",
                            "version",
                        ],
                        "isolated_builds": isolated_builds,
                    },
                    "dry_run": True,
                    "expected_image_id": expected_image_id,
                    "expected_manifest_digest": expected_manifest_digest,
                    "image_verified": False,
                    "schema": PLAN_SCHEMA,
                }
            else:
                if expected_image_id is None and not args.bootstrap_identities:
                    raise OracleContainerError("image_pin_required")
                source_identity = require_clean_source(lock)
                build_result = execute_build(
                    engine,
                    image,
                    lock_sha256,
                    expected_image_id,
                    expected_manifest_digest,
                )
                image_id = build_result.image_id
                manifest_digest = build_result.manifest_digest
                document = {
                    "build_contract_sha256": lock_sha256,
                    "built_image_id": image_id,
                    "built_manifest_digest": manifest_digest,
                    "expected_image_id": expected_image_id,
                    "expected_manifest_digest": expected_manifest_digest,
                    "image_identity_status": (
                        "pinned_match"
                        if (
                            expected_image_id is not None
                            and expected_manifest_digest is not None
                        )
                        else "bootstrap_capture_required"
                    ),
                    "lock_file_sha256": lock_file_sha256,
                    "platform": "linux/amd64",
                    "reproducibility": build_result.evidence(),
                    "schema": BUILD_EVIDENCE_SCHEMA,
                    "source_commit": source_identity.commit,
                    "status": "ok",
                    "wrapper_sha256": source_identity.wrapper_sha256,
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
                config,
                engine,
                image,
                lock_sha256,
                expected_image_id,
                expected_manifest_digest,
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
                expected_manifest_digest,
                lock_file_sha256,
            )
        print(canonical_json_bytes(document).decode("utf-8"), end="")
        return 0
    except OracleContainerError as error:
        print(f"render_oracle_error:{error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
