#!/usr/bin/env python3
"""Build and verify exact-SHA browser-rendering release evidence.

The hosted browser workflow retains detailed logs only inside its runner.  This
checker converts those logs into one bounded, path-neutral aggregate document,
then authenticates and revalidates that document before a tagged renderer
package can be published.
"""

from __future__ import annotations

import argparse
import base64
import binascii
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
import zipfile

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from check_render_oracle_release_evidence import (
    EvidenceError as ArtifactDownloadError,
    download_artifact_archive,
)


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_LOCK = ROOT / "bindings" / "render-wasm" / "toolchain-lock.json"
DEFAULT_PACKAGE = ROOT / "bindings" / "render-wasm" / "package.json"
SCHEMA = "rxls.render-browser-evidence.v4"
PREREQUISITE_SCHEMA = "rxls.render-browser-release-prerequisites.v4"
BEHAVIOR_SCHEMA = "rxls.render-browser-behavior.v1"
EXPECTED_REPOSITORY = "HyunjoJung/rxls"
EXPECTED_RUNTIME_TEXT = b"PASS pinned Chromium runtime closure resolved\n"
EXPECTED_NETWORK_ERROR = "net::ERR_INTERNET_DISCONNECTED"
HARD_STOP_DEADLINE_MS = 2000
SUPPORTED_PLATFORMS = {"darwin", "linux"}
HEAD_SHA_RE = re.compile(r"[0-9a-f]{40}\Z")
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
ARTIFACT_DIGEST_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")
SHA1_RE = re.compile(r"[0-9a-f]{40}\Z")
INTEGRITY_RE = re.compile(r"sha512-[A-Za-z0-9+/]+={0,2}\Z")
MAX_LOG_BYTES = 1024 * 1024
MAX_JSON_BYTES = 1024 * 1024
MAX_ARCHIVE_BYTES = 2 * 1024 * 1024
MAX_ARTIFACT_BYTES = 1024 * 1024
MAX_BEHAVIOR_PROOF_BYTES = 32 * 1024
MAX_INTEGER = (1 << 63) - 1
SUMMARY_NAME = "browser-summary.json"
EXPECTED_PACKAGE_FILES = {
    "LICENSE",
    "README.md",
    "THIRD_PARTY_NOTICES.txt",
    "js/client.mjs",
    "js/protocol.mjs",
    "js/worker-runtime.mjs",
    "js/worker.mjs",
    "package.json",
    "pkg/rxls_render_wasm.d.ts",
    "pkg/rxls_render_wasm.js",
    "pkg/rxls_render_wasm_bg.wasm",
    "pkg/rxls_render_wasm_bg.wasm.d.ts",
}

MODE_DESCRIPTIONS = {
    "source": (
        "worker/WASM rich font/image, CSP, limits, virtual tile/page "
        "and hard-stop smoke"
    ),
    "installed": (
        "installed package rich font/image, CSP, limits, virtual tile/page "
        "and hard-stop smoke"
    ),
}

PASS_RE = re.compile(
    r"^PASS (?P<product>Google Chrome(?: for Testing)?) "
    r"(?P<version>[0-9]+(?:\.[0-9]+){3}) "
    r"(?P<description>[^;\r\n]+); "
    r"heap baseline=(?P<heap_baseline>[0-9]+) "
    r"peak=(?P<heap_peak>[0-9]+) "
    r"retained=(?P<heap_retained>[0-9]+) "
    r"growth=(?P<heap_growth>[0-9]+) bytes; "
    r"rss baseline=(?P<rss_baseline>[0-9]+) "
    r"peak=(?P<rss_peak>[0-9]+) "
    r"peak-growth=(?P<rss_peak_growth>[0-9]+) "
    r"retained=(?P<rss_retained>[0-9]+) "
    r"retained-growth=(?P<rss_retained_growth>[0-9]+) bytes; "
    r"hard-stop target=(?P<elapsed>[0-9]+)/(?P<deadline>[0-9]+)ms "
    r"wasm=(?P<wasm>http://127\.0\.0\.1:[0-9]{1,5}/[A-Za-z0-9._/-]+); "
    r"CSP Network=(?P<network_error>[A-Za-z0-9:_-]+)$"
)
WASM_URL_RE = re.compile(
    r"http://127\.0\.0\.1:(?P<port>[0-9]{1,5})(?P<path>/[A-Za-z0-9._/-]+)\Z"
)
EXPECTED_WASM_PATHS = {
    "source": "/pkg/rxls_render_wasm_bg.wasm",
    "installed": "/installed-package/pkg/rxls_render_wasm_bg.wasm",
}


class BrowserEvidenceError(RuntimeError):
    """Browser evidence is malformed or does not prove the required gate."""


def _require(condition: bool, code: str) -> None:
    if not condition:
        raise BrowserEvidenceError(code)


def _positive_int(value: object, *, maximum: int = MAX_INTEGER) -> bool:
    return (
        isinstance(value, int)
        and not isinstance(value, bool)
        and 0 < value <= maximum
    )


def _nonnegative_int(value: object, *, maximum: int = MAX_INTEGER) -> bool:
    return (
        isinstance(value, int)
        and not isinstance(value, bool)
        and 0 <= value <= maximum
    )


def _read_bytes(path: Path, maximum: int, code: str) -> bytes:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise BrowserEvidenceError(code) from error
    _require(
        stat.S_ISREG(metadata.st_mode)
        and not path.is_symlink()
        and 0 < metadata.st_size <= maximum,
        code,
    )
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise BrowserEvidenceError(code) from error
    _require(len(payload) == metadata.st_size, code)
    return payload


def _object_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    document: dict[str, object] = {}
    for key, value in pairs:
        if key in document:
            raise BrowserEvidenceError("duplicate_json_key")
        document[key] = value
    return document


def _reject_constant(_value: str) -> None:
    raise BrowserEvidenceError("invalid_json_constant")


def _json_payload(path: Path, maximum: int, code: str) -> tuple[Any, bytes]:
    payload = _read_bytes(path, maximum, code)
    try:
        document = json.loads(
            payload,
            object_pairs_hook=_object_pairs,
            parse_constant=_reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise BrowserEvidenceError(code) from error
    return document, payload


def _canonical_payload(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def _sha256(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def _validate_behavior_contract(document: object) -> dict[str, object]:
    _require(
        isinstance(document, dict)
        and set(document)
        == {
            "schema",
            "fixture",
            "capabilitiesSha256",
            "cancellation",
            "progress",
            "limits",
            "tile",
            "pages",
            "hardStop",
            "network",
        }
        and document.get("schema") == BEHAVIOR_SCHEMA,
        "behavior_shape",
    )
    capabilities_sha256 = document.get("capabilitiesSha256")
    _require(
        isinstance(capabilities_sha256, str)
        and SHA256_RE.fullmatch(capabilities_sha256) is not None,
        "behavior_capabilities",
    )
    fixture = document.get("fixture")
    _require(
        isinstance(fixture, dict)
        and set(fixture)
        == {
            "workbookBytes",
            "workbookSha256",
            "fontPackSha256",
            "renderedImageBytes",
            "renderedImageSha256",
        }
        and _positive_int(fixture.get("workbookBytes"), maximum=32 * 1024 * 1024)
        and _positive_int(
            fixture.get("renderedImageBytes"), maximum=16 * 1024 * 1024
        )
        and all(
            isinstance(fixture.get(field), str)
            and SHA256_RE.fullmatch(fixture[field]) is not None
            for field in (
                "workbookSha256",
                "fontPackSha256",
                "renderedImageSha256",
            )
        ),
        "behavior_fixture",
    )
    _require(
        document.get("cancellation")
        == {
            "abortSignal": "AbortError",
            "activeOpen": "AbortError",
            "reopenedDocument": True,
        },
        "behavior_cancellation",
    )
    _require(
        document.get("progress")
        == [
            {"completed": 0, "total": 3, "stage": "accepted"},
            {"completed": 1, "total": 3, "stage": "parsing"},
            {"completed": 2, "total": 3, "stage": "finalizing"},
            {"completed": 3, "total": 3, "stage": "complete"},
        ],
        "behavior_progress",
    )
    _require(
        document.get("limits")
        == {
            "fontFiles": {"code": "limit_exceeded", "resource": "fontFiles"},
            "hardPage": {"code": "limit_exceeded", "resource": "pages"},
            "dpi": {"code": "dpi_out_of_range", "resource": None},
            "outputBytes": {
                "code": "limit_exceeded",
                "resource": "output_bytes",
            },
            "imageCount": {"code": "limit_exceeded", "resource": "maxImages"},
            "imageBytes": {
                "code": "limit_exceeded",
                "resource": "maxImageBytes",
            },
        },
        "behavior_limits",
    )
    tile = document.get("tile")
    _require(
        isinstance(tile, dict)
        and set(tile)
        == {
            "firstRow",
            "firstCol",
            "lastRow",
            "lastCol",
            "bytes",
            "sha256",
        }
        and {
            "firstRow": tile.get("firstRow"),
            "firstCol": tile.get("firstCol"),
            "lastRow": tile.get("lastRow"),
            "lastCol": tile.get("lastCol"),
        }
        == {"firstRow": 0, "firstCol": 0, "lastRow": 63, "lastCol": 31}
        and _positive_int(tile.get("bytes"), maximum=16 * 1024 * 1024)
        and tile["bytes"] >= 250_000
        and isinstance(tile.get("sha256"), str)
        and SHA256_RE.fullmatch(tile["sha256"]) is not None,
        "behavior_tile",
    )
    pages = document.get("pages")
    _require(
        isinstance(pages, dict)
        and set(pages) == {"count", "paper", "first", "nonzero", "outOfRange"}
        and _positive_int(pages.get("count"), maximum=512)
        and pages["count"] >= 8,
        "behavior_pages",
    )
    paper = pages.get("paper")
    _require(
        isinstance(paper, dict)
        and set(paper) == {"widthRaw", "heightRaw"}
        and _positive_int(paper.get("widthRaw"))
        and _positive_int(paper.get("heightRaw")),
        "behavior_paper",
    )
    _validate_behavior_svg_page(
        pages.get("first"),
        expected_index=0,
        paper=paper,
        require_png=False,
    )
    nonzero_index = pages["count"] - 1
    _validate_behavior_svg_page(
        pages.get("nonzero"),
        expected_index=nonzero_index,
        paper=paper,
        require_png=True,
    )
    _require(
        pages["first"]["svg"]["sha256"] != pages["nonzero"]["svg"]["sha256"],
        "behavior_page_isolation",
    )
    _require(
        pages.get("outOfRange")
        == {
            "pageIndex": pages["count"],
            "code": "page_index_out_of_range",
        },
        "behavior_page_range",
    )
    _require(
        document.get("hardStop")
        == {"deadlineMs": HARD_STOP_DEADLINE_MS, "rejectedRequests": 2},
        "behavior_hard_stop",
    )
    _require(
        document.get("network")
        == {
            "cspNegativeControl": True,
            "unexpectedExternalResources": 0,
        },
        "behavior_network",
    )
    _require(
        len(_canonical_payload(document)) <= MAX_BEHAVIOR_PROOF_BYTES,
        "behavior_size",
    )
    return document


def _validate_behavior_svg_page(
    page: object,
    *,
    expected_index: int,
    paper: dict[str, object],
    require_png: bool,
) -> None:
    expected_keys = {
        "pageIndex",
        "responsePageIndex",
        "pageMapSha256",
        "svg",
    }
    if require_png:
        expected_keys.add("png")
    _require(
        isinstance(page, dict)
        and set(page) == expected_keys
        and page.get("pageIndex") == expected_index
        and page.get("responsePageIndex") == expected_index
        and isinstance(page.get("pageMapSha256"), str)
        and SHA256_RE.fullmatch(page["pageMapSha256"]) is not None,
        "behavior_page_identity",
    )
    svg = page.get("svg")
    expected_svg_keys = {"bytes", "sha256", "widthRaw", "heightRaw"}
    if require_png:
        expected_svg_keys.add("repeatSha256")
    _require(
        isinstance(svg, dict)
        and set(svg) == expected_svg_keys
        and _positive_int(svg.get("bytes"), maximum=16 * 1024 * 1024)
        and isinstance(svg.get("sha256"), str)
        and SHA256_RE.fullmatch(svg["sha256"]) is not None
        and svg.get("widthRaw") == paper["widthRaw"]
        and svg.get("heightRaw") == paper["heightRaw"],
        "behavior_page_svg",
    )
    if not require_png:
        return
    _require(
        isinstance(svg.get("repeatSha256"), str)
        and svg["repeatSha256"] == svg["sha256"],
        "behavior_page_repeat",
    )
    png = page.get("png")
    _require(
        isinstance(png, dict)
        and set(png) == {"bytes", "sha256", "width", "height", "dpi"}
        and _positive_int(png.get("bytes"), maximum=16 * 1024 * 1024)
        and isinstance(png.get("sha256"), str)
        and SHA256_RE.fullmatch(png["sha256"]) is not None
        and png.get("dpi") == 96
        and png.get("width") == _raster_dimension(paper["widthRaw"], 96)
        and png.get("height") == _raster_dimension(paper["heightRaw"], 96),
        "behavior_page_png",
    )


def _raster_dimension(raw: object, dpi: int) -> int:
    _require(_positive_int(raw) and _positive_int(dpi), "behavior_raster_dimension")
    numerator = raw * dpi
    denominator = 1024 * 96
    _require(numerator <= MAX_INTEGER, "behavior_raster_dimension")
    return (numerator + denominator - 1) // denominator


def _load_contract(lock_path: Path, package_path: Path) -> dict[str, object]:
    lock, lock_payload = _json_payload(lock_path, MAX_JSON_BYTES, "toolchain_lock")
    package, package_payload = _json_payload(
        package_path, MAX_JSON_BYTES, "package_metadata"
    )
    _require(
        isinstance(lock, dict)
        and lock.get("schema") == "rxls.render-browser-toolchain.v2",
        "toolchain_lock",
    )
    chromium = lock.get("chromium")
    _require(
        isinstance(chromium, dict)
        and set(chromium) == {
            "archive",
            "heapGate",
            "product",
            "testingProduct",
            "version",
        },
        "toolchain_lock",
    )
    archive = chromium.get("archive")
    heap_gate = chromium.get("heapGate")
    _require(
        isinstance(archive, dict)
        and set(archive) == {"file", "platform", "sha256", "sizeBytes"}
        and archive.get("platform") == "linux64"
        and archive.get("file") == "chrome-linux64.zip"
        and _positive_int(archive.get("sizeBytes"), maximum=512 * 1024 * 1024)
        and isinstance(archive.get("sha256"), str)
        and SHA256_RE.fullmatch(archive["sha256"]) is not None,
        "toolchain_lock",
    )
    _require(
        isinstance(heap_gate, dict)
        and set(heap_gate) == {
            "maxAccountedBytes",
            "maxProcessTreePeakGrowthBytes",
            "maxProcessTreeRetainedGrowthBytes",
            "maxRetainedGrowthBytes",
            "platformOverrides",
        }
        and _positive_int(heap_gate.get("maxAccountedBytes"))
        and _positive_int(heap_gate.get("maxRetainedGrowthBytes"))
        and heap_gate["maxRetainedGrowthBytes"] <= heap_gate["maxAccountedBytes"],
        "toolchain_lock",
    )
    process_peak_growth = heap_gate.get("maxProcessTreePeakGrowthBytes")
    process_retained_growth = heap_gate.get("maxProcessTreeRetainedGrowthBytes")
    platform_overrides = heap_gate.get("platformOverrides")
    _require(
        _positive_int(process_peak_growth)
        and _positive_int(process_retained_growth)
        and process_retained_growth <= process_peak_growth
        and isinstance(platform_overrides, dict)
        and set(platform_overrides) == {"darwin"}
        and isinstance(platform_overrides.get("darwin"), dict)
        and set(platform_overrides["darwin"])
        == {
            "maxProcessTreePeakGrowthBytes",
            "maxProcessTreeRetainedGrowthBytes",
        }
        and _positive_int(
            platform_overrides["darwin"].get("maxProcessTreePeakGrowthBytes")
        )
        and _positive_int(
            platform_overrides["darwin"].get(
                "maxProcessTreeRetainedGrowthBytes"
            )
        )
        and platform_overrides["darwin"]["maxProcessTreeRetainedGrowthBytes"]
        <= platform_overrides["darwin"]["maxProcessTreePeakGrowthBytes"],
        "toolchain_lock",
    )
    _require(
        chromium.get("product") == "Google Chrome"
        and chromium.get("testingProduct") == "Google Chrome for Testing"
        and isinstance(chromium.get("version"), str)
        and re.fullmatch(r"[0-9]+(?:\.[0-9]+){3}", chromium["version"]) is not None,
        "toolchain_lock",
    )
    _require(
        isinstance(package, dict)
        and package.get("name") == "@rxls/render-worker"
        and isinstance(package.get("version"), str)
        and re.fullmatch(
            r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)",
            package["version"],
        )
        is not None,
        "package_metadata",
    )
    return {
        "archive_sha256": archive["sha256"],
        "archive_size_bytes": archive["sizeBytes"],
        "heap": {
            "max_accounted_bytes": heap_gate["maxAccountedBytes"],
            "max_process_tree_retained_growth_bytes": (
                heap_gate["maxProcessTreeRetainedGrowthBytes"]
            ),
            "max_process_tree_peak_growth_bytes": (
                heap_gate["maxProcessTreePeakGrowthBytes"]
            ),
            "max_retained_growth_bytes": heap_gate["maxRetainedGrowthBytes"],
        },
        "platform_overrides": {
            "darwin": {
                "max_process_tree_retained_growth_bytes": (
                    platform_overrides["darwin"][
                        "maxProcessTreeRetainedGrowthBytes"
                    ]
                ),
                "max_process_tree_peak_growth_bytes": (
                    platform_overrides["darwin"][
                        "maxProcessTreePeakGrowthBytes"
                    ]
                ),
            }
        },
        "package_name": package["name"],
        "package_version": package["version"],
        "product": chromium["testingProduct"],
        "toolchain_lock_sha256": _sha256(lock_payload),
        "package_metadata_sha256": _sha256(package_payload),
        "version": chromium["version"],
    }


def _platform_limits(
    contract: dict[str, object], platform: str
) -> dict[str, int]:
    _require(platform in SUPPORTED_PLATFORMS, "platform")
    limits = dict(contract["heap"])
    override = contract["platform_overrides"].get(platform)
    if override is not None:
        limits.update(override)
    return limits


def _parse_mode_log(
    path: Path,
    mode: str,
    contract: dict[str, object],
    limits: dict[str, int],
) -> tuple[dict[str, object], dict[str, object]]:
    payload = _read_bytes(path, MAX_LOG_BYTES, f"{mode}_log")
    try:
        text = payload.decode("utf-8")
    except UnicodeDecodeError as error:
        raise BrowserEvidenceError(f"{mode}_log_encoding") from error
    _require("\x00" not in text and "\x1b" not in text, f"{mode}_log_control")
    _require(
        not any(line.startswith("FAIL ") for line in text.splitlines()),
        f"{mode}_log_failure",
    )
    nonblank = [line for line in text.splitlines() if line.strip()]
    pass_lines = [line for line in nonblank if line.startswith("PASS ")]
    _require(
        len(pass_lines) == 1 and nonblank[-1] == pass_lines[0],
        f"{mode}_pass_line",
    )
    proof_lines = [line for line in nonblank if line.startswith("PROOF ")]
    _require(
        len(proof_lines) == 1
        and len(nonblank) >= 2
        and nonblank[-2] == proof_lines[0],
        f"{mode}_behavior_line",
    )
    proof_payload = proof_lines[0].removeprefix("PROOF ").encode("utf-8")
    _require(
        0 < len(proof_payload) <= MAX_BEHAVIOR_PROOF_BYTES,
        f"{mode}_behavior_size",
    )
    try:
        behavior = json.loads(
            proof_payload,
            object_pairs_hook=_object_pairs,
            parse_constant=_reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise BrowserEvidenceError(f"{mode}_behavior_json") from error
    behavior = _validate_behavior_contract(behavior)
    behavior_sha256 = _sha256(_canonical_payload(behavior))
    match = PASS_RE.fullmatch(pass_lines[0])
    _require(match is not None, f"{mode}_pass_line")
    values = match.groupdict()
    _require(
        values["product"] == contract["product"]
        and values["version"] == contract["version"]
        and values["description"] == MODE_DESCRIPTIONS[mode],
        f"{mode}_runtime_identity",
    )
    heap_baseline = int(values["heap_baseline"])
    heap_peak = int(values["heap_peak"])
    heap_retained = int(values["heap_retained"])
    heap_growth = int(values["heap_growth"])
    rss_baseline = int(values["rss_baseline"])
    rss_peak = int(values["rss_peak"])
    rss_peak_growth = int(values["rss_peak_growth"])
    rss_retained = int(values["rss_retained"])
    rss_retained_growth = int(values["rss_retained_growth"])
    elapsed = int(values["elapsed"])
    deadline = int(values["deadline"])
    _require(
        all(
            _nonnegative_int(value)
            for value in (
                heap_baseline,
                heap_peak,
                heap_retained,
                heap_growth,
                elapsed,
                deadline,
            )
        )
        and heap_baseline > 0
        and heap_retained > 0
        and heap_peak >= max(heap_baseline, heap_retained)
        and heap_growth == max(0, heap_retained - heap_baseline)
        and heap_peak <= limits["max_accounted_bytes"]
        and heap_growth <= limits["max_retained_growth_bytes"],
        f"{mode}_heap",
    )
    _require(
        all(
            _nonnegative_int(value)
            for value in (
                rss_baseline,
                rss_peak,
                rss_peak_growth,
                rss_retained,
                rss_retained_growth,
            )
        )
        and rss_baseline > 0
        and rss_retained > 0
        and rss_peak >= max(rss_baseline, rss_retained)
        and rss_peak_growth == rss_peak - rss_baseline
        and rss_retained_growth == max(0, rss_retained - rss_baseline)
        and rss_peak_growth
        <= limits["max_process_tree_peak_growth_bytes"]
        and rss_retained_growth
        <= limits["max_process_tree_retained_growth_bytes"],
        f"{mode}_rss",
    )
    wasm = WASM_URL_RE.fullmatch(values["wasm"])
    _require(
        wasm is not None
        and 0 < int(wasm["port"]) <= 65535
        and wasm["path"] == EXPECTED_WASM_PATHS[mode],
        f"{mode}_wasm",
    )
    _require(
        values["network_error"] == EXPECTED_NETWORK_ERROR,
        f"{mode}_network",
    )
    _require(
        deadline == HARD_STOP_DEADLINE_MS and 0 < elapsed <= deadline,
        f"{mode}_hard_stop",
    )
    return {
        "behavior_sha256": behavior_sha256,
        "hard_stop": {
            "deadline_ms": deadline,
            "elapsed_ms": elapsed,
            "future_requests_rejected": True,
            "rejected_requests": 2,
            "target_destroyed": True,
            "wasm_frame_confirmed": True,
        },
        "heap": {
            "baseline_bytes": heap_baseline,
            "peak_bytes": heap_peak,
            "retained_bytes": heap_retained,
            "retained_growth_bytes": heap_growth,
        },
        "log_sha256": _sha256(payload),
        "network": {
            "csp_negative_control": True,
            "error_text": EXPECTED_NETWORK_ERROR,
            "offline_control_intercepted": True,
            "response_received": False,
            "sink_requests": 0,
        },
        "process_tree_rss": {
            "baseline_bytes": rss_baseline,
            "peak_bytes": rss_peak,
            "peak_growth_bytes": rss_peak_growth,
            "retained_bytes": rss_retained,
            "retained_growth_bytes": rss_retained_growth,
        },
        "status": "pass",
    }, behavior


def _validate_pack(
    npm_pack_path: Path,
    archive_path: Path,
    contract: dict[str, object],
) -> dict[str, object]:
    document, npm_payload = _json_payload(
        npm_pack_path, MAX_JSON_BYTES, "npm_pack_json"
    )
    _require(
        isinstance(document, list)
        and len(document) == 1
        and isinstance(document[0], dict),
        "npm_pack_json",
    )
    packed = document[0]
    version = contract["package_version"]
    expected_filename = f"rxls-render-worker-{version}.tgz"
    _require(
        packed.get("name") == contract["package_name"]
        and packed.get("version") == version
        and packed.get("filename") == expected_filename
        and packed.get("entryCount") == 12
        and _positive_int(packed.get("size"), maximum=2 * 1024 * 1024)
        and _positive_int(packed.get("unpackedSize"), maximum=5 * 1024 * 1024)
        and isinstance(packed.get("shasum"), str)
        and SHA1_RE.fullmatch(packed["shasum"]) is not None
        and isinstance(packed.get("integrity"), str)
        and INTEGRITY_RE.fullmatch(packed["integrity"]) is not None,
        "npm_pack_contract",
    )
    files = packed.get("files")
    _require(
        isinstance(files, list)
        and len(files) == 12
        and all(
            isinstance(row, dict)
            and isinstance(row.get("path"), str)
            and _positive_int(row.get("size"), maximum=5 * 1024 * 1024)
            for row in files
        ),
        "npm_pack_files",
    )
    paths = [row["path"] for row in files]
    files_by_path = {row["path"]: row for row in files}
    _require(
        len(paths) == len(set(paths))
        and set(paths) == EXPECTED_PACKAGE_FILES
        and sum(row["size"] for row in files) == packed["unpackedSize"]
        and files_by_path["pkg/rxls_render_wasm_bg.wasm"]["size"]
        <= 4 * 1024 * 1024
        and not any(
            path.startswith("tests/")
            or path.startswith("/")
            or "\\" in path
            or ".." in Path(path).parts
            for path in paths
        ),
        "npm_pack_files",
    )
    archive = _read_bytes(archive_path, MAX_ARCHIVE_BYTES, "npm_archive")
    _require(
        archive_path.name == expected_filename
        and len(archive) == packed["size"],
        "npm_archive",
    )
    try:
        decoded_integrity = base64.b64decode(
            packed["integrity"].removeprefix("sha512-"),
            validate=True,
        )
    except (ValueError, binascii.Error) as error:
        raise BrowserEvidenceError("npm_integrity") from error
    _require(
        len(decoded_integrity) == 64
        and decoded_integrity == hashlib.sha512(archive).digest()
        and packed["shasum"] == hashlib.sha1(archive).hexdigest(),
        "npm_integrity",
    )
    return {
        "archive_bytes": len(archive),
        "archive_sha256": _sha256(archive),
        "entry_count": packed["entryCount"],
        "integrity": packed["integrity"],
        "name": packed["name"],
        "npm_pack_json_sha256": _sha256(npm_payload),
        "shasum": packed["shasum"],
        "unpacked_bytes": packed["unpackedSize"],
        "version": packed["version"],
    }


def build_summary(
    *,
    source_log: Path,
    installed_log: Path,
    runtime_evidence: Path,
    npm_pack: Path,
    npm_archive: Path,
    head_sha: str,
    platform: str,
    repository: str,
    workflow_run_id: int,
    workflow_run_attempt: int,
    lock_path: Path = DEFAULT_LOCK,
    package_path: Path = DEFAULT_PACKAGE,
) -> dict[str, object]:
    _require(HEAD_SHA_RE.fullmatch(head_sha) is not None, "head_sha")
    _require(platform in SUPPORTED_PLATFORMS, "platform")
    _require(repository == EXPECTED_REPOSITORY, "repository")
    _require(_positive_int(workflow_run_id), "workflow_run_id")
    _require(_positive_int(workflow_run_attempt), "workflow_run_attempt")
    runtime_payload = _read_bytes(
        runtime_evidence, MAX_LOG_BYTES, "runtime_evidence"
    )
    _require(runtime_payload == EXPECTED_RUNTIME_TEXT, "runtime_evidence")
    contract = _load_contract(lock_path, package_path)
    limits = _platform_limits(contract, platform)
    installed_mode, installed_behavior = _parse_mode_log(
        installed_log, "installed", contract, limits
    )
    source_mode, source_behavior = _parse_mode_log(
        source_log, "source", contract, limits
    )
    _require(source_behavior == installed_behavior, "behavior_parity")
    behavior_sha256 = _sha256(_canonical_payload(source_behavior))
    return {
        "behavior": {
            "contract": source_behavior,
            "sha256": behavior_sha256,
            "source_installed_equal": True,
        },
        "chromium": {
            "archive_sha256": contract["archive_sha256"],
            "archive_size_bytes": contract["archive_size_bytes"],
            "product": contract["product"],
            "runtime_closure_status": "pass",
            "version": contract["version"],
        },
        "head_sha": head_sha,
        "limits": limits,
        "modes": {
            "installed": installed_mode,
            "source": source_mode,
        },
        "package": _validate_pack(npm_pack, npm_archive, contract),
        "package_metadata_sha256": contract["package_metadata_sha256"],
        "platform": platform,
        "repository": repository,
        "schema": SCHEMA,
        "toolchain_lock_sha256": contract["toolchain_lock_sha256"],
        "workflow": {
            "run_attempt": workflow_run_attempt,
            "run_id": workflow_run_id,
        },
    }


def validate_summary(
    document: object,
    *,
    head_sha: str,
    platform: str,
    repository: str,
    workflow_run_id: int,
    workflow_run_attempt: int,
    lock_path: Path = DEFAULT_LOCK,
    package_path: Path = DEFAULT_PACKAGE,
) -> dict[str, object]:
    _require(isinstance(document, dict), "summary_shape")
    _require(
        set(document)
        == {
            "behavior",
            "chromium",
            "head_sha",
            "limits",
            "modes",
            "package",
            "package_metadata_sha256",
            "platform",
            "repository",
            "schema",
            "toolchain_lock_sha256",
            "workflow",
        },
        "summary_fields",
    )
    _require(
        document.get("schema") == SCHEMA
        and document.get("head_sha") == head_sha
        and document.get("platform") == platform
        and platform in SUPPORTED_PLATFORMS
        and document.get("repository") == repository == EXPECTED_REPOSITORY,
        "summary_binding",
    )
    workflow = document.get("workflow")
    _require(
        isinstance(workflow, dict)
        and workflow
        == {
            "run_attempt": workflow_run_attempt,
            "run_id": workflow_run_id,
        },
        "summary_workflow",
    )
    contract = _load_contract(lock_path, package_path)
    limits = _platform_limits(contract, platform)
    _require(
        document.get("toolchain_lock_sha256")
        == contract["toolchain_lock_sha256"]
        and document.get("package_metadata_sha256")
        == contract["package_metadata_sha256"]
        and document.get("limits") == limits,
        "summary_contract",
    )
    _require(
        document.get("chromium")
        == {
            "archive_sha256": contract["archive_sha256"],
            "archive_size_bytes": contract["archive_size_bytes"],
            "product": contract["product"],
            "runtime_closure_status": "pass",
            "version": contract["version"],
        },
        "summary_chromium",
    )
    behavior = document.get("behavior")
    _require(
        isinstance(behavior, dict)
        and set(behavior) == {"contract", "sha256", "source_installed_equal"}
        and behavior.get("source_installed_equal") is True
        and isinstance(behavior.get("sha256"), str)
        and SHA256_RE.fullmatch(behavior["sha256"]) is not None,
        "summary_behavior",
    )
    behavior_contract = _validate_behavior_contract(behavior.get("contract"))
    _require(
        behavior["sha256"] == _sha256(_canonical_payload(behavior_contract)),
        "summary_behavior_digest",
    )
    modes = document.get("modes")
    _require(
        isinstance(modes, dict) and set(modes) == {"installed", "source"},
        "summary_modes",
    )
    for mode in ("installed", "source"):
        evidence = modes[mode]
        _require(
            isinstance(evidence, dict)
            and set(evidence)
            == {
                "behavior_sha256",
                "hard_stop",
                "heap",
                "log_sha256",
                "network",
                "process_tree_rss",
                "status",
            }
            and evidence.get("status") == "pass"
            and evidence.get("behavior_sha256") == behavior["sha256"]
            and isinstance(evidence.get("log_sha256"), str)
            and SHA256_RE.fullmatch(evidence["log_sha256"]) is not None,
            f"summary_{mode}",
        )
        heap = evidence.get("heap")
        hard_stop = evidence.get("hard_stop")
        network = evidence.get("network")
        process_tree_rss = evidence.get("process_tree_rss")
        _require(
            isinstance(heap, dict)
            and set(heap)
            == {
                "baseline_bytes",
                "peak_bytes",
                "retained_bytes",
                "retained_growth_bytes",
            }
            and all(_nonnegative_int(value) for value in heap.values())
            and heap["baseline_bytes"] > 0
            and heap["retained_bytes"] > 0
            and heap["peak_bytes"]
            >= max(heap["baseline_bytes"], heap["retained_bytes"])
            and heap["retained_growth_bytes"]
            == max(0, heap["retained_bytes"] - heap["baseline_bytes"])
            and heap["peak_bytes"] <= limits["max_accounted_bytes"]
            and heap["retained_growth_bytes"]
            <= limits["max_retained_growth_bytes"],
            f"summary_{mode}_heap",
        )
        _require(
            isinstance(hard_stop, dict)
            and hard_stop
            == {
                "deadline_ms": HARD_STOP_DEADLINE_MS,
                "elapsed_ms": hard_stop.get("elapsed_ms"),
                "future_requests_rejected": True,
                "rejected_requests": 2,
                "target_destroyed": True,
                "wasm_frame_confirmed": True,
            }
            and _positive_int(
                hard_stop.get("elapsed_ms"), maximum=HARD_STOP_DEADLINE_MS
            ),
            f"summary_{mode}_hard_stop",
        )
        _require(
            isinstance(process_tree_rss, dict)
            and set(process_tree_rss)
            == {
                "baseline_bytes",
                "peak_bytes",
                "peak_growth_bytes",
                "retained_bytes",
                "retained_growth_bytes",
            }
            and all(
                _nonnegative_int(value) for value in process_tree_rss.values()
            )
            and process_tree_rss["baseline_bytes"] > 0
            and process_tree_rss["retained_bytes"] > 0
            and process_tree_rss["peak_bytes"]
            >= max(
                process_tree_rss["baseline_bytes"],
                process_tree_rss["retained_bytes"],
            )
            and process_tree_rss["peak_growth_bytes"]
            == (
                process_tree_rss["peak_bytes"]
                - process_tree_rss["baseline_bytes"]
            )
            and process_tree_rss["retained_growth_bytes"]
            == max(
                0,
                process_tree_rss["retained_bytes"]
                - process_tree_rss["baseline_bytes"],
            )
            and process_tree_rss["peak_growth_bytes"]
            <= limits["max_process_tree_peak_growth_bytes"]
            and process_tree_rss["retained_growth_bytes"]
            <= limits["max_process_tree_retained_growth_bytes"],
            f"summary_{mode}_rss",
        )
        _require(
            network
            == {
                "csp_negative_control": True,
                "error_text": EXPECTED_NETWORK_ERROR,
                "offline_control_intercepted": True,
                "response_received": False,
                "sink_requests": 0,
            },
            f"summary_{mode}_network",
        )
    package = document.get("package")
    _require(
        isinstance(package, dict)
        and set(package)
        == {
            "archive_bytes",
            "archive_sha256",
            "entry_count",
            "integrity",
            "name",
            "npm_pack_json_sha256",
            "shasum",
            "unpacked_bytes",
            "version",
        }
        and package.get("name") == contract["package_name"]
        and package.get("version") == contract["package_version"]
        and package.get("entry_count") == 12
        and _positive_int(package.get("archive_bytes"), maximum=2 * 1024 * 1024)
        and _positive_int(package.get("unpacked_bytes"), maximum=5 * 1024 * 1024)
        and isinstance(package.get("archive_sha256"), str)
        and SHA256_RE.fullmatch(package["archive_sha256"]) is not None
        and isinstance(package.get("npm_pack_json_sha256"), str)
        and SHA256_RE.fullmatch(package["npm_pack_json_sha256"]) is not None
        and isinstance(package.get("shasum"), str)
        and SHA1_RE.fullmatch(package["shasum"]) is not None
        and isinstance(package.get("integrity"), str)
        and INTEGRITY_RE.fullmatch(package["integrity"]) is not None,
        "summary_package",
    )
    return document


def _read_summary(path: Path) -> tuple[dict[str, Any], bytes]:
    document, payload = _json_payload(path, MAX_JSON_BYTES, "summary_json")
    _require(isinstance(document, dict), "summary_shape")
    return document, payload


def _authenticated_artifact_summary(
    archive_path: Path,
    *,
    expected_size: int,
    expected_digest: str,
) -> tuple[dict[str, Any], bytes]:
    _require(
        _positive_int(expected_size, maximum=MAX_ARTIFACT_BYTES),
        "artifact_size",
    )
    _require(
        isinstance(expected_digest, str)
        and ARTIFACT_DIGEST_RE.fullmatch(expected_digest) is not None,
        "artifact_digest",
    )
    payload = _read_bytes(archive_path, MAX_ARTIFACT_BYTES, "artifact_archive")
    _require(len(payload) == expected_size, "artifact_size")
    _require(f"sha256:{_sha256(payload)}" == expected_digest, "artifact_digest")
    try:
        with zipfile.ZipFile(io.BytesIO(payload), "r") as archive:
            members = archive.infolist()
            _require(
                len(members) == 1
                and members[0].filename == SUMMARY_NAME
                and members[0].orig_filename == SUMMARY_NAME,
                "artifact_file_set",
            )
            member = members[0]
            unix_mode = (member.external_attr >> 16) & 0xFFFF
            _require(
                not member.is_dir()
                and "/" not in member.filename
                and "\\" not in member.filename
                and not (member.flag_bits & 0x1)
                and member.compress_type
                in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED)
                and stat.S_IFMT(unix_mode) in (0, stat.S_IFREG)
                and 0 < member.file_size <= MAX_JSON_BYTES
                and 0 < member.compress_size <= len(payload),
                "artifact_member",
            )
            with archive.open(member, "r") as source:
                summary_payload = source.read(MAX_JSON_BYTES + 1)
            _require(
                len(summary_payload) == member.file_size <= MAX_JSON_BYTES,
                "artifact_member_size",
            )
    except BrowserEvidenceError:
        raise
    except (OSError, RuntimeError, zipfile.BadZipFile, zipfile.LargeZipFile) as error:
        raise BrowserEvidenceError("artifact_archive_invalid") from error
    try:
        document = json.loads(
            summary_payload,
            object_pairs_hook=_object_pairs,
            parse_constant=_reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise BrowserEvidenceError("summary_json") from error
    _require(isinstance(document, dict), "summary_shape")
    return document, summary_payload


def validate_artifact(
    archive_path: Path,
    *,
    artifact_id: int,
    artifact_name: str,
    artifact_size_bytes: int,
    artifact_digest: str,
    head_sha: str,
    platform: str,
    repository: str,
    workflow_run_id: int,
    workflow_run_attempt: int,
    lock_path: Path = DEFAULT_LOCK,
    package_path: Path = DEFAULT_PACKAGE,
) -> dict[str, object]:
    _require(_positive_int(artifact_id), "artifact_id")
    _require(HEAD_SHA_RE.fullmatch(head_sha) is not None, "head_sha")
    _require(repository == EXPECTED_REPOSITORY, "repository")
    expected_name = (
        f"render-browser-{head_sha}-{workflow_run_id}-{workflow_run_attempt}"
    )
    _require(artifact_name == expected_name, "artifact_name")
    document, payload = _authenticated_artifact_summary(
        archive_path,
        expected_size=artifact_size_bytes,
        expected_digest=artifact_digest,
    )
    summary = validate_summary(
        document,
        head_sha=head_sha,
        platform=platform,
        repository=repository,
        workflow_run_id=workflow_run_id,
        workflow_run_attempt=workflow_run_attempt,
        lock_path=lock_path,
        package_path=package_path,
    )
    return {
        "artifact_digest": artifact_digest,
        "artifact_id": artifact_id,
        "artifact_name": artifact_name,
        "artifact_repository": repository,
        "artifact_size_bytes": artifact_size_bytes,
        "behavior_sha256": summary["behavior"]["sha256"],
        "browser_evidence_sha256": _sha256(payload),
        "chromium": summary["chromium"],
        "head_sha": head_sha,
        "package": summary["package"],
        "passed": True,
        "platform": platform,
        "schema": PREREQUISITE_SCHEMA,
        "workflow_run_attempt": workflow_run_attempt,
        "workflow_run_id": workflow_run_id,
    }


def download_and_validate(
    *,
    artifact_id: int,
    artifact_name: str,
    artifact_size_bytes: int,
    artifact_digest: str,
    head_sha: str,
    platform: str,
    repository: str,
    workflow_run_id: int,
    workflow_run_attempt: int,
    lock_path: Path = DEFAULT_LOCK,
    package_path: Path = DEFAULT_PACKAGE,
) -> dict[str, object]:
    with tempfile.TemporaryDirectory(prefix="rxls-render-browser-evidence-") as raw:
        root = Path(raw)
        archive_path = root / "artifact.zip"
        try:
            download_artifact_archive(
                repository,
                artifact_id,
                archive_path,
                artifact_size_bytes,
                artifact_digest,
            )
        except ArtifactDownloadError as error:
            raise BrowserEvidenceError(f"artifact_download:{error}") from error
        return validate_artifact(
            archive_path,
            artifact_id=artifact_id,
            artifact_name=artifact_name,
            artifact_size_bytes=artifact_size_bytes,
            artifact_digest=artifact_digest,
            head_sha=head_sha,
            platform=platform,
            repository=repository,
            workflow_run_id=workflow_run_id,
            workflow_run_attempt=workflow_run_attempt,
            lock_path=lock_path,
            package_path=package_path,
        )


def _write_json(path: Path, document: object) -> None:
    payload = _canonical_payload(document)
    _require(len(payload) <= MAX_JSON_BYTES, "output_size")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(payload)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    build = subparsers.add_parser("build")
    build.add_argument("--source-log", type=Path, required=True)
    build.add_argument("--installed-log", type=Path, required=True)
    build.add_argument("--runtime-evidence", type=Path, required=True)
    build.add_argument("--npm-pack", type=Path, required=True)
    build.add_argument("--npm-archive", type=Path, required=True)
    build.add_argument("--head-sha", required=True)
    build.add_argument("--platform", choices=sorted(SUPPORTED_PLATFORMS), required=True)
    build.add_argument("--repository", required=True)
    build.add_argument("--workflow-run-id", type=int, required=True)
    build.add_argument("--workflow-run-attempt", type=int, required=True)
    build.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    build.add_argument("--package", type=Path, default=DEFAULT_PACKAGE)
    build.add_argument("--output", type=Path, required=True)

    verify = subparsers.add_parser("verify")
    verify.add_argument("--summary", type=Path, required=True)
    verify.add_argument("--head-sha", required=True)
    verify.add_argument("--platform", choices=sorted(SUPPORTED_PLATFORMS), required=True)
    verify.add_argument("--repository", required=True)
    verify.add_argument("--workflow-run-id", type=int, required=True)
    verify.add_argument("--workflow-run-attempt", type=int, required=True)
    verify.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    verify.add_argument("--package", type=Path, default=DEFAULT_PACKAGE)

    download = subparsers.add_parser("download")
    download.add_argument("--repository", required=True)
    download.add_argument("--artifact-id", type=int, required=True)
    download.add_argument("--artifact-name", required=True)
    download.add_argument("--artifact-size-bytes", type=int, required=True)
    download.add_argument("--artifact-digest", required=True)
    download.add_argument("--head-sha", required=True)
    download.add_argument(
        "--platform", choices=sorted(SUPPORTED_PLATFORMS), required=True
    )
    download.add_argument("--workflow-run-id", type=int, required=True)
    download.add_argument("--workflow-run-attempt", type=int, required=True)
    download.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    download.add_argument("--package", type=Path, default=DEFAULT_PACKAGE)
    download.add_argument("--output", type=Path, required=True)

    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        if args.command == "build":
            summary = build_summary(
                source_log=args.source_log,
                installed_log=args.installed_log,
                runtime_evidence=args.runtime_evidence,
                npm_pack=args.npm_pack,
                npm_archive=args.npm_archive,
                head_sha=args.head_sha,
                platform=args.platform,
                repository=args.repository,
                workflow_run_id=args.workflow_run_id,
                workflow_run_attempt=args.workflow_run_attempt,
                lock_path=args.lock,
                package_path=args.package,
            )
            validate_summary(
                summary,
                head_sha=args.head_sha,
                platform=args.platform,
                repository=args.repository,
                workflow_run_id=args.workflow_run_id,
                workflow_run_attempt=args.workflow_run_attempt,
                lock_path=args.lock,
                package_path=args.package,
            )
            _write_json(args.output, summary)
        elif args.command == "verify":
            summary, _ = _read_summary(args.summary)
            validate_summary(
                summary,
                head_sha=args.head_sha,
                platform=args.platform,
                repository=args.repository,
                workflow_run_id=args.workflow_run_id,
                workflow_run_attempt=args.workflow_run_attempt,
                lock_path=args.lock,
                package_path=args.package,
            )
        else:
            report = download_and_validate(
                artifact_id=args.artifact_id,
                artifact_name=args.artifact_name,
                artifact_size_bytes=args.artifact_size_bytes,
                artifact_digest=args.artifact_digest,
                head_sha=args.head_sha,
                platform=args.platform,
                repository=args.repository,
                workflow_run_id=args.workflow_run_id,
                workflow_run_attempt=args.workflow_run_attempt,
                lock_path=args.lock,
                package_path=args.package,
            )
            _write_json(args.output, report)
    except BrowserEvidenceError as error:
        print(f"render browser evidence: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
