#!/usr/bin/env python3
"""Validate full, exact-SHA Render Oracle evidence before npm publication."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import stat
import subprocess
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

try:
    from strict_json_contract import type_exact_equal
except ModuleNotFoundError:  # Imported as ``scripts.*`` by unit tests.
    from scripts.strict_json_contract import type_exact_equal


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_ORACLE_LOCK = ROOT / "scripts" / "render-oracle-container" / "lock.json"
DEFAULT_ORACLE_WRAPPER = ROOT / "scripts" / "run-render-oracle-container.py"
DEFAULT_REVIEWED_BASELINE = ROOT / "scripts" / "render-parity-baseline-full.json"
BASELINE_CHECKER = ROOT / "scripts" / "check-render-parity-baseline.py"
FAILURE_SUMMARIZER = (
    ROOT / "scripts" / "summarize-render-oracle-failure.py"
)
FAILURE_SUMMARY_NAME = "render-oracle-failure-summary.json"
FAILURE_SUMMARY_SCHEMA = "rxls.render-oracle-failure-summary.v10"
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
MAX_FAILURE_SUMMARY_BYTES = 2 * 1024 * 1024
MAX_LOCK_BYTES = 256 * 1024
MAX_WRAPPER_BYTES = 512 * 1024
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_JSON_INTEGER_DIGITS = 128
DOWNLOAD_CHUNK_BYTES = 1024 * 1024
DOWNLOAD_TIMEOUT_SECONDS = 60
EXPECTED_REPOSITORY = "HyunjoJung/rxls"
EXPECTED_REPOSITORY_ID = 1_297_467_060
EXPECTED_HOSTED_FULL_MANIFEST_SHA256 = (
    "5c6466a53e4328bb50f04cd3c63d102bf53da1a6b3478380f3724574c31b248d"
)
EXPECTED_HOSTED_FULL_INPUT_SET_SHA256 = (
    "45dfaaac5e94e98da038c561d98eed48e8785f56749760d39bac8a720b132db9"
)
EXPECTED_HOSTED_FULL_BINDING_INPUT_SET_SHA256 = (
    "0ed4f623a243da0b3bee6f6a5d05359fca2e5b7ce51c79e399f0a720a10ebd89"
)
EXPECTED_HOSTED_FULL_GROUP_TOPOLOGY_SHA256 = (
    "559cf641df08738419af941f30c35a831ca9d000e85ab1e5753c391486f0d251"
)
GITHUB_API_VERSION = "2022-11-28"
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
WARNING_CODE_RE = re.compile(r"^[a-z][a-z0-9_]{0,63}$")
HEAD_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
IMAGE_DIGEST_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
ARTIFACT_EXTENSION_RE = re.compile(
    r"\.(?:xls|xlsx|xlsb|xlsm|ods|fods|pdf|png|svg)\Z",
    re.IGNORECASE,
)
PATH_TRAVERSAL_RE = re.compile(r"(?:^|[\\/])\.\.(?:$|[\\/])")
SECRET_TEXT_RE = re.compile(
    r"(?:"
    r"gh[pousr]_[A-Za-z0-9_]{8,}"
    r"|github_pat_[A-Za-z0-9_]{8,}"
    r"|(?:AKIA|ASIA)[A-Z0-9]{12,}"
    r"|xox[baprs]-[A-Za-z0-9-]{8,}"
    r")",
    re.IGNORECASE,
)
BUILD_SCHEMA = "rxls.render-oracle-container-build.v3"
LOCK_SCHEMA = "rxls.render-oracle-container-lock.v3"
BOOTSTRAP_RECEIPT_SCHEMA = "rxls.render-oracle-bootstrap-receipt.v1"
HOSTED_CAMPAIGN_SCHEMA = "rxls.render-oracle-hosted-campaign.v7"
ADOPTION_RECEIPT_SCHEMA = "rxls.render-parity-baseline-adoption.v1"
MAX_GITHUB_API_BYTES = 4 * 1024 * 1024
MAX_SOURCE_REPORT_BYTES = 256 * 1024 * 1024
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
EXPECTED_FORMAT_COUNTS = {
    "ods": 200,
    "xls": 200,
    "xlsb": 200,
    "xlsx": 200,
}
EXPECTED_FEATURE_COUNTS = {
    "border": 200,
    "cell-fill": 200,
    "chart": 100,
    "chinese-text": 400,
    "column-width": 400,
    "conditional-format": 100,
    "date-format": 400,
    "formula-cached": 400,
    "hidden-column": 400,
    "hidden-row": 400,
    "image-drawing": 100,
    "japanese-text": 400,
    "korean-text": 416,
    "latin-text": 800,
    "merged-cells": 400,
    "noto-ofl-font": 600,
    "number-cell": 800,
    "percent-format": 400,
    "print-settings": 400,
    "right-to-left-layout": 200,
    "row-height": 400,
    "rtl-text": 400,
    "sparkline": 100,
    "unicode-text": 752,
    "wrapped-text": 200,
}
EXPECTED_HARD_FEATURE_COUNTS = {
    "chart": 100,
    "conditional_format": 100,
    "image_drawing": 100,
    "print_settings": 400,
    "rtl": 500,
    "sparkline": 100,
    "wrapped_text": 200,
}
EXPECTED_HARD_FEATURE_COHORTS = {
    "chart": ["chart"],
    "conditional_format": ["conditional-format"],
    "image_drawing": ["image-drawing"],
    "print_settings": ["print-settings"],
    "rtl": ["right-to-left-layout", "rtl-text"],
    "sparkline": ["sparkline"],
    "wrapped_text": ["wrapped-text"],
}
EXPECTED_CORE_EXCLUDED_FEATURES = [
    "chart",
    "conditional-format",
    "image-drawing",
    "print-settings",
    "right-to-left-layout",
    "rtl-text",
    "sparkline",
    "wrapped-text",
]
EXPECTED_FIDELITY_THRESHOLDS = {
    "broad_similarity_min_ppm": 950_000,
    "core_similarity_min_ppm": 980_000,
    "edge_f1_min_ppm": 970_000,
    "page_box_max_millipoints": 5_000,
    "page_box_median_max_millipoints": 1_000,
    "page_box_p95_max_millipoints": 2_500,
    "pdf_imported_page_box_quantization_max_micropoints": 15_000,
    "pdf_xhtml_crosscheck_max_micropoints": 1_000,
    "semantic_codepoint_precision_min_ppm": 999_000,
    "semantic_codepoint_recall_min_ppm": 999_000,
    "text_box_match_min_ppm": 999_000,
    "text_box_median_max_millipoints": 1_000,
    "text_box_p95_max_millipoints": 2_500,
}
EXPECTED_FIDELITY_POLICY = {
    "core_excluded_features": EXPECTED_CORE_EXCLUDED_FEATURES,
    "hard_feature_cohorts": EXPECTED_HARD_FEATURE_COHORTS,
    "minimum_broad_workbooks": 40,
    "minimum_core_text_boxes": 100,
    "minimum_core_workbooks": 10,
    "minimum_hard_feature_workbooks": 1,
    "oracle_formats": ["ods", "xls", "xlsb", "xlsx"],
}
EXPECTED_AUTHORED_THRESHOLDS = {
    "edge_f1_min_ppm": 970_000,
    "page_box_max_millipoints": 5_000,
    "page_box_median_max_millipoints": 1_000,
    "page_box_p95_max_millipoints": 2_500,
    "pdf_point_geometry_exact": True,
    "pdf_xhtml_crosscheck_max_micropoints": 1_000,
    "semantic_codepoint_precision_min_ppm": 999_000,
    "semantic_codepoint_recall_min_ppm": 999_000,
    "similarity_mean_min_ppm": 950_000,
    "text_box_match_min_ppm": 999_000,
    "text_box_median_max_millipoints": 1_000,
    "text_box_p95_max_millipoints": 2_500,
}
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


class _StrictJSONError(ValueError):
    pass


def _require(condition: bool, code: str) -> None:
    if not condition:
        raise EvidenceError(code)


def _load_baseline_checker() -> Any:
    module_name = "_rxls_render_parity_baseline"
    existing = sys.modules.get(module_name)
    if existing is not None:
        return existing
    spec = importlib.util.spec_from_file_location(module_name, BASELINE_CHECKER)
    _require(spec is not None and spec.loader is not None, "baseline_checker_import")
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    try:
        spec.loader.exec_module(module)
    except Exception as error:
        sys.modules.pop(module_name, None)
        raise EvidenceError("baseline_checker_import") from error
    return module


def _load_failure_summarizer() -> Any:
    module_name = "_rxls_render_oracle_failure_summary"
    existing = sys.modules.get(module_name)
    if existing is not None:
        return existing
    spec = importlib.util.spec_from_file_location(
        module_name, FAILURE_SUMMARIZER
    )
    _require(
        spec is not None and spec.loader is not None,
        "failure_summary_checker_import",
    )
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    try:
        spec.loader.exec_module(module)
    except Exception as error:
        sys.modules.pop(module_name, None)
        raise EvidenceError(
            "failure_summary_checker_import"
        ) from error
    return module


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
    campaign: str,
    baseline_mode: str,
    workflow_run_id: int,
    workflow_run_attempt: int,
    artifact_id: int,
    artifact_name: str,
    artifact_size_bytes: int,
    artifact_digest: str,
    repository: str,
) -> None:
    _require(repository == EXPECTED_REPOSITORY, "artifact_repository")
    _require(campaign == "full", "artifact_campaign")
    _require(baseline_mode in {"candidate", "verify"}, "artifact_baseline_mode")
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
            f"{workflow_run_attempt}-{campaign}-{baseline_mode}"
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
    raise _StrictJSONError(f"invalid_json_constant:{value}")


def _reject_json_number(_value: str) -> object:
    raise _StrictJSONError("non_integral_number")


def _parse_json_integer(token: str) -> int:
    digits = token[1:] if token.startswith("-") else token
    if len(digits) > MAX_JSON_INTEGER_DIGITS:
        raise _StrictJSONError("integer_limit")
    return int(token)


def _preflight_json_text(text: str) -> None:
    closers: list[str] = []
    structural_nodes = 0
    index = 0
    while index < len(text):
        character = text[index]
        if character == '"':
            index += 1
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                    continue
                if text[index] == '"':
                    index += 1
                    break
                index += 1
            continue
        if character in "[{":
            structural_nodes += 1
            if structural_nodes > MAX_JSON_NODES:
                raise _StrictJSONError("json_complexity")
            closers.append("]" if character == "[" else "}")
            if len(closers) > MAX_JSON_DEPTH:
                raise _StrictJSONError("json_depth")
        elif character in "]}":
            if not closers or closers.pop() != character:
                raise _StrictJSONError("json_structure")
        elif character == ",":
            structural_nodes += 1
            if structural_nodes > MAX_JSON_NODES:
                raise _StrictJSONError("json_complexity")
        elif character == "-" or "0" <= character <= "9":
            start = index
            if character == "-":
                index += 1
            digit_start = index
            while index < len(text) and "0" <= text[index] <= "9":
                index += 1
            if index == digit_start:
                index = start + 1
                continue
            if index - digit_start > MAX_JSON_INTEGER_DIGITS:
                raise _StrictJSONError("integer_limit")
            if index < len(text) and text[index] in ".eE":
                raise _StrictJSONError("non_integral_number")
            continue
        index += 1
    if closers:
        raise _StrictJSONError("json_structure")


def _strict_json_loads(payload: bytes, code: str) -> object:
    try:
        text = payload.decode("utf-8")
        _preflight_json_text(text)
        return json.loads(
            text,
            object_pairs_hook=_object_pairs,
            parse_constant=_reject_json_constant,
            parse_float=_reject_json_number,
            parse_int=_parse_json_integer,
        )
    except EvidenceError:
        raise
    except (RecursionError, ValueError, UnicodeDecodeError) as error:
        raise EvidenceError(code) from error


def _github_api_json(
    repository: str,
    endpoint: str,
    token: str,
    *,
    opener: object | None = None,
) -> object:
    _require(repository == EXPECTED_REPOSITORY, "artifact_repository")
    _require(
        0 < len(token) <= 4096
        and all(0x21 <= ord(character) <= 0x7E for character in token),
        "github_token",
    )
    _require(
        endpoint.startswith("actions/")
        and endpoint.isascii()
        and not endpoint.startswith("/")
        and ".." not in endpoint
        and len(endpoint) <= 1024,
        "github_api_endpoint",
    )
    url = f"https://api.github.com/repos/{repository}/{endpoint}"
    request = Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": "rxls-render-oracle-baseline-adoption",
            "X-GitHub-Api-Version": GITHUB_API_VERSION,
        },
        method="GET",
    )
    api_opener = opener if opener is not None else build_opener(_NoRedirect())
    response: object | None = None
    try:
        response = api_opener.open(request, timeout=DOWNLOAD_TIMEOUT_SECONDS)
        _require(_response_status(response) == 200, "github_api_status")
        final_url_getter = getattr(response, "geturl", None)
        if callable(final_url_getter):
            _require(final_url_getter() == url, "github_api_final_url")
        content_encoding = response.headers.get("Content-Encoding")
        _require(
            content_encoding in (None, "", "identity"),
            "github_api_encoding",
        )
        content_length = response.headers.get("Content-Length")
        if content_length is not None:
            _require(
                re.fullmatch(r"[1-9][0-9]*", content_length) is not None
                and int(content_length) <= MAX_GITHUB_API_BYTES,
                "github_api_size",
            )
        payload = response.read(MAX_GITHUB_API_BYTES + 1)
        _require(
            isinstance(payload, bytes) and 0 < len(payload) <= MAX_GITHUB_API_BYTES,
            "github_api_size",
        )
        _require(response.read(1) == b"", "github_api_size")
    except HTTPError as error:
        error.close()
        raise EvidenceError("github_api_status") from error
    except (OSError, TimeoutError, URLError) as error:
        raise EvidenceError("github_api_request") from error
    finally:
        if response is not None:
            response.close()
    return _strict_json_loads(payload, "github_api_json")


def authenticate_candidate_run_artifact(
    *,
    repository: str,
    head_sha: str,
    workflow_run_id: int,
    workflow_run_attempt: int,
    artifact_id: int,
    artifact_name: str,
    artifact_size_bytes: int,
    artifact_digest: str,
    token: str | None = None,
    run_opener: object | None = None,
    artifacts_opener: object | None = None,
) -> dict[str, object]:
    """Live-authenticate the successful exact-SHA candidate run and artifact."""

    _validate_artifact_binding(
        head_sha=head_sha,
        campaign="full",
        baseline_mode="candidate",
        workflow_run_id=workflow_run_id,
        workflow_run_attempt=workflow_run_attempt,
        artifact_id=artifact_id,
        artifact_name=artifact_name,
        artifact_size_bytes=artifact_size_bytes,
        artifact_digest=artifact_digest,
        repository=repository,
    )
    credential = token if token is not None else os.environ.get("GH_TOKEN", "")
    run = _github_api_json(
        repository,
        f"actions/runs/{workflow_run_id}",
        credential,
        opener=run_opener,
    )
    _require(isinstance(run, dict), "github_run")
    run_repository = run.get("repository")
    _require(
        isinstance(run_repository, dict)
        and run_repository.get("id") == EXPECTED_REPOSITORY_ID
        and run_repository.get("full_name") == EXPECTED_REPOSITORY,
        "github_run_repository",
    )
    _require(
        _positive_int(run.get("id"))
        and run.get("id") == workflow_run_id
        and _positive_int(run.get("run_attempt"))
        and run.get("run_attempt") == workflow_run_attempt
        and run.get("head_sha") == head_sha
        and run.get("event") == "workflow_dispatch"
        and run.get("status") == "completed"
        and run.get("conclusion") == "success"
        and run.get("path")
        in {
            ".github/workflows/fuzz.yml",
            ".github/workflows/render-oracle.yml",
        },
        "github_run_identity",
    )

    artifact_listing = _github_api_json(
        repository,
        f"actions/runs/{workflow_run_id}/artifacts?per_page=100",
        credential,
        opener=artifacts_opener,
    )
    _require(
        isinstance(artifact_listing, dict)
        and set(artifact_listing) >= {"artifacts", "total_count"},
        "github_artifacts",
    )
    artifacts = artifact_listing.get("artifacts")
    total_count = artifact_listing.get("total_count")
    _require(
        type(total_count) is int
        and 0 <= total_count <= 100
        and isinstance(artifacts, list)
        and len(artifacts) == total_count,
        "github_artifacts_count",
    )
    matches = [
        row
        for row in artifacts
        if isinstance(row, dict) and row.get("name") == artifact_name
    ]
    _require(len(matches) == 1, "github_artifact_uniqueness")
    artifact = matches[0]
    _require(
        _positive_int(artifact.get("id"))
        and artifact.get("id") == artifact_id
        and artifact.get("expired") is False
        and _positive_int(
            artifact.get("size_in_bytes"),
            MAX_ARTIFACT_ARCHIVE_BYTES,
        )
        and artifact.get("size_in_bytes") == artifact_size_bytes
        and artifact.get("digest") == artifact_digest,
        "github_artifact_identity",
    )
    workflow_run = artifact.get("workflow_run")
    if workflow_run is not None:
        _require(
            isinstance(workflow_run, dict)
            and _positive_int(workflow_run.get("id"))
            and workflow_run.get("id") == workflow_run_id
            and workflow_run.get("head_sha") == head_sha,
            "github_artifact_run",
        )
    return {
        "artifact_digest": artifact_digest,
        "artifact_id": artifact_id,
        "artifact_name": artifact_name,
        "artifact_repository": repository,
        "artifact_size_bytes": artifact_size_bytes,
        "head_sha": head_sha,
        "workflow_path": run["path"],
        "workflow_run_attempt": workflow_run_attempt,
        "workflow_run_id": workflow_run_id,
    }


def _read_json(path: Path) -> tuple[dict[str, Any], bytes]:
    payload = _regular_file_payload(
        path,
        MAX_FILE_BYTES,
        "evidence_file",
    )
    document = _strict_json_loads(payload, "evidence_invalid_json")
    _require(isinstance(document, dict), "evidence_not_object")
    return document, payload


def _sha256(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def _canonical_sha256(value: object) -> str:
    payload = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")
    return _sha256(payload)


def _path_neutral(value: object) -> None:
    stack = [value]
    visited = 0
    while stack:
        item = stack.pop()
        visited += 1
        _require(visited <= MAX_JSON_NODES, "json_complexity")
        if isinstance(item, dict):
            for key, child in item.items():
                _require(isinstance(key, str), "path_bearing_key")
                normalized_key = re.sub(r"[^a-z0-9]", "", key.lower())
                if key == "paths_or_content_retained":
                    _require(child is False, "path_retention_attestation")
                    continue
                _require(
                    "path" not in normalized_key,
                    "path_bearing_key",
                )
                stack.append(key)
                stack.append(child)
        elif isinstance(item, list):
            stack.extend(item)
        elif isinstance(item, str):
            lowered = item.lower()
            _require(
                len(item) <= 16_384
                and not any(
                    character < " " or character == "\x7f"
                    for character in item
                ),
                "unsafe_text",
            )
            _require(SECRET_TEXT_RE.search(item) is None, "secret_text")
            _require(not item.startswith("/"), "absolute_path")
            _require(re.match(r"^[A-Za-z]:[\\/]", item) is None, "windows_path")
            _require(not lowered.startswith("file://"), "file_uri")
            _require("\\" not in item, "backslash_path")
            _require(PATH_TRAVERSAL_RE.search(item) is None, "path_traversal")
            _require(
                ARTIFACT_EXTENSION_RE.search(item) is None,
                "relative_artifact_path",
            )
            _require("local/render-corpus" not in lowered, "corpus_path")
            _require("render-corpus-generated" not in lowered, "corpus_path")
            _require("payload/" not in lowered, "payload_path")


def _git(
    repository_root: Path,
    arguments: list[str],
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            ["git", *arguments],
            cwd=repository_root,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise EvidenceError("adoption_git") from error


def validate_adoption_checkout(
    destination: Path,
    head_sha: str,
    *,
    repository_root: Path = ROOT,
) -> Path:
    """Require an exact, clean candidate checkout and absent canonical output."""

    _require(HEAD_SHA_RE.fullmatch(head_sha) is not None, "adoption_head_sha")
    try:
        root = repository_root.resolve(strict=True)
        expected = (root / "scripts" / "render-parity-baseline-full.json")
        provided = destination.expanduser()
        if not provided.is_absolute():
            provided = Path.cwd() / provided
        provided_absolute = provided.absolute()
        try:
            provided_metadata = provided_absolute.lstat()
        except FileNotFoundError:
            provided_metadata = None
        _require(provided_metadata is None, "adoption_destination_exists")
        canonical = provided_absolute.resolve(strict=False)
        _require(canonical == expected, "adoption_destination")
        parent_metadata = expected.parent.lstat()
        _require(
            stat.S_ISDIR(parent_metadata.st_mode)
            and not expected.parent.is_symlink(),
            "adoption_destination_parent",
        )
    except OSError as error:
        raise EvidenceError("adoption_destination") from error

    top_level = _git(root, ["rev-parse", "--show-toplevel"])
    _require(
        top_level.returncode == 0
        and Path(top_level.stdout.strip()).resolve(strict=True) == root,
        "adoption_repository",
    )
    revision = _git(root, ["rev-parse", "--verify", "HEAD"])
    _require(
        revision.returncode == 0 and revision.stdout.strip() == head_sha,
        "adoption_checkout_head",
    )
    status = _git(root, ["status", "--porcelain=v1", "--untracked-files=all"])
    _require(
        status.returncode == 0 and status.stdout == "",
        "adoption_checkout_dirty",
    )
    relative = expected.relative_to(root).as_posix()
    tracked = _git(root, ["ls-files", "--error-unmatch", "--", relative])
    _require(
        tracked.returncode == 1 and tracked.stdout == "",
        "adoption_previous_baseline",
    )
    return expected


def _fsync_directory(path: Path) -> None:
    flags = os.O_RDONLY
    if hasattr(os, "O_DIRECTORY"):
        flags |= os.O_DIRECTORY
    descriptor = os.open(path, flags)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _remove_exact_new_file(path: Path, payload: bytes) -> None:
    metadata = path.lstat()
    _require(
        stat.S_ISREG(metadata.st_mode)
        and not path.is_symlink()
        and path.read_bytes() == payload,
        "adoption_rollback_identity",
    )
    path.unlink()
    _fsync_directory(path.parent)


def write_new_atomic(path: Path, payload: bytes) -> None:
    """Atomically publish one new regular file without clobbering any entry."""

    _require(isinstance(payload, bytes) and bool(payload), "adoption_payload")
    temporary: Path | None = None
    linked = False
    interrupted = False
    try:
        parent_metadata = path.parent.lstat()
        _require(
            stat.S_ISDIR(parent_metadata.st_mode) and not path.parent.is_symlink(),
            "adoption_destination_parent",
        )
        try:
            path.lstat()
        except FileNotFoundError:
            pass
        else:
            raise EvidenceError("adoption_destination_exists")
        descriptor, temporary_name = tempfile.mkstemp(
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
        )
        temporary = Path(temporary_name)
        with os.fdopen(descriptor, "wb") as output:
            output.write(payload)
            output.flush()
            os.fchmod(output.fileno(), 0o644)
            os.fsync(output.fileno())
        os.link(temporary, path, follow_symlinks=False)
        linked = True
        _fsync_directory(path.parent)
    except BaseException as error:
        interrupted = isinstance(error, (KeyboardInterrupt, SystemExit))
        if linked:
            try:
                _remove_exact_new_file(path, payload)
            except (EvidenceError, OSError) as rollback_error:
                if interrupted:
                    raise error
                raise EvidenceError("adoption_rollback") from rollback_error
        if isinstance(error, EvidenceError):
            raise
        if isinstance(error, FileExistsError):
            raise EvidenceError("adoption_destination_exists") from error
        if isinstance(error, OSError):
            raise EvidenceError("adoption_write") from error
        raise
    finally:
        if temporary is not None:
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass
            except OSError as error:
                if not interrupted:
                    raise EvidenceError("adoption_cleanup") from error


def write_adoption_pair_atomic(
    baseline_path: Path,
    baseline_payload: bytes,
    receipt_path: Path,
    receipt_payload: bytes,
) -> None:
    """Install a receipt before its baseline and roll it back on baseline failure."""

    _require(
        isinstance(baseline_payload, bytes)
        and bool(baseline_payload)
        and isinstance(receipt_payload, bytes)
        and bool(receipt_payload),
        "adoption_payload",
    )
    _require(
        baseline_path.resolve(strict=False) != receipt_path.resolve(strict=False),
        "adoption_receipt_destination",
    )
    receipt_installed = False
    try:
        write_new_atomic(receipt_path, receipt_payload)
        receipt_installed = True
        write_new_atomic(baseline_path, baseline_payload)
    except BaseException as error:
        interrupted = isinstance(error, (KeyboardInterrupt, SystemExit))
        if receipt_installed:
            try:
                _remove_exact_new_file(receipt_path, receipt_payload)
            except (EvidenceError, OSError) as rollback_error:
                if interrupted:
                    raise error
                raise EvidenceError("adoption_rollback") from rollback_error
        if isinstance(error, EvidenceError):
            raise
        if isinstance(error, OSError):
            raise EvidenceError("adoption_write") from error
        raise


def write_atomic(path: Path, payload: bytes) -> None:
    """Atomically replace a diagnostic output after all validation passes."""

    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        descriptor, temporary_name = tempfile.mkstemp(
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
        )
        temporary = Path(temporary_name)
        try:
            with os.fdopen(descriptor, "wb") as output:
                output.write(payload)
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, path)
        finally:
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass
    except OSError as error:
        raise EvidenceError("report_write") from error


def _hash_matches(value: object) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _nonzero_hash_matches(value: object) -> bool:
    return _hash_matches(value) and value != "0" * 64


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
    descriptor = -1
    try:
        metadata = path.lstat()
        _require(stat.S_ISREG(metadata.st_mode) and not path.is_symlink(), f"{code}_type")
        _require(0 < metadata.st_size <= maximum, f"{code}_size")
        flags = os.O_RDONLY
        flags |= getattr(os, "O_CLOEXEC", 0)
        flags |= getattr(os, "O_NOFOLLOW", 0)
        flags |= getattr(os, "O_NONBLOCK", 0)
        descriptor = os.open(path, flags)
        with os.fdopen(descriptor, "rb") as source:
            descriptor = -1
            opened = os.fstat(source.fileno())
            _require(
                stat.S_ISREG(opened.st_mode)
                and (opened.st_dev, opened.st_ino)
                == (metadata.st_dev, metadata.st_ino)
                and opened.st_size == metadata.st_size,
                f"{code}_changed",
            )
            payload = source.read(maximum + 1)
        final = path.lstat()
        _require(
            stat.S_ISREG(final.st_mode)
            and not path.is_symlink()
            and (final.st_dev, final.st_ino, final.st_size)
            == (opened.st_dev, opened.st_ino, opened.st_size),
            f"{code}_changed",
        )
    except EvidenceError:
        raise
    except OSError as error:
        raise EvidenceError(f"{code}_unreadable") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    _require(
        len(payload) == metadata.st_size and len(payload) <= maximum,
        f"{code}_changed",
    )
    return payload


def validate_failure_summary(
    path: Path,
    *,
    head_sha: str,
    profile: str,
    baseline_mode: str,
) -> dict[str, Any]:
    """Validate a diagnostic artifact without treating it as release evidence."""

    _require(
        path.name == FAILURE_SUMMARY_NAME,
        "failure_summary_name",
    )
    _require(
        HEAD_SHA_RE.fullmatch(head_sha) is not None,
        "failure_summary_head_sha",
    )
    _require(
        profile in {"pilot", "full", "ooxml-row-diagnostic"},
        "failure_summary_profile",
    )
    _require(
        baseline_mode in {"candidate", "verify"}
        and (profile == "full" or baseline_mode == "verify"),
        "failure_summary_baseline_mode",
    )
    payload = _regular_file_payload(
        path,
        MAX_FAILURE_SUMMARY_BYTES,
        "failure_summary",
    )
    value = _strict_json_loads(payload, "failure_summary_json")
    _require(
        isinstance(value, dict),
        "failure_summary_schema",
    )
    summarizer = _load_failure_summarizer()
    try:
        summarizer._validate_output(value)
    except Exception as error:
        raise EvidenceError("failure_summary_schema") from error
    _require(
        value.get("schema") == FAILURE_SUMMARY_SCHEMA
        and value.get("head_sha") == head_sha
        and value.get("profile") == profile
        and value.get("baseline_mode") == baseline_mode,
        "failure_summary_binding",
    )
    _require(
        payload == summarizer._json(value),
        "failure_summary_canonical",
    )
    stack = [value]
    visited = 0
    while stack:
        item = stack.pop()
        visited += 1
        _require(
            visited <= MAX_JSON_NODES,
            "failure_summary_complexity",
        )
        if isinstance(item, dict):
            for key, child in item.items():
                _require(
                    isinstance(key, str),
                    "failure_summary_unsafe_text",
                )
                stack.append(key)
                stack.append(child)
        elif isinstance(item, list):
            stack.extend(item)
        elif isinstance(item, str):
            lowered = item.lower()
            _require(
                len(item) <= 16_384
                and not any(
                    character < " " or character == "\x7f"
                    for character in item
                )
                and SECRET_TEXT_RE.search(item) is None
                and not item.startswith("/")
                and re.match(r"^[A-Za-z]:[\\/]", item) is None
                and "://" not in lowered
                and "\\" not in item
                and PATH_TRAVERSAL_RE.search(item) is None
                and ARTIFACT_EXTENSION_RE.search(item) is None
                and "local/render-corpus" not in lowered
                and "render-corpus-generated" not in lowered
                and "payload/" not in lowered,
                "failure_summary_unsafe_text",
            )
    return value


def _release_contract(
    lock_path: Path,
    wrapper_path: Path,
) -> dict[str, str]:
    """Authenticate the checked-out lock and wrapper used by build evidence."""

    lock_payload = _regular_file_payload(lock_path, MAX_LOCK_BYTES, "oracle_lock")
    lock = _strict_json_loads(lock_payload, "oracle_lock_json")
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

    normalized = _strict_json_loads(
        json.dumps(lock).encode("utf-8"),
        "oracle_lock_json",
    )
    assert isinstance(normalized, dict)
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
        labels == expected_labels,
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


def _bounded_int(
    value: object,
    code: str,
    *,
    minimum: int = 0,
    maximum: int = (1 << 63) - 1,
) -> int:
    _require(
        type(value) is int and minimum <= value <= maximum,
        code,
    )
    return value


def _ppm(value: object, code: str) -> int:
    return _bounded_int(value, code, maximum=1_000_000)


def _ratio_ppm(numerator: int, denominator: int, *, empty: int = 0) -> int:
    if denominator == 0:
        return empty
    return (numerator * 1_000_000 + denominator // 2) // denominator


def _validate_report_identity(
    value: object,
    code: str,
    *,
    bytes_key: str = "bytes",
    sha256_key: str = "sha256",
) -> dict[str, object]:
    _require(
        isinstance(value, dict)
        and set(value) == {bytes_key, sha256_key},
        f"{code}_schema",
    )
    size = _bounded_int(
        value[bytes_key],
        f"{code}_bytes",
        minimum=1,
        maximum=MAX_SOURCE_REPORT_BYTES,
    )
    digest = value[sha256_key]
    _require(_nonzero_hash_matches(digest), f"{code}_sha256")
    return {"bytes": size, "sha256": digest}


def _validate_host_tools(value: object) -> dict[str, Any]:
    _require(
        isinstance(value, dict)
        and set(value)
        == {
            "captured_identity_sha256",
            "expected_identity_sha256",
            "identity",
            "identity_status",
            "lock_file_sha256",
            "schema",
            "scope",
        }
        and value.get("schema")
        == "rxls.render-oracle-host-tools-evidence.v1"
        and value.get("scope") == "all"
        and value.get("identity_status") == "pinned_match",
        "host_identity_schema",
    )
    identity = value["identity"]
    captured = value["captured_identity_sha256"]
    _require(
        isinstance(identity, dict)
        and bool(identity)
        and _nonzero_hash_matches(captured)
        and value["expected_identity_sha256"] == captured
        and value["lock_file_sha256"]
        and _nonzero_hash_matches(value["lock_file_sha256"])
        and _canonical_sha256(identity) == captured,
        "host_identity_mismatch",
    )
    return value


def _validate_font_pack(value: object, expected_sha256: object) -> None:
    _require(
        isinstance(value, dict)
        and set(value)
        == {
            "alias_count",
            "attestation_required",
            "configured",
            "font_count",
            "fonts_conf_sha256",
            "license",
            "pack_sha256",
            "pdf_identities_sha256",
            "pdf_identity_count",
        }
        and value["attestation_required"] is True
        and value["configured"] is True
        and value["alias_count"] == 10
        and value["font_count"] == 26
        and value["pdf_identity_count"] == 59
        and value["license"] == "SIL-OFL-1.1"
        and value["pack_sha256"] == expected_sha256
        and _nonzero_hash_matches(value["fonts_conf_sha256"])
        and _nonzero_hash_matches(value["pdf_identities_sha256"]),
        "summary_font_pack",
    )


def _validate_text_box_metrics(
    value: object,
    code: str,
    *,
    minimum_matches: int = 1,
) -> None:
    _require(
        isinstance(value, dict)
        and set(value)
        == {
            "ambiguous",
            "f1_ppm",
            "libreoffice_items",
            "libreoffice_unmatched",
            "matched",
            "median_error_millipoints",
            "p95_error_millipoints",
            "precision_ppm",
            "recall_ppm",
            "rxls_items",
            "rxls_unmatched",
        },
        f"{code}_schema",
    )
    matched = _bounded_int(value["matched"], code, minimum=minimum_matches)
    rxls_items = _bounded_int(value["rxls_items"], code, minimum=matched)
    libreoffice_items = _bounded_int(
        value["libreoffice_items"],
        code,
        minimum=matched,
    )
    ambiguous = _bounded_int(value["ambiguous"], code)
    rxls_unmatched = _bounded_int(value["rxls_unmatched"], code)
    libreoffice_unmatched = _bounded_int(
        value["libreoffice_unmatched"],
        code,
    )
    _require(
        rxls_items == matched + ambiguous + rxls_unmatched
        and libreoffice_items == matched + libreoffice_unmatched,
        f"{code}_matching",
    )
    precision = _ratio_ppm(matched, rxls_items)
    recall = _ratio_ppm(matched, libreoffice_items)
    f1 = _ratio_ppm(2 * matched, rxls_items + libreoffice_items)
    _require(
        _ppm(value["precision_ppm"], code) == precision >= 999_000
        and _ppm(value["recall_ppm"], code) == recall >= 999_000
        and _ppm(value["f1_ppm"], code) == f1 >= 999_000,
        f"{code}_score",
    )
    median = _bounded_int(
        value["median_error_millipoints"],
        code,
        maximum=1_000,
    )
    p95 = _bounded_int(
        value["p95_error_millipoints"],
        code,
        maximum=2_500,
    )
    _require(median <= p95, f"{code}_order")


def _validate_hard_feature_metrics(
    value: object,
    *,
    name: str,
    expected_workbooks: int,
) -> None:
    code = f"fidelity_hard_feature:{name}"
    _require(
        isinstance(value, dict)
        and set(value)
        == {
            "edge_f1_ppm",
            "edge_libreoffice_pixels",
            "edge_rxls_pixels",
            "semantic_codepoint_libreoffice_items",
            "semantic_codepoint_precision_ppm",
            "semantic_codepoint_recall_ppm",
            "semantic_codepoint_rxls_items",
            "similarity_mean_ppm",
            "text_box",
            "text_line_box",
            "workbooks",
        },
        f"{code}_schema",
    )
    _require(value["workbooks"] == expected_workbooks, f"{code}_coverage")
    _bounded_int(value["edge_rxls_pixels"], code, minimum=1)
    _bounded_int(value["edge_libreoffice_pixels"], code, minimum=1)
    _bounded_int(
        value["semantic_codepoint_rxls_items"],
        code,
        minimum=1,
    )
    _bounded_int(
        value["semantic_codepoint_libreoffice_items"],
        code,
        minimum=1,
    )
    _require(
        _ppm(value["edge_f1_ppm"], code) >= 970_000
        and _ppm(value["similarity_mean_ppm"], code) >= 950_000
        and _ppm(value["semantic_codepoint_precision_ppm"], code)
        >= 999_000
        and _ppm(value["semantic_codepoint_recall_ppm"], code) >= 999_000,
        f"{code}_threshold",
    )
    _validate_text_box_metrics(value["text_box"], f"{code}_text_box")
    _validate_text_box_metrics(
        value["text_line_box"],
        f"{code}_text_line_box",
    )


def _validate_fidelity_gate(value: dict[str, Any]) -> dict[str, object]:
    _require(
        set(value)
        == {
            "coverage",
            "evidence",
            "failures",
            "metrics",
            "passed",
            "policy",
            "schema",
            "thresholds",
        }
        and value.get("schema") == "rxls.render-fidelity-targets.v1",
        "fidelity_schema",
    )
    _require(
        value.get("passed") is True and value.get("failures") == [],
        "fidelity_failed",
    )
    _require(
        type_exact_equal(
            value.get("thresholds"),
            EXPECTED_FIDELITY_THRESHOLDS,
        ),
        "fidelity_thresholds",
    )
    _require(
        type_exact_equal(
            value.get("policy"),
            EXPECTED_FIDELITY_POLICY,
        ),
        "fidelity_policy",
    )

    evidence = value.get("evidence")
    evidence_keys = {
        "bytes",
        "feature_map_sha256",
        "font_pack_sha256",
        "host_tools_identity_sha256",
        "input_set_sha256",
        "manifest_sha256",
        "oracle_build_contract_sha256",
        "oracle_image_config_digest",
        "oracle_image_manifest_digest",
        "oracle_libreoffice_artifact_sha256",
        "oracle_lock_file_sha256",
        "pdffonts_sha256",
        "pdfinfo_sha256",
        "pdftoppm_sha256",
        "pdftotext_sha256",
        "renderer_sha256",
        "sha256",
    }
    _require(
        isinstance(evidence, dict) and set(evidence) == evidence_keys,
        "fidelity_evidence_schema",
    )
    source_report = _validate_report_identity(
        {"bytes": evidence["bytes"], "sha256": evidence["sha256"]},
        "fidelity_source_report",
    )
    for key in evidence_keys - {
        "bytes",
        "oracle_image_config_digest",
        "oracle_image_manifest_digest",
        "sha256",
    }:
        _require(_nonzero_hash_matches(evidence[key]), f"fidelity_evidence:{key}")
    _require(
        _image_digest_matches(evidence["oracle_image_config_digest"])
        and _image_digest_matches(evidence["oracle_image_manifest_digest"])
        and evidence["oracle_libreoffice_artifact_sha256"]
        == LIBREOFFICE_ARTIFACT_SHA256,
        "fidelity_oracle_identity",
    )

    coverage = value.get("coverage")
    _require(
        isinstance(coverage, dict)
        and set(coverage)
        == {
            "broad_workbooks",
            "core_text_box_ambiguous",
            "core_text_box_candidates",
            "core_text_box_libreoffice_items",
            "core_text_box_libreoffice_unmatched",
            "core_text_box_matches",
            "core_text_box_unmatched",
            "core_text_line_box_ambiguous",
            "core_text_line_box_candidates",
            "core_text_line_box_libreoffice_items",
            "core_text_line_box_libreoffice_unmatched",
            "core_text_line_box_matches",
            "core_text_line_box_unmatched",
            "core_workbooks",
            "format_workbooks",
            "hard_feature_workbooks",
            "libreoffice_pdf_font_objects",
            "native_pdf_documents",
            "native_pdf_font_objects",
            "native_pdf_type0_cff_font_objects",
            "native_pdf_type0_font_objects",
            "native_pdf_type0_truetype_font_objects",
            "native_pdf_type3_font_objects",
            "pages",
            "report_workbooks",
            "status_counts",
        },
        "fidelity_coverage_schema",
    )
    _require(
        coverage["report_workbooks"] == 800
        and coverage["broad_workbooks"] == 800
        and coverage["core_workbooks"] == 118
        and coverage["format_workbooks"] == EXPECTED_FORMAT_COUNTS
        and coverage["status_counts"] == {"compared": 800}
        and coverage["hard_feature_workbooks"]
        == EXPECTED_HARD_FEATURE_COUNTS,
        "fidelity_coverage",
    )
    _bounded_int(coverage["pages"], "fidelity_pages", minimum=1, maximum=1_000_000)
    for key in ("libreoffice_pdf_font_objects", "native_pdf_documents"):
        _bounded_int(coverage[key], f"fidelity_coverage:{key}", minimum=1)
    native_pdf_font_objects = _bounded_int(
        coverage["native_pdf_font_objects"],
        "fidelity_coverage:native_pdf_font_objects",
        minimum=1,
    )
    native_pdf_type0_font_objects = _bounded_int(
        coverage["native_pdf_type0_font_objects"],
        "fidelity_coverage:native_pdf_type0_font_objects",
        minimum=1,
        maximum=native_pdf_font_objects,
    )
    native_pdf_type0_truetype_font_objects = _bounded_int(
        coverage["native_pdf_type0_truetype_font_objects"],
        "fidelity_coverage:native_pdf_type0_truetype_font_objects",
        maximum=native_pdf_type0_font_objects,
    )
    native_pdf_type0_cff_font_objects = _bounded_int(
        coverage["native_pdf_type0_cff_font_objects"],
        "fidelity_coverage:native_pdf_type0_cff_font_objects",
        maximum=native_pdf_type0_font_objects,
    )
    native_pdf_type3_font_objects = _bounded_int(
        coverage["native_pdf_type3_font_objects"],
        "fidelity_coverage:native_pdf_type3_font_objects",
        maximum=native_pdf_font_objects,
    )
    _require(
        native_pdf_type0_truetype_font_objects
        + native_pdf_type0_cff_font_objects
        == native_pdf_type0_font_objects
        and native_pdf_type0_font_objects + native_pdf_type3_font_objects
        == native_pdf_font_objects,
        "fidelity_native_pdf_coverage",
    )
    for prefix, minimum in (("core_text_box", 100), ("core_text_line_box", 1)):
        candidates = _bounded_int(
            coverage[f"{prefix}_candidates"],
            f"fidelity_coverage:{prefix}",
            minimum=minimum,
        )
        references = _bounded_int(
            coverage[f"{prefix}_libreoffice_items"],
            f"fidelity_coverage:{prefix}",
            minimum=minimum,
        )
        matches = _bounded_int(
            coverage[f"{prefix}_matches"],
            f"fidelity_coverage:{prefix}",
            minimum=minimum,
        )
        ambiguous = _bounded_int(
            coverage[f"{prefix}_ambiguous"],
            f"fidelity_coverage:{prefix}",
        )
        unmatched = _bounded_int(
            coverage[f"{prefix}_unmatched"],
            f"fidelity_coverage:{prefix}",
        )
        reference_unmatched = _bounded_int(
            coverage[f"{prefix}_libreoffice_unmatched"],
            f"fidelity_coverage:{prefix}",
        )
        _require(
            candidates == matches + ambiguous + unmatched
            and references == matches + reference_unmatched,
            f"fidelity_coverage:{prefix}_matching",
        )

    metrics = value.get("metrics")
    _require(
        isinstance(metrics, dict)
        and set(metrics)
        == {
            "broad_similarity_mean_ppm",
            "core_edge_f1_ppm",
            "core_semantic_codepoint_precision_ppm",
            "core_semantic_codepoint_recall_ppm",
            "core_similarity_mean_ppm",
            "hard_feature_cohorts",
            "page_box_max_millipoints",
            "page_box_median_millipoints",
            "page_box_p95_millipoints",
            "pdf_point_geometry_mismatches",
            "pdf_xhtml_crosscheck_max_micropoints",
            "text_box_f1_ppm",
            "text_box_match_coverage_ppm",
            "text_box_median_error_millipoints",
            "text_box_p95_error_millipoints",
            "text_box_precision_ppm",
            "text_box_recall_ppm",
            "text_line_box_f1_ppm",
            "text_line_box_median_error_millipoints",
            "text_line_box_p95_error_millipoints",
            "text_line_box_precision_ppm",
            "text_line_box_recall_ppm",
        },
        "fidelity_metrics_schema",
    )
    _require(
        _ppm(metrics["broad_similarity_mean_ppm"], "fidelity_metrics")
        >= 950_000
        and _ppm(metrics["core_similarity_mean_ppm"], "fidelity_metrics")
        >= 980_000
        and _ppm(metrics["core_edge_f1_ppm"], "fidelity_metrics") >= 970_000
        and _ppm(
            metrics["core_semantic_codepoint_precision_ppm"],
            "fidelity_metrics",
        )
        >= 999_000
        and _ppm(
            metrics["core_semantic_codepoint_recall_ppm"],
            "fidelity_metrics",
        )
        >= 999_000,
        "fidelity_metric_threshold",
    )
    for metric_prefix, coverage_prefix in (
        ("text_box", "core_text_box"),
        ("text_line_box", "core_text_line_box"),
    ):
        matched = coverage[f"{coverage_prefix}_matches"]
        candidates = coverage[f"{coverage_prefix}_candidates"]
        references = coverage[f"{coverage_prefix}_libreoffice_items"]
        precision = _ratio_ppm(matched, candidates)
        recall = _ratio_ppm(matched, references)
        f1 = _ratio_ppm(2 * matched, candidates + references)
        _require(
            _ppm(
                metrics[f"{metric_prefix}_precision_ppm"],
                f"fidelity_metrics:{metric_prefix}",
            )
            == precision
            >= 999_000
            and _ppm(
                metrics[f"{metric_prefix}_recall_ppm"],
                f"fidelity_metrics:{metric_prefix}",
            )
            == recall
            >= 999_000
            and _ppm(
                metrics[f"{metric_prefix}_f1_ppm"],
                f"fidelity_metrics:{metric_prefix}",
            )
            == f1
            >= 999_000,
            "fidelity_text_threshold",
        )
    _require(
        _ppm(
            metrics["text_box_match_coverage_ppm"],
            "fidelity_metrics:text_box",
        )
        == metrics["text_box_precision_ppm"],
        "fidelity_text_coverage",
    )
    text_median = _bounded_int(
        metrics["text_box_median_error_millipoints"],
        "fidelity_text_geometry",
        maximum=1_000,
    )
    text_p95 = _bounded_int(
        metrics["text_box_p95_error_millipoints"],
        "fidelity_text_geometry",
        maximum=2_500,
    )
    line_median = _bounded_int(
        metrics["text_line_box_median_error_millipoints"],
        "fidelity_line_geometry",
        maximum=1_000,
    )
    line_p95 = _bounded_int(
        metrics["text_line_box_p95_error_millipoints"],
        "fidelity_line_geometry",
        maximum=2_500,
    )
    page_median = _bounded_int(
        metrics["page_box_median_millipoints"],
        "fidelity_page_geometry",
        maximum=1_000,
    )
    page_p95 = _bounded_int(
        metrics["page_box_p95_millipoints"],
        "fidelity_page_geometry",
        maximum=2_500,
    )
    page_max = _bounded_int(
        metrics["page_box_max_millipoints"],
        "fidelity_page_geometry",
        maximum=5_000,
    )
    _require(
        text_median <= text_p95
        and line_median <= line_p95
        and page_median <= page_p95 <= page_max
        and metrics["pdf_point_geometry_mismatches"] == 0
        and _bounded_int(
            metrics["pdf_xhtml_crosscheck_max_micropoints"],
            "fidelity_page_geometry",
            maximum=1_000,
        )
        <= 1_000,
        "fidelity_geometry",
    )
    hard = metrics.get("hard_feature_cohorts")
    _require(
        isinstance(hard, dict) and set(hard) == set(EXPECTED_HARD_FEATURE_COUNTS),
        "fidelity_hard_feature_schema",
    )
    for name, count in EXPECTED_HARD_FEATURE_COUNTS.items():
        _validate_hard_feature_metrics(
            hard[name],
            name=name,
            expected_workbooks=count,
        )
    return {
        "evidence": evidence,
        "source_report": source_report,
    }


def _validate_authored_gate(value: dict[str, Any]) -> dict[str, object]:
    _require(
        set(value)
        == {
            "coverage",
            "evidence",
            "expected",
            "failures",
            "metrics",
            "passed",
            "schema",
            "thresholds",
        }
        and value.get("schema") == "rxls.authored-print-parity.v2",
        "authored_schema",
    )
    _require(
        value.get("passed") is True and value.get("failures") == [],
        "authored_failed",
    )
    _require(
        type_exact_equal(
            value.get("thresholds"),
            EXPECTED_AUTHORED_THRESHOLDS,
        ),
        "authored_thresholds",
    )
    _require(
        value.get("expected")
        == {
            "page_box_pixels": {"height": 1056, "width": 816},
            "page_box_points": {"height": "792/1", "width": "612/1"},
            "pages_per_workbook_by_scale_mode": {"fit": 1, "scale": 4},
            "workbooks_by_scale_mode": {"fit": 50, "scale": 50},
        },
        "authored_expected",
    )

    evidence = value.get("evidence")
    evidence_keys = {
        "feature_map_sha256",
        "font_pack_sha256",
        "host_tools_identity_sha256",
        "input_set_sha256",
        "manifest_sha256",
        "oracle_build_contract_sha256",
        "oracle_image_config_digest",
        "oracle_image_manifest_digest",
        "oracle_libreoffice_artifact_sha256",
        "oracle_lock_file_sha256",
        "pdffonts_sha256",
        "pdfinfo_sha256",
        "pdftoppm_sha256",
        "pdftotext_sha256",
        "renderer_sha256",
        "report_bytes",
        "report_sha256",
    }
    _require(
        isinstance(evidence, dict) and set(evidence) == evidence_keys,
        "authored_evidence_schema",
    )
    source_report = _validate_report_identity(
        {
            "bytes": evidence["report_bytes"],
            "sha256": evidence["report_sha256"],
        },
        "authored_source_report",
    )
    for key in evidence_keys - {
        "oracle_image_config_digest",
        "oracle_image_manifest_digest",
        "report_bytes",
        "report_sha256",
    }:
        _require(_nonzero_hash_matches(evidence[key]), f"authored_evidence:{key}")
    _require(
        _image_digest_matches(evidence["oracle_image_config_digest"])
        and _image_digest_matches(evidence["oracle_image_manifest_digest"])
        and evidence["oracle_libreoffice_artifact_sha256"]
        == LIBREOFFICE_ARTIFACT_SHA256,
        "authored_oracle_identity",
    )

    coverage = value.get("coverage")
    _require(
        isinstance(coverage, dict)
        and set(coverage)
        == {
            "by_scale_mode",
            "edge_libreoffice_pixels",
            "edge_rxls_pixels",
            "libreoffice_pdf_font_objects",
            "native_pdf_documents",
            "native_pdf_font_objects",
            "native_pdf_type0_cff_font_objects",
            "native_pdf_type0_font_objects",
            "native_pdf_type0_truetype_font_objects",
            "native_pdf_type3_font_objects",
            "page_count_histogram",
            "pages",
            "semantic_codepoint_libreoffice_items",
            "semantic_codepoint_rxls_items",
            "text_box_candidates",
            "text_box_libreoffice_items",
            "text_box_matches",
            "text_line_box_candidates",
            "text_line_box_libreoffice_items",
            "text_line_box_matches",
            "workbooks",
        },
        "authored_coverage_schema",
    )
    _require(
        coverage["workbooks"] == 100
        and coverage["pages"] == 250
        and coverage["page_count_histogram"] == {"1": 50, "4": 50}
        and coverage["by_scale_mode"] == {"fit": 50, "scale": 50},
        "authored_coverage",
    )
    for key in (
        "edge_libreoffice_pixels",
        "edge_rxls_pixels",
        "libreoffice_pdf_font_objects",
        "native_pdf_documents",
        "semantic_codepoint_libreoffice_items",
        "semantic_codepoint_rxls_items",
    ):
        _bounded_int(coverage[key], f"authored_coverage:{key}", minimum=1)
    native_pdf_font_objects = _bounded_int(
        coverage["native_pdf_font_objects"],
        "authored_coverage:native_pdf_font_objects",
        minimum=1,
    )
    native_pdf_type0_font_objects = _bounded_int(
        coverage["native_pdf_type0_font_objects"],
        "authored_coverage:native_pdf_type0_font_objects",
        minimum=1,
        maximum=native_pdf_font_objects,
    )
    native_pdf_type0_truetype_font_objects = _bounded_int(
        coverage["native_pdf_type0_truetype_font_objects"],
        "authored_coverage:native_pdf_type0_truetype_font_objects",
        maximum=native_pdf_type0_font_objects,
    )
    native_pdf_type0_cff_font_objects = _bounded_int(
        coverage["native_pdf_type0_cff_font_objects"],
        "authored_coverage:native_pdf_type0_cff_font_objects",
        maximum=native_pdf_type0_font_objects,
    )
    native_pdf_type3_font_objects = _bounded_int(
        coverage["native_pdf_type3_font_objects"],
        "authored_coverage:native_pdf_type3_font_objects",
        maximum=native_pdf_font_objects,
    )
    _require(
        native_pdf_type0_truetype_font_objects
        + native_pdf_type0_cff_font_objects
        == native_pdf_type0_font_objects
        and native_pdf_type0_font_objects + native_pdf_type3_font_objects
        == native_pdf_font_objects,
        "authored_native_pdf_coverage",
    )
    metrics = value.get("metrics")
    _require(
        isinstance(metrics, dict)
        and set(metrics)
        == {
            "edge_f1_ppm",
            "page_box_max_millipoints",
            "page_box_median_millipoints",
            "page_box_p95_millipoints",
            "pdf_point_geometry_mismatches",
            "pdf_xhtml_crosscheck_max_micropoints",
            "semantic_codepoint_precision_ppm",
            "semantic_codepoint_recall_ppm",
            "similarity_mean_ppm",
            "text_box_ambiguous",
            "text_box_f1_ppm",
            "text_box_libreoffice_unmatched",
            "text_box_match_coverage_ppm",
            "text_box_median_error_millipoints",
            "text_box_p95_error_millipoints",
            "text_box_precision_ppm",
            "text_box_recall_ppm",
            "text_box_unmatched",
            "text_line_box_ambiguous",
            "text_line_box_f1_ppm",
            "text_line_box_libreoffice_unmatched",
            "text_line_box_median_error_millipoints",
            "text_line_box_p95_error_millipoints",
            "text_line_box_precision_ppm",
            "text_line_box_recall_ppm",
            "text_line_box_unmatched",
        },
        "authored_metrics_schema",
    )
    _require(
        _ppm(metrics["similarity_mean_ppm"], "authored_metrics") >= 950_000
        and _ppm(metrics["edge_f1_ppm"], "authored_metrics") >= 970_000
        and _ppm(
            metrics["semantic_codepoint_precision_ppm"],
            "authored_metrics",
        )
        >= 999_000
        and _ppm(
            metrics["semantic_codepoint_recall_ppm"],
            "authored_metrics",
        )
        >= 999_000,
        "authored_metric_threshold",
    )
    for prefix in ("text_box", "text_line_box"):
        candidates = _bounded_int(
            coverage[f"{prefix}_candidates"],
            f"authored_coverage:{prefix}",
            minimum=1,
        )
        references = _bounded_int(
            coverage[f"{prefix}_libreoffice_items"],
            f"authored_coverage:{prefix}",
            minimum=1,
        )
        matches = _bounded_int(
            coverage[f"{prefix}_matches"],
            f"authored_coverage:{prefix}",
            minimum=1,
        )
        ambiguous = _bounded_int(
            metrics[f"{prefix}_ambiguous"],
            f"authored_metrics:{prefix}",
        )
        unmatched = _bounded_int(
            metrics[f"{prefix}_unmatched"],
            f"authored_metrics:{prefix}",
        )
        reference_unmatched = _bounded_int(
            metrics[f"{prefix}_libreoffice_unmatched"],
            f"authored_metrics:{prefix}",
        )
        _require(
            candidates == matches + ambiguous + unmatched
            and references == matches + reference_unmatched,
            f"authored_coverage:{prefix}_matching",
        )
        precision = _ratio_ppm(matches, candidates)
        recall = _ratio_ppm(matches, references)
        f1 = _ratio_ppm(2 * matches, candidates + references)
        _require(
            _ppm(
                metrics[f"{prefix}_precision_ppm"],
                f"authored_metrics:{prefix}",
            )
            == precision
            >= 999_000
            and _ppm(
                metrics[f"{prefix}_recall_ppm"],
                f"authored_metrics:{prefix}",
            )
            == recall
            >= 999_000
            and _ppm(
                metrics[f"{prefix}_f1_ppm"],
                f"authored_metrics:{prefix}",
            )
            == f1
            >= 999_000,
            "authored_text_threshold",
        )
    _require(
        _ppm(
            metrics["text_box_match_coverage_ppm"],
            "authored_metrics:text_box",
        )
        == metrics["text_box_precision_ppm"],
        "authored_text_coverage",
    )
    _require(
        metrics["pdf_point_geometry_mismatches"] == 0,
        "authored_point_geometry",
    )
    text_median = _bounded_int(
        metrics["text_box_median_error_millipoints"],
        "authored_text_geometry",
        maximum=1_000,
    )
    text_p95 = _bounded_int(
        metrics["text_box_p95_error_millipoints"],
        "authored_text_geometry",
        maximum=2_500,
    )
    line_median = _bounded_int(
        metrics["text_line_box_median_error_millipoints"],
        "authored_line_geometry",
        maximum=1_000,
    )
    line_p95 = _bounded_int(
        metrics["text_line_box_p95_error_millipoints"],
        "authored_line_geometry",
        maximum=2_500,
    )
    page_median = _bounded_int(
        metrics["page_box_median_millipoints"],
        "authored_page_geometry",
        maximum=1_000,
    )
    page_p95 = _bounded_int(
        metrics["page_box_p95_millipoints"],
        "authored_page_geometry",
        maximum=2_500,
    )
    page_max = _bounded_int(
        metrics["page_box_max_millipoints"],
        "authored_page_geometry",
        maximum=5_000,
    )
    _require(
        text_median <= text_p95
        and line_median <= line_p95
        and page_median <= page_p95 <= page_max
        and _bounded_int(
            metrics["pdf_xhtml_crosscheck_max_micropoints"],
            "authored_page_geometry",
            maximum=1_000,
        )
        <= 1_000,
        "authored_geometry",
    )
    return {
        "evidence": evidence,
        "source_report": source_report,
    }


def _validate_repeatability_distribution(
    value: object,
    *,
    maximum_ppm: int,
) -> int:
    _require(
        isinstance(value, dict)
        and set(value)
        == {"absolute_deltas_ppm", "count", "max_absolute_delta_ppm"},
        "repeatability_distribution",
    )
    deltas = value.get("absolute_deltas_ppm")
    count = value.get("count")
    maximum = value.get("max_absolute_delta_ppm")
    _require(
        isinstance(deltas, list)
        and bool(deltas)
        and type(count) is int
        and count == len(deltas)
        and count <= 10_000_000
        and type(maximum) is int
        and all(
            type(delta) is int and 0 <= delta <= maximum_ppm
            for delta in deltas
        )
        and deltas == sorted(deltas)
        and maximum == deltas[-1],
        "repeatability_distribution",
    )
    return count


def _validate_repeatability(value: dict[str, Any]) -> None:
    _require(
        set(value)
        == {
            "coverage",
            "drift",
            "failures",
            "identity",
            "metric_policy",
            "reports",
            "schema",
            "status",
            "thresholds_ppm",
        }
        and value.get("schema")
        == "rxls.libreoffice-render-repeatability.v2",
        "repeatability_schema",
    )
    thresholds = value.get("thresholds_ppm")
    _require(
        thresholds
        == {
            "blurred_luma_similarity_max_absolute_drift": 20_000,
            "mask_f1_max_absolute_drift": 20_000,
            "similarity_max_absolute_drift": 20_000,
        },
        "repeatability_thresholds",
    )
    _require(
        value.get("status") == "pass" and value.get("failures") == [],
        "repeatability_failed",
    )
    drift = value.get("drift")
    _require(
        isinstance(drift, dict)
        and set(drift)
        == {"blurred_luma_similarity", "mask_f1", "similarity"},
        "repeatability_drift",
    )
    similarity_count = _validate_repeatability_distribution(
        drift["similarity"],
        maximum_ppm=20_000,
    )
    blur_count = _validate_repeatability_distribution(
        drift["blurred_luma_similarity"],
        maximum_ppm=20_000,
    )
    masks = drift.get("mask_f1")
    _require(
        isinstance(masks, dict)
        and set(masks)
        == {
            "edge",
            "foreground",
            "max_absolute_delta_ppm",
            "text_ink",
        },
        "repeatability_masks",
    )
    mask_counts = [
        _validate_repeatability_distribution(
            masks[key],
            maximum_ppm=20_000,
        )
        for key in ("edge", "foreground", "text_ink")
    ]
    mask_maximum = max(
        masks[key]["max_absolute_delta_ppm"]
        for key in ("edge", "foreground", "text_ink")
    )
    _require(
        masks["max_absolute_delta_ppm"] == mask_maximum
        and type(masks["max_absolute_delta_ppm"]) is int
        and similarity_count == blur_count
        and mask_counts == [similarity_count] * 3,
        "repeatability_observations",
    )
    coverage = value.get("coverage")
    _require(
        isinstance(coverage, dict)
        and set(coverage)
        == {"pages", "visual_observations_per_metric", "workbooks"}
        and coverage.get("workbooks") == 800
        and type(coverage.get("pages")) is int
        and coverage["pages"] > 0
        and coverage.get("visual_observations_per_metric")
        == similarity_count,
        "repeatability_coverage",
    )
    _require(
        similarity_count == coverage["workbooks"] + coverage["pages"],
        "repeatability_coverage",
    )
    identity = value.get("identity")
    _require(
        isinstance(identity, dict)
        and set(identity)
        == {
            "baseline_contract",
            "configuration",
            "input_set",
            "preflight",
            "renderer_binary",
        },
        "repeatability_identity",
    )
    for key in ("configuration", "preflight"):
        item = identity[key]
        _require(
            isinstance(item, dict)
            and set(item)
            == {"baseline_sha256", "candidate_sha256", "equal"}
            and item["equal"] is True
            and _nonzero_hash_matches(item["baseline_sha256"])
            and item["baseline_sha256"] == item["candidate_sha256"],
            "repeatability_identity",
        )
    input_set = identity["input_set"]
    _require(
        isinstance(input_set, dict)
        and set(input_set)
        == {
            "baseline_count",
            "baseline_sha256",
            "candidate_count",
            "candidate_sha256",
            "equal",
        }
        and input_set["equal"] is True
        and type(input_set["baseline_count"]) is int
        and input_set["baseline_count"] == coverage["workbooks"]
        and input_set["candidate_count"] == input_set["baseline_count"]
        and _nonzero_hash_matches(input_set["baseline_sha256"])
        and input_set["candidate_sha256"] == input_set["baseline_sha256"],
        "repeatability_identity",
    )
    baseline_contract = identity["baseline_contract"]
    _require(
        isinstance(baseline_contract, dict)
        and set(baseline_contract) == {"configuration", "input_set"},
        "repeatability_baseline_contract",
    )
    baseline_configuration = baseline_contract["configuration"]
    _require(
        isinstance(baseline_configuration, dict)
        and set(baseline_configuration)
        == {"baseline_sha256", "candidate_sha256", "equal"}
        and baseline_configuration["equal"] is True
        and _nonzero_hash_matches(
            baseline_configuration["baseline_sha256"]
        )
        and baseline_configuration["candidate_sha256"]
        == baseline_configuration["baseline_sha256"],
        "repeatability_baseline_contract",
    )
    baseline_input_set = baseline_contract["input_set"]
    _require(
        isinstance(baseline_input_set, dict)
        and set(baseline_input_set)
        == {
            "baseline_count",
            "baseline_sha256",
            "candidate_count",
            "candidate_sha256",
            "equal",
        }
        and baseline_input_set["equal"] is True
        and type(baseline_input_set["baseline_count"]) is int
        and baseline_input_set["baseline_count"] == coverage["workbooks"]
        and baseline_input_set["candidate_count"]
        == baseline_input_set["baseline_count"]
        and _nonzero_hash_matches(
            baseline_input_set["baseline_sha256"]
        )
        and baseline_input_set["candidate_sha256"]
        == baseline_input_set["baseline_sha256"],
        "repeatability_baseline_contract",
    )
    renderer_binary = identity["renderer_binary"]
    _require(
        isinstance(renderer_binary, dict)
        and set(renderer_binary) == {"baseline", "candidate", "equal"}
        and renderer_binary["equal"] is True
        and isinstance(renderer_binary["baseline"], dict)
        and set(renderer_binary["baseline"]) == {"bytes", "sha256"}
        and type(renderer_binary["baseline"]["bytes"]) is int
        and renderer_binary["baseline"]["bytes"] > 0
        and _nonzero_hash_matches(renderer_binary["baseline"]["sha256"])
        and renderer_binary["candidate"] == renderer_binary["baseline"],
        "repeatability_identity",
    )
    reports = value.get("reports")
    _require(
        isinstance(reports, dict)
        and set(reports) == {"baseline", "candidate"}
        and all(
            isinstance(reports[key], dict)
            and set(reports[key]) == {"bytes", "sha256"}
            and type(reports[key]["bytes"]) is int
            and reports[key]["bytes"] > 0
            and _nonzero_hash_matches(reports[key]["sha256"])
            for key in reports
        ),
        "repeatability_reports",
    )
    _require(
        type_exact_equal(
            value.get("metric_policy"),
            {
            "distribution": "sorted_absolute_paired_integer_ppm_deltas",
            "input_pairing": "sha256",
            "observations": "workbook_aggregate_and_page",
            "paths_or_content_retained": False,
            "unique_text_geometry": (
                "schema_validated_exact_same_sha_diagnostic_non_scoring"
            ),
        },
        ),
        "repeatability_policy",
    )


def _validate_repeatability_bindings(
    value: dict[str, Any],
    candidates: list[dict[str, Any]],
    renderer: dict[str, Any],
    fidelities: list[dict[str, object]],
) -> None:
    _require(
        len(candidates) == 2 and len(fidelities) == 2,
        "repeatability_candidate_count",
    )
    identity = value["identity"]
    baseline_contract = identity["baseline_contract"]
    _require(
        baseline_contract["configuration"]["baseline_sha256"]
        == candidates[0]["configuration_sha256"]
        and baseline_contract["configuration"]["candidate_sha256"]
        == candidates[1]["configuration_sha256"],
        "repeatability_configuration_binding",
    )
    input_set = baseline_contract["input_set"]
    _require(
        input_set["baseline_sha256"] == candidates[0]["input_set_sha256"]
        and input_set["candidate_sha256"] == candidates[1]["input_set_sha256"]
        and input_set["baseline_count"] == candidates[0]["input_files"]
        and input_set["candidate_count"] == candidates[1]["input_files"],
        "repeatability_input_binding",
    )
    _require(
        identity["renderer_binary"]["baseline"] == renderer
        and identity["renderer_binary"]["candidate"] == renderer,
        "repeatability_renderer_binding",
    )
    raw_input_set = identity["input_set"]
    _require(
        raw_input_set["baseline_count"] == 800
        and raw_input_set["candidate_count"] == 800
        and raw_input_set["baseline_sha256"]
        == fidelities[0]["evidence"]["input_set_sha256"]
        and raw_input_set["candidate_sha256"]
        == fidelities[1]["evidence"]["input_set_sha256"],
        "repeatability_fidelity_input_binding",
    )
    _require(
        value["reports"]["baseline"] == fidelities[0]["source_report"]
        and value["reports"]["candidate"] == fidelities[1]["source_report"],
        "repeatability_source_report_binding",
    )


def _repeatability_score_drift_limits(
    value: dict[str, Any],
) -> dict[str, int]:
    drift = value["drift"]
    masks = drift["mask_f1"]
    return {
        "blurred_luma_similarity_ppm": drift[
            "blurred_luma_similarity"
        ]["max_absolute_delta_ppm"],
        "edge_f1_ppm": masks["edge"]["max_absolute_delta_ppm"],
        "foreground_f1_ppm": masks["foreground"][
            "max_absolute_delta_ppm"
        ],
        "similarity_ppm": drift["similarity"][
            "max_absolute_delta_ppm"
        ],
        "text_ink_f1_ppm": masks["text_ink"][
            "max_absolute_delta_ppm"
        ],
    }


def _validate_baseline_gate(
    gate: dict[str, Any],
    reviewed: dict[str, Any],
    candidate: dict[str, Any],
    candidate_payload: bytes,
    source_evidence: dict[str, object],
) -> None:
    baseline_checker = _load_baseline_checker()
    try:
        expected = baseline_checker.compare(reviewed, candidate)
    except baseline_checker.BaselineError as error:
        raise EvidenceError(f"baseline_compare:{error}") from error
    expected["source_evidence"] = source_evidence
    _require(gate == expected, "baseline_gate_recomputed")
    _require(
        gate["passed"] is True and gate["failures"] == [],
        "ratchet_failed",
    )
    _require(
        _sha256(candidate_payload) == _canonical_sha256(candidate),
        "candidate_encoding",
    )
    _require(
        gate["campaign"]["sha256"]
        == _canonical_sha256(candidate["campaign"]),
        "gate_campaign_sha256",
    )


def _validate_candidate_gate(
    gate: dict[str, Any],
    candidate: dict[str, Any],
    candidate_payload: bytes,
    source_evidence: dict[str, object],
) -> None:
    _require(
        set(gate)
        == {
            "baseline_sha256",
            "created",
            "passed",
            "schema",
            "source_evidence",
        },
        "candidate_gate_keys",
    )
    _require(
        gate.get("schema") == "rxls.render-parity-baseline-check.v1",
        "candidate_gate_schema",
    )
    _require(
        gate.get("created") is True and gate.get("passed") is True,
        "candidate_gate_failed",
    )
    _require(
        gate.get("source_evidence") == source_evidence,
        "candidate_source_evidence",
    )
    candidate_sha256 = _canonical_sha256(candidate)
    _require(
        gate.get("baseline_sha256") == candidate_sha256,
        "candidate_gate_identity",
    )
    _require(
        _sha256(candidate_payload) == candidate_sha256,
        "candidate_encoding",
    )


def _validate_candidate(candidate: dict[str, Any]) -> dict[str, Any]:
    baseline_checker = _load_baseline_checker()
    try:
        validated = baseline_checker.validate_observed_candidate(candidate)
    except baseline_checker.BaselineError as error:
        raise EvidenceError(f"candidate_invalid:{error}") from error
    _require(validated == candidate, "candidate_normalization")
    _require(
        candidate.get("schema")
        == baseline_checker.OBSERVED_CANDIDATE_SCHEMA,
        "candidate_schema",
    )
    _require(
        candidate.get("input_files") == 800,
        "candidate_case_count",
    )
    _require(
        candidate.get("comparable_files") == 800,
        "candidate_comparable_count",
    )
    _require(
        candidate.get("statuses") == {"compared": 800}
        and candidate.get("classifications") == {"within_threshold": 800},
        "candidate_statuses",
    )
    campaign = candidate["campaign"]
    _require(
        campaign.get("schema") == "rxls.render-parity-campaign.v1",
        "campaign_schema",
    )
    _require(
        campaign.get("kind") == "project_generated_hosted_full",
        "campaign_kind",
    )
    _require(campaign.get("profile") == "full", "campaign_profile")
    _require(
        campaign.get("generator") == "rxls-synthetic-render-corpus"
        and campaign.get("generator_version") == "1.5.0",
        "campaign_generator",
    )
    _require(campaign.get("case_count") == 800, "campaign_case_count")
    _require(
        campaign.get("format_counts") == EXPECTED_FORMAT_COUNTS,
        "campaign_format_counts",
    )
    _require(
        campaign.get("feature_counts") == EXPECTED_FEATURE_COUNTS,
        "campaign_feature_counts",
    )
    _require(
        campaign.get("manifest_sha256")
        == EXPECTED_HOSTED_FULL_MANIFEST_SHA256,
        "campaign_manifest",
    )
    _require(
        campaign.get("input_set_sha256")
        == candidate.get("input_set_sha256")
        == EXPECTED_HOSTED_FULL_INPUT_SET_SHA256,
        "campaign_input_identity",
    )
    _require(
        _nonzero_hash_matches(candidate.get("configuration_sha256")),
        "candidate_configuration",
    )
    groups = candidate.get("groups")
    _require(
        isinstance(groups, list)
        and len(groups) == 96
        and baseline_checker.group_topology_sha256(groups)
        == EXPECTED_HOSTED_FULL_GROUP_TOPOLOGY_SHA256,
        "candidate_group_topology",
    )
    all_cohort = candidate["cohorts"]["all"]
    _require(
        all_cohort["workbooks"] == 800
        and all_cohort["comparable_workbooks"] == 800,
        "candidate_all_coverage",
    )
    for dimension in ("all", "by_feature", "by_format"):
        rows = (
            {"all": candidate["cohorts"]["all"]}
            if dimension == "all"
            else candidate["cohorts"][dimension]
        )
        for name, cohort in rows.items():
            _require(
                cohort["comparable_workbooks"] == cohort["workbooks"]
                and set(cohort["scores"])
                == baseline_checker.EXPECTED_SCORE_METRICS
                and set(cohort["deltas"])
                == baseline_checker.EXPECTED_DELTA_METRICS,
                f"candidate_metric_coverage:{dimension}:{name}",
            )
    warning_counts = candidate.get("warning_counts")
    _require(
        isinstance(warning_counts, dict)
        and len(warning_counts) <= 256
        and all(
            WARNING_CODE_RE.fullmatch(code) is not None
            and type(count) is int
            and 1 <= count <= 1_000_000
            for code, count in warning_counts.items()
        ),
        "candidate_warnings",
    )
    return campaign


def validate(
    artifact_dir: Path,
    head_sha: str,
    reviewed_baseline: Path | None,
    *,
    campaign: str = "full",
    baseline_mode: str = "verify",
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
    _require(campaign == "full", "campaign")
    _require(baseline_mode in {"candidate", "verify"}, "baseline_mode")
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
            campaign=campaign,
            baseline_mode=baseline_mode,
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

    baseline_checker = _load_baseline_checker()
    reviewed: dict[str, Any] | None = None
    if baseline_mode == "verify":
        _require(reviewed_baseline is not None, "reviewed_baseline_required")
        reviewed, reviewed_payload = _read_json(reviewed_baseline)
        try:
            normalized_reviewed = baseline_checker.validate_reviewed_ratchet(
                reviewed
            )
        except baseline_checker.BaselineError as error:
            raise EvidenceError(f"reviewed_invalid:{error}") from error
        _require(
            reviewed.get("schema")
            == baseline_checker.RATCHET_ENVELOPE_SCHEMA
            and normalized_reviewed == reviewed,
            "reviewed_schema",
        )
        _require(
            _sha256(reviewed_payload) == _canonical_sha256(reviewed),
            "reviewed_encoding",
        )
        reviewed_sha256: str | None = _canonical_sha256(reviewed)
    else:
        _require(reviewed_baseline is None, "candidate_reviewed_baseline_forbidden")
        reviewed_sha256 = None
    contract = _release_contract(oracle_lock, oracle_wrapper)

    fidelities = [
        documents["fidelity-a.json"],
        documents["fidelity-b.json"],
    ]
    fidelity_results = [
        _validate_fidelity_gate(fidelity)
        for fidelity in fidelities
    ]
    fidelity_evidence = [
        result["evidence"]
        for result in fidelity_results
    ]
    _require(
        all(isinstance(evidence, dict) for evidence in fidelity_evidence),
        "fidelity_evidence",
    )
    full_corpus_keys = {
        "feature_map_sha256",
        "input_set_sha256",
        "manifest_sha256",
    }
    fidelity_bindings = [
        {
            key: evidence[key]
            for key in full_corpus_keys
        }
        for evidence in fidelity_evidence
    ]
    _require(
        fidelity_bindings[0] == fidelity_bindings[1],
        "fidelity_corpus_repeatability",
    )

    authored = documents["authored-print-gate.json"]
    authored_result = _validate_authored_gate(authored)
    authored_evidence = authored_result["evidence"]
    _require(isinstance(authored_evidence, dict), "authored_evidence")

    candidates: list[dict[str, Any]] = []
    candidate_campaigns: list[dict[str, Any]] = []
    gates: list[dict[str, Any]] = []
    for label in ("a", "b"):
        index = "ab".index(label)
        candidate = documents[f"baseline-candidate-{label}.json"]
        candidate_campaign = _validate_candidate(candidate)
        gate = documents[f"baseline-gate-{label}.json"]
        source_report = fidelity_results[index]["source_report"]
        _require(isinstance(source_report, dict), "fidelity_source_report")
        if baseline_mode == "verify":
            assert reviewed is not None
            assert reviewed_sha256 is not None
            _validate_baseline_gate(
                gate,
                reviewed,
                candidate,
                payloads[f"baseline-candidate-{label}.json"],
                source_report,
            )
            _require(
                gate["campaign"]["manifest_sha256"]
                == candidate_campaign["manifest_sha256"],
                "gate_campaign_identity",
            )
        else:
            _validate_candidate_gate(
                gate,
                candidate,
                payloads[f"baseline-candidate-{label}.json"],
                source_report,
            )
        candidates.append(candidate)
        candidate_campaigns.append(candidate_campaign)
        gates.append(gate)
    _require(
        candidates[0]["campaign"] == candidates[1]["campaign"],
        "campaign_repeatability",
    )
    _require(
        candidates[0]["warning_counts"] == candidates[1]["warning_counts"],
        "warning_repeatability",
    )

    repeatability = documents["repeatability.json"]
    _validate_repeatability(repeatability)

    build = documents["build.json"]
    _validate_build(build, head_sha, contract)
    for evidence in fidelity_evidence:
        _validate_gate_image_binding(
            evidence,
            build,
            "fidelity",
        )
    _validate_gate_image_binding(authored_evidence, build, "authored")
    host_tools = _validate_host_tools(documents["host-tools.json"])
    renderer = documents["renderer.json"]
    _require(
        set(renderer) == {"bytes", "sha256"}
        and type(renderer.get("bytes")) is int
        and renderer["bytes"] > 0
        and _nonzero_hash_matches(renderer.get("sha256")),
        "renderer_identity",
    )
    _validate_repeatability_bindings(
        repeatability,
        candidates,
        renderer,
        fidelity_results,
    )

    shared_tool_keys = {
        "font_pack_sha256",
        "host_tools_identity_sha256",
        "oracle_build_contract_sha256",
        "oracle_image_config_digest",
        "oracle_image_manifest_digest",
        "oracle_libreoffice_artifact_sha256",
        "oracle_lock_file_sha256",
        "pdffonts_sha256",
        "pdfinfo_sha256",
        "pdftoppm_sha256",
        "pdftotext_sha256",
        "renderer_sha256",
    }
    shared_tools = {
        key: fidelity_evidence[0][key]
        for key in shared_tool_keys
    }
    _require(
        all(
            {
                key: evidence[key]
                for key in shared_tool_keys
            }
            == shared_tools
            for evidence in (*fidelity_evidence[1:], authored_evidence)
        ),
        "gate_toolchain_repeatability",
    )
    _require(
        shared_tools["renderer_sha256"] == renderer["sha256"]
        and shared_tools["host_tools_identity_sha256"]
        == host_tools.get("captured_identity_sha256"),
        "gate_toolchain_binding",
    )
    _require(
        authored_evidence["manifest_sha256"]
        == fidelity_bindings[0]["manifest_sha256"]
        and authored_evidence["input_set_sha256"]
        != fidelity_bindings[0]["input_set_sha256"]
        and authored_evidence["feature_map_sha256"]
        != fidelity_bindings[0]["feature_map_sha256"],
        "authored_corpus_binding",
    )

    summary = documents["hosted-summary.json"]
    _require(
        set(summary)
        == {
            "authored_print",
            "baseline_mode",
            "baseline_ratcheting",
            "campaign",
            "container",
            "corpus",
            "evidence_runs",
            "fidelity",
            "font_pack",
            "head_sha",
            "host_tools",
            "metrics",
            "renderer",
            "repeatability",
            "schema",
            "summary",
        },
        "summary_keys",
    )
    _require(summary.get("schema") == HOSTED_CAMPAIGN_SCHEMA, "summary_schema")
    _require(summary.get("head_sha") == head_sha, "summary_head_sha")
    _require(
        summary.get("baseline_mode") == baseline_mode,
        "summary_baseline_mode",
    )
    campaign_summary = summary.get("campaign")
    _require(
        isinstance(campaign_summary, dict)
        and set(campaign_summary)
        == {
            "case_count",
            "mode",
            "parallel_shards",
            "repetitions",
            "sha256",
            "shard_case_counts",
            "shard_count",
            "shard_format_counts",
        }
        and campaign_summary.get("mode") == campaign
        and campaign_summary.get("case_count") == 800
        and campaign_summary.get("repetitions") == 2
        and campaign_summary.get("shard_count") == 4
        and campaign_summary.get("parallel_shards") == 2,
        "summary_campaign",
    )
    shard_counts = campaign_summary.get("shard_case_counts")
    _require(
        isinstance(shard_counts, list)
        and len(shard_counts) == 4
        and sum(shard_counts) == 800
        and all(type(count) is int and 180 <= count <= 220 for count in shard_counts),
        "summary_shards",
    )
    shard_format_counts = campaign_summary["shard_format_counts"]
    _require(
        isinstance(shard_format_counts, list)
        and len(shard_format_counts) == 4
        and all(
            isinstance(row, dict)
            and set(row) == set(EXPECTED_FORMAT_COUNTS)
            and all(
                type(count) is int and 40 <= count <= 60
                for count in row.values()
            )
            for row in shard_format_counts
        )
        and [
            sum(row.values())
            for row in shard_format_counts
        ]
        == shard_counts
        and {
            name: sum(row[name] for row in shard_format_counts)
            for name in EXPECTED_FORMAT_COUNTS
        }
        == EXPECTED_FORMAT_COUNTS,
        "summary_shard_formats",
    )
    candidate_campaign = candidate_campaigns[0]
    _require(
        campaign_summary["sha256"]
        == _canonical_sha256(candidate_campaign),
        "summary_campaign_sha256",
    )
    report_summary = summary.get("summary")
    _require(
        isinstance(report_summary, dict)
        and set(report_summary)
        == {
            "by_classification",
            "by_status",
            "files",
            "input_bytes_considered",
            "warning_counts",
        }
        and report_summary["files"] == candidates[1]["input_files"] == 800
        and report_summary["by_status"] == candidates[1]["statuses"]
        and report_summary["by_classification"]
        == candidates[1]["classifications"]
        and report_summary["warning_counts"]
        == candidates[1]["warning_counts"]
        and type(report_summary["input_bytes_considered"]) is int
        and report_summary["input_bytes_considered"] > 0,
        "summary_coverage",
    )
    summary_corpus = summary.get("corpus")
    _require(
        isinstance(summary_corpus, dict)
        and set(summary_corpus)
        == {
            "acquired_corpus_included",
            "case_count",
            "feature_counts",
            "format_counts",
            "generator",
            "generator_version",
            "group_topology_sha256",
            "input_set_sha256",
            "license",
            "manifest_sha256",
            "profile",
            "render_redistributable",
            "redistribution",
            "rights_tier",
            "schema_version",
            "scope",
            "source_redistributable",
        },
        "summary_corpus_schema",
    )
    _require(
        summary_corpus
        == {
            "acquired_corpus_included": False,
            "case_count": candidate_campaign["case_count"],
            "feature_counts": candidate_campaign["feature_counts"],
            "format_counts": candidate_campaign["format_counts"],
            "generator": "rxls-synthetic-render-corpus",
            "generator_version": "1.5.0",
            "group_topology_sha256": (
                EXPECTED_HOSTED_FULL_GROUP_TOPOLOGY_SHA256
            ),
            "input_set_sha256": (
                EXPECTED_HOSTED_FULL_INPUT_SET_SHA256
            ),
            "license": "MIT",
            "manifest_sha256": candidate_campaign["manifest_sha256"],
            "profile": candidate_campaign["profile"],
            "render_redistributable": True,
            "redistribution": "allowed",
            "rights_tier": "S",
            "schema_version": 1,
            "scope": "project_generated_hosted_acceptance",
            "source_redistributable": True,
        },
        "summary_corpus",
    )
    _require(
        fidelity_bindings[0]["manifest_sha256"]
        == candidate_campaign["manifest_sha256"]
        and fidelity_bindings[0]["input_set_sha256"]
        == EXPECTED_HOSTED_FULL_BINDING_INPUT_SET_SHA256
        and candidate_campaign["input_set_sha256"]
        == EXPECTED_HOSTED_FULL_INPUT_SET_SHA256,
        "candidate_fidelity_corpus_binding",
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
        and container.get("oracle_artifact_sha256")
        == LIBREOFFICE_ARTIFACT_SHA256
        and container.get("oracle_version") == "26.2.3.2",
        "summary_container",
    )
    _validate_font_pack(
        summary.get("font_pack"),
        shared_tools["font_pack_sha256"],
    )
    _require(
        summary.get("metrics") == candidates[1]["cohorts"],
        "summary_metrics",
    )

    baseline_summary = summary.get("baseline_ratcheting")
    _require(
        isinstance(baseline_summary, dict)
        and set(baseline_summary)
        == {
            "applies",
            "candidate_baselines",
            "gates",
            "mode",
            "passed",
            "reviewed_baseline_available",
            "reviewed_warning_policy",
        },
        "summary_baseline",
    )
    _require(baseline_summary.get("mode") == baseline_mode, "summary_ratchet_mode")
    if baseline_mode == "verify":
        _require(
            baseline_summary.get("applies") is True
            and baseline_summary.get("passed") is True
            and baseline_summary.get("reviewed_baseline_available") is True,
            "summary_ratchet",
        )
    else:
        _require(
            baseline_summary.get("applies") is False
            and baseline_summary.get("passed") is True
            and baseline_summary.get("reviewed_baseline_available") is False
            and baseline_summary.get("reviewed_warning_policy") is None,
            "summary_candidate",
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
        if baseline_mode == "verify":
            expected_gate_summary = {
                "baseline_sha256": gate["baseline_sha256"],
                "bytes": len(payloads[f"baseline-gate-{label}.json"]),
                "candidate_sha256": gate["candidate_sha256"],
                "failures": gate["failures"],
                "passed": gate["passed"],
                "sha256": _sha256(payloads[f"baseline-gate-{label}.json"]),
                "warning_policy": gate["warning_policy"],
            }
        else:
            expected_gate_summary = {
                "baseline_sha256": None,
                "bytes": len(payloads[f"baseline-gate-{label}.json"]),
                "candidate_sha256": _canonical_sha256(candidate),
                "failures": [],
                "passed": True,
                "sha256": _sha256(payloads[f"baseline-gate-{label}.json"]),
                "warning_policy": None,
            }
        _require(summary_gates[index] == expected_gate_summary, "summary_gate_identity")
        expected_candidate_summary = {
            "bytes": len(payloads[f"baseline-candidate-{label}.json"]),
            "campaign_sha256": _canonical_sha256(candidate["campaign"]),
            "sha256": _sha256(
                payloads[f"baseline-candidate-{label}.json"]
            ),
            "warning_counts": candidate["warning_counts"],
        }
        _require(
            summary_candidates[index] == expected_candidate_summary,
            "summary_candidate_identity",
        )
    if baseline_mode == "verify":
        _require(
            baseline_summary.get("reviewed_warning_policy")
            == gates[0]["warning_policy"]
            == gates[1]["warning_policy"],
            "summary_warning_policy",
        )

    evidence_runs = summary.get("evidence_runs")
    _require(isinstance(evidence_runs, list) and len(evidence_runs) == 2, "summary_evidence_runs")
    for index, label in enumerate(("a", "b")):
        source_report = fidelity_results[index]["source_report"]
        assert isinstance(source_report, dict)
        candidate_payload = payloads[f"baseline-candidate-{label}.json"]
        gate_payload = payloads[f"baseline-gate-{label}.json"]
        fidelity_payload = payloads[f"fidelity-{label}.json"]
        expected_evidence_run = {
            "baseline_candidate_bytes": len(candidate_payload),
            "baseline_candidate_sha256": _sha256(candidate_payload),
            "baseline_gate_bytes": len(gate_payload),
            "baseline_gate_sha256": _sha256(gate_payload),
            "campaign_sha256": _canonical_sha256(
                candidates[index]["campaign"]
            ),
            "fidelity_gate_bytes": len(fidelity_payload),
            "fidelity_gate_sha256": _sha256(fidelity_payload),
            "report_bytes": source_report["bytes"],
            "report_sha256": source_report["sha256"],
        }
        _require(
            evidence_runs[index] == expected_evidence_run,
            "summary_fidelity_identity",
        )
        expected_fidelity = {
            key: fidelities[index][key]
            for key in ("coverage", "metrics", "passed", "thresholds")
        }
        _require(summary.get("fidelity", [])[index] == expected_fidelity, "summary_fidelity")
    _require(
        repeatability["reports"]["baseline"]
        == {
            "bytes": evidence_runs[0]["report_bytes"],
            "sha256": evidence_runs[0]["report_sha256"],
        }
        and repeatability["reports"]["candidate"]
        == {
            "bytes": evidence_runs[1]["report_bytes"],
            "sha256": evidence_runs[1]["report_sha256"],
        },
        "repeatability_report_binding",
    )
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
        "baseline_mode": baseline_mode,
        "bootstrap_source_commit": contract["bootstrap_source_commit"],
        "build_contract_sha256": build["build_contract_sha256"],
        "campaign": campaign,
        "campaign_sha256": _canonical_sha256(candidate_campaign),
        "head_sha": head_sha,
        "full_cases": 800,
        "lock_file_sha256": build["lock_file_sha256"],
        "oracle_config_digest": build["built_image_id"],
        "oracle_manifest_digest": build["built_manifest_digest"],
        "ratchets": 2,
        "repeatability_sha256": _sha256(
            payloads["repeatability.json"]
        ),
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


def build_adoption_baseline_and_receipt(
    artifact_dir: Path,
    *,
    head_sha: str,
    workflow_run_id: int,
    workflow_run_attempt: int,
    artifact_id: int,
    artifact_name: str,
    artifact_size_bytes: int,
    artifact_digest: str,
    artifact_repository: str,
    oracle_lock: Path = DEFAULT_ORACLE_LOCK,
    oracle_wrapper: Path = DEFAULT_ORACLE_WRAPPER,
) -> tuple[bytes, dict[str, object]]:
    """Derive and attest the reviewed baseline after full artifact validation."""

    validation_report = validate(
        artifact_dir,
        head_sha,
        None,
        campaign="full",
        baseline_mode="candidate",
        workflow_run_id=workflow_run_id,
        workflow_run_attempt=workflow_run_attempt,
        artifact_id=artifact_id,
        artifact_name=artifact_name,
        artifact_size_bytes=artifact_size_bytes,
        artifact_digest=artifact_digest,
        artifact_repository=artifact_repository,
        oracle_lock=oracle_lock,
        oracle_wrapper=oracle_wrapper,
    )
    candidate_a, candidate_a_payload = _read_json(
        artifact_dir / "baseline-candidate-a.json"
    )
    candidate_b, candidate_b_payload = _read_json(
        artifact_dir / "baseline-candidate-b.json"
    )
    repeatability, repeatability_payload = _read_json(
        artifact_dir / "repeatability.json"
    )
    renderer, _ = _read_json(artifact_dir / "renderer.json")
    fidelity_a, _ = _read_json(artifact_dir / "fidelity-a.json")
    fidelity_b, _ = _read_json(artifact_dir / "fidelity-b.json")
    _validate_candidate(candidate_a)
    _validate_candidate(candidate_b)
    _validate_repeatability(repeatability)
    _require(
        set(renderer) == {"bytes", "sha256"}
        and type(renderer.get("bytes")) is int
        and renderer["bytes"] > 0
        and _nonzero_hash_matches(renderer.get("sha256")),
        "renderer_identity",
    )
    fidelity_results = [
        _validate_fidelity_gate(fidelity)
        for fidelity in (fidelity_a, fidelity_b)
    ]
    _validate_repeatability_bindings(
        repeatability,
        [candidate_a, candidate_b],
        renderer,
        fidelity_results,
    )
    baseline_checker = _load_baseline_checker()
    try:
        adopted = baseline_checker.conservative_adoption_baseline(
            candidate_a,
            candidate_b,
            max_score_drift_ppm=_repeatability_score_drift_limits(
                repeatability
            ),
        )
    except baseline_checker.BaselineError as error:
        raise EvidenceError(f"baseline_adoption:{error}") from error
    adopted_payload = baseline_checker.canonical_bytes(adopted)
    _require(
        adopted
        == baseline_checker.validate_ratchet_envelope(
            _strict_json_loads(adopted_payload, "adopted_baseline_json")
        ),
        "adopted_baseline_validation",
    )
    for candidate in (candidate_a, candidate_b):
        _require(
            baseline_checker.compare(adopted, candidate)["passed"] is True,
            "adopted_baseline_candidate",
        )
    candidate_sha256s = sorted(
        [
            _sha256(candidate_a_payload),
            _sha256(candidate_b_payload),
        ]
    )
    _require(
        adopted["source_policy"]["candidate_sha256s"]
        == candidate_sha256s,
        "adopted_baseline_sources",
    )

    campaign = adopted["campaign"]
    fidelity_bindings = [
        {
            "feature_map_sha256": evidence.get("feature_map_sha256"),
            "input_set_sha256": evidence.get("input_set_sha256"),
            "manifest_sha256": evidence.get("manifest_sha256"),
        }
        for evidence in (
            fidelity_results[0]["evidence"],
            fidelity_results[1]["evidence"],
        )
        if isinstance(evidence, dict)
    ]
    _require(
        len(fidelity_bindings) == 2
        and fidelity_bindings[0] == fidelity_bindings[1]
        and fidelity_bindings[0]["manifest_sha256"]
        == campaign["manifest_sha256"]
        and fidelity_bindings[0]["input_set_sha256"]
        == EXPECTED_HOSTED_FULL_BINDING_INPUT_SET_SHA256
        and campaign["input_set_sha256"]
        == EXPECTED_HOSTED_FULL_INPUT_SET_SHA256,
        "adoption_fidelity_binding",
    )
    _require(
        repeatability.get("status") == "pass"
        and repeatability.get("failures") == [],
        "adoption_repeatability",
    )
    receipt: dict[str, object] = {
        "adopted_baseline_sha256": _sha256(adopted_payload),
        "artifact": {
            "digest": artifact_digest,
            "id": artifact_id,
            "name": artifact_name,
            "repository": artifact_repository,
            "size_in_bytes": artifact_size_bytes,
        },
        "baseline_mode": "candidate",
        "campaign": {
            "case_count": campaign["case_count"],
            "feature_map_sha256": fidelity_bindings[0]["feature_map_sha256"],
            "input_set_sha256": campaign["input_set_sha256"],
            "manifest_sha256": campaign["manifest_sha256"],
            "sha256": _canonical_sha256(campaign),
        },
        "candidate_sha256": candidate_sha256s,
        "head_sha": head_sha,
        "passed": True,
        "policy": {
            "candidate_order_independent": True,
            "delta_drift_metrics": [],
            "id": baseline_checker.ADOPTION_POLICY,
            "maximum_score_drift_ppm": (
                baseline_checker.ADOPTION_MAX_SCORE_DRIFT_PPM
            ),
            "observed_score_drift_maximum_ppm": (
                _repeatability_score_drift_limits(repeatability)
            ),
            "score_drift_metrics": sorted(
                baseline_checker.ADOPTION_SCORE_METRICS
            ),
            "source_policy": adopted["source_policy"],
        },
        "previous_baseline_sha256": None,
        "repeatability_sha256": _sha256(repeatability_payload),
        "release_evidence": {
            "campaign_sha256": validation_report["campaign_sha256"],
            "source_reports": [
                fidelity_results[0]["source_report"],
                fidelity_results[1]["source_report"],
            ],
        },
        "schema": ADOPTION_RECEIPT_SCHEMA,
        "workflow": {
            "run_attempt": workflow_run_attempt,
            "run_id": workflow_run_id,
        },
    }
    _require(len(receipt["candidate_sha256"]) == 2, "adoption_candidates")
    _path_neutral(receipt)
    return adopted_payload, receipt


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--download-repository", required=True)
    parser.add_argument("--github-artifact-id", type=int, required=True)
    parser.add_argument("--artifact-name", required=True)
    parser.add_argument("--artifact-size-bytes", type=int, required=True)
    parser.add_argument("--baseline-mode", choices=("candidate", "verify"), required=True)
    parser.add_argument("--campaign", choices=("full",), required=True)
    parser.add_argument("--head-sha", required=True)
    parser.add_argument("--reviewed-baseline", type=Path)
    parser.add_argument("--workflow-run-id", type=int, required=True)
    parser.add_argument("--workflow-run-attempt", type=int, required=True)
    parser.add_argument("--artifact-digest", required=True)
    parser.add_argument("--adopt-baseline", type=Path)
    parser.add_argument("--write-report", type=Path)
    args = parser.parse_args()
    adopted_payload: bytes | None = None
    adoption_destination: Path | None = None
    receipt_path: Path | None = None
    try:
        _validate_artifact_binding(
            head_sha=args.head_sha,
            campaign=args.campaign,
            baseline_mode=args.baseline_mode,
            workflow_run_id=args.workflow_run_id,
            workflow_run_attempt=args.workflow_run_attempt,
            artifact_id=args.github_artifact_id,
            artifact_name=args.artifact_name,
            artifact_size_bytes=args.artifact_size_bytes,
            artifact_digest=args.artifact_digest,
            repository=args.download_repository,
        )
        if args.adopt_baseline is not None:
            _require(
                args.baseline_mode == "candidate"
                and args.campaign == "full"
                and args.reviewed_baseline is None,
                "adoption_mode",
            )
            _require(args.write_report is not None, "adoption_receipt_required")
            adoption_destination = validate_adoption_checkout(
                args.adopt_baseline,
                args.head_sha,
            )
            receipt_path = args.write_report.expanduser()
            if not receipt_path.is_absolute():
                receipt_path = Path.cwd() / receipt_path
            _require(
                receipt_path.resolve(strict=False) != adoption_destination,
                "adoption_receipt_destination",
            )
            authenticate_candidate_run_artifact(
                repository=args.download_repository,
                head_sha=args.head_sha,
                workflow_run_id=args.workflow_run_id,
                workflow_run_attempt=args.workflow_run_attempt,
                artifact_id=args.github_artifact_id,
                artifact_name=args.artifact_name,
                artifact_size_bytes=args.artifact_size_bytes,
                artifact_digest=args.artifact_digest,
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
                campaign=args.campaign,
                baseline_mode=args.baseline_mode,
                workflow_run_id=args.workflow_run_id,
                workflow_run_attempt=args.workflow_run_attempt,
                artifact_id=args.github_artifact_id,
                artifact_name=args.artifact_name,
                artifact_size_bytes=args.artifact_size_bytes,
                artifact_digest=args.artifact_digest,
                artifact_repository=args.download_repository,
            )
            if adoption_destination is not None:
                adopted_payload, report = build_adoption_baseline_and_receipt(
                    artifact_dir,
                    head_sha=args.head_sha,
                    workflow_run_id=args.workflow_run_id,
                    workflow_run_attempt=args.workflow_run_attempt,
                    artifact_id=args.github_artifact_id,
                    artifact_name=args.artifact_name,
                    artifact_size_bytes=args.artifact_size_bytes,
                    artifact_digest=args.artifact_digest,
                    artifact_repository=args.download_repository,
                )
        if adoption_destination is not None:
            assert (
                adopted_payload is not None
                and args.write_report is not None
                and receipt_path is not None
            )
            validate_adoption_checkout(
                adoption_destination,
                args.head_sha,
            )
            write_adoption_pair_atomic(
                adoption_destination,
                adopted_payload,
                receipt_path,
                (json.dumps(report, indent=2, sort_keys=True) + "\n").encode(
                    "utf-8"
                ),
            )
        if args.write_report is not None:
            if adoption_destination is None:
                write_atomic(
                    args.write_report,
                    (json.dumps(report, indent=2, sort_keys=True) + "\n").encode(
                        "utf-8"
                    ),
                )
    except (EvidenceError, OSError) as error:
        print(f"render release prerequisites: {error}", file=sys.stderr)
        return 1
    if adoption_destination is not None:
        print(
            "render baseline adoption: "
            f"head_sha={report['head_sha']} "
            f"run_id={report['workflow']['run_id']} "
            f"adopted_sha256={report['adopted_baseline_sha256']} passed=true"
        )
        return 0
    print(
        "render release prerequisites: "
        f"head_sha={report['head_sha']} campaign={report['campaign']} "
        f"baseline_mode={report['baseline_mode']} full_cases=800 "
        "ratchets=2 passed=true"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
