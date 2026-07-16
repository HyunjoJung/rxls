#!/usr/bin/env python3
"""Fail-closed aggregate gate for authored LibreOffice print pagination.

The input is a complete authored-print parity report. The output contains only
hashes, identities, counts, page-box distributions, thresholds, and stable
failure codes. Workbook labels, source text, and per-file measurements are
never copied into the gate result.
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
OUTPUT_SCHEMA = "rxls.authored-print-parity.v1"
CONTAINER_IDENTITY_SCHEMA = "rxls.render-oracle-container-identity.v1"
CONTAINER_EXECUTION_SCHEMA = "rxls.render-oracle-container-execution.v2"
CONTAINER_LIBREOFFICE_ARTIFACT_SHA256 = (
    "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5"
)
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_WORKBOOKS = 10_000
MAX_PAGES = 100_000
PAGE_MEDIAN_MAX_MILLIPOINTS = 1_000
PAGE_P95_MAX_MILLIPOINTS = 2_500
PAGE_MAX_MILLIPOINTS = 5_000
SEMANTIC_CODEPOINT_MIN_PPM = 999_000
EDGE_F1_MIN_PPM = 970_000
SIMILARITY_MEAN_MIN_PPM = 950_000
TEXT_BOX_MATCH_MIN_PPM = 999_000
TEXT_BOX_MEDIAN_MAX_MILLIPOINTS = 1_000
TEXT_BOX_P95_MAX_MILLIPOINTS = 2_500
EXPECTED_PAGE_WIDTH = 816
EXPECTED_PAGE_HEIGHT = 1056
EXPECTED_PAGES_PER_WORKBOOK = 4


class GateError(RuntimeError):
    """The report is malformed or violates the authored-print contract."""


def _reject_duplicate_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise GateError("duplicate_json_key")
        result[key] = value
    return result


def _read(path: Path) -> tuple[dict[str, Any], str, int]:
    try:
        size = path.stat().st_size
        payload = path.read_bytes()
    except OSError as error:
        raise GateError("report_unreadable") from error
    if not 0 < size <= MAX_REPORT_BYTES or len(payload) != size:
        raise GateError("report_size")
    try:
        document = json.loads(payload, object_pairs_hook=_reject_duplicate_pairs)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise GateError("report_json") from error
    if not isinstance(document, dict):
        raise GateError("report_shape")
    return document, hashlib.sha256(payload).hexdigest(), size


def _integer(
    value: object,
    code: str,
    *,
    minimum: int = 0,
    maximum: int | None = None,
) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise GateError(code)
    if maximum is not None and value > maximum:
        raise GateError(code)
    return value


def _sha(value: object, code: str) -> str:
    if not isinstance(value, str) or SHA256_RE.fullmatch(value) is None:
        raise GateError(code)
    return value


def _ppm(value: object, code: str) -> int:
    return _integer(value, code, maximum=1_000_000)


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
        raise GateError("empty_page_geometry")
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


def _file_similarity(rows: Sequence[dict[str, Any]]) -> int:
    if not rows or any(not isinstance(row, dict) for row in rows):
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


def _edge_f1(rows: Sequence[dict[str, Any]]) -> int:
    rxls_pixels = sum(
        _integer(row.get("edge_rxls_pixels"), "edge_metric") for row in rows
    )
    libreoffice_pixels = sum(
        _integer(row.get("edge_libreoffice_pixels"), "edge_metric") for row in rows
    )
    rxls_matched = sum(
        _integer(row.get("edge_rxls_matched_1px"), "edge_metric") for row in rows
    )
    libreoffice_matched = sum(
        _integer(row.get("edge_libreoffice_matched_1px"), "edge_metric")
        for row in rows
    )
    if rxls_matched > rxls_pixels or libreoffice_matched > libreoffice_pixels:
        raise GateError("edge_metric")
    if rxls_pixels == 0 and libreoffice_pixels == 0:
        return 1_000_000
    denominator = rxls_matched * libreoffice_pixels + libreoffice_matched * rxls_pixels
    if denominator == 0:
        return 0
    return _ratio_ppm(2 * rxls_matched * libreoffice_matched, denominator)


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


def _text_box_histogram(
    page: dict[str, Any],
) -> tuple[int, int, int, int, Counter[int]]:
    candidates = _integer(
        page.get("text_box_candidate_items"),
        "text_box_metric",
        minimum=1,
        maximum=1_000_000,
    )
    matched = _integer(
        page.get("text_box_matched_items"),
        "text_box_metric",
        maximum=candidates,
    )
    ambiguous = _integer(
        page.get("text_box_ambiguous_items"),
        "text_box_metric",
        maximum=candidates,
    )
    unmatched = _integer(
        page.get("text_box_unmatched_items"),
        "text_box_metric",
        maximum=candidates,
    )
    if candidates != matched + ambiguous + unmatched:
        raise GateError("text_box_partition")
    rows = page.get("text_box_error_histogram_millipoints")
    if not isinstance(rows, list) or len(rows) > 1_000_000:
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
        count = _integer(
            row["count"], "text_box_histogram", minimum=1, maximum=1_000_000
        )
        if error <= previous:
            raise GateError("text_box_histogram_order")
        previous = error
        histogram[error] = count
    if sum(histogram.values()) != matched:
        raise GateError("text_box_histogram_count")
    coverage = _ppm(page.get("text_box_match_coverage_ppm"), "text_box_metric")
    if coverage != _ratio_ppm(matched, candidates, empty=1_000_000):
        raise GateError("text_box_metric_inconsistent")
    median = _histogram_quantile(histogram, 1, 2) if histogram else None
    p95 = _histogram_quantile(histogram, 95, 100) if histogram else None
    if (
        page.get("text_box_median_error_millipoints") != median
        or page.get("text_box_p95_error_millipoints") != p95
    ):
        raise GateError("text_box_quantile_inconsistent")
    return candidates, matched, ambiguous, unmatched, histogram


def _container_identity(configuration: dict[str, Any]) -> dict[str, Any]:
    identity = configuration.get("oracle_lock")
    if not isinstance(identity, dict) or identity.get("schema") != CONTAINER_IDENTITY_SCHEMA:
        raise GateError("oracle_identity")
    image = identity.get("image")
    if not isinstance(image, dict):
        raise GateError("oracle_image")
    digest = image.get("config_digest")
    if (
        not isinstance(digest, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is None
        or image.get("expected_config_digest") != digest
        or image.get("identity_status") != "pinned_match"
        or image.get("architecture") != "linux/amd64"
    ):
        raise GateError("oracle_image")
    _sha(identity.get("build_contract_sha256"), "oracle_identity")
    _sha(identity.get("lock_file_sha256"), "oracle_identity")
    _sha(identity.get("font_pack_sha256"), "oracle_identity")
    libreoffice = identity.get("libreoffice")
    if libreoffice != {
        "artifact_sha256": CONTAINER_LIBREOFFICE_ARTIFACT_SHA256,
        "name": "LibreOffice",
        "version": "26.2.3.2",
    }:
        raise GateError("oracle_identity")
    pdf_font_inspector = identity.get("pdf_font_inspector")
    if (
        not isinstance(pdf_font_inspector, dict)
        or pdf_font_inspector.get("kind") != "poppler"
    ):
        raise GateError("oracle_identity")
    _sha(pdf_font_inspector.get("pdffonts_sha256"), "oracle_identity")
    if identity.get("runtime") != "docker":
        raise GateError("oracle_identity")
    return identity


def _metric_policy(configuration: dict[str, Any]) -> None:
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
        or not isinstance(implementation.get("version"), str)
        or not implementation["version"]
        or len(implementation["version"]) > 64
    ):
        raise GateError("metric_policy")


def _attestation(row: dict[str, Any]) -> str:
    evidence = row.get("authored_print")
    expected_keys = {
        "expected_page_height_pixels",
        "expected_page_width_pixels",
        "header_footer",
        "manual_col_breaks",
        "manual_row_breaks",
        "margins",
        "paper_code",
        "print_area",
        "repeated_cols",
        "repeated_rows",
        "scale_mode",
    }
    if not isinstance(evidence, dict) or set(evidence) != expected_keys:
        raise GateError("source_attestation")
    if (
        evidence.get("expected_page_width_pixels") != EXPECTED_PAGE_WIDTH
        or evidence.get("expected_page_height_pixels") != EXPECTED_PAGE_HEIGHT
        or evidence.get("paper_code") != 1
        or evidence.get("header_footer") is not True
        or evidence.get("margins") is not True
        or evidence.get("print_area") is not True
        or evidence.get("repeated_rows") is not True
        or evidence.get("repeated_cols") is not True
        or _integer(
            evidence.get("manual_row_breaks"),
            "source_attestation",
            minimum=1,
            maximum=1,
        )
        != 1
        or _integer(
            evidence.get("manual_col_breaks"),
            "source_attestation",
            minimum=1,
            maximum=1,
        )
        != 1
        or evidence.get("scale_mode") not in {"fit", "scale"}
    ):
        raise GateError("source_attestation")
    return str(evidence["scale_mode"])


def _font_attestation(row: dict[str, Any]) -> int:
    evidence = row.get("font_attestation")
    if not isinstance(evidence, dict):
        raise GateError("font_attestation")
    objects = _integer(evidence.get("font_objects"), "font_attestation", minimum=1)
    for key in (
        "embedded_font_objects",
        "matched_font_objects",
        "subset_font_objects",
        "unicode_font_objects",
    ):
        if evidence.get(key) != objects:
            raise GateError("font_attestation")
    _sha(evidence.get("normalized_identities_sha256"), "font_attestation")
    return objects


def _adapter(row: dict[str, Any], identity: dict[str, Any]) -> None:
    adapter = row.get("oracle_adapter")
    image = adapter.get("image") if isinstance(adapter, dict) else None
    expected_image = identity["image"]["config_digest"]
    if (
        not isinstance(adapter, dict)
        or adapter.get("schema") != CONTAINER_EXECUTION_SCHEMA
        or not isinstance(image, dict)
        or image.get("id") != expected_image
        or image.get("expected_id") != expected_image
        or image.get("identity_status") != "pinned_match"
        or image.get("architecture") != "linux/amd64"
        or adapter.get("lock_sha256") != identity["build_contract_sha256"]
        or adapter.get("lock_file_sha256") != identity["lock_file_sha256"]
        or adapter.get("font_pack_sha256") != identity["font_pack_sha256"]
        or adapter.get("oracle") != identity["libreoffice"]
        or adapter.get("runtime") != "docker"
    ):
        raise GateError("oracle_adapter")


def evaluate(
    report: dict[str, Any],
    *,
    report_sha256: str,
    report_bytes: int,
    expected_workbooks: int,
) -> dict[str, Any]:
    _sha(report_sha256, "report_identity")
    _integer(report_bytes, "report_identity", minimum=1, maximum=MAX_REPORT_BYTES)
    _integer(expected_workbooks, "workbook_coverage", minimum=1, maximum=MAX_WORKBOOKS)
    if report.get("schema") != EVIDENCE_SCHEMA or report.get("mode") != "compare":
        raise GateError("report_schema")
    configuration = report.get("configuration")
    if not isinstance(configuration, dict):
        raise GateError("configuration")
    if configuration.get("print_mode") != "authored" or configuration.get("dpi") != 96:
        raise GateError("print_mode")
    if configuration.get("lane_filter") != {
        "formats": ["xlsx"],
        "required_features": ["print-settings"],
    }:
        raise GateError("lane_filter")
    _metric_policy(configuration)
    identity = _container_identity(configuration)
    renderer = configuration.get("renderer_binary")
    font_pack = configuration.get("font_pack")
    if not isinstance(renderer, dict) or not isinstance(font_pack, dict):
        raise GateError("tool_identity")
    renderer_sha = _sha(renderer.get("sha256"), "tool_identity")
    font_pack_sha = _sha(font_pack.get("pack_sha256"), "tool_identity")
    if font_pack_sha != identity["font_pack_sha256"]:
        raise GateError("tool_identity")

    files = report.get("files")
    summary = report.get("summary")
    if (
        not isinstance(files, list)
        or not 1 <= len(files) <= MAX_WORKBOOKS
        or len(files) != expected_workbooks
        or not isinstance(summary, dict)
        or summary.get("files") != len(files)
        or summary.get("by_status") != {"compared": len(files)}
        or summary.get("by_classification") != {"within_threshold": len(files)}
    ):
        raise GateError("workbook_coverage")

    failures: set[str] = set()
    page_errors: list[int] = []
    metric_pages: list[dict[str, Any]] = []
    file_similarities: list[int] = []
    text_box_histogram: Counter[int] = Counter()
    text_box_candidates = 0
    text_box_matched = 0
    text_box_ambiguous = 0
    text_box_unmatched = 0
    scale_modes: Counter[str] = Counter()
    font_objects = 0
    total_pages = 0
    page_count_histogram: Counter[int] = Counter()
    for row in files:
        if (
            not isinstance(row, dict)
            or row.get("format") != "xlsx"
            or row.get("status") != "compared"
            or row.get("classification") != "within_threshold"
            or not isinstance(row.get("features"), list)
            or any(
                not isinstance(feature, str) or not feature
                for feature in row.get("features", [])
            )
            or "print-settings" not in row["features"]
            or row["features"] != sorted(set(row["features"]))
        ):
            raise GateError("workbook_row")
        _sha(row.get("sha256"), "workbook_identity")
        scale_modes[_attestation(row)] += 1
        font_objects += _font_attestation(row)
        _adapter(row, identity)
        pages = row.get("pages")
        scenes = row.get("scenes")
        artifacts = row.get("artifacts")
        metrics = row.get("metrics")
        if (
            not isinstance(pages, list)
            or not isinstance(scenes, list)
            or not isinstance(artifacts, dict)
            or not isinstance(metrics, dict)
        ):
            raise GateError("page_mapping")
        file_similarity = _file_similarity(pages)
        if (
            _ppm(metrics.get("similarity_ppm"), "similarity_metric")
            != file_similarity
            or metrics.get("pages") != len(pages)
        ):
            raise GateError("similarity_metric_inconsistent")
        file_similarities.append(file_similarity)
        page_count = len(pages)
        total_pages += page_count
        if total_pages > MAX_PAGES:
            raise GateError("page_limit")
        page_count_histogram[page_count] += 1
        if (
            page_count != EXPECTED_PAGES_PER_WORKBOOK
            or artifacts.get("rxls_pages") != page_count
            or artifacts.get("libreoffice_pages") != page_count
            or len(scenes) != page_count
        ):
            failures.add("page_count_mismatch")
        for index, page in enumerate(pages):
            if not isinstance(page, dict) or page.get("sheet_index") != index:
                raise GateError("page_mapping")
            scene = scenes[index] if index < len(scenes) else None
            if not isinstance(scene, dict) or scene.get("sheet_index") != index:
                raise GateError("page_mapping")
            metric_pages.append(page)
            candidates, matched, ambiguous, unmatched, histogram = (
                _text_box_histogram(page)
            )
            text_box_candidates += candidates
            text_box_matched += matched
            text_box_ambiguous += ambiguous
            text_box_unmatched += unmatched
            text_box_histogram.update(histogram)
            rxls_size = page.get("rxls_size")
            libreoffice_size = page.get("libreoffice_size")
            if not isinstance(rxls_size, dict) or not isinstance(libreoffice_size, dict):
                raise GateError("page_geometry")
            rxls_width = _integer(rxls_size.get("width"), "page_geometry", minimum=1)
            rxls_height = _integer(rxls_size.get("height"), "page_geometry", minimum=1)
            lo_width = _integer(libreoffice_size.get("width"), "page_geometry", minimum=1)
            lo_height = _integer(libreoffice_size.get("height"), "page_geometry", minimum=1)
            if (rxls_width, rxls_height) != (EXPECTED_PAGE_WIDTH, EXPECTED_PAGE_HEIGHT):
                failures.add("renderer_page_box_mismatch")
            pixel_delta = max(abs(rxls_width - lo_width), abs(rxls_height - lo_height))
            page_errors.append((pixel_delta * 72_000 + 48) // 96)

    expected_scale_modes = {
        "fit": expected_workbooks // 2,
        "scale": expected_workbooks // 2,
    }
    if expected_workbooks % 2 != 0 or dict(scale_modes) != expected_scale_modes:
        failures.add("scale_fit_coverage_incomplete")
    page_median = _nearest_rank(page_errors, 1, 2)
    page_p95 = _nearest_rank(page_errors, 95, 100)
    page_max = max(page_errors)
    similarity_mean = _mean(file_similarities)
    edge_f1 = _edge_f1(metric_pages)
    semantic_precision, semantic_recall = _semantic_codepoint(metric_pages)
    text_box_coverage = _ratio_ppm(
        text_box_matched,
        text_box_candidates,
        empty=1_000_000,
    )
    if sum(text_box_histogram.values()) != text_box_matched:
        raise GateError("text_box_histogram_count")
    text_box_median = (
        _histogram_quantile(text_box_histogram, 1, 2)
        if text_box_histogram
        else None
    )
    text_box_p95 = (
        _histogram_quantile(text_box_histogram, 95, 100)
        if text_box_histogram
        else None
    )
    if similarity_mean < SIMILARITY_MEAN_MIN_PPM:
        failures.add("similarity_mean_below_target")
    if edge_f1 < EDGE_F1_MIN_PPM:
        failures.add("edge_f1_below_target")
    if semantic_precision < SEMANTIC_CODEPOINT_MIN_PPM:
        failures.add("semantic_codepoint_precision_below_target")
    if semantic_recall < SEMANTIC_CODEPOINT_MIN_PPM:
        failures.add("semantic_codepoint_recall_below_target")
    if text_box_coverage < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_box_match_coverage_below_target")
    if text_box_ambiguous != 0:
        failures.add("text_box_mapping_ambiguous")
    if text_box_unmatched != 0:
        failures.add("text_box_mapping_unmatched")
    if (
        text_box_median is None
        or text_box_median > TEXT_BOX_MEDIAN_MAX_MILLIPOINTS
    ):
        failures.add("text_box_median_error_above_target")
    if text_box_p95 is None or text_box_p95 > TEXT_BOX_P95_MAX_MILLIPOINTS:
        failures.add("text_box_p95_error_above_target")
    if page_median > PAGE_MEDIAN_MAX_MILLIPOINTS:
        failures.add("page_box_median_above_target")
    if page_p95 > PAGE_P95_MAX_MILLIPOINTS:
        failures.add("page_box_p95_above_target")
    if page_max > PAGE_MAX_MILLIPOINTS:
        failures.add("page_box_max_above_target")

    return {
        "coverage": {
            "by_scale_mode": dict(sorted(scale_modes.items())),
            "libreoffice_pdf_font_objects": font_objects,
            "page_count_histogram": {
                str(key): value for key, value in sorted(page_count_histogram.items())
            },
            "pages": total_pages,
            "text_box_candidates": text_box_candidates,
            "text_box_matches": text_box_matched,
            "workbooks": len(files),
        },
        "evidence": {
            "font_pack_sha256": font_pack_sha,
            "oracle_build_contract_sha256": identity["build_contract_sha256"],
            "oracle_image_config_digest": identity["image"]["config_digest"],
            "oracle_lock_file_sha256": identity["lock_file_sha256"],
            "oracle_libreoffice_artifact_sha256": identity["libreoffice"][
                "artifact_sha256"
            ],
            "pdffonts_sha256": identity["pdf_font_inspector"]["pdffonts_sha256"],
            "renderer_sha256": renderer_sha,
            "report_bytes": report_bytes,
            "report_sha256": report_sha256,
        },
        "expected": {
            "page_box_pixels": {
                "height": EXPECTED_PAGE_HEIGHT,
                "width": EXPECTED_PAGE_WIDTH,
            },
            "pages_per_workbook": EXPECTED_PAGES_PER_WORKBOOK,
            "workbooks_by_scale_mode": expected_scale_modes,
        },
        "failures": sorted(failures),
        "metrics": {
            "edge_f1_ppm": edge_f1,
            "page_box_max_millipoints": page_max,
            "page_box_median_millipoints": page_median,
            "page_box_p95_millipoints": page_p95,
            "semantic_codepoint_precision_ppm": semantic_precision,
            "semantic_codepoint_recall_ppm": semantic_recall,
            "similarity_mean_ppm": similarity_mean,
            "text_box_ambiguous": text_box_ambiguous,
            "text_box_match_coverage_ppm": text_box_coverage,
            "text_box_median_error_millipoints": text_box_median,
            "text_box_p95_error_millipoints": text_box_p95,
            "text_box_unmatched": text_box_unmatched,
        },
        "passed": not failures,
        "schema": OUTPUT_SCHEMA,
        "thresholds": {
            "edge_f1_min_ppm": EDGE_F1_MIN_PPM,
            "page_box_max_millipoints": PAGE_MAX_MILLIPOINTS,
            "page_box_median_max_millipoints": PAGE_MEDIAN_MAX_MILLIPOINTS,
            "page_box_p95_max_millipoints": PAGE_P95_MAX_MILLIPOINTS,
            "semantic_codepoint_precision_min_ppm": SEMANTIC_CODEPOINT_MIN_PPM,
            "semantic_codepoint_recall_min_ppm": SEMANTIC_CODEPOINT_MIN_PPM,
            "similarity_mean_min_ppm": SIMILARITY_MEAN_MIN_PPM,
            "text_box_match_min_ppm": TEXT_BOX_MATCH_MIN_PPM,
            "text_box_median_max_millipoints": TEXT_BOX_MEDIAN_MAX_MILLIPOINTS,
            "text_box_p95_max_millipoints": TEXT_BOX_P95_MAX_MILLIPOINTS,
        },
    }


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--expected-workbooks", type=int, required=True)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    if not 1 <= args.expected_workbooks <= MAX_WORKBOOKS:
        print("check-authored-print-parity: expected_workbooks", file=sys.stderr)
        return 2
    try:
        report, digest, size = _read(args.report)
        result = evaluate(
            report,
            report_sha256=digest,
            report_bytes=size,
            expected_workbooks=args.expected_workbooks,
        )
    except GateError as error:
        print(f"check-authored-print-parity: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
