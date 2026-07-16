#!/usr/bin/env python3
"""Gate repeat LibreOffice parity runs without publishing corpus paths.

The two inputs must be complete ``rxls.libreoffice-render-parity.v1``
campaign reports produced with exactly the same configuration, preflight, and
renderer binary.  Input workbooks are paired only by their SHA-256 identity;
host paths are deliberately ignored and never copied to the result.

Everything owned by rxls (renderer metadata, scene hashes, page mapping,
semantic counts, and page dimensions) must be exact.  The only tolerated
variation is integer visual evidence derived from the LibreOffice oracle.  The
gate publishes sorted, path-neutral distributions of absolute PPM deltas for
plain similarity, blurred-luma similarity, and the three mask F1 scores.  The
20,000 PPM defaults are deliberately bounded just above the clean locked
40-workbook profile maxima (11,447 visual PPM and 16,828 mask PPM).

Exit status is 0 for a pass, 1 for an identity/stability/threshold failure, and
2 for malformed, incomplete, duplicate, oversized, or unreadable evidence.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import tempfile
from typing import Any, Sequence


INPUT_SCHEMA = "rxls.libreoffice-render-parity.v1"
OUTPUT_SCHEMA = "rxls.libreoffice-render-repeatability.v1"
MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_TOTAL_REPORT_BYTES = 512 * 1024 * 1024
MAX_FILES = 1_000_000
DEFAULT_MAX_DRIFT_PPM = 20_000
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")

SIMILARITY_METRIC = "similarity_ppm"
BLUR_METRIC = "blurred_luma_similarity_ppm"
MASK_METRICS = ("edge_f1_ppm", "foreground_f1_ppm", "text_ink_f1_ppm")
DRIFT_METRICS = (SIMILARITY_METRIC, BLUR_METRIC, *MASK_METRICS)

PAGE_DIMENSION_KEYS = (
    "canvas_size",
    "libreoffice_size",
    "metric_work_units",
    "pixels",
    "rxls_size",
)
AGGREGATE_DIMENSION_KEYS = (
    "max_page_height_delta_pixels",
    "max_page_width_delta_pixels",
    "metric_work_units",
    "page_dimension_mismatches",
    "pages",
    "pixels",
    "stacked_canvas_size",
)
RENDERER_METRIC_KEYS = (
    "edge_rxls_pixels",
    "foreground_rxls_bbox",
    "foreground_rxls_centroid_x_millipixels",
    "foreground_rxls_centroid_y_millipixels",
    "foreground_rxls_pixels",
    "foreground_rxls_x_sum",
    "foreground_rxls_y_sum",
    "text_ink_rxls_bbox",
    "text_ink_rxls_centroid_x_millipixels",
    "text_ink_rxls_centroid_y_millipixels",
    "text_ink_rxls_pixels",
    "text_ink_rxls_x_sum",
    "text_ink_rxls_y_sum",
)
ORACLE_VISUAL_METRIC_KEYS = frozenset(
    {
        "absolute_error_sum",
        "blurred_luma_absolute_error_sum",
        "blurred_luma_mean_absolute_error_ppm",
        "blurred_luma_similarity_ppm",
        "changed_pixels",
        "edge_f1_ppm",
        "edge_libreoffice_matched_1px",
        "edge_libreoffice_pixels",
        "edge_precision_ppm",
        "edge_recall_ppm",
        "edge_rxls_matched_1px",
        "exact_pages",
        "foreground_alignment_comparable",
        "foreground_bbox_alignment_max_delta_pixels",
        "foreground_bbox_delta_pixels",
        "foreground_centroid_delta_x_millipixels",
        "foreground_centroid_delta_y_millipixels",
        "foreground_centroid_distance_millipixels",
        "foreground_f1_ppm",
        "foreground_libreoffice_bbox",
        "foreground_libreoffice_centroid_x_millipixels",
        "foreground_libreoffice_centroid_y_millipixels",
        "foreground_libreoffice_matched_1px",
        "foreground_libreoffice_pixels",
        "foreground_libreoffice_x_sum",
        "foreground_libreoffice_y_sum",
        "foreground_matched_color_absolute_error_sum",
        "foreground_matched_color_mean_absolute_error_ppm",
        "foreground_matched_color_samples",
        "foreground_matched_color_similarity_ppm",
        "foreground_precision_ppm",
        "foreground_recall_ppm",
        "foreground_rxls_matched_1px",
        "max_channel_delta",
        "mean_absolute_error_ppm",
        "mismatch_ppm",
        "root_mean_square_error_ppm",
        "similarity_ppm",
        "squared_error_sum",
        "text_ink_alignment_comparable",
        "text_ink_bbox_alignment_max_delta_pixels",
        "text_ink_bbox_delta_pixels",
        "text_ink_centroid_delta_x_millipixels",
        "text_ink_centroid_delta_y_millipixels",
        "text_ink_centroid_distance_millipixels",
        "text_ink_f1_ppm",
        "text_ink_libreoffice_bbox",
        "text_ink_libreoffice_centroid_x_millipixels",
        "text_ink_libreoffice_centroid_y_millipixels",
        "text_ink_libreoffice_matched_1px",
        "text_ink_libreoffice_pixels",
        "text_ink_libreoffice_x_sum",
        "text_ink_libreoffice_y_sum",
        "text_ink_precision_ppm",
        "text_ink_recall_ppm",
        "text_ink_rxls_matched_1px",
    }
)


class MalformedReport(RuntimeError):
    """The supplied evidence cannot safely participate in the gate."""


@dataclass(frozen=True)
class LoadedReport:
    document: dict[str, Any]
    bytes: int
    sha256: str


@dataclass(frozen=True)
class ValidatedReport:
    loaded: LoadedReport
    files: dict[str, dict[str, Any]]
    page_count: int


def canonical_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def canonical_sha256(value: object) -> str:
    return hashlib.sha256(canonical_bytes(value)).hexdigest()


def _strict_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise MalformedReport("report_duplicate_json_key")
        result[key] = value
    return result


def _reject_json_constant(_value: str) -> object:
    raise MalformedReport("report_nonfinite_number")


def _integer(value: object, code: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise MalformedReport(code)
    return value


def _ppm(value: object, code: str) -> int:
    number = _integer(value, code)
    if number > 1_000_000:
        raise MalformedReport(code)
    return number


def _sha256(value: object, code: str) -> str:
    if not isinstance(value, str) or SHA256_RE.fullmatch(value) is None:
        raise MalformedReport(code)
    return value


def _text(value: object, code: str, *, maximum: int = 16_384) -> str:
    if not isinstance(value, str) or not value or len(value) > maximum:
        raise MalformedReport(code)
    return value


def _size(value: object, code: str) -> dict[str, int]:
    if not isinstance(value, dict) or set(value) != {"height", "width"}:
        raise MalformedReport(code)
    width = _integer(value.get("width"), code, minimum=1)
    height = _integer(value.get("height"), code, minimum=1)
    return {"height": height, "width": width}


def read_report(path: Path, remaining_bytes: int) -> LoadedReport:
    byte_limit = min(MAX_REPORT_BYTES, remaining_bytes)
    if byte_limit <= 0:
        raise MalformedReport("report_bytes_limit")
    try:
        with path.open("rb") as source:
            payload = source.read(byte_limit + 1)
    except OSError as error:
        raise MalformedReport("report_unreadable") from error
    if not payload or len(payload) > byte_limit:
        raise MalformedReport("report_bytes_limit")
    try:
        document = json.loads(
            payload,
            object_pairs_hook=_strict_object,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
        raise MalformedReport("report_invalid_json") from error
    if not isinstance(document, dict):
        raise MalformedReport("report_not_object")
    return LoadedReport(
        document=document,
        bytes=len(payload),
        sha256=hashlib.sha256(payload).hexdigest(),
    )


def _validate_renderer_identity(configuration: dict[str, Any], preflight: dict[str, Any]) -> None:
    identity = configuration.get("renderer_binary")
    if not isinstance(identity, dict) or set(identity) != {"bytes", "sha256"}:
        raise MalformedReport("renderer_binary_identity")
    _integer(identity.get("bytes"), "renderer_binary_identity", minimum=1)
    _sha256(identity.get("sha256"), "renderer_binary_identity")
    rxls_command = preflight.get("rxls_command")
    if not isinstance(rxls_command, dict):
        raise MalformedReport("preflight_renderer_identity")
    if rxls_command.get("binary_identity") != identity:
        raise MalformedReport("preflight_renderer_identity")


def _validate_semantic_metrics(metrics: dict[str, Any], code: str) -> None:
    semantic = {key: value for key, value in metrics.items() if key.startswith("semantic_")}
    if not semantic:
        raise MalformedReport(code)
    for value in semantic.values():
        _integer(value, code)


def _validate_renderer_metrics(metrics: dict[str, Any], code: str) -> None:
    for key in RENDERER_METRIC_KEYS:
        if key not in metrics:
            raise MalformedReport(code)
    for key in ("foreground_rxls_bbox", "text_ink_rxls_bbox"):
        bbox = metrics.get(key)
        if not isinstance(bbox, dict) or set(bbox) != {
            "bottom",
            "left",
            "present",
            "right",
            "top",
        }:
            raise MalformedReport(code)
        for value in bbox.values():
            _integer(value, code)
    for key in set(RENDERER_METRIC_KEYS) - {
        "foreground_rxls_bbox",
        "text_ink_rxls_bbox",
    }:
        _integer(metrics.get(key), code)


def _validate_page(page: object) -> dict[str, Any]:
    if not isinstance(page, dict):
        raise MalformedReport("page_not_object")
    _integer(page.get("sheet_index"), "page_mapping")
    for key in DRIFT_METRICS:
        _ppm(page.get(key), "page_visual_metric")
    for key in PAGE_DIMENSION_KEYS:
        if key not in page:
            raise MalformedReport("page_dimension_evidence")
    _size(page["canvas_size"], "page_dimension_evidence")
    _size(page["libreoffice_size"], "page_dimension_evidence")
    _size(page["rxls_size"], "page_dimension_evidence")
    _integer(page["pixels"], "page_dimension_evidence", minimum=1)
    _integer(page["metric_work_units"], "page_dimension_evidence", minimum=1)
    _validate_semantic_metrics(page, "page_semantic_evidence")
    _validate_renderer_metrics(page, "page_renderer_evidence")
    return page


def _validate_aggregate(metrics: object, page_count: int) -> dict[str, Any]:
    if not isinstance(metrics, dict):
        raise MalformedReport("aggregate_metrics")
    for key in DRIFT_METRICS:
        _ppm(metrics.get(key), "aggregate_visual_metric")
    for key in AGGREGATE_DIMENSION_KEYS:
        if key not in metrics:
            raise MalformedReport("aggregate_dimension_evidence")
    if _integer(metrics["pages"], "aggregate_dimension_evidence") != page_count:
        raise MalformedReport("aggregate_page_count")
    mismatches = _integer(
        metrics["page_dimension_mismatches"], "aggregate_dimension_evidence"
    )
    if mismatches > page_count:
        raise MalformedReport("aggregate_dimension_evidence")
    _integer(metrics["max_page_height_delta_pixels"], "aggregate_dimension_evidence")
    _integer(metrics["max_page_width_delta_pixels"], "aggregate_dimension_evidence")
    _integer(metrics["pixels"], "aggregate_dimension_evidence", minimum=1)
    _integer(metrics["metric_work_units"], "aggregate_dimension_evidence", minimum=1)
    _size(metrics["stacked_canvas_size"], "aggregate_dimension_evidence")
    _validate_semantic_metrics(metrics, "aggregate_semantic_evidence")
    _validate_renderer_metrics(metrics, "aggregate_renderer_evidence")
    return metrics


def _validate_comparable_row(row: dict[str, Any]) -> int:
    renderer = row.get("renderer")
    scenes = row.get("scenes")
    artifacts = row.get("artifacts")
    pages = row.get("pages")
    if not isinstance(renderer, dict) or not renderer:
        raise MalformedReport("renderer_evidence")
    if not isinstance(scenes, list) or not scenes:
        raise MalformedReport("scene_evidence")
    if not isinstance(artifacts, dict) or set(artifacts) != {
        "libreoffice_pages",
        "rxls_pages",
    }:
        raise MalformedReport("artifact_evidence")
    if not isinstance(pages, list) or not pages:
        raise MalformedReport("page_evidence")
    if len(pages) > 1_000_000:
        raise MalformedReport("page_count_limit")
    if _integer(artifacts.get("libreoffice_pages"), "artifact_evidence") != len(pages):
        raise MalformedReport("artifact_page_count")
    if _integer(artifacts.get("rxls_pages"), "artifact_evidence") != len(pages):
        raise MalformedReport("artifact_page_count")
    seen_sheets: set[int] = set()
    for raw_page in pages:
        page = _validate_page(raw_page)
        sheet = int(page["sheet_index"])
        if sheet in seen_sheets:
            raise MalformedReport("duplicate_page_mapping")
        seen_sheets.add(sheet)
    _validate_aggregate(row.get("metrics"), len(pages))
    return len(pages)


def validate_report(loaded: LoadedReport) -> ValidatedReport:
    report = loaded.document
    if set(report) != {
        "configuration",
        "discovery",
        "files",
        "mode",
        "preflight",
        "schema",
        "summary",
    }:
        raise MalformedReport("report_shape")
    if report.get("schema") != INPUT_SCHEMA or report.get("mode") != "compare":
        raise MalformedReport("report_schema_or_mode")
    configuration = report.get("configuration")
    preflight = report.get("preflight")
    discovery = report.get("discovery")
    summary = report.get("summary")
    rows = report.get("files")
    if not isinstance(configuration, dict) or not configuration:
        raise MalformedReport("configuration")
    if not isinstance(preflight, dict) or not preflight:
        raise MalformedReport("preflight")
    if (
        not isinstance(discovery, dict)
        or not isinstance(summary, dict)
        or not isinstance(rows, list)
    ):
        raise MalformedReport("report_payload")
    _validate_renderer_identity(configuration, preflight)

    if set(discovery) != {
        "candidate_count",
        "pre_shard_selected_count",
        "selected_count",
        "shard_candidate_count",
        "shard_count",
        "shard_index",
        "truncated",
    }:
        raise MalformedReport("discovery_shape")
    shard_count = _integer(discovery.get("shard_count"), "campaign_incomplete")
    shard_index = _integer(discovery.get("shard_index"), "campaign_incomplete")
    if shard_count != 1 or shard_index != 0 or discovery.get("truncated") is not False:
        raise MalformedReport("campaign_incomplete")
    selected = _integer(discovery.get("selected_count"), "campaign_coverage", minimum=1)
    pre_shard = _integer(discovery.get("pre_shard_selected_count"), "campaign_coverage")
    shard_candidates = _integer(discovery.get("shard_candidate_count"), "campaign_coverage")
    candidates = _integer(discovery.get("candidate_count"), "campaign_coverage")
    if selected > MAX_FILES:
        raise MalformedReport("file_count_limit")
    if (
        selected != pre_shard
        or selected != shard_candidates
        or selected != len(rows)
        or candidates < selected
    ):
        raise MalformedReport("campaign_coverage")

    if set(summary) != {
        "by_classification",
        "by_status",
        "files",
        "input_bytes_considered",
        "metric_cohorts",
    }:
        raise MalformedReport("summary_shape")
    if _integer(summary.get("files"), "summary_file_count") != selected:
        raise MalformedReport("summary_file_count")
    _integer(summary.get("input_bytes_considered"), "summary_input_bytes")
    if not isinstance(summary.get("metric_cohorts"), dict):
        raise MalformedReport("summary_metric_cohorts")

    files: dict[str, dict[str, Any]] = {}
    statuses: dict[str, int] = {}
    classifications: dict[str, int] = {}
    page_count = 0
    for row in rows:
        if not isinstance(row, dict):
            raise MalformedReport("file_row")
        digest = _sha256(row.get("sha256"), "input_sha256")
        if digest in files:
            raise MalformedReport("overlapping_input")
        _text(row.get("path"), "input_path")
        _integer(row.get("bytes"), "input_bytes")
        _text(row.get("format"), "input_format", maximum=32)
        status = _text(row.get("status"), "file_status", maximum=128)
        classification = _text(
            row.get("classification"), "file_classification", maximum=256
        )
        if status in {"compared", "different"}:
            page_count += _validate_comparable_row(row)
        elif "metrics" in row or "pages" in row:
            raise MalformedReport("incomparable_row_metrics")
        files[digest] = row
        statuses[status] = statuses.get(status, 0) + 1
        classifications[classification] = classifications.get(classification, 0) + 1

    if summary.get("by_status") != dict(sorted(statuses.items())):
        raise MalformedReport("summary_status_counts")
    if summary.get("by_classification") != dict(sorted(classifications.items())):
        raise MalformedReport("summary_classification_counts")
    return ValidatedReport(loaded=loaded, files=files, page_count=page_count)


def _metric_subset(metrics: dict[str, Any], keys: Sequence[str]) -> dict[str, Any]:
    return {key: metrics.get(key) for key in keys}


def _semantic_subset(metrics: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in metrics.items() if key.startswith("semantic_")}


def _non_oracle_subset(metrics: dict[str, Any]) -> dict[str, Any]:
    """Return evidence that may not vary with the LibreOffice visual oracle."""
    return {
        key: value
        for key, value in metrics.items()
        if key not in ORACLE_VISUAL_METRIC_KEYS
    }


def _distribution(values: list[int]) -> dict[str, Any]:
    ordered = sorted(values)
    return {
        "absolute_deltas_ppm": ordered,
        "count": len(ordered),
        "max_absolute_delta_ppm": max(ordered) if ordered else None,
    }


def _identity_result(
    baseline: ValidatedReport, candidate: ValidatedReport
) -> dict[str, Any]:
    left = baseline.loaded.document
    right = candidate.loaded.document
    left_inputs = sorted(baseline.files)
    right_inputs = sorted(candidate.files)
    return {
        "configuration": {
            "baseline_sha256": canonical_sha256(left["configuration"]),
            "candidate_sha256": canonical_sha256(right["configuration"]),
            "equal": left["configuration"] == right["configuration"],
        },
        "input_set": {
            "baseline_count": len(left_inputs),
            "baseline_sha256": canonical_sha256(left_inputs),
            "candidate_count": len(right_inputs),
            "candidate_sha256": canonical_sha256(right_inputs),
            "equal": left_inputs == right_inputs,
        },
        "preflight": {
            "baseline_sha256": canonical_sha256(left["preflight"]),
            "candidate_sha256": canonical_sha256(right["preflight"]),
            "equal": left["preflight"] == right["preflight"],
        },
        "renderer_binary": {
            "baseline": left["configuration"]["renderer_binary"],
            "candidate": right["configuration"]["renderer_binary"],
            "equal": left["configuration"]["renderer_binary"]
            == right["configuration"]["renderer_binary"],
        },
    }


def compare_reports(
    baseline: ValidatedReport,
    candidate: ValidatedReport,
    *,
    max_similarity_drift_ppm: int = DEFAULT_MAX_DRIFT_PPM,
    max_blur_drift_ppm: int = DEFAULT_MAX_DRIFT_PPM,
    max_mask_drift_ppm: int = DEFAULT_MAX_DRIFT_PPM,
) -> dict[str, Any]:
    for value in (
        max_similarity_drift_ppm,
        max_blur_drift_ppm,
        max_mask_drift_ppm,
    ):
        _ppm(value, "threshold")

    identity = _identity_result(baseline, candidate)
    failures: set[str] = set()
    if not identity["configuration"]["equal"]:
        failures.add("configuration_mismatch")
    if not identity["preflight"]["equal"]:
        failures.add("preflight_mismatch")
    if not identity["renderer_binary"]["equal"]:
        failures.add("renderer_binary_mismatch")
    if not identity["input_set"]["equal"]:
        failures.add("input_set_mismatch")

    deltas: dict[str, list[int]] = {key: [] for key in DRIFT_METRICS}
    compared_pages = 0
    if identity["input_set"]["equal"]:
        for digest in sorted(baseline.files):
            left = baseline.files[digest]
            right = candidate.files[digest]
            if left.get("status") != right.get("status") or left.get(
                "classification"
            ) != right.get("classification"):
                failures.add("status_or_classification_mismatch")
            for key, failure in (
                ("renderer", "renderer_evidence_mismatch"),
                ("scenes", "scene_evidence_mismatch"),
                ("artifacts", "artifact_evidence_mismatch"),
            ):
                if left.get(key) != right.get(key):
                    failures.add(failure)

            excluded = {
                "artifacts",
                "classification",
                "metrics",
                "pages",
                "path",
                "renderer",
                "scenes",
                "status",
            }
            left_evidence = {key: value for key, value in left.items() if key not in excluded}
            right_evidence = {key: value for key, value in right.items() if key not in excluded}
            if left_evidence != right_evidence:
                failures.add("file_evidence_mismatch")

            left_pages = left.get("pages")
            right_pages = right.get("pages")
            left_metrics = left.get("metrics")
            right_metrics = right.get("metrics")
            if left_pages is None and right_pages is None:
                continue
            assert isinstance(left_pages, list) and isinstance(right_pages, list)
            assert isinstance(left_metrics, dict) and isinstance(right_metrics, dict)
            if len(left_pages) != len(right_pages):
                failures.add("page_mapping_mismatch")
                continue
            if set(left_metrics) != set(right_metrics):
                failures.add("metric_shape_mismatch")
            if _semantic_subset(left_metrics) != _semantic_subset(right_metrics):
                failures.add("semantic_counts_mismatch")
            if _metric_subset(left_metrics, AGGREGATE_DIMENSION_KEYS) != _metric_subset(
                right_metrics, AGGREGATE_DIMENSION_KEYS
            ):
                failures.add("page_dimensions_mismatch")
            if _metric_subset(left_metrics, RENDERER_METRIC_KEYS) != _metric_subset(
                right_metrics, RENDERER_METRIC_KEYS
            ):
                failures.add("renderer_metric_evidence_mismatch")
            if _non_oracle_subset(left_metrics) != _non_oracle_subset(right_metrics):
                failures.add("non_oracle_metric_evidence_mismatch")
            for key in DRIFT_METRICS:
                deltas[key].append(abs(int(left_metrics[key]) - int(right_metrics[key])))

            for left_page, right_page in zip(left_pages, right_pages):
                if set(left_page) != set(right_page):
                    failures.add("page_metric_shape_mismatch")
                if left_page.get("sheet_index") != right_page.get("sheet_index"):
                    failures.add("page_mapping_mismatch")
                if _semantic_subset(left_page) != _semantic_subset(right_page):
                    failures.add("semantic_counts_mismatch")
                if _metric_subset(left_page, PAGE_DIMENSION_KEYS) != _metric_subset(
                    right_page, PAGE_DIMENSION_KEYS
                ):
                    failures.add("page_dimensions_mismatch")
                if _metric_subset(left_page, RENDERER_METRIC_KEYS) != _metric_subset(
                    right_page, RENDERER_METRIC_KEYS
                ):
                    failures.add("renderer_metric_evidence_mismatch")
                if _non_oracle_subset(left_page) != _non_oracle_subset(right_page):
                    failures.add("non_oracle_metric_evidence_mismatch")
                for key in DRIFT_METRICS:
                    deltas[key].append(abs(int(left_page[key]) - int(right_page[key])))
                compared_pages += 1

    distributions = {key: _distribution(deltas[key]) for key in DRIFT_METRICS}
    similarity_max = distributions[SIMILARITY_METRIC]["max_absolute_delta_ppm"]
    blur_max = distributions[BLUR_METRIC]["max_absolute_delta_ppm"]
    mask_maxima = [
        distributions[key]["max_absolute_delta_ppm"]
        for key in MASK_METRICS
        if distributions[key]["max_absolute_delta_ppm"] is not None
    ]
    mask_max = max(mask_maxima) if mask_maxima else None
    if similarity_max is not None and similarity_max > max_similarity_drift_ppm:
        failures.add("similarity_drift_threshold")
    if blur_max is not None and blur_max > max_blur_drift_ppm:
        failures.add("blur_drift_threshold")
    if mask_max is not None and mask_max > max_mask_drift_ppm:
        failures.add("mask_drift_threshold")
    if identity["input_set"]["equal"] and not deltas[SIMILARITY_METRIC]:
        failures.add("no_comparable_visual_evidence")

    failure_list = sorted(failures)
    return {
        "coverage": {
            "pages": compared_pages,
            "visual_observations_per_metric": len(deltas[SIMILARITY_METRIC]),
            "workbooks": len(baseline.files) if identity["input_set"]["equal"] else 0,
        },
        "drift": {
            "blurred_luma_similarity": distributions[BLUR_METRIC],
            "mask_f1": {
                "edge": distributions["edge_f1_ppm"],
                "foreground": distributions["foreground_f1_ppm"],
                "max_absolute_delta_ppm": mask_max,
                "text_ink": distributions["text_ink_f1_ppm"],
            },
            "similarity": distributions[SIMILARITY_METRIC],
        },
        "failures": failure_list,
        "identity": identity,
        "metric_policy": {
            "distribution": "sorted_absolute_paired_integer_ppm_deltas",
            "input_pairing": "sha256",
            "observations": "workbook_aggregate_and_page",
            "paths_or_content_retained": False,
        },
        "reports": {
            "baseline": {
                "bytes": baseline.loaded.bytes,
                "sha256": baseline.loaded.sha256,
            },
            "candidate": {
                "bytes": candidate.loaded.bytes,
                "sha256": candidate.loaded.sha256,
            },
        },
        "schema": OUTPUT_SCHEMA,
        "status": "pass" if not failure_list else "fail",
        "thresholds_ppm": {
            "blurred_luma_similarity_max_absolute_drift": max_blur_drift_ppm,
            "mask_f1_max_absolute_drift": max_mask_drift_ppm,
            "similarity_max_absolute_drift": max_similarity_drift_ppm,
        },
    }


def write_atomic(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent, prefix=f".{path.name}.", suffix=".tmp"
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    except BaseException:
        try:
            temporary.unlink()
        except OSError:
            pass
        raise


def _threshold(value: str) -> int:
    try:
        parsed = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be an integer PPM value") from error
    if not 0 <= parsed <= 1_000_000:
        raise argparse.ArgumentTypeError("must be between 0 and 1000000 PPM")
    return parsed


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--max-similarity-drift-ppm",
        type=_threshold,
        default=DEFAULT_MAX_DRIFT_PPM,
    )
    parser.add_argument(
        "--max-blur-drift-ppm", type=_threshold, default=DEFAULT_MAX_DRIFT_PPM
    )
    parser.add_argument(
        "--max-mask-drift-ppm", type=_threshold, default=DEFAULT_MAX_DRIFT_PPM
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        baseline_loaded = read_report(args.baseline, MAX_TOTAL_REPORT_BYTES)
        candidate_loaded = read_report(
            args.candidate, MAX_TOTAL_REPORT_BYTES - baseline_loaded.bytes
        )
        baseline = validate_report(baseline_loaded)
        candidate = validate_report(candidate_loaded)
        result = compare_reports(
            baseline,
            candidate,
            max_similarity_drift_ppm=args.max_similarity_drift_ppm,
            max_blur_drift_ppm=args.max_blur_drift_ppm,
            max_mask_drift_ppm=args.max_mask_drift_ppm,
        )
        write_atomic(args.output, canonical_bytes(result))
        return 0 if result["status"] == "pass" else 1
    except MalformedReport as error:
        print(f"compare-render-parity-runs: {error}", file=sys.stderr)
        return 2
    except OSError:
        print("compare-render-parity-runs: filesystem_error", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
