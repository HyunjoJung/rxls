#!/usr/bin/env python3
"""Fail-closed absolute LibreOffice rendering-fidelity acceptance gate.

The input is a complete ``rxls.libreoffice-render-parity.v1`` comparison
report.  The output deliberately retains only hashes, counts, aggregate
metrics, thresholds, and stable failure codes: workbook paths and workbook
content never cross the gate boundary.

The core cohort is deterministic.  It contains feature-tagged workbooks in
the LibreOffice-oracle formats which do not exercise one of the explicitly
broad-only feature buckets below.  The broad cohort contains every workbook
in those formats, including XLSB workbooks.
"""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import sys
from typing import Any, Sequence


EVIDENCE_SCHEMA = "rxls.libreoffice-render-parity.v1"
OUTPUT_SCHEMA = "rxls.render-fidelity-targets.v1"
CONTAINER_EXECUTION_SCHEMA = "rxls.render-oracle-container-execution.v2"
CONTAINER_IDENTITY_SCHEMA = "rxls.render-oracle-container-identity.v1"
CONTAINER_LIBREOFFICE_ARTIFACT_SHA256 = (
    "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5"
)
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_FILES = 100_000
MAX_PAGES = 1_000_000
MAX_HISTOGRAM_BUCKETS = 1_000_000
ORACLE_FORMATS = ("ods", "xls", "xlsb", "xlsx")
CORE_EXCLUDED_FEATURES = frozenset(
    {
        "chart",
        "conditional-format",
        "image-drawing",
        "print-settings",
        "right-to-left-layout",
        "rtl-text",
        "sparkline",
        "wrapped-text",
    }
)

# Absolute release-quality thresholds.  PPM scores are higher-is-better;
# geometry is retained in thousandths of a PostScript point and is
# lower-is-better.
SEMANTIC_CODEPOINT_MIN_PPM = 999_000
EDGE_F1_MIN_PPM = 970_000
CORE_SIMILARITY_MIN_PPM = 980_000
BROAD_SIMILARITY_MIN_PPM = 950_000
TEXT_BOX_MEDIAN_MAX_MILLIPOINTS = 1_000
TEXT_BOX_P95_MAX_MILLIPOINTS = 2_500
PAGE_BOX_MEDIAN_MAX_MILLIPOINTS = 1_000
PAGE_BOX_P95_MAX_MILLIPOINTS = 2_500
PAGE_BOX_MAX_MILLIPOINTS = 5_000
TEXT_BOX_MATCH_MIN_PPM = 999_000
MIN_CORE_WORKBOOKS = 10
MIN_BROAD_WORKBOOKS = 40
MIN_CORE_TEXT_BOXES = 100


class GateError(RuntimeError):
    """The input evidence is malformed or violates the gate contract."""


def _reject_duplicate_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise GateError("duplicate_json_key")
        result[key] = value
    return result


def _read_report(path: Path) -> tuple[dict[str, Any], str, int]:
    try:
        size = path.stat().st_size
    except OSError as error:
        raise GateError("report_unreadable") from error
    if size <= 0 or size > MAX_REPORT_BYTES:
        raise GateError("report_size_limit")
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise GateError("report_unreadable") from error
    try:
        value = json.loads(payload, object_pairs_hook=_reject_duplicate_pairs)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise GateError("report_invalid_json") from error
    if not isinstance(value, dict):
        raise GateError("report_shape")
    return value, hashlib.sha256(payload).hexdigest(), size


def _integer(value: object, code: str, *, minimum: int = 0, maximum: int | None = None) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise GateError(code)
    if maximum is not None and value > maximum:
        raise GateError(code)
    return value


def _ppm(value: object, code: str) -> int:
    return _integer(value, code, maximum=1_000_000)


def _sha256(value: object, code: str) -> str:
    if not isinstance(value, str) or SHA256_RE.fullmatch(value) is None:
        raise GateError(code)
    return value


def _exact_object(value: object, keys: set[str], code: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        raise GateError(code)
    return value


def _ratio_ppm(numerator: int, denominator: int, *, empty: int = 0) -> int:
    if denominator == 0:
        return empty
    return (numerator * 1_000_000 + denominator // 2) // denominator


def _mean(values: Sequence[int]) -> int:
    if not values:
        raise GateError("empty_metric_cohort")
    return (sum(values) + len(values) // 2) // len(values)


def _nearest_rank(values: Sequence[int], numerator: int, denominator: int) -> int:
    if not values:
        raise GateError("empty_metric_distribution")
    ordered = sorted(values)
    rank = max(1, (len(ordered) * numerator + denominator - 1) // denominator)
    return ordered[min(len(ordered) - 1, rank - 1)]


def _histogram_quantile(
    histogram: Counter[int], numerator: int, denominator: int
) -> int:
    total = sum(histogram.values())
    if total <= 0:
        raise GateError("empty_text_box_distribution")
    rank = max(1, (total * numerator + denominator - 1) // denominator)
    seen = 0
    for error, count in sorted(histogram.items()):
        seen += count
        if seen >= rank:
            return error
    raise GateError("text_box_histogram_inconsistent")


def _features(value: object) -> tuple[str, ...] | None:
    if value is None:
        return None
    if (
        not isinstance(value, list)
        or len(value) > 256
        or any(not isinstance(item, str) or not item or len(item) > 128 for item in value)
        or value != sorted(set(value))
    ):
        raise GateError("file_features")
    return tuple(value)


def _text_box_histogram(
    page: dict[str, Any]
) -> tuple[int, int, int, int, Counter[int]]:
    candidates = _integer(
        page.get("text_box_candidate_items"),
        "text_box_candidate_items",
        maximum=1_000_000,
    )
    matched = _integer(
        page.get("text_box_matched_items"),
        "text_box_matched_items",
        maximum=candidates,
    )
    ambiguous = _integer(
        page.get("text_box_ambiguous_items"),
        "text_box_ambiguous_items",
        maximum=candidates,
    )
    unmatched = _integer(
        page.get("text_box_unmatched_items"),
        "text_box_unmatched_items",
        maximum=candidates,
    )
    if candidates != matched + ambiguous + unmatched:
        raise GateError("text_box_partition")
    rows = page.get("text_box_error_histogram_millipoints")
    if not isinstance(rows, list) or len(rows) > MAX_HISTOGRAM_BUCKETS:
        raise GateError("text_box_histogram")
    histogram: Counter[int] = Counter()
    previous = -1
    for row in rows:
        if not isinstance(row, dict) or set(row) != {"count", "error_millipoints"}:
            raise GateError("text_box_histogram")
        error = _integer(
            row["error_millipoints"],
            "text_box_histogram",
            maximum=1_000_000_000,
        )
        count = _integer(row["count"], "text_box_histogram", minimum=1, maximum=1_000_000)
        if error <= previous:
            raise GateError("text_box_histogram_order")
        previous = error
        histogram[error] = count
    if sum(histogram.values()) != matched:
        raise GateError("text_box_histogram_count")
    coverage = _ppm(page.get("text_box_match_coverage_ppm"), "text_box_coverage")
    if coverage != _ratio_ppm(matched, candidates, empty=1_000_000):
        raise GateError("text_box_coverage_inconsistent")
    expected_median = (
        _histogram_quantile(histogram, 1, 2) if histogram else None
    )
    expected_p95 = (
        _histogram_quantile(histogram, 95, 100) if histogram else None
    )
    if (
        page.get("text_box_median_error_millipoints") != expected_median
        or page.get("text_box_p95_error_millipoints") != expected_p95
    ):
        raise GateError("text_box_quantile_inconsistent")
    return candidates, matched, ambiguous, unmatched, histogram


def _edge_f1(rows: Sequence[dict[str, Any]]) -> int:
    rxls_pixels = sum(
        _integer(row.get("edge_rxls_pixels"), "edge_metric") for row in rows
    )
    lo_pixels = sum(
        _integer(row.get("edge_libreoffice_pixels"), "edge_metric") for row in rows
    )
    rxls_matched = sum(
        _integer(row.get("edge_rxls_matched_1px"), "edge_metric") for row in rows
    )
    lo_matched = sum(
        _integer(row.get("edge_libreoffice_matched_1px"), "edge_metric") for row in rows
    )
    if rxls_matched > rxls_pixels or lo_matched > lo_pixels:
        raise GateError("edge_metric")
    both_empty = rxls_pixels == 0 and lo_pixels == 0
    denominator = rxls_matched * lo_pixels + lo_matched * rxls_pixels
    if both_empty:
        return 1_000_000
    if denominator == 0:
        return 0
    return _ratio_ppm(2 * rxls_matched * lo_matched, denominator)


def _semantic_codepoint(rows: Sequence[dict[str, Any]]) -> tuple[int, int]:
    rxls = sum(
        _integer(row.get("semantic_codepoint_rxls_items"), "semantic_metric")
        for row in rows
    )
    libreoffice = sum(
        _integer(row.get("semantic_codepoint_libreoffice_items"), "semantic_metric")
        for row in rows
    )
    matched = sum(
        _integer(row.get("semantic_codepoint_matched_items"), "semantic_metric")
        for row in rows
    )
    if matched > rxls or matched > libreoffice:
        raise GateError("semantic_metric")
    both_empty = rxls == 0 and libreoffice == 0
    return (
        _ratio_ppm(matched, rxls, empty=1_000_000 if both_empty else 0),
        _ratio_ppm(matched, libreoffice, empty=1_000_000 if both_empty else 0),
    )


def _file_similarity(rows: Sequence[dict[str, Any]]) -> int:
    if any(not isinstance(row, dict) for row in rows):
        raise GateError("page_row")
    pixels = sum(
        _integer(row.get("pixels"), "similarity_metric", minimum=1)
        for row in rows
    )
    absolute = sum(
        _integer(row.get("absolute_error_sum"), "similarity_metric")
        for row in rows
    )
    denominator = pixels * 3 * 255
    if absolute > denominator:
        raise GateError("similarity_metric")
    return max(0, 1_000_000 - _ratio_ppm(absolute, denominator))


def _container_oracle_identity(
    value: object,
    *,
    dpi: int,
    font_pack_sha256: str,
) -> dict[str, Any]:
    row = _exact_object(
        value,
        {
            "build_contract_sha256",
            "font_pack_sha256",
            "image",
            "libreoffice",
            "lock_file_sha256",
            "pdf_font_inspector",
            "runtime",
            "schema",
        },
        "configuration_container_identity",
    )
    if (
        row.get("schema") != CONTAINER_IDENTITY_SCHEMA
        or row.get("font_pack_sha256") != font_pack_sha256
    ):
        raise GateError("configuration_container_identity")
    image = _exact_object(
        row.get("image"),
        {
            "architecture",
            "config_digest",
            "expected_config_digest",
            "identity_status",
        },
        "configuration_container_image",
    )
    config_digest = image.get("config_digest")
    if (
        image.get("architecture") != "linux/amd64"
        or not isinstance(config_digest, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", config_digest) is None
        or image.get("expected_config_digest") != config_digest
        or image.get("identity_status") != "pinned_match"
    ):
        raise GateError("configuration_container_image")
    libreoffice = _exact_object(
        row.get("libreoffice"),
        {"artifact_sha256", "name", "version"},
        "configuration_container_libreoffice",
    )
    if libreoffice != {
        "artifact_sha256": CONTAINER_LIBREOFFICE_ARTIFACT_SHA256,
        "name": "LibreOffice",
        "version": "26.2.3.2",
    }:
        raise GateError("configuration_container_libreoffice")
    inspector = _exact_object(
        row.get("pdf_font_inspector"),
        {"kind", "pdffonts_sha256"},
        "configuration_container_pdffonts",
    )
    if inspector.get("kind") != "poppler":
        raise GateError("configuration_container_pdffonts")
    _sha256(inspector.get("pdffonts_sha256"), "configuration_container_pdffonts")
    _sha256(row.get("build_contract_sha256"), "configuration_container_identity")
    _sha256(row.get("lock_file_sha256"), "configuration_container_identity")
    if row.get("runtime") not in {"docker", "podman"}:
        raise GateError("configuration_container_runtime")
    # DPI remains a report-wide metric configuration. Keep the argument here
    # so callers cannot accidentally validate a detached identity object.
    if not 36 <= dpi <= 1200:
        raise GateError("configuration_dpi")
    return row


def _adapter_identity(
    value: object,
    *,
    aggregate: dict[str, Any],
) -> dict[str, Any]:
    row = _exact_object(
        value,
        {
            "font_pack_sha256",
            "image",
            "lock_file_sha256",
            "lock_sha256",
            "oracle",
            "runtime",
            "schema",
        },
        "file_oracle_adapter",
    )
    image = _exact_object(
        row.get("image"),
        {"architecture", "expected_id", "id", "identity_status"},
        "file_oracle_adapter_image",
    )
    expected_image = aggregate["image"]
    if (
        row.get("schema") != CONTAINER_EXECUTION_SCHEMA
        or row.get("font_pack_sha256") != aggregate["font_pack_sha256"]
        or image.get("architecture") != expected_image["architecture"]
        or image.get("id") != expected_image["config_digest"]
        or image.get("expected_id") != expected_image["expected_config_digest"]
        or image.get("identity_status") != expected_image["identity_status"]
        or row.get("lock_sha256") != aggregate["build_contract_sha256"]
        or row.get("lock_file_sha256") != aggregate["lock_file_sha256"]
        or row.get("oracle") != aggregate["libreoffice"]
        or row.get("runtime") != aggregate["runtime"]
    ):
        raise GateError("file_oracle_adapter_identity")
    return row


def _font_attestation(value: object) -> int:
    row = _exact_object(
        value,
        {
            "embedded_font_objects",
            "font_objects",
            "matched_font_objects",
            "normalized_identities_sha256",
            "subset_font_objects",
            "unicode_font_objects",
            "unique_font_identities",
        },
        "font_attestation",
    )
    objects = _integer(
        row.get("font_objects"),
        "font_attestation",
        minimum=1,
        maximum=1_000_000,
    )
    for key in (
        "embedded_font_objects",
        "matched_font_objects",
        "subset_font_objects",
        "unicode_font_objects",
    ):
        if _integer(row.get(key), "font_attestation", maximum=objects) != objects:
            raise GateError("font_attestation_incomplete")
    unique = _integer(
        row.get("unique_font_identities"),
        "font_attestation",
        minimum=1,
        maximum=objects,
    )
    if unique > objects:
        raise GateError("font_attestation")
    _sha256(row.get("normalized_identities_sha256"), "font_attestation")
    return objects


def _configuration(
    report: dict[str, Any],
) -> tuple[int, dict[str, str], str, dict[str, Any] | None]:
    configuration = report.get("configuration")
    if not isinstance(configuration, dict):
        raise GateError("configuration")
    dpi = _integer(configuration.get("dpi"), "configuration_dpi", minimum=36, maximum=1200)
    font_pack = configuration.get("font_pack")
    oracle_lock = configuration.get("oracle_lock")
    renderer = configuration.get("renderer_binary")
    if not isinstance(font_pack, dict) or not isinstance(oracle_lock, dict) or not isinstance(renderer, dict):
        raise GateError("configuration_identity")
    identities = {
        "font_pack_sha256": font_pack.get("pack_sha256"),
        "renderer_sha256": renderer.get("sha256"),
    }
    if not all(
        isinstance(value, str) and SHA256_RE.fullmatch(value)
        for value in identities.values()
    ):
        raise GateError("configuration_identity")
    container_identity: dict[str, Any] | None = None
    if oracle_lock.get("schema") == CONTAINER_IDENTITY_SCHEMA:
        container_identity = _container_oracle_identity(
            oracle_lock,
            dpi=dpi,
            font_pack_sha256=identities["font_pack_sha256"],
        )
        oracle_mode = "container"
        identities.update(
            {
                "oracle_build_contract_sha256": container_identity["build_contract_sha256"],
                "oracle_image_config_digest": container_identity["image"]["config_digest"],
                "oracle_lock_file_sha256": container_identity["lock_file_sha256"],
                "oracle_libreoffice_artifact_sha256": container_identity["libreoffice"]["artifact_sha256"],
                "pdffonts_sha256": container_identity["pdf_font_inspector"]["pdffonts_sha256"],
            }
        )
        python_identity = None
    else:
        oracle_mode = "direct"
        profile = oracle_lock.get("profile")
        if not isinstance(profile, str) or not profile or len(profile) > 256:
            raise GateError("configuration_identity")
        identities["oracle_profile"] = profile
        oracle_configuration = oracle_lock.get("configuration")
        libreoffice = oracle_lock.get("libreoffice")
        python_identity = oracle_lock.get("python")
        pdf_rasterizer = oracle_lock.get("pdf_rasterizer")
        if (
            not isinstance(oracle_configuration, dict)
            or oracle_configuration.get("dpi") != dpi
            or not isinstance(libreoffice, dict)
            or not isinstance(python_identity, dict)
            or not isinstance(pdf_rasterizer, dict)
            or pdf_rasterizer.get("kind") != "poppler"
            or oracle_lock.get("font_pack_sha256")
            != identities["font_pack_sha256"]
        ):
            raise GateError("configuration_identity")
        locked_hashes = {
            "oracle_profile_sha256": oracle_configuration.get("profile_sha256"),
            "libreoffice_sha256": libreoffice.get("executable_sha256"),
            "pdfinfo_sha256": pdf_rasterizer.get("pdfinfo_sha256"),
            "pdffonts_sha256": pdf_rasterizer.get("pdffonts_sha256"),
            "pdftoppm_sha256": pdf_rasterizer.get("pdftoppm_sha256"),
            "pdftotext_sha256": pdf_rasterizer.get("pdftotext_sha256"),
        }
        if not all(
            isinstance(value, str) and SHA256_RE.fullmatch(value)
            for value in locked_hashes.values()
        ):
            raise GateError("configuration_identity")
        identities.update(locked_hashes)
    policy = configuration.get("metric_policy")
    implementation = policy.get("implementation") if isinstance(policy, dict) else None
    if (
        not isinstance(policy, dict)
        or policy.get("mask_match_tolerance_pixels") != 1
        or policy.get("edge_luma_delta") != 32
        or policy.get("semantic_content_retained") is not False
        or policy.get("semantic_text_source")
        != "svg_data-rxls-visible-label_vs_pdftotext_layout"
        or policy.get("text_box_content_retained") is not False
        or policy.get("text_box_error_units") != "millipoints"
        or policy.get("text_box_source")
        != "svg_clipped_glyph_bounds_vs_pdftotext_bbox_layout"
        or policy.get("text_box_matching")
        != "exact_svg_data-rxls-visible-label_nearest_unique_pdftotext_bbox_layout"
        or not isinstance(implementation, dict)
        or implementation.get("kind") != "numpy_integer_exact_v1"
        or (
            oracle_mode == "direct"
            and implementation.get("version") != python_identity.get("numpy_version")
        )
        or (
            oracle_mode == "container"
            and (
                not isinstance(implementation.get("version"), str)
                or not implementation["version"]
                or len(implementation["version"]) > 64
            )
        )
    ):
        raise GateError("metric_policy")
    return dpi, identities, oracle_mode, container_identity


def evaluate(report: dict[str, Any], evidence_sha256: str, evidence_bytes: int) -> dict[str, Any]:
    if (
        not isinstance(evidence_sha256, str)
        or SHA256_RE.fullmatch(evidence_sha256) is None
        or isinstance(evidence_bytes, bool)
        or not isinstance(evidence_bytes, int)
        or not 0 < evidence_bytes <= MAX_REPORT_BYTES
    ):
        raise GateError("evidence_identity")
    if report.get("schema") != EVIDENCE_SCHEMA or report.get("mode") != "compare":
        raise GateError("report_schema_or_mode")
    dpi, identities, oracle_mode, container_identity = _configuration(report)
    files = report.get("files")
    if not isinstance(files, list) or not 0 < len(files) <= MAX_FILES:
        raise GateError("files")
    summary = report.get("summary")
    if not isinstance(summary, dict) or summary.get("files") != len(files):
        raise GateError("summary_files")

    broad_rows: list[dict[str, Any]] = []
    core_rows: list[dict[str, Any]] = []
    broad_similarities: list[int] = []
    core_similarities: list[int] = []
    broad_files: list[dict[str, Any]] = []
    core_files: list[dict[str, Any]] = []
    format_counts: Counter[str] = Counter()
    status_counts: Counter[str] = Counter()
    page_errors_millipoints: list[int] = []
    text_box_histogram: Counter[int] = Counter()
    text_box_candidates = 0
    text_box_matched = 0
    text_box_ambiguous = 0
    text_box_unmatched = 0
    total_pages = 0
    font_objects = 0
    failures: set[str] = set()

    for item in files:
        if not isinstance(item, dict):
            raise GateError("file_row")
        format_name = item.get("format")
        status = item.get("status")
        if not isinstance(format_name, str) or not isinstance(status, str):
            raise GateError("file_identity")
        if status not in {"compared", "different", "skipped", "error"}:
            raise GateError("file_status")
        status_counts[status] += 1
        if format_name not in ORACLE_FORMATS:
            raise GateError("file_format")
        if status not in {"compared", "different"}:
            failures.add("broad_coverage_incomplete")
            continue
        font_objects += _font_attestation(item.get("font_attestation"))
        if oracle_mode == "container":
            if container_identity is None:
                raise GateError("configuration_container_identity")
            _adapter_identity(
                item.get("oracle_adapter"), aggregate=container_identity
            )
        elif item.get("oracle_adapter") is not None:
            raise GateError("file_oracle_adapter_unexpected")
        format_counts[format_name] += 1
        metrics = item.get("metrics")
        pages = item.get("pages")
        scenes = item.get("scenes")
        artifacts = item.get("artifacts")
        if (
            not isinstance(metrics, dict)
            or not isinstance(pages, list)
            or not isinstance(scenes, list)
            or not isinstance(artifacts, dict)
        ):
            raise GateError("file_metrics")
        if not pages:
            raise GateError("page_mapping")
        file_similarity = _file_similarity(pages)
        if _ppm(metrics.get("similarity_ppm"), "similarity_metric") != file_similarity:
            raise GateError("similarity_metric_inconsistent")
        page_count = len(pages)
        total_pages += page_count
        if total_pages > MAX_PAGES:
            raise GateError("page_limit")
        rxls_pages = _integer(artifacts.get("rxls_pages"), "page_mapping")
        libreoffice_pages = _integer(
            artifacts.get("libreoffice_pages"), "page_mapping"
        )
        page_indices = []
        for page in pages:
            if not isinstance(page, dict):
                raise GateError("page_row")
            page_indices.append(_integer(page.get("sheet_index"), "page_mapping"))
        scene_indices = []
        for scene in scenes:
            if not isinstance(scene, dict):
                raise GateError("scene_row")
            scene_indices.append(_integer(scene.get("sheet_index"), "page_mapping"))
        if (
            page_count == 0
            or rxls_pages != page_count
            or libreoffice_pages != page_count
            or page_indices != list(range(page_count))
            or scene_indices != page_indices
        ):
            failures.add("sheet_page_mapping_not_exact")
        for page in pages:
            rxls_size = page.get("rxls_size")
            libreoffice_size = page.get("libreoffice_size")
            if not isinstance(rxls_size, dict) or not isinstance(libreoffice_size, dict):
                raise GateError("page_geometry")
            width_delta = abs(
                _integer(rxls_size.get("width"), "page_geometry", minimum=1)
                - _integer(libreoffice_size.get("width"), "page_geometry", minimum=1)
            )
            height_delta = abs(
                _integer(rxls_size.get("height"), "page_geometry", minimum=1)
                - _integer(libreoffice_size.get("height"), "page_geometry", minimum=1)
            )
            pixels = max(width_delta, height_delta)
            page_errors_millipoints.append(
                (pixels * 72_000 + dpi // 2) // dpi
            )

        broad_rows.extend(pages)
        broad_similarities.append(file_similarity)
        broad_files.append(item)
        features = _features(item.get("features"))
        if features is not None and not CORE_EXCLUDED_FEATURES.intersection(features):
            core_rows.extend(pages)
            core_similarities.append(file_similarity)
            core_files.append(item)
            for page in pages:
                candidates, matched, ambiguous, unmatched, histogram = (
                    _text_box_histogram(page)
                )
                text_box_candidates += candidates
                text_box_matched += matched
                text_box_ambiguous += ambiguous
                text_box_unmatched += unmatched
                text_box_histogram.update(histogram)

    by_status = summary.get("by_status")
    if (
        not isinstance(by_status, dict)
        or any(
            not isinstance(key, str)
            or isinstance(value, bool)
            or not isinstance(value, int)
            or value < 0
            for key, value in by_status.items()
        )
        or by_status != dict(sorted(status_counts.items()))
    ):
        raise GateError("summary_status_counts")

    if len(broad_files) < MIN_BROAD_WORKBOOKS:
        failures.add("broad_coverage_below_minimum")
    if len(core_files) < MIN_CORE_WORKBOOKS:
        failures.add("core_coverage_below_minimum")
    for required in ORACLE_FORMATS:
        if format_counts[required] == 0:
            failures.add(f"broad_format_missing:{required}")
    if not broad_rows:
        raise GateError("empty_broad_cohort")
    if not core_rows:
        raise GateError("empty_core_cohort")

    core_precision, core_recall = _semantic_codepoint(core_rows)
    core_edge_f1 = _edge_f1(core_rows)
    core_similarity = _mean(core_similarities)
    broad_similarity = _mean(broad_similarities)
    text_box_coverage = _ratio_ppm(
        text_box_matched,
        text_box_candidates,
        empty=1_000_000,
    )
    if text_box_matched < MIN_CORE_TEXT_BOXES:
        failures.add("text_box_coverage_below_minimum")
    if sum(text_box_histogram.values()) != text_box_matched:
        raise GateError("text_box_histogram_count")
    text_box_median = (
        _histogram_quantile(text_box_histogram, 1, 2) if text_box_histogram else None
    )
    text_box_p95 = (
        _histogram_quantile(text_box_histogram, 95, 100)
        if text_box_histogram
        else None
    )
    page_median = _nearest_rank(page_errors_millipoints, 1, 2)
    page_p95 = _nearest_rank(page_errors_millipoints, 95, 100)
    page_max = max(page_errors_millipoints)

    if core_precision < SEMANTIC_CODEPOINT_MIN_PPM:
        failures.add("semantic_codepoint_precision_below_target")
    if core_recall < SEMANTIC_CODEPOINT_MIN_PPM:
        failures.add("semantic_codepoint_recall_below_target")
    if core_edge_f1 < EDGE_F1_MIN_PPM:
        failures.add("edge_f1_below_target")
    if core_similarity < CORE_SIMILARITY_MIN_PPM:
        failures.add("core_similarity_below_target")
    if broad_similarity < BROAD_SIMILARITY_MIN_PPM:
        failures.add("broad_similarity_below_target")
    if text_box_coverage < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_box_match_coverage_below_target")
    if text_box_ambiguous != 0:
        failures.add("text_box_mapping_ambiguous")
    if text_box_unmatched != 0:
        failures.add("text_box_mapping_unmatched")
    if text_box_median is None or text_box_median > TEXT_BOX_MEDIAN_MAX_MILLIPOINTS:
        failures.add("text_box_median_error_above_target")
    if text_box_p95 is None or text_box_p95 > TEXT_BOX_P95_MAX_MILLIPOINTS:
        failures.add("text_box_p95_error_above_target")
    if page_median > PAGE_BOX_MEDIAN_MAX_MILLIPOINTS:
        failures.add("page_box_median_error_above_target")
    if page_p95 > PAGE_BOX_P95_MAX_MILLIPOINTS:
        failures.add("page_box_p95_error_above_target")
    if page_max > PAGE_BOX_MAX_MILLIPOINTS:
        failures.add("page_box_max_error_above_target")

    thresholds = {
        "broad_similarity_min_ppm": BROAD_SIMILARITY_MIN_PPM,
        "core_similarity_min_ppm": CORE_SIMILARITY_MIN_PPM,
        "edge_f1_min_ppm": EDGE_F1_MIN_PPM,
        "page_box_max_millipoints": PAGE_BOX_MAX_MILLIPOINTS,
        "page_box_median_max_millipoints": PAGE_BOX_MEDIAN_MAX_MILLIPOINTS,
        "page_box_p95_max_millipoints": PAGE_BOX_P95_MAX_MILLIPOINTS,
        "semantic_codepoint_precision_min_ppm": SEMANTIC_CODEPOINT_MIN_PPM,
        "semantic_codepoint_recall_min_ppm": SEMANTIC_CODEPOINT_MIN_PPM,
        "text_box_match_min_ppm": TEXT_BOX_MATCH_MIN_PPM,
        "text_box_median_max_millipoints": TEXT_BOX_MEDIAN_MAX_MILLIPOINTS,
        "text_box_p95_max_millipoints": TEXT_BOX_P95_MAX_MILLIPOINTS,
    }
    return {
        "coverage": {
            "broad_workbooks": len(broad_files),
            "core_text_box_candidates": text_box_candidates,
            "core_text_box_matches": text_box_matched,
            "core_text_box_ambiguous": text_box_ambiguous,
            "core_text_box_unmatched": text_box_unmatched,
            "core_workbooks": len(core_files),
            "format_workbooks": dict(sorted(format_counts.items())),
            "libreoffice_pdf_font_objects": font_objects,
            "pages": total_pages,
            "report_workbooks": len(files),
            "status_counts": dict(sorted(status_counts.items())),
        },
        "evidence": {
            "bytes": evidence_bytes,
            "sha256": evidence_sha256,
            **identities,
        },
        "failures": sorted(failures),
        "metrics": {
            "broad_similarity_mean_ppm": broad_similarity,
            "core_edge_f1_ppm": core_edge_f1,
            "core_semantic_codepoint_precision_ppm": core_precision,
            "core_semantic_codepoint_recall_ppm": core_recall,
            "core_similarity_mean_ppm": core_similarity,
            "page_box_max_millipoints": page_max,
            "page_box_median_millipoints": page_median,
            "page_box_p95_millipoints": page_p95,
            "text_box_match_coverage_ppm": text_box_coverage,
            "text_box_median_error_millipoints": text_box_median,
            "text_box_p95_error_millipoints": text_box_p95,
        },
        "passed": not failures,
        "policy": {
            "core_excluded_features": sorted(CORE_EXCLUDED_FEATURES),
            "minimum_broad_workbooks": MIN_BROAD_WORKBOOKS,
            "minimum_core_text_boxes": MIN_CORE_TEXT_BOXES,
            "minimum_core_workbooks": MIN_CORE_WORKBOOKS,
            "oracle_formats": list(ORACLE_FORMATS),
        },
        "schema": OUTPUT_SCHEMA,
        "thresholds": thresholds,
    }


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path, help="complete parity report")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        report, digest, size = _read_report(args.report)
        result = evaluate(report, digest, size)
    except GateError as error:
        print(f"check-render-fidelity-targets: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
