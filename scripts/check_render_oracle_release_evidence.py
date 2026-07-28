#!/usr/bin/env python3
"""Validate full, exact-SHA Render Oracle evidence before npm publication."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import (
    HTTPRedirectHandler,
    Request,
    build_opener,
)
import zipfile


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_ORACLE_LOCK = ROOT / "scripts" / "render-oracle-container" / "lock.json"
DEFAULT_ORACLE_WRAPPER = ROOT / "scripts" / "run-render-oracle-container.py"
EXPECTED_FILES = frozenset(
    {
        "authored-print-gate.json",
        "baseline-candidate-a.json",
        "baseline-candidate-b.json",
        "baseline-gate-a.json",
        "baseline-gate-b.json",
        "build.json",
        "fidelity-a.json",
        "fidelity-b.json",
        "host-tools.json",
        "hosted-summary.json",
        "renderer.json",
        "repeatability.json",
    }
)
MAX_FILE_BYTES = 16 * 1024 * 1024
MAX_TOTAL_BYTES = 48 * 1024 * 1024
MAX_ARTIFACT_ARCHIVE_BYTES = 64 * 1024 * 1024
MAX_LOCK_BYTES = 256 * 1024
MAX_WRAPPER_BYTES = 512 * 1024
DOWNLOAD_CHUNK_BYTES = 1024 * 1024
DOWNLOAD_TIMEOUT_SECONDS = 60
EXPECTED_REPOSITORY = "HyunjoJung/rxls"
GITHUB_API_VERSION = "2022-11-28"
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
HEAD_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
IMAGE_DIGEST_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
BUILD_SCHEMA = "rxls.render-oracle-container-build.v3"
LOCK_SCHEMA = "rxls.render-oracle-container-lock.v3"
BOOTSTRAP_RECEIPT_SCHEMA = "rxls.render-oracle-bootstrap-receipt.v1"
HOSTED_CAMPAIGN_SCHEMA = "rxls.render-oracle-hosted-campaign.v5"
SOURCE_DATE_EPOCH = 1_783_900_800
SOURCE_DATE_EPOCH_RFC3339 = "2026-07-13T00:00:00Z"
DOCKER_V2_MANIFEST_MEDIA_TYPE = (
    "application/vnd.docker.distribution.manifest.v2+json"
)
LIBREOFFICE_ARTIFACT_SHA256 = (
    "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5"
)
BUILDX_VERSION = "v0.35.0"
BUILDX_COMMIT = "a319e5b15052cf6557ceb666eb8ff6e32380b782"
BUILDKIT_VERSION = "v0.31.2"
BUILDKIT_COMMIT = "e42e1bfd389af7203238cce77b1f7dad447285e9"
BUILDKIT_IMAGE = (
    "docker.io/moby/buildkit:v0.31.2@sha256:"
    "2f5adac4ecd194d9f8c10b7b5d7bceb5186853db1b26e5abd3a657af0b7e26ec"
)
BUILD_KEYS = frozenset(
    {
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
    }
)
IDENTITY_KEYS = frozenset(
    {
        "config_id",
        "created",
        "descriptor",
        "identity_sha256",
        "labels",
        "manifest_digest",
        "platform",
        "rootfs_diff_ids",
        "rootfs_diff_ids_sha256",
    }
)
REPRODUCIBILITY_KEYS = frozenset(
    {
        "build_count",
        "buildkit_commit",
        "buildkit_compatibility",
        "buildkit_image",
        "buildkit_version",
        "buildx_commit",
        "buildx_version",
        "config_ids",
        "descriptor_digests",
        "descriptor_media_types",
        "descriptor_sizes",
        "driver",
        "export_archive_max_bytes",
        "export_destination",
        "export_media_type",
        "export_tar",
        "identities",
        "identity_sha256",
        "manifest_digests",
        "no_cache",
        "provenance",
        "rewrite_timestamp",
        "rootfs_diff_ids_sha256",
        "sbom",
        "snapshotter",
        "source_date_epoch",
        "status",
    }
)


class EvidenceError(ValueError):
    """Raised when hosted release evidence is absent or inconsistent."""


def _require(condition: bool, code: str) -> None:
    if not condition:
        raise EvidenceError(code)


class _NoRedirect(HTTPRedirectHandler):
    """Expose the authenticated API redirect without following it."""

    def redirect_request(
        self,
        request: Request,
        file_pointer: object,
        code: int,
        message: str,
        headers: object,
        new_url: str,
    ) -> None:
        return None


class _HttpsOnlyRedirect(HTTPRedirectHandler):
    """Permit signed archive redirects only while they remain HTTPS."""

    def redirect_request(
        self,
        request: Request,
        file_pointer: object,
        code: int,
        message: str,
        headers: object,
        new_url: str,
    ) -> Request | None:
        _require(_safe_https_url(new_url), "artifact_download_redirect")
        return super().redirect_request(
            request,
            file_pointer,
            code,
            message,
            headers,
            new_url,
        )


def _safe_https_url(value: object) -> bool:
    if not isinstance(value, str) or not value or len(value) > 16 * 1024:
        return False
    if any(ord(character) < 0x20 or ord(character) == 0x7F for character in value):
        return False
    try:
        parsed = urlsplit(value)
        port = parsed.port
    except ValueError:
        return False
    return (
        parsed.scheme == "https"
        and parsed.hostname is not None
        and parsed.username is None
        and parsed.password is None
        and port in (None, 443)
        and not parsed.fragment
    )


def _response_status(response: object) -> int | None:
    status = getattr(response, "status", None)
    if isinstance(status, int):
        return status
    getcode = getattr(response, "getcode", None)
    if callable(getcode):
        value = getcode()
        if isinstance(value, int):
            return value
    return None


def _artifact_download_redirect(
    repository: str,
    artifact_id: int,
    token: str,
    *,
    opener: object | None = None,
) -> str:
    _require(repository == EXPECTED_REPOSITORY, "artifact_repository")
    _require(_positive_int(artifact_id), "artifact_id")
    _require(
        0 < len(token) <= 4096
        and all(0x21 <= ord(character) <= 0x7E for character in token),
        "github_token",
    )
    request = Request(
        (
            f"https://api.github.com/repos/{repository}/actions/artifacts/"
            f"{artifact_id}/zip"
        ),
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": "rxls-render-oracle-release-evidence",
            "X-GitHub-Api-Version": GITHUB_API_VERSION,
        },
        method="GET",
    )
    api_opener = opener if opener is not None else build_opener(_NoRedirect())
    response: object | None = None
    try:
        response = api_opener.open(request, timeout=DOWNLOAD_TIMEOUT_SECONDS)
        _require(_response_status(response) == 302, "artifact_api_status")
        location = response.headers.get("Location")
    except HTTPError as error:
        try:
            _require(error.code == 302, "artifact_api_status")
            location = error.headers.get("Location")
        finally:
            error.close()
    except (OSError, TimeoutError, URLError) as error:
        raise EvidenceError("artifact_api_request") from error
    finally:
        if response is not None:
            response.close()
    _require(_safe_https_url(location), "artifact_api_redirect")
    return location


def _write_all(file_descriptor: int, payload: bytes) -> None:
    view = memoryview(payload)
    while view:
        written = os.write(file_descriptor, view)
        _require(written > 0, "artifact_archive_write")
        view = view[written:]


def download_artifact_archive(
    repository: str,
    artifact_id: int,
    destination: Path,
    expected_size: int,
    expected_digest: str,
    *,
    token: str | None = None,
    api_opener: object | None = None,
    archive_opener: object | None = None,
) -> None:
    """Download the exact artifact-ID ZIP with a strict byte and digest bound."""

    _require(
        _positive_int(expected_size, MAX_ARTIFACT_ARCHIVE_BYTES),
        "artifact_archive_size",
    )
    _require(
        isinstance(expected_digest, str)
        and IMAGE_DIGEST_RE.fullmatch(expected_digest) is not None,
        "artifact_digest",
    )
    try:
        parent_metadata = destination.parent.lstat()
    except OSError as error:
        raise EvidenceError("artifact_archive_parent") from error
    _require(
        stat.S_ISDIR(parent_metadata.st_mode)
        and not destination.parent.is_symlink(),
        "artifact_archive_parent",
    )
    try:
        destination.lstat()
    except FileNotFoundError:
        pass
    except OSError as error:
        raise EvidenceError("artifact_archive_destination") from error
    else:
        raise EvidenceError("artifact_archive_destination")

    credential = token if token is not None else os.environ.get("GH_TOKEN", "")
    location = _artifact_download_redirect(
        repository,
        artifact_id,
        credential,
        opener=api_opener,
    )
    request = Request(
        location,
        headers={
            "Accept-Encoding": "identity",
            "User-Agent": "rxls-render-oracle-release-evidence",
        },
        method="GET",
    )
    signed_opener = (
        archive_opener
        if archive_opener is not None
        else build_opener(_HttpsOnlyRedirect())
    )
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    file_descriptor: int | None = None
    response: object | None = None
    completed = False
    try:
        response = signed_opener.open(request, timeout=DOWNLOAD_TIMEOUT_SECONDS)
        _require(_response_status(response) == 200, "artifact_download_status")
        final_url_getter = getattr(response, "geturl", None)
        if callable(final_url_getter):
            _require(
                _safe_https_url(final_url_getter()),
                "artifact_download_final_url",
            )
        content_encoding = response.headers.get("Content-Encoding")
        _require(
            content_encoding in (None, "", "identity"),
            "artifact_download_encoding",
        )
        content_length = response.headers.get("Content-Length")
        if content_length is not None:
            _require(
                re.fullmatch(r"[1-9][0-9]*", content_length) is not None
                and int(content_length) == expected_size,
                "artifact_download_content_length",
            )

        file_descriptor = os.open(destination, flags, 0o600)
        digest = hashlib.sha256()
        downloaded = 0
        while True:
            remaining = expected_size - downloaded
            chunk = response.read(min(DOWNLOAD_CHUNK_BYTES, remaining + 1))
            _require(isinstance(chunk, bytes), "artifact_download_read")
            if not chunk:
                break
            downloaded += len(chunk)
            _require(downloaded <= expected_size, "artifact_download_oversize")
            digest.update(chunk)
            _write_all(file_descriptor, chunk)
        _require(downloaded == expected_size, "artifact_download_size")
        _require(
            f"sha256:{digest.hexdigest()}" == expected_digest,
            "artifact_download_digest",
        )
        metadata = os.fstat(file_descriptor)
        _require(
            stat.S_ISREG(metadata.st_mode) and metadata.st_size == expected_size,
            "artifact_archive_written",
        )
        completed = True
    except HTTPError as error:
        error.close()
        raise EvidenceError("artifact_download_status") from error
    except (OSError, TimeoutError, URLError) as error:
        raise EvidenceError("artifact_download_request") from error
    finally:
        if response is not None:
            response.close()
        if file_descriptor is not None:
            os.close(file_descriptor)
        if not completed:
            try:
                destination.unlink()
            except FileNotFoundError:
                pass


def _authenticated_archive_payload(
    archive_path: Path,
    expected_size: int,
    expected_digest: str,
) -> bytes:
    _require(
        _positive_int(expected_size, MAX_ARTIFACT_ARCHIVE_BYTES),
        "artifact_archive_size",
    )
    _require(
        isinstance(expected_digest, str)
        and IMAGE_DIGEST_RE.fullmatch(expected_digest) is not None,
        "artifact_digest",
    )
    try:
        path_metadata = archive_path.lstat()
    except OSError as error:
        raise EvidenceError("artifact_archive_type") from error
    _require(
        stat.S_ISREG(path_metadata.st_mode)
        and not archive_path.is_symlink()
        and path_metadata.st_size == expected_size,
        "artifact_archive_type",
    )
    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        file_descriptor = os.open(archive_path, flags)
        with os.fdopen(file_descriptor, "rb") as archive_file:
            before = os.fstat(archive_file.fileno())
            _require(
                stat.S_ISREG(before.st_mode)
                and before.st_dev == path_metadata.st_dev
                and before.st_ino == path_metadata.st_ino
                and before.st_size == expected_size,
                "artifact_archive_changed",
            )
            payload = archive_file.read(expected_size + 1)
            after = os.fstat(archive_file.fileno())
    except OSError as error:
        raise EvidenceError("artifact_archive_read") from error
    _require(
        len(payload) == expected_size
        and after.st_dev == before.st_dev
        and after.st_ino == before.st_ino
        and after.st_size == before.st_size,
        "artifact_archive_changed",
    )
    _require(
        f"sha256:{_sha256(payload)}" == expected_digest,
        "artifact_archive_digest",
    )
    return payload


def extract_authenticated_artifact(
    archive_path: Path,
    artifact_dir: Path,
    expected_size: int,
    expected_digest: str,
) -> None:
    """Authenticate and safely extract the exact path-neutral evidence ZIP."""

    archive_payload = _authenticated_archive_payload(
        archive_path,
        expected_size,
        expected_digest,
    )
    extracted: dict[str, bytes] = {}
    try:
        with zipfile.ZipFile(io.BytesIO(archive_payload), "r") as archive:
            members = archive.infolist()
            names = [member.filename for member in members]
            _require(
                len(members) == len(EXPECTED_FILES)
                and len(names) == len(set(names))
                and set(names) == EXPECTED_FILES,
                "artifact_archive_file_set",
            )
            total_size = 0
            for member in members:
                _require(
                    member.filename == member.orig_filename
                    and member.filename.isascii()
                    and "/" not in member.filename
                    and "\\" not in member.filename
                    and not member.is_dir(),
                    "artifact_archive_member_name",
                )
                _require(
                    not (member.flag_bits & 0x1)
                    and member.compress_type
                    in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED),
                    "artifact_archive_member_encoding",
                )
                unix_mode = (member.external_attr >> 16) & 0xFFFF
                file_type = stat.S_IFMT(unix_mode)
                _require(
                    file_type in (0, stat.S_IFREG),
                    "artifact_archive_member_type",
                )
                _require(
                    0 < member.file_size <= MAX_FILE_BYTES
                    and 0 < member.compress_size <= len(archive_payload)
                    and 0 <= member.header_offset < len(archive_payload),
                    "artifact_archive_member_size",
                )
                total_size += member.file_size
                _require(total_size <= MAX_TOTAL_BYTES, "artifact_total_size")
                with archive.open(member, "r") as source:
                    chunks: list[bytes] = []
                    member_size = 0
                    while True:
                        remaining = member.file_size - member_size
                        chunk = source.read(min(DOWNLOAD_CHUNK_BYTES, remaining + 1))
                        _require(
                            isinstance(chunk, bytes),
                            "artifact_archive_member_read",
                        )
                        if not chunk:
                            break
                        member_size += len(chunk)
                        _require(
                            member_size <= member.file_size
                            and member_size <= MAX_FILE_BYTES,
                            "artifact_archive_member_oversize",
                        )
                        chunks.append(chunk)
                _require(
                    member_size == member.file_size,
                    "artifact_archive_member_changed",
                )
                extracted[member.filename] = b"".join(chunks)
    except (OSError, RuntimeError, zipfile.BadZipFile, zipfile.LargeZipFile) as error:
        raise EvidenceError("artifact_archive_invalid") from error

    try:
        parent_metadata = artifact_dir.parent.lstat()
    except OSError as error:
        raise EvidenceError("artifact_extract_parent") from error
    _require(
        stat.S_ISDIR(parent_metadata.st_mode)
        and not artifact_dir.parent.is_symlink(),
        "artifact_extract_parent",
    )
    try:
        artifact_dir.lstat()
    except FileNotFoundError:
        pass
    except OSError as error:
        raise EvidenceError("artifact_extract_destination") from error
    else:
        raise EvidenceError("artifact_extract_destination")
    try:
        artifact_dir.mkdir(mode=0o700)
        for name in sorted(extracted):
            destination = artifact_dir / name
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
            if hasattr(os, "O_NOFOLLOW"):
                flags |= os.O_NOFOLLOW
            file_descriptor = os.open(destination, flags, 0o600)
            try:
                _write_all(file_descriptor, extracted[name])
                metadata = os.fstat(file_descriptor)
                _require(
                    stat.S_ISREG(metadata.st_mode)
                    and metadata.st_size == len(extracted[name]),
                    "artifact_extract_member",
                )
            finally:
                os.close(file_descriptor)
    except OSError as error:
        raise EvidenceError("artifact_extract_write") from error


def _validate_artifact_binding(
    *,
    head_sha: str,
    workflow_run_id: int,
    workflow_run_attempt: int,
    artifact_id: int,
    artifact_name: str,
    artifact_size_bytes: int,
    artifact_digest: str,
    repository: str,
) -> None:
    _require(repository == EXPECTED_REPOSITORY, "artifact_repository")
    _require(_positive_int(workflow_run_id), "workflow_run_id")
    _require(_positive_int(workflow_run_attempt), "workflow_run_attempt")
    _require(_positive_int(artifact_id), "artifact_id")
    _require(
        _positive_int(artifact_size_bytes, MAX_ARTIFACT_ARCHIVE_BYTES),
        "artifact_archive_size",
    )
    _require(
        artifact_name
        == (
            f"render-oracle-{head_sha}-{workflow_run_id}-"
            f"{workflow_run_attempt}-full"
        ),
        "artifact_name",
    )
    _require(
        IMAGE_DIGEST_RE.fullmatch(artifact_digest) is not None,
        "artifact_digest",
    )


def _object_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise EvidenceError("duplicate_json_key")
        result[key] = value
    return result


def _reject_json_constant(value: str) -> object:
    raise EvidenceError(f"invalid_json_constant:{value}")


def _read_json(path: Path) -> tuple[dict[str, Any], bytes]:
    _require(path.is_file() and not path.is_symlink(), "evidence_file_type")
    payload = path.read_bytes()
    _require(0 < len(payload) <= MAX_FILE_BYTES, "evidence_file_size")
    try:
        document = json.loads(
            payload,
            object_pairs_hook=_object_pairs,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise EvidenceError("evidence_invalid_json") from error
    _require(isinstance(document, dict), "evidence_not_object")
    return document, payload


def _sha256(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def _canonical_sha256(value: object) -> str:
    payload = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")
    return _sha256(payload)


def _path_neutral(value: object) -> None:
    if isinstance(value, dict):
        _require("path" not in value, "path_bearing_key")
        for item in value.values():
            _path_neutral(item)
    elif isinstance(value, list):
        for item in value:
            _path_neutral(item)
    elif isinstance(value, str):
        lowered = value.lower()
        _require(not value.startswith("/"), "absolute_path")
        _require(re.match(r"^[A-Za-z]:[\\/]", value) is None, "windows_path")
        _require(not lowered.startswith("file://"), "file_uri")
        _require("local/render-corpus" not in lowered, "corpus_path")
        _require("payload/" not in lowered, "payload_path")


def _hash_matches(value: object) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _image_digest_matches(value: object) -> bool:
    return isinstance(value, str) and IMAGE_DIGEST_RE.fullmatch(value) is not None


def _positive_int(
    value: object,
    maximum: int | None = (1 << 63) - 1,
) -> bool:
    return (
        isinstance(value, int)
        and not isinstance(value, bool)
        and value > 0
        and (maximum is None or value <= maximum)
    )


def _validate_bootstrap_receipt(receipt: object) -> str:
    _require(
        isinstance(receipt, dict)
        and set(receipt)
        == {"artifact", "evidence", "job", "repository", "run", "schema"}
        and receipt.get("schema") == BOOTSTRAP_RECEIPT_SCHEMA,
        "oracle_bootstrap_receipt_schema",
    )
    artifact = receipt.get("artifact")
    evidence = receipt.get("evidence")
    job = receipt.get("job")
    repository = receipt.get("repository")
    run = receipt.get("run")
    _require(
        isinstance(artifact, dict)
        and set(artifact) == {"digest", "id", "name", "size_in_bytes"}
        and re.fullmatch(r"sha256:[0-9a-f]{64}", str(artifact.get("digest")))
        is not None
        and _positive_int(artifact.get("id"))
        and _positive_int(artifact.get("size_in_bytes"), 1024 * 1024),
        "oracle_bootstrap_receipt_artifact",
    )
    _require(
        isinstance(evidence, dict)
        and set(evidence) == {"bytes", "member", "sha256"}
        and _positive_int(evidence.get("bytes"), MAX_LOCK_BYTES)
        and evidence.get("member") == "render-oracle-image-build.json"
        and _hash_matches(evidence.get("sha256")),
        "oracle_bootstrap_receipt_evidence",
    )
    _require(
        isinstance(job, dict)
        and set(job)
        == {"conclusion", "id", "name", "run_attempt", "run_id"}
        and job.get("conclusion") == "failure"
        and _positive_int(job.get("id"))
        and job.get("name") == "locked LibreOffice oracle image"
        and _positive_int(job.get("run_attempt"))
        and _positive_int(job.get("run_id")),
        "oracle_bootstrap_receipt_job",
    )
    _require(
        repository == {"full_name": "HyunjoJung/rxls", "id": 1_297_467_060},
        "oracle_bootstrap_receipt_repository",
    )
    _require(
        isinstance(run, dict)
        and set(run)
        == {
            "conclusion",
            "event",
            "head_sha",
            "id",
            "run_attempt",
            "workflow",
        }
        and run.get("conclusion") == "failure"
        and run.get("event") == "pull_request"
        and HEAD_SHA_RE.fullmatch(str(run.get("head_sha"))) is not None
        and _positive_int(run.get("id"))
        and _positive_int(run.get("run_attempt"))
        and run.get("workflow") == ".github/workflows/render-hardening.yml",
        "oracle_bootstrap_receipt_run",
    )
    source_commit = run["head_sha"]
    _require(
        job["run_id"] == run["id"]
        and job["run_attempt"] == run["run_attempt"]
        and artifact["name"]
        == (
            f"render-oracle-image-{source_commit}-{run['id']}-"
            f"{run['run_attempt']}"
        ),
        "oracle_bootstrap_receipt_binding",
    )
    return source_commit


def _regular_file_payload(path: Path, maximum: int, code: str) -> bytes:
    try:
        metadata = path.lstat()
        _require(stat.S_ISREG(metadata.st_mode) and not path.is_symlink(), f"{code}_type")
        _require(0 < metadata.st_size <= maximum, f"{code}_size")
        payload = path.read_bytes()
    except OSError as error:
        raise EvidenceError(f"{code}_unreadable") from error
    _require(len(payload) == metadata.st_size, f"{code}_changed")
    return payload


def _release_contract(
    lock_path: Path,
    wrapper_path: Path,
) -> dict[str, str]:
    """Authenticate the checked-out lock and wrapper used by build evidence."""

    lock_payload = _regular_file_payload(lock_path, MAX_LOCK_BYTES, "oracle_lock")
    try:
        lock = json.loads(
            lock_payload,
            object_pairs_hook=_object_pairs,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise EvidenceError("oracle_lock_json") from error
    _require(isinstance(lock, dict) and lock.get("schema") == LOCK_SCHEMA, "oracle_lock_schema")

    built_image = lock.get("built_image")
    _require(
        isinstance(built_image, dict)
        and set(built_image)
        == {
            "bootstrap_receipt",
            "expected_id",
            "expected_manifest_digest",
            "identity_kind",
            "source_date_epoch",
            "unpinned_verification",
        },
        "oracle_lock_built_image",
    )
    expected_image_id = built_image.get("expected_id")
    expected_manifest_digest = built_image.get("expected_manifest_digest")
    _require(
        _image_digest_matches(expected_image_id)
        and _image_digest_matches(expected_manifest_digest),
        "oracle_lock_pin_pair",
    )
    bootstrap_source_commit = _validate_bootstrap_receipt(
        built_image.get("bootstrap_receipt")
    )
    _require(
        built_image.get("source_date_epoch") == SOURCE_DATE_EPOCH,
        "oracle_lock_source_date_epoch",
    )
    _require(
        built_image.get("identity_kind")
        == "docker_schema2_manifest_digest_plus_oci_image_config_digest"
        and built_image.get("unpinned_verification")
        == (
            "bootstrap_only_two_isolated_no_cache_builds_plus_exact_config_"
            "manifest_descriptor_rootfs_contract_and_labels"
        ),
        "oracle_lock_identity_contract",
    )

    wrapper = lock.get("wrapper")
    _require(
        isinstance(wrapper, dict)
        and set(wrapper) == {"bytes", "path", "sha256"}
        and wrapper.get("path") == "scripts/run-render-oracle-container.py",
        "oracle_lock_wrapper",
    )
    wrapper_payload = _regular_file_payload(
        wrapper_path, MAX_WRAPPER_BYTES, "oracle_wrapper"
    )
    wrapper_sha256 = _sha256(wrapper_payload)
    _require(
        wrapper.get("bytes") == len(wrapper_payload)
        and wrapper.get("sha256") == wrapper_sha256
        and _hash_matches(wrapper_sha256),
        "oracle_wrapper_identity",
    )

    normalized = json.loads(json.dumps(lock))
    normalized["built_image"]["bootstrap_receipt"] = None
    normalized["built_image"]["expected_id"] = None
    normalized["built_image"]["expected_manifest_digest"] = None
    return {
        "build_contract_sha256": _canonical_sha256(normalized),
        "bootstrap_source_commit": bootstrap_source_commit,
        "expected_image_id": expected_image_id,
        "expected_manifest_digest": expected_manifest_digest,
        "lock_file_sha256": _sha256(lock_payload),
        "wrapper_sha256": wrapper_sha256,
    }


def _validate_identity_row(
    row: object,
    *,
    config_digest: str,
    manifest_digest: str,
    build_contract_sha256: str,
) -> dict[str, Any]:
    _require(isinstance(row, dict) and set(row) == IDENTITY_KEYS, "build_identity_schema")
    identity = row
    _require(
        identity.get("config_id") == config_digest
        and identity.get("manifest_digest") == manifest_digest
        and identity.get("platform") == "linux/amd64"
        and identity.get("created") == SOURCE_DATE_EPOCH_RFC3339,
        "build_identity_core",
    )

    diff_ids = identity.get("rootfs_diff_ids")
    _require(
        isinstance(diff_ids, list)
        and 0 < len(diff_ids) <= 4096
        and all(_image_digest_matches(value) for value in diff_ids),
        "build_identity_rootfs",
    )
    _require(
        identity.get("rootfs_diff_ids_sha256") == _canonical_sha256(diff_ids),
        "build_identity_rootfs_authentication",
    )

    labels = identity.get("labels")
    _require(
        isinstance(labels, dict)
        and all(
            isinstance(key, str) and isinstance(value, str)
            for key, value in labels.items()
        ),
        "build_identity_labels",
    )
    expected_labels = {
        "org.opencontainers.image.version": "26.2.3.2",
        "org.rxls.render-oracle.architecture": "linux/amd64",
        "org.rxls.render-oracle.libreoffice-artifact-sha256": (
            LIBREOFFICE_ARTIFACT_SHA256
        ),
        "org.rxls.render-oracle.lock-sha256": build_contract_sha256,
    }
    _require(
        all(labels.get(key) == value for key, value in expected_labels.items()),
        "build_identity_labels",
    )

    descriptor = identity.get("descriptor")
    _require(
        isinstance(descriptor, dict)
        and set(descriptor)
        in (
            {"annotations", "digest", "mediaType", "size"},
            {"annotations", "digest", "mediaType", "platform", "size"},
        ),
        "build_identity_descriptor",
    )
    _require(
        descriptor.get("digest") == manifest_digest
        and descriptor.get("mediaType") == DOCKER_V2_MANIFEST_MEDIA_TYPE
        and isinstance(descriptor.get("size"), int)
        and not isinstance(descriptor.get("size"), bool)
        and descriptor["size"] > 0,
        "build_identity_descriptor",
    )
    _require(
        descriptor.get("annotations")
        == {"org.opencontainers.image.created": SOURCE_DATE_EPOCH_RFC3339},
        "build_identity_descriptor_annotations",
    )
    if "platform" in descriptor:
        _require(
            descriptor["platform"] == {"architecture": "amd64", "os": "linux"},
            "build_identity_descriptor_platform",
        )

    normalized = {
        key: value
        for key, value in identity.items()
        if key not in {"identity_sha256", "rootfs_diff_ids_sha256"}
    }
    _require(
        identity.get("identity_sha256") == _canonical_sha256(normalized),
        "build_identity_authentication",
    )
    return identity


def _validate_build(
    build: dict[str, Any],
    head_sha: str,
    contract: dict[str, str],
) -> None:
    _require(set(build) == BUILD_KEYS, "build_schema_keys")
    _require(build.get("schema") == BUILD_SCHEMA, "build_schema")
    _require(
        build.get("status") == "ok"
        and build.get("platform") == "linux/amd64"
        and build.get("image_identity_status") == "pinned_match",
        "build_status",
    )
    _require(
        build.get("source_commit") == head_sha
        and build.get("wrapper_sha256") == contract["wrapper_sha256"],
        "build_provenance",
    )
    _require(
        build.get("build_contract_sha256") == contract["build_contract_sha256"]
        and build.get("lock_file_sha256") == contract["lock_file_sha256"],
        "build_lock_contract",
    )

    config_digest = contract["expected_image_id"]
    manifest_digest = contract["expected_manifest_digest"]
    _require(
        build.get("expected_image_id")
        == build.get("built_image_id")
        == config_digest,
        "build_config_pin",
    )
    _require(
        build.get("expected_manifest_digest")
        == build.get("built_manifest_digest")
        == manifest_digest,
        "build_manifest_pin",
    )

    reproducibility = build.get("reproducibility")
    _require(
        isinstance(reproducibility, dict)
        and set(reproducibility) == REPRODUCIBILITY_KEYS,
        "build_reproducibility_schema",
    )
    rows = reproducibility.get("identities")
    _require(
        isinstance(rows, list) and len(rows) == 2 and rows[0] == rows[1],
        "build_reproducibility_identities",
    )
    identity = _validate_identity_row(
        rows[0],
        config_digest=config_digest,
        manifest_digest=manifest_digest,
        build_contract_sha256=contract["build_contract_sha256"],
    )
    _validate_identity_row(
        rows[1],
        config_digest=config_digest,
        manifest_digest=manifest_digest,
        build_contract_sha256=contract["build_contract_sha256"],
    )
    descriptor = identity["descriptor"]
    identity_sha256 = identity["identity_sha256"]
    rootfs_sha256 = identity["rootfs_diff_ids_sha256"]
    expected_reproducibility = {
        "build_count": 2,
        "buildkit_compatibility": {
            "explicit": False,
            "source": "pinned-buildkit-default",
            "version": 30,
        },
        "buildkit_commit": BUILDKIT_COMMIT,
        "buildkit_image": BUILDKIT_IMAGE,
        "buildkit_version": BUILDKIT_VERSION,
        "buildx_commit": BUILDX_COMMIT,
        "buildx_version": BUILDX_VERSION,
        "config_ids": [config_digest, config_digest],
        "descriptor_digests": [manifest_digest, manifest_digest],
        "descriptor_media_types": [
            DOCKER_V2_MANIFEST_MEDIA_TYPE,
            DOCKER_V2_MANIFEST_MEDIA_TYPE,
        ],
        "descriptor_sizes": [descriptor["size"], descriptor["size"]],
        "driver": "docker-container",
        "export_archive_max_bytes": 4 * 1024 * 1024 * 1024,
        "export_destination": "stdout",
        "export_media_type": DOCKER_V2_MANIFEST_MEDIA_TYPE,
        "export_tar": True,
        "identities": [identity, identity],
        "identity_sha256": [identity_sha256, identity_sha256],
        "manifest_digests": [manifest_digest, manifest_digest],
        "no_cache": True,
        "provenance": False,
        "rewrite_timestamp": True,
        "rootfs_diff_ids_sha256": [rootfs_sha256, rootfs_sha256],
        "sbom": False,
        "snapshotter": "overlayfs",
        "source_date_epoch": SOURCE_DATE_EPOCH,
        "status": "matched",
    }
    _require(
        reproducibility == expected_reproducibility,
        "build_reproducibility_authentication",
    )


def _validate_gate_image_binding(
    evidence: object,
    build: dict[str, Any],
    code: str,
) -> None:
    _require(isinstance(evidence, dict), f"{code}_evidence")
    _require(
        evidence.get("oracle_build_contract_sha256")
        == build.get("build_contract_sha256")
        and evidence.get("oracle_image_config_digest")
        == build.get("built_image_id")
        and evidence.get("oracle_image_manifest_digest")
        == build.get("built_manifest_digest")
        and evidence.get("oracle_lock_file_sha256")
        == build.get("lock_file_sha256"),
        f"{code}_image_binding",
    )


def _validate_baseline_gate(
    gate: dict[str, Any],
    candidate: dict[str, Any],
    candidate_payload: bytes,
    reviewed_baseline_sha256: str,
) -> None:
    _require(gate.get("schema") == "rxls.render-parity-baseline-check.v1", "gate_schema")
    _require(gate.get("passed") is True and gate.get("failures") == [], "ratchet_failed")
    _require(gate.get("baseline_sha256") == reviewed_baseline_sha256, "baseline_identity")
    _require(
        gate.get("candidate_sha256") == _canonical_sha256(candidate),
        "candidate_identity",
    )
    _require(_sha256(candidate_payload) == _canonical_sha256(candidate), "candidate_encoding")
    campaign = gate.get("campaign")
    _require(isinstance(campaign, dict), "gate_campaign")
    _require(campaign.get("case_count") == 800, "gate_case_count")
    _require(campaign.get("kind") == "project_generated_hosted_full", "gate_kind")
    _require(_hash_matches(campaign.get("manifest_sha256")), "gate_manifest_identity")
    warning_policy = gate.get("warning_policy")
    _require(isinstance(warning_policy, dict), "warning_policy")
    _require(warning_policy.get("unclassified_codes") == [], "unreviewed_warning")
    reviewed_count = warning_policy.get("reviewed_code_count")
    candidate_count = warning_policy.get("candidate_code_count")
    _require(
        isinstance(reviewed_count, int)
        and isinstance(candidate_count, int)
        and reviewed_count >= candidate_count >= 0,
        "warning_policy_counts",
    )


def _validate_candidate(candidate: dict[str, Any]) -> dict[str, Any]:
    _require(candidate.get("schema") == "rxls.render-parity-baseline.v2", "candidate_schema")
    _require(candidate.get("input_files") == 800, "candidate_case_count")
    campaign = candidate.get("campaign")
    _require(isinstance(campaign, dict), "candidate_campaign")
    _require(campaign.get("schema") == "rxls.render-parity-campaign.v1", "campaign_schema")
    _require(campaign.get("kind") == "project_generated_hosted_full", "campaign_kind")
    _require(campaign.get("profile") == "full", "campaign_profile")
    _require(campaign.get("case_count") == 800, "campaign_case_count")
    _require(
        campaign.get("format_counts")
        == {"ods": 200, "xls": 200, "xlsb": 200, "xlsx": 200},
        "campaign_format_counts",
    )
    _require(_hash_matches(campaign.get("manifest_sha256")), "campaign_manifest")
    _require(
        campaign.get("input_set_sha256") == candidate.get("input_set_sha256")
        and _hash_matches(candidate.get("input_set_sha256")),
        "campaign_input_identity",
    )
    _require(isinstance(candidate.get("warning_counts"), dict), "candidate_warnings")
    return campaign


def validate(
    artifact_dir: Path,
    head_sha: str,
    reviewed_baseline: Path,
    *,
    workflow_run_id: int | None = None,
    workflow_run_attempt: int | None = None,
    artifact_id: int | None = None,
    artifact_name: str | None = None,
    artifact_size_bytes: int | None = None,
    artifact_digest: str | None = None,
    artifact_repository: str | None = None,
    oracle_lock: Path = DEFAULT_ORACLE_LOCK,
    oracle_wrapper: Path = DEFAULT_ORACLE_WRAPPER,
) -> dict[str, object]:
    _require(HEAD_SHA_RE.fullmatch(head_sha) is not None, "head_sha")
    artifact_binding = (
        workflow_run_id,
        workflow_run_attempt,
        artifact_id,
        artifact_name,
        artifact_size_bytes,
        artifact_digest,
        artifact_repository,
    )
    if any(value is not None for value in artifact_binding):
        _require(
            all(value is not None for value in artifact_binding),
            "artifact_binding_incomplete",
        )
        _validate_artifact_binding(
            head_sha=head_sha,
            workflow_run_id=workflow_run_id,
            workflow_run_attempt=workflow_run_attempt,
            artifact_id=artifact_id,
            artifact_name=artifact_name,
            artifact_size_bytes=artifact_size_bytes,
            artifact_digest=artifact_digest,
            repository=artifact_repository,
        )
    try:
        artifact_metadata = artifact_dir.lstat()
    except OSError as error:
        raise EvidenceError("artifact_directory") from error
    _require(
        stat.S_ISDIR(artifact_metadata.st_mode) and not artifact_dir.is_symlink(),
        "artifact_directory",
    )
    artifact_dir = artifact_dir.resolve()
    members = list(artifact_dir.iterdir())
    _require(all(item.is_file() and not item.is_symlink() for item in members), "artifact_member_type")
    _require({item.name for item in members} == EXPECTED_FILES, "artifact_file_set")
    _require(
        sum(item.stat().st_size for item in members) <= MAX_TOTAL_BYTES,
        "artifact_total_size",
    )

    documents: dict[str, dict[str, Any]] = {}
    payloads: dict[str, bytes] = {}
    for name in sorted(EXPECTED_FILES):
        document, payload = _read_json(artifact_dir / name)
        _path_neutral(document)
        documents[name] = document
        payloads[name] = payload

    reviewed, _ = _read_json(reviewed_baseline)
    _require(reviewed.get("schema") == "rxls.render-parity-baseline.v2", "reviewed_schema")
    reviewed_sha256 = _canonical_sha256(reviewed)
    contract = _release_contract(oracle_lock, oracle_wrapper)

    candidates = []
    gates = []
    for label in ("a", "b"):
        candidate = documents[f"baseline-candidate-{label}.json"]
        campaign = _validate_candidate(candidate)
        gate = documents[f"baseline-gate-{label}.json"]
        _validate_baseline_gate(
            gate,
            candidate,
            payloads[f"baseline-candidate-{label}.json"],
            reviewed_sha256,
        )
        _require(
            gate["campaign"]["manifest_sha256"] == campaign["manifest_sha256"],
            "gate_campaign_identity",
        )
        candidates.append(candidate)
        gates.append(gate)
    _require(candidates[0]["campaign"] == candidates[1]["campaign"], "campaign_repeatability")
    _require(candidates[0]["warning_counts"] == candidates[1]["warning_counts"], "warning_repeatability")

    fidelities = [documents["fidelity-a.json"], documents["fidelity-b.json"]]
    for fidelity in fidelities:
        _require(fidelity.get("schema") == "rxls.render-fidelity-targets.v1", "fidelity_schema")
        _require(fidelity.get("passed") is True and fidelity.get("failures") == [], "fidelity_failed")

    authored = documents["authored-print-gate.json"]
    _require(authored.get("schema") == "rxls.authored-print-parity.v1", "authored_schema")
    _require(authored.get("passed") is True and authored.get("failures") == [], "authored_failed")
    _require(
        authored.get("coverage", {}).get("workbooks") == 100
        and authored.get("coverage", {}).get("pages") == 400,
        "authored_coverage",
    )

    repeatability = documents["repeatability.json"]
    _require(
        repeatability.get("schema") == "rxls.libreoffice-render-repeatability.v1",
        "repeatability_schema",
    )
    _require(
        repeatability.get("status") == "pass" and repeatability.get("failures") == [],
        "repeatability_failed",
    )
    _require(
        repeatability.get("coverage", {}).get("workbooks") == 800,
        "repeatability_coverage",
    )

    build = documents["build.json"]
    _validate_build(build, head_sha, contract)
    for fidelity in fidelities:
        _validate_gate_image_binding(
            fidelity.get("evidence"),
            build,
            "fidelity",
        )
    _validate_gate_image_binding(authored.get("evidence"), build, "authored")
    host_tools = documents["host-tools.json"]
    _require(host_tools.get("identity_status") == "pinned_match", "host_identity")
    _require(
        host_tools.get("captured_identity_sha256")
        == host_tools.get("expected_identity_sha256"),
        "host_identity_mismatch",
    )
    renderer = documents["renderer.json"]
    _require(_hash_matches(renderer.get("sha256")), "renderer_identity")

    summary = documents["hosted-summary.json"]
    _require(summary.get("schema") == HOSTED_CAMPAIGN_SCHEMA, "summary_schema")
    _require(summary.get("head_sha") == head_sha, "summary_head_sha")
    campaign = summary.get("campaign", {})
    _require(
        campaign.get("mode") == "full"
        and campaign.get("case_count") == 800
        and campaign.get("repetitions") == 2
        and campaign.get("shard_count") == 4
        and campaign.get("parallel_shards") == 2,
        "summary_campaign",
    )
    shard_counts = campaign.get("shard_case_counts")
    _require(
        isinstance(shard_counts, list)
        and len(shard_counts) == 4
        and sum(shard_counts) == 800
        and all(isinstance(count, int) and 180 <= count <= 220 for count in shard_counts),
        "summary_shards",
    )
    _require(
        summary.get("summary", {}).get("files") == 800
        and summary.get("summary", {}).get("by_status") == {"compared": 800},
        "summary_coverage",
    )
    _require(
        summary.get("corpus", {}).get("profile") == "full"
        and summary.get("corpus", {}).get("case_count") == 800
        and summary.get("corpus", {}).get("rights_tier") == "S"
        and summary.get("corpus", {}).get("redistribution") == "allowed",
        "summary_corpus",
    )
    _require(summary.get("renderer") == renderer, "summary_renderer")
    _require(summary.get("host_tools") == host_tools, "summary_host_tools")
    container = summary.get("container")
    _require(
        isinstance(container, dict)
        and set(container)
        == {
            "build_contract_sha256",
            "expected_image_id",
            "expected_manifest_digest",
            "identity_status",
            "image_id",
            "lock_file_sha256",
            "manifest_digest",
            "oracle_artifact_sha256",
            "oracle_version",
            "source_commit",
            "wrapper_sha256",
        },
        "summary_container_schema",
    )
    _require(
        container.get("identity_status") == "pinned_match"
        and container.get("image_id")
        == container.get("expected_image_id")
        == build.get("built_image_id")
        and container.get("manifest_digest")
        == container.get("expected_manifest_digest")
        == build.get("built_manifest_digest")
        and container.get("build_contract_sha256")
        == build.get("build_contract_sha256")
        and container.get("lock_file_sha256") == build.get("lock_file_sha256")
        and container.get("source_commit") == build.get("source_commit") == head_sha
        and container.get("wrapper_sha256") == build.get("wrapper_sha256")
        and _hash_matches(container.get("oracle_artifact_sha256"))
        and container.get("oracle_version") == "26.2.3.2",
        "summary_container",
    )

    baseline_summary = summary.get("baseline_ratcheting")
    _require(isinstance(baseline_summary, dict), "summary_baseline")
    _require(
        baseline_summary.get("applies") is True
        and baseline_summary.get("passed") is True
        and baseline_summary.get("reviewed_baseline_available") is True,
        "summary_ratchet",
    )
    summary_gates = baseline_summary.get("gates")
    summary_candidates = baseline_summary.get("candidate_baselines")
    _require(
        isinstance(summary_gates, list)
        and len(summary_gates) == 2
        and isinstance(summary_candidates, list)
        and len(summary_candidates) == 2,
        "summary_ratchet_runs",
    )
    for index, label in enumerate(("a", "b")):
        gate = gates[index]
        candidate = candidates[index]
        expected_gate_summary = {
            "baseline_sha256": gate["baseline_sha256"],
            "candidate_sha256": gate["candidate_sha256"],
            "failures": gate["failures"],
            "passed": gate["passed"],
            "sha256": _sha256(payloads[f"baseline-gate-{label}.json"]),
            "warning_policy": gate["warning_policy"],
        }
        _require(summary_gates[index] == expected_gate_summary, "summary_gate_identity")
        _require(
            summary_candidates[index].get("sha256")
            == _sha256(payloads[f"baseline-candidate-{label}.json"])
            and summary_candidates[index].get("campaign_sha256")
            == _canonical_sha256(candidate["campaign"])
            and summary_candidates[index].get("warning_counts")
            == candidate["warning_counts"],
            "summary_candidate_identity",
        )
    _require(
        baseline_summary.get("reviewed_warning_policy") == gates[0]["warning_policy"]
        == gates[1]["warning_policy"],
        "summary_warning_policy",
    )

    evidence_runs = summary.get("evidence_runs")
    _require(isinstance(evidence_runs, list) and len(evidence_runs) == 2, "summary_evidence_runs")
    for index, label in enumerate(("a", "b")):
        _require(
            evidence_runs[index].get("fidelity_gate_sha256")
            == _sha256(payloads[f"fidelity-{label}.json"])
            and _hash_matches(evidence_runs[index].get("report_sha256"))
            and isinstance(evidence_runs[index].get("report_bytes"), int)
            and evidence_runs[index]["report_bytes"] > 0,
            "summary_fidelity_identity",
        )
        expected_fidelity = {
            key: fidelities[index][key]
            for key in ("coverage", "metrics", "passed", "thresholds")
        }
        _require(summary.get("fidelity", [])[index] == expected_fidelity, "summary_fidelity")
    expected_authored = {
        key: authored[key]
        for key in ("coverage", "evidence", "expected", "metrics", "passed", "thresholds")
    }
    expected_authored["sha256"] = _sha256(payloads["authored-print-gate.json"])
    _require(summary.get("authored_print") == expected_authored, "summary_authored")
    expected_repeatability = {
        key: repeatability[key]
        for key in ("coverage", "status", "thresholds_ppm")
    }
    expected_repeatability["sha256"] = _sha256(payloads["repeatability.json"])
    _require(summary.get("repeatability") == expected_repeatability, "summary_repeatability")

    report: dict[str, object] = {
        "schema": "rxls.render-worker-release-prerequisites.v1",
        "bootstrap_source_commit": contract["bootstrap_source_commit"],
        "build_contract_sha256": build["build_contract_sha256"],
        "head_sha": head_sha,
        "full_cases": 800,
        "lock_file_sha256": build["lock_file_sha256"],
        "oracle_config_digest": build["built_image_id"],
        "oracle_manifest_digest": build["built_manifest_digest"],
        "ratchets": 2,
        "reviewed_baseline_sha256": reviewed_sha256,
        "source_commit": build["source_commit"],
        "wrapper_sha256": build["wrapper_sha256"],
        "passed": True,
    }
    if workflow_run_id is not None:
        report.update(
            {
                "artifact_digest": artifact_digest,
                "artifact_id": artifact_id,
                "artifact_name": artifact_name,
                "artifact_repository": artifact_repository,
                "artifact_size_bytes": artifact_size_bytes,
                "workflow_run_attempt": workflow_run_attempt,
                "workflow_run_id": workflow_run_id,
            }
        )
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--download-repository", required=True)
    parser.add_argument("--github-artifact-id", type=int, required=True)
    parser.add_argument("--artifact-name", required=True)
    parser.add_argument("--artifact-size-bytes", type=int, required=True)
    parser.add_argument("--head-sha", required=True)
    parser.add_argument("--reviewed-baseline", type=Path, required=True)
    parser.add_argument("--workflow-run-id", type=int, required=True)
    parser.add_argument("--workflow-run-attempt", type=int, required=True)
    parser.add_argument("--artifact-digest", required=True)
    parser.add_argument("--write-report", type=Path)
    args = parser.parse_args()
    try:
        _validate_artifact_binding(
            head_sha=args.head_sha,
            workflow_run_id=args.workflow_run_id,
            workflow_run_attempt=args.workflow_run_attempt,
            artifact_id=args.github_artifact_id,
            artifact_name=args.artifact_name,
            artifact_size_bytes=args.artifact_size_bytes,
            artifact_digest=args.artifact_digest,
            repository=args.download_repository,
        )
        with tempfile.TemporaryDirectory(
            prefix="rxls-render-oracle-release-"
        ) as temporary:
            temporary_root = Path(temporary)
            archive_path = temporary_root / "artifact.zip"
            artifact_dir = temporary_root / "artifact"
            download_artifact_archive(
                args.download_repository,
                args.github_artifact_id,
                archive_path,
                args.artifact_size_bytes,
                args.artifact_digest,
            )
            extract_authenticated_artifact(
                archive_path,
                artifact_dir,
                args.artifact_size_bytes,
                args.artifact_digest,
            )
            report = validate(
                artifact_dir,
                args.head_sha,
                args.reviewed_baseline,
                workflow_run_id=args.workflow_run_id,
                workflow_run_attempt=args.workflow_run_attempt,
                artifact_id=args.github_artifact_id,
                artifact_name=args.artifact_name,
                artifact_size_bytes=args.artifact_size_bytes,
                artifact_digest=args.artifact_digest,
                artifact_repository=args.download_repository,
            )
        if args.write_report is not None:
            args.write_report.parent.mkdir(parents=True, exist_ok=True)
            args.write_report.write_text(
                json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
    except (EvidenceError, OSError) as error:
        print(f"render release prerequisites: {error}", file=sys.stderr)
        return 1
    print(
        "render release prerequisites: "
        f"head_sha={report['head_sha']} full_cases=800 ratchets=2 passed=true"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
