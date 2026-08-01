#!/usr/bin/env python3
"""Reduce failed Render Oracle reports to a bounded path-neutral summary."""

from __future__ import annotations

import argparse
from collections import Counter
import copy
from fractions import Fraction
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import secrets
import stat
import sys
import tempfile
from typing import Any, Iterable, Sequence

try:
    from strict_json_contract import type_exact_equal
except ModuleNotFoundError:  # Imported as ``scripts.*`` by unit tests.
    from scripts.strict_json_contract import type_exact_equal


INPUT_SCHEMA = "rxls.libreoffice-render-parity.v1"
OUTPUT_SCHEMA = "rxls.render-oracle-failure-summary.v10"
OUTPUT_NAME = "render-oracle-failure-summary.json"
MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_TOTAL_BYTES = 768 * 1024 * 1024
MAX_OUTPUT_BYTES = 2 * 1024 * 1024
MAX_ROOT_ENTRIES = 128
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_JSON_INTEGER_DIGITS = 128
MAX_PAGE_COUNT = 64
MAX_CASE_DIAGNOSTICS_PER_REPORT = 64
MAX_SEMANTIC_CODEPOINTS_PER_WORKBOOK = 1_000_000
MAX_POPPLER_ITEMS_PER_PAGE = 250_000
MAX_RASTER_PIXELS_PER_PAGE = 1_000_000_000
MAX_POINT_RATIONAL_DIGITS = 32
MAX_POINT_ABSOLUTE_VALUE = 1_000_000
MAX_POINT_DELTA_MICROPOINTS = MAX_POINT_ABSOLUTE_VALUE * 1_000_000
SHARDS = 4

HEAD_RE = re.compile(r"[0-9a-f]{40}\Z")
HASH_RE = re.compile(r"[0-9a-f]{64}\Z")
CODE_RE = re.compile(r"[a-z][a-z0-9_]{0,95}\Z")
RAW_REPORT_RE = re.compile(
    r"(?:parity-report-|parity-[ab]-shard-|"
    r"authored-print-report|authored-print-shard-)"
)
STATUSES = frozenset({"compared", "different", "error", "skipped"})
METRIC_BEARING_STATUSES = frozenset({"compared", "different"})
PREIDENTITY_CLASSIFICATION_STATUSES = {
    "corpus_input_budget_exceeded": "skipped",
    "input_limit": "skipped",
    "manifest_size_mismatch": "error",
    "missing_input": "skipped",
    "symlink_input": "skipped",
    "unreadable_input": "skipped",
}
FORMATS = frozenset({"ods", "xls", "xlsb", "xlsx"})
CASE_ID_DOMAIN = b"rxls.render-oracle-failure-case.v1\0"
CASE_ID_KEY_BYTES = 32
# The key is created once per summary and is never serialized. Identifiers are
# stable inside one artifact but deliberately cannot be correlated across runs.
CASE_ID_POLICY = {
    "algorithm": "hmac-sha256",
    "correlation": "within_summary_only",
    "domain": "rxls.render-oracle-failure-case.v1",
    "input": "domain_separated_workbook_digest",
    "key": "ephemeral_non_exported",
    "max_cases_per_report": MAX_CASE_DIAGNOSTICS_PER_REPORT,
    "selection": "lexicographically_lowest_case_ids",
}
UNREVIEWED_CLASSIFICATION = "unreviewed_classification"
# This is deliberately a finite public vocabulary. It covers the stable terminal
# outcomes and command/runtime failures emitted by evaluate_case and the locked
# runtime smoke adapter. Any new or injected snake_case value is reduced to the
# fixed bucket until it receives an explicit privacy review here.
REVIEWED_CLASSIFICATIONS = frozenset(
    {
        "below_similarity_threshold",
        "input_limit",
        "libreoffice_adapter_profile_path_missing",
        "libreoffice_adapter_profile_setup_failed",
        "libreoffice_command_output_limit",
        "libreoffice_failed",
        "libreoffice_file_output_limit",
        "libreoffice_font_pack_mismatch",
        "libreoffice_not_found",
        "libreoffice_oracle_empty",
        "libreoffice_oracle_rejected",
        "libreoffice_timeout",
        "manifest_sha256_mismatch",
        "manifest_size_mismatch",
        "page_count_mismatch",
        "renderer_command_output_limit",
        "renderer_failed",
        "renderer_file_output_limit",
        "renderer_not_found",
        "renderer_pdf_type3_path_text_missing",
        "renderer_timeout",
        "semantic_content_one_sided",
        "unreadable_input",
        "visual_dependencies_missing",
        "within_threshold",
    }
)
# Unknown detailed classifications may still carry path- or content-derived
# fragments even when they satisfy CODE_RE. These ordered prefixes therefore
# select only a fixed, privacy-reviewed stage label; no part of the input value
# is copied into the output. Values outside these finite families remain in the
# single unreviewed bucket.
COARSE_CLASSIFICATION_PREFIXES = (
    (
        "authored_print_stage",
        ("authored_print_", "render_manifest_authored_"),
    ),
    (
        "input_stage",
        ("corpus_", "input_", "manifest_"),
    ),
    (
        "renderer_pdf_attestation_stage",
        ("renderer_pdf_font_", "renderer_pdf_type3_"),
    ),
    (
        "renderer_page_map_stage",
        (
            "renderer_pdf_page_count",
            "renderer_pdf_page_map",
            "renderer_print_pdf_",
        ),
    ),
    (
        "renderer_raster_stage",
        (
            "renderer_pdf_page_limit",
            "renderer_pdf_page_pixel_",
            "renderer_pdf_raster_",
            "renderer_pdf_total_pixel_",
        ),
    ),
    (
        "renderer_semantic_stage",
        ("renderer_semantic_", "semantic_svg_"),
    ),
    (
        "renderer_bundle_stage",
        ("live_output_", "render_manifest_"),
    ),
    (
        "renderer_stage",
        (
            "renderer_",
            "svg_",
        ),
    ),
    (
        "oracle_font_attestation_stage",
        ("libreoffice_font_",),
    ),
    (
        "oracle_adapter_stage",
        ("libreoffice_adapter_", "oracle_"),
    ),
    (
        "oracle_pdf_stage",
        ("libreoffice_pdf_",),
    ),
    (
        "oracle_raster_stage",
        (
            "libreoffice_page_",
            "libreoffice_raster_",
            "libreoffice_total_pixel_",
        ),
    ),
    (
        "oracle_stage",
        ("libreoffice_",),
    ),
    (
        "environment_stage",
        (
            "font_pack_",
            "numpy_",
            "pillow_",
            "poppler_",
            "visual_dependencies_",
        ),
    ),
    (
        "measurement_geometry_stage",
        ("pdfinfo_",),
    ),
    (
        "measurement_raster_stage",
        ("pdf_raster_", "pdftoppm_", "raster_"),
    ),
    (
        "measurement_semantic_stage",
        ("semantic_bbox_", "semantic_text_", "text_box_"),
    ),
    (
        "measurement_stage",
        (
            "artifact_",
            "comparison_",
            "metric_",
            "page_count_",
            "pdf_",
            "pdffonts_",
            "semantic_",
        ),
    ),
)
COARSE_CLASSIFICATIONS = frozenset(
    bucket for bucket, _ in COARSE_CLASSIFICATION_PREFIXES
)
OUTPUT_CLASSIFICATIONS = (
    REVIEWED_CLASSIFICATIONS
    | COARSE_CLASSIFICATIONS
    | {UNREVIEWED_CLASSIFICATION}
)
DIAGNOSTIC_FEATURES = frozenset(
    {
        "auto-bold-font",
        "auto-bold-font-wrapped",
        "auto-heading-western-asian",
        "auto-heading-western-complex",
        "auto-large-font",
        "auto-long-unwrapped",
        "auto-numeric-color-conditional",
        "auto-numeric-no-conditional",
        "auto-wrapped-color-conditional",
        "auto-wrapped-explicit",
        "auto-wrapped-hidden",
        "auto-wrapped-image",
        "auto-wrapped-long",
        "auto-wrapped-long-anchor",
        "auto-wrapped-merged",
        "auto-wrapped-no-conditional",
        "auto-wrapped-rtl",
        "auto-wrapped-wide",
        "hidden-heading-western-asian",
        "hidden-heading-western-complex",
        "manual-heading-western-asian",
        "manual-heading-western-complex",
    }
)
FEATURES = frozenset(
    {
        "border",
        "cell-fill",
        "chart",
        "chinese-text",
        "column-width",
        "conditional-format",
        "date-format",
        "explicit-row-height",
        "formula-cached",
        "hidden-column",
        "hidden-row",
        "image-drawing",
        "japanese-text",
        "korean-text",
        "latin-text",
        "merged-cells",
        "normal-font-carlito",
        "normal-font-noto",
        "normal-size-11",
        "normal-size-12",
        "noto-ofl-font",
        "number-cell",
        "ooxml-implicit-row",
        "percent-format",
        "print-settings",
        "right-to-left-layout",
        "row-height",
        "rtl-text",
        "sheet-format-missing",
        "sheet-format-present",
        "sparkline",
        "unicode-text",
        "wrapped-text",
    }
)
LABELS = ("authored-print", "parity-a", "parity-b")
MERGED = {
    "authored-print": "authored-print-report.json",
    "parity-a": "parity-report-a.json",
    "parity-b": "parity-report-b.json",
}
SHARDED = {
    "authored-print": "authored-print-shard-{index}.json",
    "parity-a": "parity-a-shard-{index}.json",
    "parity-b": "parity-b-shard-{index}.json",
}
CASES = {"full": 800, "ooxml-row-diagnostic": 34, "pilot": 40}
LANES = {
    "full": {"authored-print": 100, "parity-a": 800, "parity-b": 800},
    "ooxml-row-diagnostic": {
        "authored-print": 0,
        "parity-a": 34,
        "parity-b": 0,
    },
    "pilot": {"authored-print": 4, "parity-a": 40, "parity-b": 0},
}
REPORT_KEYS = {
    "configuration",
    "discovery",
    "files",
    "mode",
    "preflight",
    "schema",
    "summary",
}
DISCOVERY_KEYS = {
    "candidate_count",
    "pre_shard_selected_count",
    "selected_count",
    "shard_candidate_count",
    "shard_count",
    "shard_index",
    "truncated",
}
PDF_POINT_DELTA_KEYS = (
    "crop_box_height",
    "crop_box_width",
    "libreoffice_xhtml_page_size_height",
    "libreoffice_xhtml_page_size_width",
    "media_box_height",
    "media_box_width",
    "rxls_xhtml_page_size_height",
    "rxls_xhtml_page_size_width",
    "xhtml_height",
    "xhtml_width",
)
PDF_DIRECT_POINT_DELTA_KEYS = frozenset(
    {
        "crop_box_height",
        "crop_box_width",
        "media_box_height",
        "media_box_width",
    }
)
PDF_XHTML_CROSSCHECK_DELTA_KEYS = frozenset(PDF_POINT_DELTA_KEYS) - (
    PDF_DIRECT_POINT_DELTA_KEYS
)
PDF_XHTML_CROSSCHECK_MAX_POINTS = Fraction(1, 1000)
GEOMETRY_KEYS = {
    "by_delta",
    "max_direct_absolute_delta_micropoints",
    "max_internal_xhtml_crosscheck_micropoints",
    "mismatch_pages",
    "pages",
    "workbooks",
}
GEOMETRY_DELTA_KEYS = {
    "max_absolute_micropoints",
    "nonzero_pages",
}
PAGE_BOX_GEOMETRY_AXES = ("height", "width")
PAGE_BOX_GEOMETRY_FEATURES = frozenset(
    {
        "chart",
        "column-width",
        "explicit-row-height",
        "hidden-column",
        "hidden-row",
        "image-drawing",
        "ooxml-implicit-row",
        "print-settings",
        "row-height",
        "sheet-format-missing",
        "sheet-format-present",
        "wrapped-text",
    }
)
PAGE_BOX_GEOMETRY_KEYS = {
    "all",
    "box",
    "by_feature",
    "by_format",
    "delta_direction",
    "histogram",
    "rounding",
    "units",
}
PAGE_BOX_GEOMETRY_COHORT_KEYS = {
    "by_axis",
    "pages",
    "workbooks",
}
PAGE_BOX_GEOMETRY_AGGREGATE_AXIS_KEYS = {
    "max_delta_micropoints",
    "min_delta_micropoints",
    "nonzero_pages",
    "sum_delta_micropoints",
}
PAGE_BOX_GEOMETRY_HISTOGRAM_AXIS_KEYS = (
    PAGE_BOX_GEOMETRY_AGGREGATE_AXIS_KEYS | {"histogram"}
)
PAGE_BOX_GEOMETRY_BUCKET_ORDER = (
    "negative_over_100_points",
    "negative_50_to_100_points",
    "negative_25_to_50_points",
    "negative_10_to_25_points",
    "negative_5_to_10_points",
    "negative_1_to_5_points",
    "negative_0_1_to_1_points",
    "negative_up_to_0_1_points",
    "zero",
    "positive_up_to_0_1_points",
    "positive_0_1_to_1_points",
    "positive_1_to_5_points",
    "positive_5_to_10_points",
    "positive_10_to_25_points",
    "positive_25_to_50_points",
    "positive_50_to_100_points",
    "positive_over_100_points",
)
PAGE_BOX_GEOMETRY_MAGNITUDE_UPPER_BOUNDS_MICROPOINTS = (
    100_000,
    1_000_000,
    5_000_000,
    10_000_000,
    25_000_000,
    50_000_000,
    100_000_000,
)
PAGE_BOX_GEOMETRY_BUCKET_INTERVALS = {
    "negative_over_100_points": (
        -MAX_POINT_DELTA_MICROPOINTS,
        -100_000_001,
    ),
    "negative_50_to_100_points": (-100_000_000, -50_000_001),
    "negative_25_to_50_points": (-50_000_000, -25_000_001),
    "negative_10_to_25_points": (-25_000_000, -10_000_001),
    "negative_5_to_10_points": (-10_000_000, -5_000_001),
    "negative_1_to_5_points": (-5_000_000, -1_000_001),
    "negative_0_1_to_1_points": (-1_000_000, -100_001),
    "negative_up_to_0_1_points": (-100_000, -1),
    "zero": (0, 0),
    "positive_up_to_0_1_points": (1, 100_000),
    "positive_0_1_to_1_points": (100_001, 1_000_000),
    "positive_1_to_5_points": (1_000_001, 5_000_000),
    "positive_5_to_10_points": (5_000_001, 10_000_000),
    "positive_10_to_25_points": (10_000_001, 25_000_000),
    "positive_25_to_50_points": (25_000_001, 50_000_000),
    "positive_50_to_100_points": (50_000_001, 100_000_000),
    "positive_over_100_points": (
        100_000_001,
        MAX_POINT_DELTA_MICROPOINTS,
    ),
}
MAX_PAGE_BOX_GEOMETRY_HISTOGRAM_BUCKETS = len(
    PAGE_BOX_GEOMETRY_BUCKET_ORDER
)
PAGE_BOX_GEOMETRY_POLICY = {
    "box": "pdf_crop_box",
    "delta_direction": "rxls_minus_libreoffice",
    "histogram": {
        "absolute_limit_micropoints": (
            MAX_POINT_DELTA_MICROPOINTS
        ),
        "bucket_order": list(PAGE_BOX_GEOMETRY_BUCKET_ORDER),
        "cohorts": "all_and_by_format",
        "encoding": "fixed_signed_magnitude_bands",
        "magnitude_upper_bounds_micropoints": list(
            PAGE_BOX_GEOMETRY_MAGNITUDE_UPPER_BOUNDS_MICROPOINTS
        ),
        "max_buckets_per_axis": (
            MAX_PAGE_BOX_GEOMETRY_HISTOGRAM_BUCKETS
        ),
        "ranges": "lower_exclusive_upper_inclusive_by_magnitude",
    },
    "rounding": "away_from_zero",
    "units": "micropoints",
}
TEXT_GEOMETRY_AXES = (
    "x_min",
    "x_max",
    "y_min",
    "y_max",
    "center_x",
    "center_y",
    "width",
    "height",
)
TEXT_GEOMETRY_POLICY = {
    "content_retained": False,
    "coordinates": "pdf_points_y_down",
    "delta_direction": "rxls_minus_libreoffice",
    "diagnostic_only": True,
    "exact_delta_absolute_limit_millipoints": 1_000_000_000,
    "exact_summary": "count_sum_min_max_and_signed_overflow_counts",
    "histogram": {
        "exact_absolute_limit_millipoints": 2,
        "max_buckets_per_axis": 21,
        "middle_absolute_limit_millipoints": 1_000,
        "middle_bucket_width_millipoints": 500,
        "outer_absolute_limit_millipoints": 10_000,
        "outer_bucket_width_millipoints": 2_000,
        "overflow_bucket_absolute_millipoints": 12_000,
        "rounding": (
            "nearest_width_multiple_half_away_from_zero_"
            "with_nonzero_sign_preserved"
        ),
    },
    "max_geometry_pages_per_report": 2_000,
    "max_histogram_buckets_per_report": 50_000,
    "max_items_per_side_per_page": 250_000,
    "matching": "exact_normalized_token_tuple_unique_on_both_sides",
    "rounding": "nearest_millipoint_half_away_from_zero_exact_rational",
    "shard_budget": "equal_floor_partition_by_declared_shard_count",
    "units": "millipoints",
}
TEXT_GEOMETRY_PAGE_KEYS = {
    "delta_histograms_millipoints",
    "exact_delta_summaries_millipoints",
    "libreoffice_unique_items",
    "matched_items",
    "rxls_unique_items",
}
TEXT_GEOMETRY_EXACT_SUMMARY_KEYS = {
    "count",
    "max_delta_millipoints",
    "min_delta_millipoints",
    "negative_overflow_items",
    "positive_overflow_items",
    "sum_delta_millipoints",
}
TEXT_GEOMETRY_BUCKET_KEYS = {"count", "delta_millipoints"}
TEXT_GEOMETRY_OUTPUT_KEYS = {"all", "by_format"}
TEXT_GEOMETRY_COHORT_KEYS = {
    "by_axis",
    "libreoffice_unique_items",
    "matched_items",
    "pages",
    "rxls_unique_items",
    "workbooks",
}
TEXT_GEOMETRY_AXIS_KEYS = {
    "exact",
    "histogram",
}
MAX_TEXT_GEOMETRY_UNIQUE_ITEMS = 250_000
MAX_TEXT_GEOMETRY_HISTOGRAM_BUCKETS = 21
MAX_TEXT_GEOMETRY_REPORT_PAGES = 2_000
MAX_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS = 50_000
MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS = 1_000_000_000
TEXT_GEOMETRY_EXACT_LIMIT_MILLIPOINTS = 2
TEXT_GEOMETRY_MIDDLE_LIMIT_MILLIPOINTS = 1_000
TEXT_GEOMETRY_MIDDLE_BUCKET_MILLIPOINTS = 500
TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS = 10_000
TEXT_GEOMETRY_OUTER_BUCKET_MILLIPOINTS = 2_000
TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS = 12_000
TEXT_GEOMETRY_ALLOWED_BUCKETS = frozenset(
    range(-2, 3)
) | frozenset(
    value
    for magnitude in (500, 1_000)
    for value in (-magnitude, magnitude)
) | frozenset(
    value
    for magnitude in range(2_000, 10_001, 2_000)
    for value in (-magnitude, magnitude)
) | {
    -TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS,
    TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS,
}

FIDELITY_RATIO_KEYS = {
    "f1_ppm",
    "libreoffice_items",
    "matched_items",
    "precision_ppm",
    "recall_ppm",
    "rxls_items",
}
FIDELITY_TEXT_KEYS = FIDELITY_RATIO_KEYS | {
    "ambiguous_items",
    "libreoffice_unmatched_items",
    "rxls_unmatched_items",
}
FIDELITY_MASK_KEYS = {
    "f1_ppm",
    "libreoffice_matched_pixels",
    "libreoffice_pixels",
    "precision_ppm",
    "recall_ppm",
    "rxls_matched_pixels",
    "rxls_pixels",
}
FIDELITY_RASTER_KEYS = {
    "absolute_error_sum",
    "blurred_luma_absolute_error_sum",
    "blurred_luma_similarity_ppm",
    "changed_pixels",
    "edge",
    "exact_pages",
    "foreground",
    "max_channel_delta",
    "mean_absolute_error_ppm",
    "mismatch_ppm",
    "pages",
    "pixels",
    "similarity_ppm",
    "text_ink",
}
FIDELITY_COHORT_KEYS = {
    "pages",
    "poppler_lines",
    "poppler_words",
    "raster",
    "semantic_visible_characters",
    "workbooks",
}
FIDELITY_OUTPUT_KEYS = {"all", "by_format"}
CASE_DIAGNOSTIC_KEYS = {
    "case_id",
    "format",
    "page_box",
    "poppler_lines",
    "poppler_words",
    "raster",
    "semantic_visible_characters",
}
CASE_DIAGNOSTICS_KEYS = {
    "available_cases",
    "available_cases_by_format",
    "cases",
    "retained_cases",
    "retained_cases_by_format",
    "truncated",
}
INGESTION_KEYS = {
    "expected_workbooks",
    "received_workbooks",
    "status",
}
INGESTION_STATUSES = frozenset(
    {"complete", "partial", "rejected", "unavailable"}
)


def _text_geometry_bucket(delta_millipoints: int) -> int:
    magnitude = abs(delta_millipoints)
    if magnitude <= TEXT_GEOMETRY_EXACT_LIMIT_MILLIPOINTS:
        return delta_millipoints
    if magnitude <= TEXT_GEOMETRY_MIDDLE_LIMIT_MILLIPOINTS:
        width = TEXT_GEOMETRY_MIDDLE_BUCKET_MILLIPOINTS
        bucket = max(width, (magnitude + width // 2) // width * width)
    elif magnitude <= TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS:
        width = TEXT_GEOMETRY_OUTER_BUCKET_MILLIPOINTS
        bucket = (magnitude + width // 2) // width * width
    else:
        bucket = TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS
    return -bucket if delta_millipoints < 0 else bucket


def _text_geometry_bucket_interval(
    bucket_millipoints: int,
) -> tuple[int, int]:
    magnitude = abs(bucket_millipoints)
    if magnitude <= TEXT_GEOMETRY_EXACT_LIMIT_MILLIPOINTS:
        lower = magnitude
        upper = magnitude
    elif magnitude <= TEXT_GEOMETRY_MIDDLE_LIMIT_MILLIPOINTS:
        width = TEXT_GEOMETRY_MIDDLE_BUCKET_MILLIPOINTS
        lower = (
            TEXT_GEOMETRY_EXACT_LIMIT_MILLIPOINTS + 1
            if magnitude == width
            else magnitude - width // 2
        )
        upper = min(
            TEXT_GEOMETRY_MIDDLE_LIMIT_MILLIPOINTS,
            magnitude + width // 2 - 1,
        )
    elif magnitude <= TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS:
        width = TEXT_GEOMETRY_OUTER_BUCKET_MILLIPOINTS
        lower = max(
            TEXT_GEOMETRY_MIDDLE_LIMIT_MILLIPOINTS + 1,
            magnitude - width // 2,
        )
        upper = min(
            TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS,
            magnitude + width // 2 - 1,
        )
    elif magnitude == TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS:
        lower = TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS + 1
        upper = MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS
    else:
        raise SummaryError("text_geometry_bucket")
    return (-upper, -lower) if bucket_millipoints < 0 else (lower, upper)


def _text_geometry_exact_sum_bounds(
    histogram: Counter[int],
    minimum: int,
    maximum: int,
    code: str,
) -> tuple[int, int]:
    minimum_bucket = _text_geometry_bucket(minimum)
    maximum_bucket = _text_geometry_bucket(maximum)
    if (
        minimum < maximum
        and minimum_bucket == maximum_bucket
        and histogram[minimum_bucket] < 2
    ):
        raise SummaryError(code)
    lower_total = 0
    upper_total = 0
    effective_intervals: dict[int, tuple[int, int]] = {}
    for bucket, count in histogram.items():
        bucket_lower, bucket_upper = _text_geometry_bucket_interval(bucket)
        lower = max(bucket_lower, minimum)
        upper = min(bucket_upper, maximum)
        if lower > upper:
            raise SummaryError(code)
        effective_intervals[bucket] = (lower, upper)
        lower_total += lower * count
        upper_total += upper * count
    maximum_lower = effective_intervals[maximum_bucket][0]
    minimum_upper = effective_intervals[minimum_bucket][1]
    lower_total += maximum - maximum_lower
    upper_total -= minimum_upper - minimum
    if lower_total > upper_total:
        raise SummaryError(code)
    return lower_total, upper_total


class SummaryError(RuntimeError):
    """A detailed report cannot be safely summarized."""


class _StrictJSONError(ValueError):
    """An input is not in the bounded JSON subset used by oracle evidence."""


def _json(value: object) -> bytes:
    return (
        json.dumps(value, indent=2, sort_keys=True, ensure_ascii=True) + "\n"
    ).encode()


def _strict_json_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise _StrictJSONError("duplicate_key")
        result[key] = value
    return result


def _reject_json_constant(_: str) -> None:
    raise _StrictJSONError("non_finite_number")


def _reject_json_number(_: str) -> None:
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


def _strict_json_loads(payload: bytes) -> object:
    try:
        text = payload.decode("utf-8")
        _preflight_json_text(text)
        return json.loads(
            text,
            object_pairs_hook=_strict_json_object,
            parse_constant=_reject_json_constant,
            parse_float=_reject_json_number,
            parse_int=_parse_json_integer,
        )
    except (UnicodeDecodeError, RecursionError, ValueError) as error:
        raise SummaryError("report_unreadable") from error


def _integer(value: object, code: str, maximum: int) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not 0 <= value <= maximum
    ):
        raise SummaryError(code)
    return value


def _signed_integer(value: object, code: str, maximum: int) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not -maximum <= value <= maximum
    ):
        raise SummaryError(code)
    return value


def _ratio_ppm(numerator: int, denominator: int, *, empty: int = 0) -> int:
    if denominator == 0:
        return empty
    return (numerator * 1_000_000 + denominator // 2) // denominator


def _ratio_evidence(
    rxls_items: int,
    libreoffice_items: int,
    matched_items: int,
) -> dict[str, int]:
    if matched_items > min(rxls_items, libreoffice_items):
        raise SummaryError("fidelity_ratio")
    both_empty = rxls_items == 0 and libreoffice_items == 0
    return {
        "f1_ppm": _ratio_ppm(
            2 * matched_items,
            rxls_items + libreoffice_items,
            empty=1_000_000,
        ),
        "libreoffice_items": libreoffice_items,
        "matched_items": matched_items,
        "precision_ppm": _ratio_ppm(
            matched_items,
            rxls_items,
            empty=1_000_000 if both_empty else 0,
        ),
        "recall_ppm": _ratio_ppm(
            matched_items,
            libreoffice_items,
            empty=1_000_000 if both_empty else 0,
        ),
        "rxls_items": rxls_items,
    }


def _text_evidence(
    rxls_items: int,
    libreoffice_items: int,
    matched_items: int,
    ambiguous_items: int,
    rxls_unmatched_items: int,
    libreoffice_unmatched_items: int,
) -> dict[str, int]:
    if (
        rxls_items
        != matched_items + ambiguous_items + rxls_unmatched_items
        or libreoffice_items
        != matched_items + libreoffice_unmatched_items
    ):
        raise SummaryError("fidelity_text")
    return {
        **_ratio_evidence(
            rxls_items,
            libreoffice_items,
            matched_items,
        ),
        "ambiguous_items": ambiguous_items,
        "libreoffice_unmatched_items": libreoffice_unmatched_items,
        "rxls_unmatched_items": rxls_unmatched_items,
    }


def _mask_evidence(
    rxls_pixels: int,
    libreoffice_pixels: int,
    rxls_matched_pixels: int,
    libreoffice_matched_pixels: int,
) -> dict[str, int]:
    if (
        rxls_matched_pixels > rxls_pixels
        or libreoffice_matched_pixels > libreoffice_pixels
    ):
        raise SummaryError("fidelity_raster")
    both_empty = rxls_pixels == 0 and libreoffice_pixels == 0
    denominator = (
        rxls_matched_pixels * libreoffice_pixels
        + libreoffice_matched_pixels * rxls_pixels
    )
    if both_empty:
        f1 = 1_000_000
    elif denominator == 0:
        f1 = 0
    else:
        f1 = _ratio_ppm(
            2 * rxls_matched_pixels * libreoffice_matched_pixels,
            denominator,
        )
    return {
        "f1_ppm": f1,
        "libreoffice_matched_pixels": libreoffice_matched_pixels,
        "libreoffice_pixels": libreoffice_pixels,
        "precision_ppm": _ratio_ppm(
            rxls_matched_pixels,
            rxls_pixels,
            empty=1_000_000 if both_empty else 0,
        ),
        "recall_ppm": _ratio_ppm(
            libreoffice_matched_pixels,
            libreoffice_pixels,
            empty=1_000_000 if both_empty else 0,
        ),
        "rxls_matched_pixels": rxls_matched_pixels,
        "rxls_pixels": rxls_pixels,
    }


def _metric_integer(
    metrics: dict[str, Any],
    key: str,
    maximum: int,
    code: str,
) -> int:
    return _integer(metrics.get(key), code, maximum)


def _require_metric_ppm(
    metrics: dict[str, Any],
    key: str,
    expected: int,
    code: str,
) -> None:
    if _metric_integer(metrics, key, 1_000_000, code) != expected:
        raise SummaryError(code)


def _metric_ratio(
    metrics: dict[str, Any],
    *,
    prefix: str,
    maximum: int,
    code: str,
) -> dict[str, int]:
    evidence = _ratio_evidence(
        _metric_integer(
            metrics, f"{prefix}_rxls_items", maximum, code
        ),
        _metric_integer(
            metrics, f"{prefix}_libreoffice_items", maximum, code
        ),
        _metric_integer(
            metrics, f"{prefix}_matched_items", maximum, code
        ),
    )
    for name in ("precision_ppm", "recall_ppm", "f1_ppm"):
        _require_metric_ppm(
            metrics,
            f"{prefix}_{name}",
            evidence[name],
            code,
        )
    return evidence


def _metric_text(
    metrics: dict[str, Any],
    *,
    prefix: str,
    maximum: int,
    code: str,
) -> dict[str, int]:
    rxls_items = _metric_integer(
        metrics, f"{prefix}_rxls_items", maximum, code
    )
    libreoffice_items = _metric_integer(
        metrics, f"{prefix}_libreoffice_items", maximum, code
    )
    evidence = _text_evidence(
        rxls_items,
        libreoffice_items,
        _metric_integer(
            metrics, f"{prefix}_matched_items", maximum, code
        ),
        _metric_integer(
            metrics, f"{prefix}_ambiguous_items", maximum, code
        ),
        _metric_integer(
            metrics, f"{prefix}_rxls_unmatched_items", maximum, code
        ),
        _metric_integer(
            metrics,
            f"{prefix}_libreoffice_unmatched_items",
            maximum,
            code,
        ),
    )
    if (
        _metric_integer(
            metrics, f"{prefix}_candidate_items", maximum, code
        )
        != rxls_items
        or _metric_integer(
            metrics, f"{prefix}_unmatched_items", maximum, code
        )
        != evidence["rxls_unmatched_items"]
    ):
        raise SummaryError(code)
    for name in ("precision_ppm", "recall_ppm", "f1_ppm"):
        _require_metric_ppm(
            metrics,
            f"{prefix}_{name}",
            evidence[name],
            code,
        )
    _require_metric_ppm(
        metrics,
        f"{prefix}_match_coverage_ppm",
        evidence["precision_ppm"],
        code,
    )
    return evidence


def _metric_mask(
    metrics: dict[str, Any],
    *,
    prefix: str,
    pixels: int,
    code: str,
) -> dict[str, int]:
    evidence = _mask_evidence(
        _metric_integer(
            metrics, f"{prefix}_rxls_pixels", pixels, code
        ),
        _metric_integer(
            metrics, f"{prefix}_libreoffice_pixels", pixels, code
        ),
        _metric_integer(
            metrics, f"{prefix}_rxls_matched_1px", pixels, code
        ),
        _metric_integer(
            metrics,
            f"{prefix}_libreoffice_matched_1px",
            pixels,
            code,
        ),
    )
    for name in ("precision_ppm", "recall_ppm", "f1_ppm"):
        _require_metric_ppm(
            metrics,
            f"{prefix}_{name}",
            evidence[name],
            code,
        )
    return evidence


def _row_fidelity(
    row: dict[str, Any],
) -> dict[str, object] | None:
    metrics = row.get("metrics")
    pages = row.get("pages")
    if metrics is None and pages is None:
        return None
    code = "fidelity_metrics"
    if not isinstance(metrics, dict) or not isinstance(pages, list):
        raise SummaryError(code)
    page_count = _metric_integer(
        metrics, "pages", MAX_PAGE_COUNT, code
    )
    if page_count == 0 or page_count != len(pages):
        raise SummaryError(code)
    semantic = _metric_ratio(
        metrics,
        prefix="semantic_codepoint",
        maximum=MAX_SEMANTIC_CODEPOINTS_PER_WORKBOOK,
        code=code,
    )
    item_limit = page_count * MAX_POPPLER_ITEMS_PER_PAGE
    words = _metric_text(
        metrics,
        prefix="text_box",
        maximum=item_limit,
        code=code,
    )
    lines = _metric_text(
        metrics,
        prefix="text_line_box",
        maximum=item_limit,
        code=code,
    )

    pixel_limit = page_count * MAX_RASTER_PIXELS_PER_PAGE
    pixels = _metric_integer(metrics, "pixels", pixel_limit, code)
    if pixels == 0:
        raise SummaryError(code)
    changed_pixels = _metric_integer(
        metrics, "changed_pixels", pixels, code
    )
    absolute_error_sum = _metric_integer(
        metrics, "absolute_error_sum", pixels * 3 * 255, code
    )
    blurred_error_sum = _metric_integer(
        metrics,
        "blurred_luma_absolute_error_sum",
        pixels * 255,
        code,
    )
    mean_absolute_error_ppm = _ratio_ppm(
        absolute_error_sum, pixels * 3 * 255
    )
    similarity_ppm = max(
        0, 1_000_000 - mean_absolute_error_ppm
    )
    mismatch_ppm = _ratio_ppm(changed_pixels, pixels)
    blurred_similarity_ppm = max(
        0,
        1_000_000
        - _ratio_ppm(blurred_error_sum, pixels * 255),
    )
    for key, expected in (
        ("mean_absolute_error_ppm", mean_absolute_error_ppm),
        ("similarity_ppm", similarity_ppm),
        ("mismatch_ppm", mismatch_ppm),
        ("blurred_luma_similarity_ppm", blurred_similarity_ppm),
    ):
        _require_metric_ppm(metrics, key, expected, code)
    raster = {
        "absolute_error_sum": absolute_error_sum,
        "blurred_luma_absolute_error_sum": blurred_error_sum,
        "blurred_luma_similarity_ppm": blurred_similarity_ppm,
        "changed_pixels": changed_pixels,
        "edge": _metric_mask(
            metrics, prefix="edge", pixels=pixels, code=code
        ),
        "exact_pages": _metric_integer(
            metrics, "exact_pages", page_count, code
        ),
        "foreground": _metric_mask(
            metrics, prefix="foreground", pixels=pixels, code=code
        ),
        "max_channel_delta": _metric_integer(
            metrics, "max_channel_delta", 255, code
        ),
        "mean_absolute_error_ppm": mean_absolute_error_ppm,
        "mismatch_ppm": mismatch_ppm,
        "pages": page_count,
        "pixels": pixels,
        "similarity_ppm": similarity_ppm,
        "text_ink": _metric_mask(
            metrics, prefix="text_ink", pixels=pixels, code=code
        ),
    }
    return {
        "pages": page_count,
        "poppler_lines": lines,
        "poppler_words": words,
        "raster": raster,
        "semantic_visible_characters": semantic,
        "workbooks": 1,
    }


def _new_fidelity_accumulator() -> dict[str, object]:
    return {
        "pages": 0,
        "poppler_lines": {
            key: 0
            for key in (
                "ambiguous_items",
                "libreoffice_items",
                "libreoffice_unmatched_items",
                "matched_items",
                "rxls_items",
                "rxls_unmatched_items",
            )
        },
        "poppler_words": {
            key: 0
            for key in (
                "ambiguous_items",
                "libreoffice_items",
                "libreoffice_unmatched_items",
                "matched_items",
                "rxls_items",
                "rxls_unmatched_items",
            )
        },
        "raster": {
            "absolute_error_sum": 0,
            "blurred_luma_absolute_error_sum": 0,
            "changed_pixels": 0,
            "exact_pages": 0,
            "masks": {
                prefix: {
                    key: 0
                    for key in (
                        "libreoffice_matched_pixels",
                        "libreoffice_pixels",
                        "rxls_matched_pixels",
                        "rxls_pixels",
                    )
                }
                for prefix in ("edge", "foreground", "text_ink")
            },
            "max_channel_delta": 0,
            "pixels": 0,
        },
        "semantic_visible_characters": {
            key: 0
            for key in (
                "libreoffice_items",
                "matched_items",
                "rxls_items",
            )
        },
        "workbooks": 0,
    }


def _merge_fidelity(
    accumulator: dict[str, object],
    evidence: dict[str, object],
) -> None:
    accumulator["workbooks"] += int(evidence["workbooks"])
    accumulator["pages"] += int(evidence["pages"])
    for name in (
        "semantic_visible_characters",
        "poppler_words",
        "poppler_lines",
    ):
        target = accumulator[name]
        source = evidence[name]
        for key in target:
            target[key] += int(source[key])
    target_raster = accumulator["raster"]
    source_raster = evidence["raster"]
    for key in (
        "absolute_error_sum",
        "blurred_luma_absolute_error_sum",
        "changed_pixels",
        "exact_pages",
        "pixels",
    ):
        target_raster[key] += int(source_raster[key])
    target_raster["max_channel_delta"] = max(
        int(target_raster["max_channel_delta"]),
        int(source_raster["max_channel_delta"]),
    )
    for prefix in ("edge", "foreground", "text_ink"):
        for key in target_raster["masks"][prefix]:
            target_raster["masks"][prefix][key] += int(
                source_raster[prefix][key]
            )


def _finish_fidelity(
    accumulator: dict[str, object],
) -> dict[str, object]:
    words = accumulator["poppler_words"]
    lines = accumulator["poppler_lines"]
    semantic = accumulator["semantic_visible_characters"]
    raster = accumulator["raster"]
    pixels = int(raster["pixels"])
    absolute_error_sum = int(raster["absolute_error_sum"])
    blurred_error_sum = int(
        raster["blurred_luma_absolute_error_sum"]
    )
    mean_absolute_error_ppm = _ratio_ppm(
        absolute_error_sum,
        pixels * 3 * 255,
        empty=0,
    )
    return {
        "pages": int(accumulator["pages"]),
        "poppler_lines": _text_evidence(
            int(lines["rxls_items"]),
            int(lines["libreoffice_items"]),
            int(lines["matched_items"]),
            int(lines["ambiguous_items"]),
            int(lines["rxls_unmatched_items"]),
            int(lines["libreoffice_unmatched_items"]),
        ),
        "poppler_words": _text_evidence(
            int(words["rxls_items"]),
            int(words["libreoffice_items"]),
            int(words["matched_items"]),
            int(words["ambiguous_items"]),
            int(words["rxls_unmatched_items"]),
            int(words["libreoffice_unmatched_items"]),
        ),
        "raster": {
            "absolute_error_sum": absolute_error_sum,
            "blurred_luma_absolute_error_sum": blurred_error_sum,
            "blurred_luma_similarity_ppm": (
                max(
                    0,
                    1_000_000
                    - _ratio_ppm(
                        blurred_error_sum,
                        pixels * 255,
                        empty=0,
                    ),
                )
                if pixels
                else 1_000_000
            ),
            "changed_pixels": int(raster["changed_pixels"]),
            "edge": _mask_evidence(
                **raster["masks"]["edge"]
            ),
            "exact_pages": int(raster["exact_pages"]),
            "foreground": _mask_evidence(
                **raster["masks"]["foreground"]
            ),
            "max_channel_delta": int(raster["max_channel_delta"]),
            "mean_absolute_error_ppm": mean_absolute_error_ppm,
            "mismatch_ppm": _ratio_ppm(
                int(raster["changed_pixels"]),
                pixels,
                empty=0,
            ),
            "pages": int(accumulator["pages"]),
            "pixels": pixels,
            "similarity_ppm": (
                max(0, 1_000_000 - mean_absolute_error_ppm)
                if pixels
                else 1_000_000
            ),
            "text_ink": _mask_evidence(
                **raster["masks"]["text_ink"]
            ),
        },
        "semantic_visible_characters": _ratio_evidence(
            int(semantic["rxls_items"]),
            int(semantic["libreoffice_items"]),
            int(semantic["matched_items"]),
        ),
        "workbooks": int(accumulator["workbooks"]),
    }


def _point_fraction(
    value: object, code: str, *, positive: bool
) -> Fraction:
    if not isinstance(value, str):
        raise SummaryError(code)
    match = re.fullmatch(r"(-?)(0|[1-9][0-9]*)/([1-9][0-9]*)", value)
    if match is None:
        raise SummaryError(code)
    numerator_digits = match.group(2)
    denominator_digits = match.group(3)
    if (
        len(numerator_digits) > MAX_POINT_RATIONAL_DIGITS
        or len(denominator_digits) > MAX_POINT_RATIONAL_DIGITS
    ):
        raise SummaryError(code)
    numerator = int(numerator_digits)
    if match.group(1):
        numerator = -numerator
    result = Fraction(numerator, int(denominator_digits))
    if value != f"{result.numerator}/{result.denominator}":
        raise SummaryError(code)
    if positive:
        if not 0 < result <= MAX_POINT_ABSOLUTE_VALUE:
            raise SummaryError(code)
    elif abs(result) > MAX_POINT_ABSOLUTE_VALUE:
        raise SummaryError(code)
    return result


def _point_side(
    value: object, code: str
) -> dict[str, tuple[Fraction, Fraction]]:
    if not isinstance(value, dict) or set(value) != {
        "crop_box",
        "media_box",
        "page_size",
    }:
        raise SummaryError(code)
    result: dict[str, tuple[Fraction, Fraction]] = {}
    for name in ("page_size", "media_box", "crop_box"):
        dimensions = value.get(name)
        if not isinstance(dimensions, dict) or set(dimensions) != {
            "height_points",
            "width_points",
        }:
            raise SummaryError(code)
        result[name] = (
            _point_fraction(
                dimensions["width_points"], code, positive=True
            ),
            _point_fraction(
                dimensions["height_points"], code, positive=True
            ),
        )
    return result


def _ceil_micropoints(value: Fraction) -> int:
    absolute = abs(value)
    return (
        absolute.numerator * 1_000_000
        + absolute.denominator
        - 1
    ) // absolute.denominator


def _signed_ceil_micropoints(value: Fraction) -> int:
    rounded = _ceil_micropoints(value)
    return -rounded if value < 0 else rounded


def _page_box_geometry_bucket(delta_micropoints: int) -> str:
    for bucket in PAGE_BOX_GEOMETRY_BUCKET_ORDER:
        lower, upper = PAGE_BOX_GEOMETRY_BUCKET_INTERVALS[bucket]
        if lower <= delta_micropoints <= upper:
            return bucket
    raise SummaryError("page_box_geometry_delta_limit")


def _ceil_millipoints(value: Fraction) -> int:
    absolute = abs(value)
    return (
        absolute.numerator * 1000 + absolute.denominator - 1
    ) // absolute.denominator


def _page_point_geometry(
    page: object,
) -> tuple[dict[str, Fraction], bool]:
    code = "geometry_page"
    if not isinstance(page, dict):
        raise SummaryError(code)
    evidence = page.get("pdf_point_geometry")
    if not isinstance(evidence, dict) or set(evidence) != {
        "deltas_points",
        "libreoffice",
        "rxls",
        "xhtml",
    }:
        raise SummaryError(code)
    rxls = _point_side(evidence["rxls"], code)
    libreoffice = _point_side(evidence["libreoffice"], code)
    xhtml = evidence["xhtml"]
    if not isinstance(xhtml, dict) or set(xhtml) != {
        "libreoffice",
        "rxls",
    }:
        raise SummaryError(code)
    xhtml_values: dict[str, tuple[Fraction, Fraction]] = {}
    for side in ("rxls", "libreoffice"):
        dimensions = xhtml[side]
        if not isinstance(dimensions, dict) or set(dimensions) != {
            "height_points",
            "width_points",
        }:
            raise SummaryError(code)
        xhtml_values[side] = (
            _point_fraction(
                dimensions["width_points"], code, positive=True
            ),
            _point_fraction(
                dimensions["height_points"], code, positive=True
            ),
        )

    expected: dict[str, Fraction] = {}
    for box in ("media_box", "crop_box"):
        for offset, axis in enumerate(("width", "height")):
            expected[f"{box}_{axis}"] = (
                rxls[box][offset] - libreoffice[box][offset]
            )
    for side in ("rxls", "libreoffice"):
        geometry = rxls if side == "rxls" else libreoffice
        for offset, axis in enumerate(("width", "height")):
            expected[f"{side}_xhtml_page_size_{axis}"] = (
                xhtml_values[side][offset]
                - geometry["page_size"][offset]
            )
    for offset, axis in enumerate(("width", "height")):
        expected[f"xhtml_{axis}"] = (
            xhtml_values["rxls"][offset]
            - xhtml_values["libreoffice"][offset]
        )

    deltas = evidence["deltas_points"]
    if not isinstance(deltas, dict) or set(deltas) != set(
        PDF_POINT_DELTA_KEYS
    ):
        raise SummaryError(code)
    parsed = {
        key: _point_fraction(deltas[key], code, positive=False)
        for key in PDF_POINT_DELTA_KEYS
    }
    if parsed != expected:
        raise SummaryError("geometry_delta")
    mismatch = any(
        parsed[key] != 0 for key in PDF_DIRECT_POINT_DELTA_KEYS
    )
    return parsed, mismatch


def _row_point_geometry(
    row: dict[str, Any],
) -> tuple[list[dict[str, Fraction]], int] | None:
    has_pages = "pages" in row
    has_metrics = "metrics" in row
    if not has_pages and not has_metrics:
        return None
    if not has_pages or not has_metrics:
        raise SummaryError("geometry_row")
    pages = row["pages"]
    metrics = row["metrics"]
    if (
        not isinstance(pages, list)
        or not 0 < len(pages) <= MAX_PAGE_COUNT
        or not isinstance(metrics, dict)
    ):
        raise SummaryError("geometry_row")
    parsed_pages: list[dict[str, Fraction]] = []
    mismatch_pages = 0
    for page_offset, page in enumerate(pages):
        if (
            not isinstance(page, dict)
            or _integer(
                page.get("oracle_output_page_index"),
                "geometry_page_index",
                len(pages) - 1,
            )
            != page_offset
        ):
            raise SummaryError("geometry_page_index")
        parsed, mismatch = _page_point_geometry(page)
        parsed_pages.append(parsed)
        mismatch_pages += int(mismatch)
    direct_max = max(
        (
            abs(page[key])
            for page in parsed_pages
            for key in PDF_DIRECT_POINT_DELTA_KEYS
        ),
        default=Fraction(),
    )
    crosscheck_max = max(
        (
            abs(page[key])
            for page in parsed_pages
            for key in PDF_XHTML_CROSSCHECK_DELTA_KEYS
        ),
        default=Fraction(),
    )
    expected_metrics = {
        "pages": len(pages),
        "pdf_point_geometry_mismatches": mismatch_pages,
        "max_pdf_point_geometry_delta_millipoints": (
            _ceil_millipoints(direct_max)
        ),
        "max_pdf_xhtml_crosscheck_delta_micropoints": (
            _ceil_micropoints(crosscheck_max)
        ),
    }
    maxima = {
        "pages": len(pages),
        "pdf_point_geometry_mismatches": len(pages),
        "max_pdf_point_geometry_delta_millipoints": (
            MAX_POINT_ABSOLUTE_VALUE * 1000
        ),
        "max_pdf_xhtml_crosscheck_delta_micropoints": (
            MAX_POINT_DELTA_MICROPOINTS
        ),
    }
    for key, expected in expected_metrics.items():
        if _integer(metrics.get(key), "geometry_aggregate", maxima[key]) != expected:
            raise SummaryError("geometry_aggregate")
    return parsed_pages, mismatch_pages


def _text_geometry_exact_summary(
    value: object,
    *,
    matched: int,
    histogram: Counter[int],
    code: str,
) -> dict[str, int | None]:
    if (
        not isinstance(value, dict)
        or set(value) != TEXT_GEOMETRY_EXACT_SUMMARY_KEYS
    ):
        raise SummaryError(code)
    count = _integer(value["count"], code, matched)
    if count != matched:
        raise SummaryError(code)
    total = _signed_integer(
        value["sum_delta_millipoints"],
        code,
        matched * MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS,
    )
    negative_overflow = _integer(
        value["negative_overflow_items"], code, matched
    )
    positive_overflow = _integer(
        value["positive_overflow_items"], code, matched
    )
    if (
        histogram.get(-TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS, 0)
        != negative_overflow
        or histogram.get(TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS, 0)
        != positive_overflow
    ):
        raise SummaryError(code)

    raw_minimum = value["min_delta_millipoints"]
    raw_maximum = value["max_delta_millipoints"]
    if matched == 0:
        if (
            raw_minimum is not None
            or raw_maximum is not None
            or total != 0
            or negative_overflow != 0
            or positive_overflow != 0
        ):
            raise SummaryError(code)
        minimum = None
        maximum = None
    else:
        minimum = _signed_integer(
            raw_minimum, code, MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS
        )
        maximum = _signed_integer(
            raw_maximum, code, MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS
        )
        if (
            minimum > maximum
            or _text_geometry_bucket(minimum) != min(histogram)
            or _text_geometry_bucket(maximum) != max(histogram)
            or (
                matched == 1
                and not minimum == maximum == total
            )
            or (negative_overflow > 0)
            != (minimum < -TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS)
            or (positive_overflow > 0)
            != (maximum > TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS)
        ):
            raise SummaryError(code)
        sum_lower, sum_upper = _text_geometry_exact_sum_bounds(
            histogram, minimum, maximum, code
        )
        if not sum_lower <= total <= sum_upper:
            raise SummaryError(code)
    return {
        "count": count,
        "max_delta_millipoints": maximum,
        "min_delta_millipoints": minimum,
        "negative_overflow_items": negative_overflow,
        "positive_overflow_items": positive_overflow,
        "sum_delta_millipoints": total,
    }


def _validate_text_geometry_axis_identities(
    summaries: dict[str, dict[str, int | None]],
    matched: int,
    code: str,
) -> None:
    sums = {
        axis: int(summary["sum_delta_millipoints"])
        for axis, summary in summaries.items()
    }
    if (
        abs(sums["width"] - (sums["x_max"] - sums["x_min"]))
        > matched
        or abs(
            2 * sums["center_x"] - sums["x_min"] - sums["x_max"]
        )
        > matched
        or abs(sums["height"] - (sums["y_max"] - sums["y_min"]))
        > matched
        or abs(
            2 * sums["center_y"] - sums["y_min"] - sums["y_max"]
        )
        > matched
    ):
        raise SummaryError(code)


def _page_unique_text_geometry(
    value: object,
) -> dict[str, object]:
    code = "text_geometry_page"
    if not isinstance(value, dict) or set(value) != TEXT_GEOMETRY_PAGE_KEYS:
        raise SummaryError(code)
    rxls_unique = _integer(
        value["rxls_unique_items"],
        code,
        MAX_TEXT_GEOMETRY_UNIQUE_ITEMS,
    )
    libreoffice_unique = _integer(
        value["libreoffice_unique_items"],
        code,
        MAX_TEXT_GEOMETRY_UNIQUE_ITEMS,
    )
    matched = _integer(
        value["matched_items"],
        code,
        MAX_TEXT_GEOMETRY_UNIQUE_ITEMS,
    )
    if matched > min(rxls_unique, libreoffice_unique):
        raise SummaryError(code)

    raw_histograms = value["delta_histograms_millipoints"]
    raw_exact_summaries = value["exact_delta_summaries_millipoints"]
    if (
        not isinstance(raw_histograms, dict)
        or set(raw_histograms) != set(TEXT_GEOMETRY_AXES)
        or not isinstance(raw_exact_summaries, dict)
        or set(raw_exact_summaries) != set(TEXT_GEOMETRY_AXES)
    ):
        raise SummaryError(code)
    histograms: dict[str, Counter[int]] = {}
    exact_summaries: dict[str, dict[str, int | None]] = {}
    for axis in TEXT_GEOMETRY_AXES:
        raw_histogram = raw_histograms[axis]
        if (
            not isinstance(raw_histogram, list)
            or len(raw_histogram) > MAX_TEXT_GEOMETRY_HISTOGRAM_BUCKETS
        ):
            raise SummaryError(code)
        histogram: Counter[int] = Counter()
        previous_delta: int | None = None
        count = 0
        for bucket in raw_histogram:
            if (
                not isinstance(bucket, dict)
                or set(bucket) != TEXT_GEOMETRY_BUCKET_KEYS
            ):
                raise SummaryError(code)
            delta = _signed_integer(
                bucket["delta_millipoints"],
                code,
                TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS,
            )
            if (
                delta not in TEXT_GEOMETRY_ALLOWED_BUCKETS
                or (previous_delta is not None and delta <= previous_delta)
            ):
                raise SummaryError(code)
            previous_delta = delta
            bucket_count = _integer(bucket["count"], code, matched)
            if bucket_count == 0:
                raise SummaryError(code)
            histogram[delta] = bucket_count
            count += bucket_count
        if count != matched:
            raise SummaryError(code)
        histograms[axis] = histogram
        exact_summaries[axis] = _text_geometry_exact_summary(
            raw_exact_summaries[axis],
            matched=matched,
            histogram=histogram,
            code=code,
        )
    _validate_text_geometry_axis_identities(
        exact_summaries, matched, code
    )
    return {
        "exact_summaries": exact_summaries,
        "histograms": histograms,
        "libreoffice_unique_items": libreoffice_unique,
        "matched_items": matched,
        "rxls_unique_items": rxls_unique,
    }


def _row_unique_text_geometry(
    row: dict[str, Any],
) -> list[tuple[dict[str, object], dict[str, object]]] | None:
    pages = row.get("pages")
    if not isinstance(pages, list):
        return None
    keys = (
        "text_box_unique_geometry",
        "text_line_box_unique_geometry",
    )
    presence = [
        tuple(key in page for key in keys)
        if isinstance(page, dict)
        else (False, False)
        for page in pages
    ]
    if any(pair != (True, True) for pair in presence):
        raise SummaryError("text_geometry_page")
    result = []
    for page in pages:
        word = _page_unique_text_geometry(page[keys[0]])
        line = _page_unique_text_geometry(page[keys[1]])
        for geometry, prefix in (
            (word, "text_box"),
            (line, "text_line_box"),
        ):
            rxls_items = _integer(
                page.get(f"{prefix}_rxls_items"),
                "text_geometry_page",
                MAX_TEXT_GEOMETRY_UNIQUE_ITEMS,
            )
            libreoffice_items = _integer(
                page.get(f"{prefix}_libreoffice_items"),
                "text_geometry_page",
                MAX_TEXT_GEOMETRY_UNIQUE_ITEMS,
            )
            paired_items = _integer(
                page.get(f"{prefix}_matched_items"),
                "text_geometry_page",
                MAX_TEXT_GEOMETRY_UNIQUE_ITEMS,
            )
            if (
                geometry["rxls_unique_items"] > rxls_items
                or geometry["libreoffice_unique_items"]
                > libreoffice_items
                or geometry["matched_items"] > paired_items
            ):
                raise SummaryError("text_geometry_page")
        result.append((word, line))
    return result


def _text_geometry_complexity(
    rows: Sequence[dict[str, Any]],
) -> tuple[int, int]:
    """Count the strict diagnostic surface in one complete report fragment."""
    pages = 0
    histogram_buckets = 0
    for row in rows:
        geometry_pages = _row_unique_text_geometry(row)
        if geometry_pages is None:
            continue
        pages += len(geometry_pages)
        histogram_buckets += sum(
            len(geometry["histograms"][axis])
            for pair in geometry_pages
            for geometry in pair
            for axis in TEXT_GEOMETRY_AXES
        )
    return pages, histogram_buckets


def _new_text_geometry_accumulator() -> dict[str, object]:
    return {
        "exact_summaries": {
            axis: {
                "count": 0,
                "max_delta_millipoints": None,
                "min_delta_millipoints": None,
                "negative_overflow_items": 0,
                "positive_overflow_items": 0,
                "sum_delta_millipoints": 0,
            }
            for axis in TEXT_GEOMETRY_AXES
        },
        "histograms": {
            axis: Counter() for axis in TEXT_GEOMETRY_AXES
        },
        "libreoffice_unique_items": 0,
        "matched_items": 0,
        "pages": 0,
        "rxls_unique_items": 0,
        "workbooks": 0,
    }


def _merge_text_geometry_page(
    accumulator: dict[str, object], page: dict[str, object]
) -> None:
    accumulator["pages"] += 1
    for key in (
        "libreoffice_unique_items",
        "matched_items",
        "rxls_unique_items",
    ):
        accumulator[key] += page[key]
    histograms = accumulator["histograms"]
    page_histograms = page["histograms"]
    exact_summaries = accumulator["exact_summaries"]
    page_exact_summaries = page["exact_summaries"]
    for axis in TEXT_GEOMETRY_AXES:
        histograms[axis].update(page_histograms[axis])
        if (
            len(histograms[axis])
            > MAX_TEXT_GEOMETRY_HISTOGRAM_BUCKETS
        ):
            raise SummaryError("text_geometry_bucket_limit")
        exact = exact_summaries[axis]
        page_exact = page_exact_summaries[axis]
        exact["count"] += page_exact["count"]
        exact["sum_delta_millipoints"] += page_exact[
            "sum_delta_millipoints"
        ]
        exact["negative_overflow_items"] += page_exact[
            "negative_overflow_items"
        ]
        exact["positive_overflow_items"] += page_exact[
            "positive_overflow_items"
        ]
        page_minimum = page_exact["min_delta_millipoints"]
        page_maximum = page_exact["max_delta_millipoints"]
        if page_minimum is not None:
            exact["min_delta_millipoints"] = (
                page_minimum
                if exact["min_delta_millipoints"] is None
                else min(exact["min_delta_millipoints"], page_minimum)
            )
            exact["max_delta_millipoints"] = (
                page_maximum
                if exact["max_delta_millipoints"] is None
                else max(exact["max_delta_millipoints"], page_maximum)
            )


def _finish_text_geometry_cohort(
    accumulator: dict[str, object],
) -> dict[str, object]:
    matched = int(accumulator["matched_items"])
    by_axis: dict[str, object] = {}
    for axis in TEXT_GEOMETRY_AXES:
        histogram = accumulator["histograms"][axis]
        exact = accumulator["exact_summaries"][axis]
        if len(histogram) > MAX_TEXT_GEOMETRY_HISTOGRAM_BUCKETS:
            raise SummaryError("text_geometry_bucket_limit")
        ordered = sorted(histogram.items())
        count = sum(bucket_count for _, bucket_count in ordered)
        if (
            count != matched
            or exact["count"] != matched
            or histogram.get(
                -TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS, 0
            )
            != exact["negative_overflow_items"]
            or histogram.get(
                TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS, 0
            )
            != exact["positive_overflow_items"]
        ):
            raise SummaryError("text_geometry_aggregate")
        by_axis[axis] = {
            "exact": dict(exact),
            "histogram": [
                {
                    "count": bucket_count,
                    "delta_millipoints": delta,
                }
                for delta, bucket_count in ordered
            ],
        }
    return {
        "by_axis": by_axis,
        "libreoffice_unique_items": int(
            accumulator["libreoffice_unique_items"]
        ),
        "matched_items": matched,
        "pages": int(accumulator["pages"]),
        "rxls_unique_items": int(accumulator["rxls_unique_items"]),
        "workbooks": int(accumulator["workbooks"]),
    }


def _empty_text_geometry_cohort() -> dict[str, object]:
    return _finish_text_geometry_cohort(
        _new_text_geometry_accumulator()
    )


def _empty_text_geometry() -> dict[str, object]:
    return {
        "all": _empty_text_geometry_cohort(),
        "by_format": {},
    }


def _new_page_box_geometry_accumulator() -> dict[str, object]:
    return {
        "by_axis": {
            axis: {
                "histogram": Counter(
                    {
                        bucket: 0
                        for bucket in PAGE_BOX_GEOMETRY_BUCKET_ORDER
                    }
                ),
                "max_delta_micropoints": None,
                "min_delta_micropoints": None,
                "nonzero_pages": 0,
                "sum_delta_micropoints": 0,
            }
            for axis in PAGE_BOX_GEOMETRY_AXES
        },
        "pages": 0,
        "workbooks": 0,
    }


def _merge_page_box_geometry_workbook(
    accumulator: dict[str, object],
    pages: Sequence[dict[str, Fraction]],
) -> None:
    accumulator["workbooks"] += 1
    accumulator["pages"] += len(pages)
    axes = accumulator["by_axis"]
    for page in pages:
        for axis in PAGE_BOX_GEOMETRY_AXES:
            value = _signed_ceil_micropoints(
                page[f"crop_box_{axis}"]
            )
            aggregate = axes[axis]
            bucket = _page_box_geometry_bucket(value)
            aggregate["histogram"][bucket] += 1
            aggregate["sum_delta_micropoints"] += value
            aggregate["nonzero_pages"] += int(value != 0)
            aggregate["min_delta_micropoints"] = (
                value
                if aggregate["min_delta_micropoints"] is None
                else min(
                    aggregate["min_delta_micropoints"], value
                )
            )
            aggregate["max_delta_micropoints"] = (
                value
                if aggregate["max_delta_micropoints"] is None
                else max(
                    aggregate["max_delta_micropoints"], value
                )
            )


def _validate_page_box_histogram_aggregates(
    histogram: Counter[str],
    *,
    pages: int,
    minimum: int | None,
    maximum: int | None,
    nonzero_pages: int,
    total_delta: int,
    code: str,
) -> None:
    if (
        set(histogram) != set(PAGE_BOX_GEOMETRY_BUCKET_ORDER)
        or any(
            isinstance(count, bool)
            or not isinstance(count, int)
            or count < 0
            for count in histogram.values()
        )
        or sum(histogram.values()) != pages
        or pages - histogram["zero"] != nonzero_pages
    ):
        raise SummaryError(code)
    nonempty = [
        bucket
        for bucket in PAGE_BOX_GEOMETRY_BUCKET_ORDER
        if histogram[bucket] > 0
    ]
    if pages == 0:
        if (
            nonempty
            or minimum is not None
            or maximum is not None
            or nonzero_pages != 0
            or total_delta != 0
        ):
            raise SummaryError(code)
        return
    if (
        minimum is None
        or maximum is None
        or minimum > maximum
        or _page_box_geometry_bucket(minimum) != nonempty[0]
        or _page_box_geometry_bucket(maximum) != nonempty[-1]
    ):
        raise SummaryError(code)
    if minimum == maximum:
        if (
            len(nonempty) != 1
            or histogram[nonempty[0]] != pages
            or total_delta != minimum * pages
        ):
            raise SummaryError(code)
        return
    if pages < 2:
        raise SummaryError(code)
    remaining = histogram.copy()
    for value in (minimum, maximum):
        bucket = _page_box_geometry_bucket(value)
        remaining[bucket] -= 1
        if remaining[bucket] < 0:
            raise SummaryError(code)
    minimum_sum = minimum + maximum
    maximum_sum = minimum + maximum
    for bucket in PAGE_BOX_GEOMETRY_BUCKET_ORDER:
        count = remaining[bucket]
        if count == 0:
            continue
        lower, upper = PAGE_BOX_GEOMETRY_BUCKET_INTERVALS[bucket]
        lower = max(lower, minimum)
        upper = min(upper, maximum)
        if lower > upper:
            raise SummaryError(code)
        minimum_sum += lower * count
        maximum_sum += upper * count
    if not minimum_sum <= total_delta <= maximum_sum:
        raise SummaryError(code)


def _finish_page_box_geometry_cohort(
    accumulator: dict[str, object],
    *,
    include_histogram: bool,
) -> dict[str, object]:
    pages = int(accumulator["pages"])
    by_axis: dict[str, dict[str, object]] = {}
    for axis in PAGE_BOX_GEOMETRY_AXES:
        aggregate = accumulator["by_axis"][axis]
        histogram = aggregate["histogram"]
        _validate_page_box_histogram_aggregates(
            histogram,
            pages=pages,
            minimum=aggregate["min_delta_micropoints"],
            maximum=aggregate["max_delta_micropoints"],
            nonzero_pages=aggregate["nonzero_pages"],
            total_delta=aggregate["sum_delta_micropoints"],
            code="page_box_geometry_aggregate",
        )
        axis_value = {
            "max_delta_micropoints": aggregate[
                "max_delta_micropoints"
            ],
            "min_delta_micropoints": aggregate[
                "min_delta_micropoints"
            ],
            "nonzero_pages": aggregate["nonzero_pages"],
            "sum_delta_micropoints": aggregate[
                "sum_delta_micropoints"
            ],
        }
        if include_histogram:
            axis_value["histogram"] = [
                histogram[bucket]
                for bucket in PAGE_BOX_GEOMETRY_BUCKET_ORDER
            ]
        by_axis[axis] = axis_value
    return {
        "by_axis": by_axis,
        "pages": pages,
        "workbooks": int(accumulator["workbooks"]),
    }


def _empty_page_box_geometry() -> dict[str, object]:
    return {
        **copy.deepcopy(PAGE_BOX_GEOMETRY_POLICY),
        "all": _finish_page_box_geometry_cohort(
            _new_page_box_geometry_accumulator(),
            include_histogram=True,
        ),
        "by_feature": {},
        "by_format": {},
    }


def _case_id_key(value: bytes | None) -> bytes:
    key = secrets.token_bytes(CASE_ID_KEY_BYTES) if value is None else value
    if type(key) is not bytes or len(key) != CASE_ID_KEY_BYTES:
        raise SummaryError("case_id_key")
    return key


def _opaque_case_id(workbook_digest: str, case_id_key: bytes) -> str:
    if (
        HASH_RE.fullmatch(workbook_digest) is None
        or type(case_id_key) is not bytes
        or len(case_id_key) != CASE_ID_KEY_BYTES
    ):
        raise SummaryError("case_id")
    return hmac.new(
        case_id_key,
        CASE_ID_DOMAIN + bytes.fromhex(workbook_digest),
        hashlib.sha256,
    ).hexdigest()


def _case_page_box(
    pages: Sequence[dict[str, Fraction]],
) -> dict[str, object]:
    accumulator = _new_page_box_geometry_accumulator()
    _merge_page_box_geometry_workbook(accumulator, pages)
    return _finish_page_box_geometry_cohort(
        accumulator,
        include_histogram=True,
    )


def _case_diagnostic(
    row: dict[str, Any],
    fidelity: dict[str, object],
    point_pages: Sequence[dict[str, Fraction]],
    case_id_key: bytes,
) -> dict[str, object]:
    digest = row.get("sha256")
    format_name = row.get("format")
    if (
        not isinstance(digest, str)
        or not isinstance(format_name, str)
        or format_name not in FORMATS
    ):
        raise SummaryError("case_id")
    return {
        "case_id": _opaque_case_id(digest, case_id_key),
        "format": format_name,
        "page_box": _case_page_box(point_pages),
        "poppler_lines": copy.deepcopy(
            fidelity["poppler_lines"]
        ),
        "poppler_words": copy.deepcopy(
            fidelity["poppler_words"]
        ),
        "raster": copy.deepcopy(fidelity["raster"]),
        "semantic_visible_characters": copy.deepcopy(
            fidelity["semantic_visible_characters"]
        ),
    }


def _finish_case_diagnostics(
    cases: Sequence[dict[str, object]],
) -> dict[str, object]:
    ordered = sorted(cases, key=lambda case: str(case["case_id"]))
    retained = ordered[:MAX_CASE_DIAGNOSTICS_PER_REPORT]
    available_by_format = Counter(
        str(case["format"]) for case in ordered
    )
    retained_by_format = Counter(
        str(case["format"]) for case in retained
    )
    return {
        "available_cases": len(ordered),
        "available_cases_by_format": dict(
            sorted(available_by_format.items())
        ),
        "cases": retained,
        "retained_cases": len(retained),
        "retained_cases_by_format": dict(
            sorted(retained_by_format.items())
        ),
        "truncated": len(retained) != len(ordered),
    }


def _empty_case_diagnostics() -> dict[str, object]:
    return _finish_case_diagnostics(())


def _empty_geometry() -> dict[str, object]:
    return {
        "by_delta": {
            key: {
                "max_absolute_micropoints": 0,
                "nonzero_pages": 0,
            }
            for key in PDF_POINT_DELTA_KEYS
        },
        "max_direct_absolute_delta_micropoints": 0,
        "max_internal_xhtml_crosscheck_micropoints": 0,
        "mismatch_pages": 0,
        "pages": 0,
        "workbooks": 0,
    }


def _page_count_pair(
    value: dict[str, Any], code: str
) -> tuple[int, int]:
    rxls_pages = _integer(value.get("rxls_pages"), code, MAX_PAGE_COUNT)
    libreoffice_pages = _integer(
        value.get("libreoffice_pages"), code, MAX_PAGE_COUNT
    )
    if rxls_pages == 0 or libreoffice_pages == 0 or rxls_pages == libreoffice_pages:
        raise SummaryError(code)
    return rxls_pages, libreoffice_pages


def _public_classification(value: str) -> str:
    if value in REVIEWED_CLASSIFICATIONS:
        return value
    for bucket, prefixes in COARSE_CLASSIFICATION_PREFIXES:
        if value.startswith(prefixes):
            return bucket
    return UNREVIEWED_CLASSIFICATION


def _count_map(
    value: object,
    total: int,
    code: str,
    allowed: frozenset[str] | None = None,
) -> dict[str, int]:
    if not isinstance(value, dict) or len(value) > 256:
        raise SummaryError(code)
    result: dict[str, int] = {}
    for key, raw_count in value.items():
        if (
            not isinstance(key, str)
            or CODE_RE.fullmatch(key) is None
            or (allowed is not None and key not in allowed)
        ):
            raise SummaryError(code)
        count = _integer(raw_count, code, total)
        if count == 0:
            raise SummaryError(code)
        result[key] = count
    if sum(result.values()) != total:
        raise SummaryError(code)
    return dict(sorted(result.items()))


def _read(path: Path, remaining: int) -> tuple[dict[str, Any], int]:
    byte_limit = min(MAX_REPORT_BYTES, remaining)
    if byte_limit <= 0:
        raise SummaryError("report_type_or_size")
    descriptor = -1
    try:
        metadata = path.lstat()
        if (
            not stat.S_ISREG(metadata.st_mode)
            or path.is_symlink()
            or not 0 < metadata.st_size <= byte_limit
        ):
            raise SummaryError("report_type_or_size")
        flags = os.O_RDONLY
        flags |= getattr(os, "O_CLOEXEC", 0)
        flags |= getattr(os, "O_NOFOLLOW", 0)
        flags |= getattr(os, "O_NONBLOCK", 0)
        descriptor = os.open(path, flags)
        with os.fdopen(descriptor, "rb") as source:
            descriptor = -1
            opened = os.fstat(source.fileno())
            if (
                not stat.S_ISREG(opened.st_mode)
                or (opened.st_dev, opened.st_ino)
                != (metadata.st_dev, metadata.st_ino)
                or opened.st_size != metadata.st_size
            ):
                raise SummaryError("report_type_or_size")
            payload = source.read(byte_limit + 1)
        if (
            len(payload) != metadata.st_size
            or len(payload) > byte_limit
        ):
            raise SummaryError("report_type_or_size")
        value = _strict_json_loads(payload)
    except SummaryError:
        raise
    except OSError as error:
        raise SummaryError("report_unreadable") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    if not isinstance(value, dict):
        raise SummaryError("report_shape")
    return value, len(payload)


def _validate_report(
    value: dict[str, Any],
    *,
    profile: str,
    label: str,
    shard: int | None,
) -> tuple[list[dict[str, Any]], tuple[str, str]]:
    allowed_features = (
        FEATURES | DIAGNOSTIC_FEATURES
        if profile == "ooxml-row-diagnostic"
        else FEATURES
    )
    if (
        set(value) != REPORT_KEYS
        or value.get("schema") != INPUT_SCHEMA
        or value.get("mode") != "compare"
        or not isinstance(value.get("configuration"), dict)
        or not isinstance(value.get("preflight"), dict)
        or not isinstance(value.get("files"), list)
        or not isinstance(value.get("summary"), dict)
    ):
        raise SummaryError("report_schema")
    metric_policy = value["configuration"].get("metric_policy")
    if (
        not isinstance(metric_policy, dict)
        or not type_exact_equal(
            metric_policy.get("unique_text_geometry"),
            TEXT_GEOMETRY_POLICY,
        )
    ):
        raise SummaryError("metric_policy")
    rows = value["files"]
    limit = LANES[profile][label]
    if len(rows) > limit:
        raise SummaryError("report_coverage")
    discovery = value.get("discovery")
    if not isinstance(discovery, dict) or set(discovery) != DISCOVERY_KEYS:
        raise SummaryError("discovery_shape")
    expected = {
        "candidate_count": CASES[profile],
        "pre_shard_selected_count": limit,
        "selected_count": len(rows),
        "shard_candidate_count": len(rows),
        "truncated": False,
    }
    if any(
        not type_exact_equal(discovery.get(key), expected_value)
        for key, expected_value in expected.items()
    ):
        raise SummaryError("discovery_coverage")
    if shard is None:
        if (
            not type_exact_equal(discovery.get("shard_count"), 1)
            or not type_exact_equal(discovery.get("shard_index"), 0)
            or len(rows) != limit
        ):
            raise SummaryError("discovery_merged")
    elif (
        not type_exact_equal(discovery.get("shard_count"), SHARDS)
        or not type_exact_equal(discovery.get("shard_index"), shard)
    ):
        raise SummaryError("discovery_shard")

    statuses: Counter[str] = Counter()
    classifications: Counter[str] = Counter()
    for row in rows:
        if not isinstance(row, dict):
            raise SummaryError("workbook_row")
        status = row.get("status")
        classification = row.get("classification")
        format_name = row.get("format")
        features = row.get("features")
        digest = row.get("sha256")
        digest_is_valid = (
            isinstance(digest, str)
            and HASH_RE.fullmatch(digest) is not None
        )
        digest_is_preidentity_omission = (
            "sha256" not in row
            and isinstance(classification, str)
            and isinstance(status, str)
            and PREIDENTITY_CLASSIFICATION_STATUSES.get(
                classification
            )
            == status
        )
        if (
            not isinstance(status, str)
            or status not in STATUSES
            or not isinstance(classification, str)
            or CODE_RE.fullmatch(classification) is None
            or not isinstance(format_name, str)
            or format_name not in FORMATS
            or not isinstance(features, list)
            or len(features) > 256
            or any(
                not isinstance(feature, str) or feature not in allowed_features
                for feature in features
            )
            or features != sorted(set(features))
            or not (
                digest_is_valid
                or digest_is_preidentity_omission
            )
        ):
            raise SummaryError("workbook_contract")
        if label == "authored-print" and (
            format_name != "xlsx" or "print-settings" not in features
        ):
            raise SummaryError("authored_print_contract")
        if profile == "ooxml-row-diagnostic" and (
            label != "parity-a"
            or format_name != "xlsx"
            or "ooxml-implicit-row" not in features
        ):
            raise SummaryError("ooxml_row_diagnostic_contract")
        if classification == "page_count_mismatch":
            if status != "error":
                raise SummaryError("page_count_diagnostic")
            _page_count_pair(row, "page_count_diagnostic")
        statuses[str(status)] += 1
        classifications[classification] += 1
    summary = value["summary"]
    if (
        _integer(
            summary.get("files"),
            "summary_count",
            limit,
        )
        != len(rows)
    ):
        raise SummaryError("summary_count")
    if _count_map(summary.get("by_status"), len(rows), "summary_status", STATUSES) != dict(
        sorted(statuses.items())
    ):
        raise SummaryError("summary_status")
    if _count_map(
        summary.get("by_classification"), len(rows), "summary_classification"
    ) != dict(sorted(classifications.items())):
        raise SummaryError("summary_classification")
    identities = tuple(
        hashlib.sha256(_json(value[key])).hexdigest()
        for key in ("configuration", "preflight")
    )
    return rows, identities


def _paths(root: Path, profile: str, label: str) -> list[tuple[Path, int | None]]:
    merged = root / MERGED[label]
    shards = [
        (root / SHARDED[label].format(index=index), index) for index in range(SHARDS)
    ]
    merged_present = merged.exists() or merged.is_symlink()
    present_shards = [
        item for item in shards if item[0].exists() or item[0].is_symlink()
    ]
    if merged_present and present_shards:
        raise SummaryError("report_fragment_ambiguity")
    if profile != "full" and present_shards:
        raise SummaryError("unsharded_profile")
    return [(merged, None)] if merged_present else present_shards


def _empty(label: str) -> dict[str, object]:
    return {
        "by_classification": {},
        "by_feature": {},
        "by_format": {},
        "by_status": {},
        "case_diagnostics": _empty_case_diagnostics(),
        "fidelity": {
            "all": _finish_fidelity(
                _new_fidelity_accumulator()
            ),
            "by_format": {},
        },
        "geometry": _empty_geometry(),
        "label": label,
        "line_geometry": _empty_text_geometry(),
        "page_box_geometry": _empty_page_box_geometry(),
        "page_count_mismatches": [],
        "word_geometry": _empty_text_geometry(),
        "workbooks": 0,
    }


def _summarize_label(
    root: Path,
    profile: str,
    label: str,
    remaining: int,
    case_id_key: bytes,
) -> tuple[dict[str, object], int]:
    paths = _paths(root, profile, label)
    if LANES[profile][label] == 0:
        if paths:
            raise SummaryError("unexpected_report")
        return _empty(label), 0
    if not paths:
        return _empty(label), 0
    rows: list[dict[str, Any]] = []
    seen: set[str] = set()
    identity: tuple[str, str] | None = None
    consumed = 0
    for path, shard in paths:
        document, size = _read(path, remaining - consumed)
        fragment, fragment_identity = _validate_report(
            document, profile=profile, label=label, shard=shard
        )
        fragment_pages, fragment_histogram_buckets = (
            _text_geometry_complexity(fragment)
        )
        budget_divisor = SHARDS if shard is not None else 1
        if (
            fragment_pages
            > MAX_TEXT_GEOMETRY_REPORT_PAGES // budget_divisor
            or fragment_histogram_buckets
            > (
                MAX_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS
                // budget_divisor
            )
        ):
            raise SummaryError("text_geometry_report_limit")
        consumed += size
        if identity is None:
            identity = fragment_identity
        elif identity != fragment_identity:
            raise SummaryError("fragment_identity")
        for row in fragment:
            digest = row.get("sha256")
            if isinstance(digest, str):
                if digest in seen:
                    raise SummaryError("duplicate_workbook")
                seen.add(digest)
            rows.append(row)
    if len(rows) > LANES[profile][label]:
        raise SummaryError("report_coverage")

    statuses: Counter[str] = Counter()
    classes: Counter[str] = Counter()
    formats: dict[str, Counter[str]] = {}
    features: dict[str, Counter[str]] = {}
    retained_features = (
        FEATURES | DIAGNOSTIC_FEATURES
        if profile == "ooxml-row-diagnostic"
        else FEATURES
    )
    page_box_features = (
        PAGE_BOX_GEOMETRY_FEATURES | DIAGNOSTIC_FEATURES
        if profile == "ooxml-row-diagnostic"
        else PAGE_BOX_GEOMETRY_FEATURES
    )
    page_count_mismatches: Counter[tuple[int, int]] = Counter()
    geometry = _empty_geometry()
    page_box_geometry_all = _new_page_box_geometry_accumulator()
    page_box_geometry_by_format = {
        format_name: _new_page_box_geometry_accumulator()
        for format_name in FORMATS
    }
    page_box_geometry_by_feature = {
        feature: _new_page_box_geometry_accumulator()
        for feature in page_box_features
    }
    word_geometry_all = _new_text_geometry_accumulator()
    line_geometry_all = _new_text_geometry_accumulator()
    word_geometry_by_format: dict[str, dict[str, object]] = {}
    line_geometry_by_format: dict[str, dict[str, object]] = {}
    fidelity_all = _new_fidelity_accumulator()
    fidelity_by_format: dict[str, dict[str, object]] = {}
    case_diagnostics: list[dict[str, object]] = []
    text_geometry_pages = 0
    text_geometry_histogram_buckets = 0
    for row in rows:
        status = str(row["status"])
        raw_code = str(row["classification"])
        code = _public_classification(raw_code)
        fmt = str(row["format"])
        statuses[status] += 1
        classes[code] += 1
        formats.setdefault(fmt, Counter())[code] += 1
        for feature in retained_features.intersection(row["features"]):
            features.setdefault(str(feature), Counter())[code] += 1
        if raw_code == "page_count_mismatch":
            page_count_mismatches[
                _page_count_pair(row, "page_count_diagnostic")
            ] += 1
        # Retained command diagnostics on terminal rows are incomparable and
        # are deliberately stripped from every public metric aggregate.
        if status in METRIC_BEARING_STATUSES:
            row_geometry = _row_point_geometry(row)
            text_geometry = _row_unique_text_geometry(row)
            row_fidelity = _row_fidelity(row)
        else:
            row_geometry = None
            text_geometry = None
            row_fidelity = None
        if status in METRIC_BEARING_STATUSES and (
            row_geometry is None
            or text_geometry is None
            or row_fidelity is None
        ):
            raise SummaryError("metric_geometry_missing")
        if text_geometry is not None:
            text_geometry_pages += len(text_geometry)
            text_geometry_histogram_buckets += sum(
                len(geometry["histograms"][axis])
                for pair in text_geometry
                for geometry in pair
                for axis in TEXT_GEOMETRY_AXES
            )
            if (
                text_geometry_pages > MAX_TEXT_GEOMETRY_REPORT_PAGES
                or text_geometry_histogram_buckets
                > MAX_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS
            ):
                raise SummaryError("text_geometry_report_limit")
        if row_geometry is not None:
            pages, mismatch_pages = row_geometry
            _merge_page_box_geometry_workbook(
                page_box_geometry_all, pages
            )
            _merge_page_box_geometry_workbook(
                page_box_geometry_by_format[fmt], pages
            )
            for feature in page_box_features.intersection(
                row["features"]
            ):
                _merge_page_box_geometry_workbook(
                    page_box_geometry_by_feature[feature], pages
                )
            geometry["workbooks"] += 1
            geometry["pages"] += len(pages)
            geometry["mismatch_pages"] += mismatch_pages
            for page in pages:
                for key in PDF_POINT_DELTA_KEYS:
                    value = _ceil_micropoints(page[key])
                    delta = geometry["by_delta"][key]
                    if value != 0:
                        delta["nonzero_pages"] += 1
                    delta["max_absolute_micropoints"] = max(
                        delta["max_absolute_micropoints"], value
                    )
            geometry["max_direct_absolute_delta_micropoints"] = max(
                geometry["by_delta"][key][
                    "max_absolute_micropoints"
                ]
                for key in PDF_DIRECT_POINT_DELTA_KEYS
            )
            geometry[
                "max_internal_xhtml_crosscheck_micropoints"
            ] = max(
                geometry["by_delta"][key][
                    "max_absolute_micropoints"
                ]
                for key in PDF_XHTML_CROSSCHECK_DELTA_KEYS
            )
        if text_geometry is not None:
            word_format = word_geometry_by_format.setdefault(
                fmt, _new_text_geometry_accumulator()
            )
            line_format = line_geometry_by_format.setdefault(
                fmt, _new_text_geometry_accumulator()
            )
            for accumulator in (
                word_geometry_all,
                line_geometry_all,
                word_format,
                line_format,
            ):
                accumulator["workbooks"] += 1
            for word_page, line_page in text_geometry:
                _merge_text_geometry_page(
                    word_geometry_all, word_page
                )
                _merge_text_geometry_page(word_format, word_page)
                _merge_text_geometry_page(
                    line_geometry_all, line_page
                )
                _merge_text_geometry_page(line_format, line_page)
        if row_fidelity is not None and row_geometry is not None:
            fidelity_format = fidelity_by_format.setdefault(
                fmt, _new_fidelity_accumulator()
            )
            _merge_fidelity(fidelity_all, row_fidelity)
            _merge_fidelity(fidelity_format, row_fidelity)
            case_diagnostics.append(
                _case_diagnostic(
                    row,
                    row_fidelity,
                    row_geometry[0],
                    case_id_key,
                )
            )

    def groups(values: dict[str, Counter[str]]) -> dict[str, object]:
        return {
            key: {
                "by_classification": dict(sorted(counts.items())),
                "workbooks": sum(counts.values()),
            }
            for key, counts in sorted(values.items())
        }

    return {
        "by_classification": dict(sorted(classes.items())),
        "by_feature": groups(features),
        "by_format": groups(formats),
        "by_status": dict(sorted(statuses.items())),
        "case_diagnostics": _finish_case_diagnostics(
            case_diagnostics
        ),
        "fidelity": {
            "all": _finish_fidelity(fidelity_all),
            "by_format": {
                fmt: _finish_fidelity(accumulator)
                for fmt, accumulator in sorted(
                    fidelity_by_format.items()
                )
            },
        },
        "geometry": geometry,
        "label": label,
        "line_geometry": {
            "all": _finish_text_geometry_cohort(line_geometry_all),
            "by_format": {
                fmt: _finish_text_geometry_cohort(accumulator)
                for fmt, accumulator in sorted(
                    line_geometry_by_format.items()
                )
            },
        },
        "page_box_geometry": {
            **copy.deepcopy(PAGE_BOX_GEOMETRY_POLICY),
            "all": _finish_page_box_geometry_cohort(
                page_box_geometry_all,
                include_histogram=True,
            ),
            "by_feature": {
                feature: _finish_page_box_geometry_cohort(
                    accumulator,
                    include_histogram=False,
                )
                for feature, accumulator in sorted(
                    page_box_geometry_by_feature.items()
                )
                if int(accumulator["workbooks"]) > 0
            },
            "by_format": {
                format_name: _finish_page_box_geometry_cohort(
                    accumulator,
                    include_histogram=True,
                )
                for format_name, accumulator in sorted(
                    page_box_geometry_by_format.items()
                )
                if int(accumulator["workbooks"]) > 0
            },
        },
        "page_count_mismatches": [
            {
                "libreoffice_pages": libreoffice_pages,
                "rxls_pages": rxls_pages,
                "workbooks": count,
            }
            for (rxls_pages, libreoffice_pages), count in sorted(
                page_count_mismatches.items()
            )
        ],
        "word_geometry": {
            "all": _finish_text_geometry_cohort(word_geometry_all),
            "by_format": {
                fmt: _finish_text_geometry_cohort(accumulator)
                for fmt, accumulator in sorted(
                    word_geometry_by_format.items()
                )
            },
        },
        "workbooks": len(rows),
    }, consumed


def _validate_namespace(root: Path) -> None:
    allowed = set(MERGED.values()) | {
        template.format(index=index)
        for template in SHARDED.values()
        for index in range(SHARDS)
    }
    try:
        entries = list(root.iterdir())
    except OSError as error:
        raise SummaryError("input_root") from error
    if len(entries) > MAX_ROOT_ENTRIES:
        raise SummaryError("input_root_entries")
    if any(RAW_REPORT_RE.match(item.name) and item.name not in allowed for item in entries):
        raise SummaryError("unexpected_report_name")


def _validate_geometry_output(
    value: object, total: int
) -> dict[str, object]:
    code = "output_geometry"
    if not isinstance(value, dict) or set(value) != GEOMETRY_KEYS:
        raise SummaryError(code)
    workbooks = _integer(value["workbooks"], code, total)
    pages = _integer(value["pages"], code, total * MAX_PAGE_COUNT)
    if (
        (workbooks == 0) != (pages == 0)
        or pages < workbooks
        or pages > workbooks * MAX_PAGE_COUNT
    ):
        raise SummaryError(code)
    mismatch_pages = _integer(value["mismatch_pages"], code, pages)
    direct_max = _integer(
        value["max_direct_absolute_delta_micropoints"],
        code,
        MAX_POINT_DELTA_MICROPOINTS,
    )
    crosscheck_max = _integer(
        value["max_internal_xhtml_crosscheck_micropoints"],
        code,
        MAX_POINT_DELTA_MICROPOINTS,
    )
    by_delta = value["by_delta"]
    if not isinstance(by_delta, dict) or set(by_delta) != set(
        PDF_POINT_DELTA_KEYS
    ):
        raise SummaryError(code)
    parsed: dict[str, tuple[int, int]] = {}
    for key in PDF_POINT_DELTA_KEYS:
        row = by_delta[key]
        if not isinstance(row, dict) or set(row) != GEOMETRY_DELTA_KEYS:
            raise SummaryError(code)
        nonzero_pages = _integer(row["nonzero_pages"], code, pages)
        maximum = _integer(
            row["max_absolute_micropoints"],
            code,
            MAX_POINT_DELTA_MICROPOINTS,
        )
        if (nonzero_pages == 0) != (maximum == 0):
            raise SummaryError(code)
        parsed[key] = (nonzero_pages, maximum)

    if direct_max != max(
        (parsed[key][1] for key in PDF_DIRECT_POINT_DELTA_KEYS),
        default=0,
    ) or crosscheck_max != max(
        (parsed[key][1] for key in PDF_XHTML_CROSSCHECK_DELTA_KEYS),
        default=0,
    ):
        raise SummaryError(code)
    direct_counts = [
        parsed[key][0] for key in PDF_DIRECT_POINT_DELTA_KEYS
    ]
    minimum_mismatches = max(
        direct_counts,
        default=0,
    )
    maximum_mismatches = min(
        pages,
        sum(direct_counts),
    )
    if not minimum_mismatches <= mismatch_pages <= maximum_mismatches:
        raise SummaryError(code)
    return {
        "by_delta": parsed,
        "pages": pages,
        "workbooks": workbooks,
    }


def _integer_cohort_sum_is_feasible(
    *,
    count: int,
    nonzero_count: int,
    total: int,
    minimum: int,
    maximum: int,
    required: Sequence[int],
) -> bool:
    """Return whether exact integer counters can realize a bounded cohort."""

    required_nonzero = sum(value != 0 for value in required)
    required_zero = len(required) - required_nonzero
    remaining_count = count - len(required)
    remaining_nonzero = nonzero_count - required_nonzero
    remaining_zero = count - nonzero_count - required_zero
    if (
        remaining_count < 0
        or remaining_nonzero < 0
        or remaining_zero < 0
        or remaining_nonzero + remaining_zero != remaining_count
        or any(value < minimum or value > maximum for value in required)
        or (remaining_zero > 0 and not minimum <= 0 <= maximum)
    ):
        return False
    target = total - sum(required)
    if remaining_nonzero == 0:
        return target == 0
    if minimum > 0:
        return (
            remaining_nonzero * minimum
            <= target
            <= remaining_nonzero * maximum
        )
    if maximum < 0:
        return (
            remaining_nonzero * minimum
            <= target
            <= remaining_nonzero * maximum
        )
    if minimum == 0:
        return (
            remaining_nonzero
            <= target
            <= remaining_nonzero * maximum
        )
    if maximum == 0:
        return (
            remaining_nonzero * minimum
            <= target
            <= -remaining_nonzero
        )
    for positive_count in range(remaining_nonzero + 1):
        negative_count = remaining_nonzero - positive_count
        minimum_sum = (
            positive_count
            + negative_count * minimum
        )
        maximum_sum = (
            positive_count * maximum
            - negative_count
        )
        if minimum_sum <= target <= maximum_sum:
            return True
    return False


def _validate_page_box_geometry_cohort(
    value: object,
    *,
    workbook_limit: int,
    include_histogram: bool,
) -> dict[str, object]:
    code = "output_page_box_geometry"
    if (
        not isinstance(value, dict)
        or set(value) != PAGE_BOX_GEOMETRY_COHORT_KEYS
    ):
        raise SummaryError(code)
    workbooks = _integer(
        value["workbooks"], code, workbook_limit
    )
    pages = _integer(
        value["pages"], code, workbooks * MAX_PAGE_COUNT
    )
    if (
        (workbooks == 0) != (pages == 0)
        or pages < workbooks
    ):
        raise SummaryError(code)
    by_axis = value["by_axis"]
    if (
        not isinstance(by_axis, dict)
        or set(by_axis) != set(PAGE_BOX_GEOMETRY_AXES)
    ):
        raise SummaryError(code)
    expected_axis_keys = (
        PAGE_BOX_GEOMETRY_HISTOGRAM_AXIS_KEYS
        if include_histogram
        else PAGE_BOX_GEOMETRY_AGGREGATE_AXIS_KEYS
    )
    axes: dict[str, dict[str, object]] = {}
    for axis in PAGE_BOX_GEOMETRY_AXES:
        raw_axis = by_axis[axis]
        if (
            not isinstance(raw_axis, dict)
            or set(raw_axis) != expected_axis_keys
        ):
            raise SummaryError(code)
        histogram: Counter[str] | None = None
        if include_histogram:
            raw_histogram = raw_axis["histogram"]
            if (
                not isinstance(raw_histogram, list)
                or len(raw_histogram)
                != MAX_PAGE_BOX_GEOMETRY_HISTOGRAM_BUCKETS
            ):
                raise SummaryError(code)
            histogram = Counter()
            for expected_bucket, count in zip(
                PAGE_BOX_GEOMETRY_BUCKET_ORDER,
                raw_histogram,
                strict=True,
            ):
                histogram[expected_bucket] = _integer(
                    count, code, pages
                )
        nonzero_pages = _integer(
            raw_axis["nonzero_pages"], code, pages
        )
        total_delta = _signed_integer(
            raw_axis["sum_delta_micropoints"],
            code,
            pages * MAX_POINT_DELTA_MICROPOINTS,
        )
        raw_minimum = raw_axis["min_delta_micropoints"]
        raw_maximum = raw_axis["max_delta_micropoints"]
        if pages == 0:
            if (
                raw_minimum is not None
                or raw_maximum is not None
                or nonzero_pages != 0
                or total_delta != 0
            ):
                raise SummaryError(code)
            minimum = None
            maximum = None
        else:
            minimum = _signed_integer(
                raw_minimum, code, MAX_POINT_DELTA_MICROPOINTS
            )
            maximum = _signed_integer(
                raw_maximum, code, MAX_POINT_DELTA_MICROPOINTS
            )
            if (
                minimum > maximum
                or (nonzero_pages == 0)
                != (minimum == maximum == total_delta == 0)
                or (
                    (minimum > 0 or maximum < 0)
                    and nonzero_pages != pages
                )
            ):
                raise SummaryError(code)
            if pages == 1:
                if minimum != maximum or total_delta != minimum:
                    raise SummaryError(code)
            else:
                minimum_sum = (
                    minimum
                    + maximum
                    + (pages - 2) * minimum
                )
                maximum_sum = (
                    minimum
                    + maximum
                    + (pages - 2) * maximum
                )
                if not minimum_sum <= total_delta <= maximum_sum:
                    raise SummaryError(code)
            if (
                nonzero_pages < pages
                and not minimum <= 0 <= maximum
            ):
                raise SummaryError(code)
            required_extrema = [minimum]
            if maximum != minimum:
                required_extrema.append(maximum)
            if not _integer_cohort_sum_is_feasible(
                count=pages,
                nonzero_count=nonzero_pages,
                total=total_delta,
                minimum=minimum,
                maximum=maximum,
                required=required_extrema,
            ):
                raise SummaryError(code)
        if histogram is not None:
            _validate_page_box_histogram_aggregates(
                histogram,
                pages=pages,
                minimum=minimum,
                maximum=maximum,
                nonzero_pages=nonzero_pages,
                total_delta=total_delta,
                code=code,
            )
        axes[axis] = {
            "max_delta_micropoints": maximum,
            "min_delta_micropoints": minimum,
            "nonzero_pages": nonzero_pages,
            "sum_delta_micropoints": total_delta,
        }
        if histogram is not None:
            axes[axis]["histogram"] = histogram
    return {
        "by_axis": axes,
        "pages": pages,
        "workbooks": workbooks,
    }


def _page_box_partition_matches(
    all_cohort: dict[str, object],
    cohorts: Iterable[dict[str, object]],
) -> bool:
    rows = list(cohorts)
    if any(
        sum(int(row[key]) for row in rows) != all_cohort[key]
        for key in ("pages", "workbooks")
    ):
        return False
    for axis in PAGE_BOX_GEOMETRY_AXES:
        all_axis = all_cohort["by_axis"][axis]
        cohort_axes = [row["by_axis"][axis] for row in rows]
        nonempty = [
            axis_value
            for axis_value in cohort_axes
            if axis_value["min_delta_micropoints"] is not None
        ]
        merged_histogram: Counter[str] = Counter()
        for axis_value in cohort_axes:
            merged_histogram.update(axis_value["histogram"])
        if (
            sum(
                int(axis_value["nonzero_pages"])
                for axis_value in cohort_axes
            )
            != all_axis["nonzero_pages"]
            or sum(
                int(axis_value["sum_delta_micropoints"])
                for axis_value in cohort_axes
            )
            != all_axis["sum_delta_micropoints"]
            or (
                min(
                    (
                        int(axis_value["min_delta_micropoints"])
                        for axis_value in nonempty
                    ),
                    default=None,
                )
                != all_axis["min_delta_micropoints"]
            )
            or (
                max(
                    (
                        int(axis_value["max_delta_micropoints"])
                        for axis_value in nonempty
                    ),
                    default=None,
                )
                != all_axis["max_delta_micropoints"]
            )
            or merged_histogram != all_axis["histogram"]
        ):
            return False
    return True


def _validate_page_box_geometry_output(
    value: object,
    *,
    total: int,
    metric_format_cohorts: dict[str, dict[str, object]],
    feature_workbooks: dict[str, int],
    allowed_features: frozenset[str],
    point_geometry: dict[str, object],
) -> dict[str, object]:
    code = "output_page_box_geometry"
    if (
        not isinstance(value, dict)
        or set(value) != PAGE_BOX_GEOMETRY_KEYS
        or not type_exact_equal(
            {
                key: value.get(key)
                for key in PAGE_BOX_GEOMETRY_POLICY
            },
            PAGE_BOX_GEOMETRY_POLICY,
        )
    ):
        raise SummaryError(code)
    all_cohort = _validate_page_box_geometry_cohort(
        value["all"],
        workbook_limit=total,
        include_histogram=True,
    )
    if (
        all_cohort["workbooks"] != point_geometry["workbooks"]
        or all_cohort["pages"] != point_geometry["pages"]
    ):
        raise SummaryError(code)
    point_deltas = point_geometry["by_delta"]
    for axis in PAGE_BOX_GEOMETRY_AXES:
        aggregate = all_cohort["by_axis"][axis]
        nonzero_pages, maximum_absolute = point_deltas[
            f"crop_box_{axis}"
        ]
        if (
            aggregate["nonzero_pages"] != nonzero_pages
            or max(
                abs(int(aggregate["min_delta_micropoints"] or 0)),
                abs(int(aggregate["max_delta_micropoints"] or 0)),
            )
            != maximum_absolute
        ):
            raise SummaryError(code)

    by_format = value["by_format"]
    if (
        not isinstance(by_format, dict)
        or set(by_format) != set(metric_format_cohorts)
    ):
        raise SummaryError(code)
    format_cohorts = {
        format_name: _validate_page_box_geometry_cohort(
            raw_cohort,
            workbook_limit=int(
                metric_format_cohorts[format_name]["workbooks"]
            ),
            include_histogram=True,
        )
        for format_name, raw_cohort in by_format.items()
    }
    if any(
        cohort["workbooks"]
        != metric_format_cohorts[format_name]["workbooks"]
        or cohort["pages"]
        != metric_format_cohorts[format_name]["pages"]
        for format_name, cohort in format_cohorts.items()
    ):
        raise SummaryError(code)
    if not _page_box_partition_matches(
        all_cohort, format_cohorts.values()
    ):
        raise SummaryError(code)

    by_feature = value["by_feature"]
    if (
        not isinstance(by_feature, dict)
        or len(by_feature) > len(allowed_features)
        or any(
            not isinstance(feature, str)
            or feature not in allowed_features
            for feature in by_feature
        )
    ):
        raise SummaryError(code)
    for feature, raw_cohort in by_feature.items():
        cohort = _validate_page_box_geometry_cohort(
            raw_cohort,
            workbook_limit=feature_workbooks.get(feature, 0),
            include_histogram=False,
        )
        if (
            cohort["workbooks"] == 0
            or cohort["workbooks"] > all_cohort["workbooks"]
            or cohort["pages"] > all_cohort["pages"]
        ):
            raise SummaryError(code)
        for axis in PAGE_BOX_GEOMETRY_AXES:
            cohort_axis = cohort["by_axis"][axis]
            all_axis = all_cohort["by_axis"][axis]
            if (
                cohort_axis["nonzero_pages"]
                > all_axis["nonzero_pages"]
                or (
                    cohort_axis["min_delta_micropoints"] is not None
                    and (
                        cohort_axis["min_delta_micropoints"]
                        < all_axis["min_delta_micropoints"]
                        or cohort_axis["max_delta_micropoints"]
                        > all_axis["max_delta_micropoints"]
                    )
                )
            ):
                raise SummaryError(code)
    return {
        "all": all_cohort,
        "by_format": format_cohorts,
    }


def _validate_text_geometry_cohort(
    value: object, total: int
) -> dict[str, object]:
    code = "output_text_geometry"
    if (
        not isinstance(value, dict)
        or set(value) != TEXT_GEOMETRY_COHORT_KEYS
    ):
        raise SummaryError(code)
    workbooks = _integer(value["workbooks"], code, total)
    pages = _integer(
        value["pages"], code, workbooks * MAX_PAGE_COUNT
    )
    if (
        (workbooks == 0) != (pages == 0)
        or pages < workbooks
    ):
        raise SummaryError(code)
    item_limit = pages * MAX_TEXT_GEOMETRY_UNIQUE_ITEMS
    rxls_unique = _integer(
        value["rxls_unique_items"], code, item_limit
    )
    libreoffice_unique = _integer(
        value["libreoffice_unique_items"], code, item_limit
    )
    matched = _integer(value["matched_items"], code, item_limit)
    if matched > min(rxls_unique, libreoffice_unique):
        raise SummaryError(code)

    by_axis = value["by_axis"]
    if (
        not isinstance(by_axis, dict)
        or set(by_axis) != set(TEXT_GEOMETRY_AXES)
    ):
        raise SummaryError(code)
    histograms: dict[str, Counter[int]] = {}
    exact_summaries: dict[str, dict[str, int | None]] = {}
    for axis in TEXT_GEOMETRY_AXES:
        axis_value = by_axis[axis]
        if (
            not isinstance(axis_value, dict)
            or set(axis_value) != TEXT_GEOMETRY_AXIS_KEYS
        ):
            raise SummaryError(code)
        raw_histogram = axis_value["histogram"]
        if (
            not isinstance(raw_histogram, list)
            or len(raw_histogram)
            > MAX_TEXT_GEOMETRY_HISTOGRAM_BUCKETS
        ):
            raise SummaryError(code)
        histogram: Counter[int] = Counter()
        previous_delta: int | None = None
        histogram_count = 0
        for bucket in raw_histogram:
            if (
                not isinstance(bucket, dict)
                or set(bucket) != TEXT_GEOMETRY_BUCKET_KEYS
            ):
                raise SummaryError(code)
            delta = _signed_integer(
                bucket["delta_millipoints"],
                code,
                TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS,
            )
            if (
                delta not in TEXT_GEOMETRY_ALLOWED_BUCKETS
                or (previous_delta is not None and delta <= previous_delta)
            ):
                raise SummaryError(code)
            previous_delta = delta
            bucket_count = _integer(
                bucket["count"], code, matched
            )
            if bucket_count == 0:
                raise SummaryError(code)
            histogram[delta] = bucket_count
            histogram_count += bucket_count
        if histogram_count != matched:
            raise SummaryError(code)
        histograms[axis] = histogram
        exact_summaries[axis] = _text_geometry_exact_summary(
            axis_value["exact"],
            matched=matched,
            histogram=histogram,
            code=code,
        )
    _validate_text_geometry_axis_identities(
        exact_summaries, matched, code
    )
    return {
        "exact_summaries": exact_summaries,
        "histograms": histograms,
        "libreoffice_unique_items": libreoffice_unique,
        "matched_items": matched,
        "pages": pages,
        "rxls_unique_items": rxls_unique,
        "workbooks": workbooks,
    }


def _validate_text_geometry_output(
    value: object,
    total: int,
    format_workbooks: dict[str, int],
) -> dict[str, object]:
    code = "output_text_geometry"
    if (
        not isinstance(value, dict)
        or set(value) != TEXT_GEOMETRY_OUTPUT_KEYS
    ):
        raise SummaryError(code)
    all_cohort = _validate_text_geometry_cohort(
        value["all"], total
    )
    by_format = value["by_format"]
    if (
        not isinstance(by_format, dict)
        or len(by_format) > len(FORMATS)
        or any(
            not isinstance(format_name, str)
            or format_name not in FORMATS
            for format_name in by_format
        )
    ):
        raise SummaryError(code)

    cohorts: dict[str, dict[str, object]] = {}
    for format_name, raw_cohort in by_format.items():
        cohort = _validate_text_geometry_cohort(
            raw_cohort,
            format_workbooks.get(format_name, 0),
        )
        if cohort["workbooks"] == 0:
            raise SummaryError(code)
        cohorts[format_name] = cohort
    scalar_keys = (
        "libreoffice_unique_items",
        "matched_items",
        "pages",
        "rxls_unique_items",
        "workbooks",
    )
    if any(
        sum(int(cohort[key]) for cohort in cohorts.values())
        != all_cohort[key]
        for key in scalar_keys
    ):
        raise SummaryError(code)
    for axis in TEXT_GEOMETRY_AXES:
        merged: Counter[int] = Counter()
        merged_count = 0
        merged_sum = 0
        merged_negative_overflow = 0
        merged_positive_overflow = 0
        merged_minimum: int | None = None
        merged_maximum: int | None = None
        for cohort in cohorts.values():
            merged.update(cohort["histograms"][axis])
            exact = cohort["exact_summaries"][axis]
            merged_count += int(exact["count"])
            merged_sum += int(exact["sum_delta_millipoints"])
            merged_negative_overflow += int(
                exact["negative_overflow_items"]
            )
            merged_positive_overflow += int(
                exact["positive_overflow_items"]
            )
            if exact["min_delta_millipoints"] is not None:
                merged_minimum = (
                    int(exact["min_delta_millipoints"])
                    if merged_minimum is None
                    else min(
                        merged_minimum,
                        int(exact["min_delta_millipoints"]),
                    )
                )
                merged_maximum = (
                    int(exact["max_delta_millipoints"])
                    if merged_maximum is None
                    else max(
                        merged_maximum,
                        int(exact["max_delta_millipoints"]),
                    )
                )
        all_exact = all_cohort["exact_summaries"][axis]
        if (
            merged != all_cohort["histograms"][axis]
            or merged_count != all_exact["count"]
            or merged_sum != all_exact["sum_delta_millipoints"]
            or merged_minimum != all_exact["min_delta_millipoints"]
            or merged_maximum != all_exact["max_delta_millipoints"]
            or merged_negative_overflow
            != all_exact["negative_overflow_items"]
            or merged_positive_overflow
            != all_exact["positive_overflow_items"]
        ):
            raise SummaryError(code)
    return {
        "all": all_cohort,
        "by_format": cohorts,
    }


def _validate_ratio_output(
    value: object,
    *,
    maximum: int,
    code: str,
) -> dict[str, int]:
    if not isinstance(value, dict) or set(value) != FIDELITY_RATIO_KEYS:
        raise SummaryError(code)
    expected = _ratio_evidence(
        _integer(value["rxls_items"], code, maximum),
        _integer(value["libreoffice_items"], code, maximum),
        _integer(value["matched_items"], code, maximum),
    )
    if not type_exact_equal(value, expected):
        raise SummaryError(code)
    return expected


def _validate_text_output(
    value: object,
    *,
    maximum: int,
    code: str,
) -> dict[str, int]:
    if not isinstance(value, dict) or set(value) != FIDELITY_TEXT_KEYS:
        raise SummaryError(code)
    expected = _text_evidence(
        _integer(value["rxls_items"], code, maximum),
        _integer(value["libreoffice_items"], code, maximum),
        _integer(value["matched_items"], code, maximum),
        _integer(value["ambiguous_items"], code, maximum),
        _integer(value["rxls_unmatched_items"], code, maximum),
        _integer(
            value["libreoffice_unmatched_items"], code, maximum
        ),
    )
    if not type_exact_equal(value, expected):
        raise SummaryError(code)
    return expected


def _validate_mask_output(
    value: object,
    *,
    pixels: int,
    code: str,
) -> dict[str, int]:
    if not isinstance(value, dict) or set(value) != FIDELITY_MASK_KEYS:
        raise SummaryError(code)
    expected = _mask_evidence(
        _integer(value["rxls_pixels"], code, pixels),
        _integer(value["libreoffice_pixels"], code, pixels),
        _integer(value["rxls_matched_pixels"], code, pixels),
        _integer(
            value["libreoffice_matched_pixels"], code, pixels
        ),
    )
    if not type_exact_equal(value, expected):
        raise SummaryError(code)
    return expected


def _validate_raster_raw_relationships(
    *,
    pages: int,
    pixels: int,
    changed_pixels: int,
    absolute_error_sum: int,
    blurred_error_sum: int,
    exact_pages: int,
    max_channel_delta: int,
    code: str,
) -> None:
    """Reject raw raster counters that cannot describe one page cohort."""

    nonexact_pages = pages - exact_pages
    if (
        (pages == 0) != (pixels == 0)
        or pixels < pages
        or changed_pixels > pixels
        or exact_pages > pages
        or changed_pixels < nonexact_pages
        or changed_pixels
        > nonexact_pages * MAX_RASTER_PIXELS_PER_PAGE
        or pixels - changed_pixels < exact_pages
        or (changed_pixels == 0) != (nonexact_pages == 0)
        or (changed_pixels == 0)
        != (absolute_error_sum == 0)
        or (changed_pixels == 0)
        != (max_channel_delta == 0)
        or (
            changed_pixels == 0
            and blurred_error_sum != 0
        )
        or blurred_error_sum > pixels * max_channel_delta
        or (
            changed_pixels > 0
            and (
                absolute_error_sum
                < max(changed_pixels, max_channel_delta)
                or absolute_error_sum
                > changed_pixels * 3 * max_channel_delta
            )
        )
    ):
        raise SummaryError(code)


def _validate_raster_masks(
    *,
    pages: int,
    pixels: int,
    exact_pages: int,
    masks: Iterable[dict[str, int]],
    code: str,
) -> None:
    """Bound every derived-mask difference to nonexact page capacity."""

    nonexact_pixel_limit = min(
        pixels - exact_pages,
        (pages - exact_pages) * MAX_RASTER_PIXELS_PER_PAGE,
    )
    for mask in masks:
        rxls_pixels = int(mask["rxls_pixels"])
        libreoffice_pixels = int(mask["libreoffice_pixels"])
        rxls_matched = int(mask["rxls_matched_pixels"])
        libreoffice_matched = int(
            mask["libreoffice_matched_pixels"]
        )
        if (
            abs(rxls_pixels - libreoffice_pixels)
            > nonexact_pixel_limit
            or rxls_pixels - rxls_matched
            > nonexact_pixel_limit
            or libreoffice_pixels - libreoffice_matched
            > nonexact_pixel_limit
            or abs(rxls_matched - libreoffice_matched)
            > nonexact_pixel_limit
        ):
            raise SummaryError(code)


def _validate_raster_output(
    value: object,
    *,
    pages: int,
    code: str,
) -> dict[str, object]:
    if not isinstance(value, dict) or set(value) != FIDELITY_RASTER_KEYS:
        raise SummaryError(code)
    if _integer(value["pages"], code, pages) != pages:
        raise SummaryError(code)
    pixel_limit = pages * MAX_RASTER_PIXELS_PER_PAGE
    pixels = _integer(value["pixels"], code, pixel_limit)
    if (pages == 0) != (pixels == 0):
        raise SummaryError(code)
    changed_pixels = _integer(
        value["changed_pixels"], code, pixels
    )
    absolute_error_sum = _integer(
        value["absolute_error_sum"], code, pixels * 3 * 255
    )
    blurred_error_sum = _integer(
        value["blurred_luma_absolute_error_sum"],
        code,
        pixels * 255,
    )
    exact_pages = _integer(
        value["exact_pages"], code, pages
    )
    max_channel_delta = _integer(
        value["max_channel_delta"], code, 255
    )
    _validate_raster_raw_relationships(
        pages=pages,
        pixels=pixels,
        changed_pixels=changed_pixels,
        absolute_error_sum=absolute_error_sum,
        blurred_error_sum=blurred_error_sum,
        exact_pages=exact_pages,
        max_channel_delta=max_channel_delta,
        code=code,
    )
    mean_absolute_error_ppm = _ratio_ppm(
        absolute_error_sum,
        pixels * 3 * 255,
        empty=0,
    )
    masks = {
        "edge": _validate_mask_output(
            value["edge"], pixels=pixels, code=code
        ),
        "foreground": _validate_mask_output(
            value["foreground"], pixels=pixels, code=code
        ),
        "text_ink": _validate_mask_output(
            value["text_ink"], pixels=pixels, code=code
        ),
    }
    _validate_raster_masks(
        pages=pages,
        pixels=pixels,
        exact_pages=exact_pages,
        masks=masks.values(),
        code=code,
    )
    expected = {
        "absolute_error_sum": absolute_error_sum,
        "blurred_luma_absolute_error_sum": blurred_error_sum,
        "blurred_luma_similarity_ppm": (
            max(
                0,
                1_000_000
                - _ratio_ppm(
                    blurred_error_sum,
                    pixels * 255,
                    empty=0,
                ),
            )
            if pixels
            else 1_000_000
        ),
        "changed_pixels": changed_pixels,
        "edge": masks["edge"],
        "exact_pages": exact_pages,
        "foreground": masks["foreground"],
        "max_channel_delta": max_channel_delta,
        "mean_absolute_error_ppm": mean_absolute_error_ppm,
        "mismatch_ppm": _ratio_ppm(
            changed_pixels, pixels, empty=0
        ),
        "pages": pages,
        "pixels": pixels,
        "similarity_ppm": (
            max(0, 1_000_000 - mean_absolute_error_ppm)
            if pixels
            else 1_000_000
        ),
        "text_ink": masks["text_ink"],
    }
    if not type_exact_equal(value, expected):
        raise SummaryError(code)
    return expected


def _validate_fidelity_cohort(
    value: object,
    *,
    workbook_limit: int,
    code: str,
) -> dict[str, object]:
    if not isinstance(value, dict) or set(value) != FIDELITY_COHORT_KEYS:
        raise SummaryError(code)
    workbooks = _integer(value["workbooks"], code, workbook_limit)
    pages = _integer(
        value["pages"], code, workbooks * MAX_PAGE_COUNT
    )
    if (
        (workbooks == 0) != (pages == 0)
        or (workbooks > 0 and pages < workbooks)
    ):
        raise SummaryError(code)
    expected = {
        "pages": pages,
        "poppler_lines": _validate_text_output(
            value["poppler_lines"],
            maximum=pages * MAX_POPPLER_ITEMS_PER_PAGE,
            code=code,
        ),
        "poppler_words": _validate_text_output(
            value["poppler_words"],
            maximum=pages * MAX_POPPLER_ITEMS_PER_PAGE,
            code=code,
        ),
        "raster": _validate_raster_output(
            value["raster"], pages=pages, code=code
        ),
        "semantic_visible_characters": _validate_ratio_output(
            value["semantic_visible_characters"],
            maximum=workbooks
            * MAX_SEMANTIC_CODEPOINTS_PER_WORKBOOK,
            code=code,
        ),
        "workbooks": workbooks,
    }
    if not type_exact_equal(value, expected):
        raise SummaryError(code)
    return expected


def _validate_fidelity_output(
    value: object,
    *,
    total: int,
    format_workbooks: dict[str, int],
    metric_format_cohorts: dict[str, dict[str, object]],
) -> dict[str, object]:
    code = "output_fidelity"
    if not isinstance(value, dict) or set(value) != FIDELITY_OUTPUT_KEYS:
        raise SummaryError(code)
    all_cohort = _validate_fidelity_cohort(
        value["all"], workbook_limit=total, code=code
    )
    by_format = value["by_format"]
    if (
        not isinstance(by_format, dict)
        or set(by_format) != set(metric_format_cohorts)
        or any(
            not isinstance(name, str) or name not in FORMATS
            for name in by_format
        )
    ):
        raise SummaryError(code)
    cohorts = {
        name: _validate_fidelity_cohort(
            cohort,
            workbook_limit=format_workbooks.get(name, 0),
            code=code,
        )
        for name, cohort in by_format.items()
    }
    if any(
        cohort["workbooks"]
        != metric_format_cohorts[name]["workbooks"]
        or cohort["pages"]
        != metric_format_cohorts[name]["pages"]
        for name, cohort in cohorts.items()
    ):
        raise SummaryError(code)
    accumulator = _new_fidelity_accumulator()
    for cohort in cohorts.values():
        _merge_fidelity(accumulator, cohort)
    if not type_exact_equal(
        _finish_fidelity(accumulator), all_cohort
    ):
        raise SummaryError(code)
    return {
        "all": all_cohort,
        "by_format": cohorts,
    }


def _require_fidelity_subset(
    retained: dict[str, object],
    total: dict[str, object],
    code: str,
) -> None:
    retained_workbooks = int(retained["workbooks"])
    total_workbooks = int(total["workbooks"])
    retained_pages = int(retained["pages"])
    total_pages = int(total["pages"])
    if (
        retained_workbooks > total_workbooks
        or retained_pages > total_pages
    ):
        raise SummaryError(code)
    residual_workbooks = total_workbooks - retained_workbooks
    residual_pages = total_pages - retained_pages
    if (
        (residual_workbooks == 0) != (residual_pages == 0)
        or residual_pages < residual_workbooks
        or residual_pages
        > residual_workbooks * MAX_PAGE_COUNT
    ):
        raise SummaryError(code)

    def residual_values(
        name: str, keys: Sequence[str]
    ) -> dict[str, int]:
        values = {}
        for key in keys:
            retained_value = int(retained[name][key])
            total_value = int(total[name][key])
            if retained_value > total_value:
                raise SummaryError(code)
            values[key] = total_value - retained_value
        return values

    semantic = residual_values(
        "semantic_visible_characters",
        ("rxls_items", "libreoffice_items", "matched_items"),
    )
    if any(
        value
        > residual_workbooks
        * MAX_SEMANTIC_CODEPOINTS_PER_WORKBOOK
        for value in semantic.values()
    ):
        raise SummaryError(code)
    _ratio_evidence(
        semantic["rxls_items"],
        semantic["libreoffice_items"],
        semantic["matched_items"],
    )
    for name in ("poppler_words", "poppler_lines"):
        text = residual_values(
            name,
            (
                "rxls_items",
                "libreoffice_items",
                "matched_items",
                "ambiguous_items",
                "rxls_unmatched_items",
                "libreoffice_unmatched_items",
            ),
        )
        if any(
            value
            > residual_pages * MAX_POPPLER_ITEMS_PER_PAGE
            for value in text.values()
        ):
            raise SummaryError(code)
        _text_evidence(
            text["rxls_items"],
            text["libreoffice_items"],
            text["matched_items"],
            text["ambiguous_items"],
            text["rxls_unmatched_items"],
            text["libreoffice_unmatched_items"],
        )

    retained_raster = retained["raster"]
    total_raster = total["raster"]
    additive = {}
    for key in (
        "absolute_error_sum",
        "blurred_luma_absolute_error_sum",
        "changed_pixels",
        "exact_pages",
        "pixels",
    ):
        retained_value = int(retained_raster[key])
        total_value = int(total_raster[key])
        if retained_value > total_value:
            raise SummaryError(code)
        additive[key] = total_value - retained_value
    residual_pixels = additive["pixels"]
    retained_maximum = int(retained_raster["max_channel_delta"])
    total_maximum = int(total_raster["max_channel_delta"])
    if (
        (residual_pages == 0) != (residual_pixels == 0)
        or residual_pixels
        > residual_pages * MAX_RASTER_PIXELS_PER_PAGE
        or additive["changed_pixels"] > residual_pixels
        or additive["exact_pages"] > residual_pages
        or additive["absolute_error_sum"]
        > residual_pixels * 3 * 255
        or additive["blurred_luma_absolute_error_sum"]
        > residual_pixels * 255
        or retained_maximum > total_maximum
    ):
        raise SummaryError(code)
    residual_changed = additive["changed_pixels"]
    residual_error = additive["absolute_error_sum"]
    if residual_changed == 0:
        if retained_maximum != total_maximum:
            raise SummaryError(code)
        residual_maximum = 0
    else:
        minimum_feasible_maximum = max(
            1,
            (
                residual_error
                + residual_changed * 3
                - 1
            )
            // (residual_changed * 3),
        )
        maximum_feasible_maximum = min(
            total_maximum,
            residual_error,
        )
        residual_maximum = (
            total_maximum
            if retained_maximum < total_maximum
            else minimum_feasible_maximum
        )
        if not (
            minimum_feasible_maximum
            <= residual_maximum
            <= maximum_feasible_maximum
        ):
            raise SummaryError(code)
    _validate_raster_raw_relationships(
        pages=residual_pages,
        pixels=residual_pixels,
        changed_pixels=residual_changed,
        absolute_error_sum=residual_error,
        blurred_error_sum=additive[
            "blurred_luma_absolute_error_sum"
        ],
        exact_pages=additive["exact_pages"],
        max_channel_delta=residual_maximum,
        code=code,
    )
    residual_masks = []
    for name in ("edge", "foreground", "text_ink"):
        mask = {}
        for key in (
            "rxls_pixels",
            "libreoffice_pixels",
            "rxls_matched_pixels",
            "libreoffice_matched_pixels",
        ):
            retained_value = int(retained_raster[name][key])
            total_value = int(total_raster[name][key])
            if retained_value > total_value:
                raise SummaryError(code)
            mask[key] = total_value - retained_value
        if (
            mask["rxls_pixels"] > residual_pixels
            or mask["libreoffice_pixels"] > residual_pixels
        ):
            raise SummaryError(code)
        residual_masks.append(_mask_evidence(**mask))
    _validate_raster_masks(
        pages=residual_pages,
        pixels=residual_pixels,
        exact_pages=additive["exact_pages"],
        masks=residual_masks,
        code=code,
    )


def _new_case_page_box_accumulator() -> dict[str, object]:
    return {
        "by_axis": {
            axis: {
                "histogram": Counter(
                    {
                        bucket: 0
                        for bucket in PAGE_BOX_GEOMETRY_BUCKET_ORDER
                    }
                ),
                "max_delta_micropoints": None,
                "min_delta_micropoints": None,
                "nonzero_pages": 0,
                "sum_delta_micropoints": 0,
            }
            for axis in PAGE_BOX_GEOMETRY_AXES
        },
        "pages": 0,
        "workbooks": 0,
    }


def _merge_case_page_box(
    accumulator: dict[str, object],
    cohort: dict[str, object],
) -> None:
    accumulator["workbooks"] += int(cohort["workbooks"])
    accumulator["pages"] += int(cohort["pages"])
    for axis in PAGE_BOX_GEOMETRY_AXES:
        target = accumulator["by_axis"][axis]
        source = cohort["by_axis"][axis]
        target["histogram"].update(source["histogram"])
        target["nonzero_pages"] += int(source["nonzero_pages"])
        target["sum_delta_micropoints"] += int(
            source["sum_delta_micropoints"]
        )
        source_minimum = source["min_delta_micropoints"]
        source_maximum = source["max_delta_micropoints"]
        if source_minimum is not None:
            target["min_delta_micropoints"] = (
                int(source_minimum)
                if target["min_delta_micropoints"] is None
                else min(
                    int(target["min_delta_micropoints"]),
                    int(source_minimum),
                )
            )
            target["max_delta_micropoints"] = (
                int(source_maximum)
                if target["max_delta_micropoints"] is None
                else max(
                    int(target["max_delta_micropoints"]),
                    int(source_maximum),
                )
            )


def _require_page_box_subset(
    retained: dict[str, object],
    total: dict[str, object],
    code: str,
) -> None:
    retained_workbooks = int(retained["workbooks"])
    total_workbooks = int(total["workbooks"])
    retained_pages = int(retained["pages"])
    total_pages = int(total["pages"])
    residual_workbooks = total_workbooks - retained_workbooks
    residual_pages = total_pages - retained_pages
    if (
        residual_workbooks < 0
        or residual_pages < 0
        or (residual_workbooks == 0) != (residual_pages == 0)
        or residual_pages < residual_workbooks
        or residual_pages
        > residual_workbooks * MAX_PAGE_COUNT
    ):
        raise SummaryError(code)
    for axis in PAGE_BOX_GEOMETRY_AXES:
        retained_axis = retained["by_axis"][axis]
        total_axis = total["by_axis"][axis]
        retained_nonzero = int(retained_axis["nonzero_pages"])
        total_nonzero = int(total_axis["nonzero_pages"])
        if retained_nonzero > total_nonzero:
            raise SummaryError(code)
        residual_nonzero = total_nonzero - retained_nonzero
        residual_sum = int(
            total_axis["sum_delta_micropoints"]
        ) - int(retained_axis["sum_delta_micropoints"])
        residual_histogram = Counter()
        for bucket in PAGE_BOX_GEOMETRY_BUCKET_ORDER:
            count = (
                int(total_axis["histogram"][bucket])
                - int(retained_axis["histogram"][bucket])
            )
            if count < 0:
                raise SummaryError(code)
            residual_histogram[bucket] = count
        if (
            sum(residual_histogram.values()) != residual_pages
            or residual_pages - residual_histogram["zero"]
            != residual_nonzero
        ):
            raise SummaryError(code)
        if residual_pages == 0:
            if (
                residual_nonzero != 0
                or residual_sum != 0
                or retained_axis["min_delta_micropoints"]
                != total_axis["min_delta_micropoints"]
                or retained_axis["max_delta_micropoints"]
                != total_axis["max_delta_micropoints"]
            ):
                raise SummaryError(code)
            continue
        if not 0 <= residual_nonzero <= residual_pages:
            raise SummaryError(code)
        total_minimum = int(total_axis["min_delta_micropoints"])
        total_maximum = int(total_axis["max_delta_micropoints"])
        retained_minimum = retained_axis["min_delta_micropoints"]
        retained_maximum = retained_axis["max_delta_micropoints"]
        if retained_minimum is not None and (
            int(retained_minimum) < total_minimum
            or int(retained_maximum) > total_maximum
        ):
            raise SummaryError(code)
        required: list[int] = []
        if (
            retained_minimum is None
            or int(retained_minimum) > total_minimum
        ):
            required.append(total_minimum)
        if (
            retained_maximum is None
            or int(retained_maximum) < total_maximum
        ) and total_maximum not in required:
            required.append(total_maximum)
        remaining = residual_histogram.copy()
        remaining_sum = residual_sum
        for required_value in required:
            bucket = _page_box_geometry_bucket(required_value)
            if (
                not total_minimum
                <= required_value
                <= total_maximum
                or remaining[bucket] == 0
            ):
                raise SummaryError(code)
            remaining[bucket] -= 1
            remaining_sum -= required_value
        minimum_sum = 0
        maximum_sum = 0
        for bucket, count in remaining.items():
            if count == 0:
                continue
            lower, upper = PAGE_BOX_GEOMETRY_BUCKET_INTERVALS[
                bucket
            ]
            lower = max(lower, total_minimum)
            upper = min(upper, total_maximum)
            if lower > upper:
                raise SummaryError(code)
            minimum_sum += lower * count
            maximum_sum += upper * count
        if not minimum_sum <= remaining_sum <= maximum_sum:
            raise SummaryError(code)


def _validate_case_diagnostics(
    value: object,
    *,
    fidelity: dict[str, object],
    page_box_geometry: dict[str, object],
) -> None:
    code = "output_case_diagnostics"
    if not isinstance(value, dict) or set(value) != CASE_DIAGNOSTICS_KEYS:
        raise SummaryError(code)
    available = _integer(
        value["available_cases"],
        code,
        int(fidelity["all"]["workbooks"]),
    )
    if available != fidelity["all"]["workbooks"]:
        raise SummaryError(code)
    expected_available_by_format = {
        name: int(cohort["workbooks"])
        for name, cohort in sorted(fidelity["by_format"].items())
    }
    if not type_exact_equal(
        value["available_cases_by_format"],
        expected_available_by_format,
    ):
        raise SummaryError(code)
    retained = _integer(
        value["retained_cases"],
        code,
        MAX_CASE_DIAGNOSTICS_PER_REPORT,
    )
    expected_retained = min(
        available, MAX_CASE_DIAGNOSTICS_PER_REPORT
    )
    cases = value["cases"]
    if (
        retained != expected_retained
        or not isinstance(cases, list)
        or len(cases) != retained
        or value["truncated"] is not (
            retained != available
        )
    ):
        raise SummaryError(code)
    previous: str | None = None
    parsed_cases: list[dict[str, object]] = []
    for case in cases:
        if not isinstance(case, dict) or set(case) != CASE_DIAGNOSTIC_KEYS:
            raise SummaryError(code)
        case_id = case["case_id"]
        format_name = case["format"]
        if (
            not isinstance(case_id, str)
            or HASH_RE.fullmatch(case_id) is None
            or (previous is not None and case_id <= previous)
            or not isinstance(format_name, str)
            or format_name not in fidelity["by_format"]
        ):
            raise SummaryError(code)
        previous = case_id
        page_box = _validate_page_box_geometry_cohort(
            case["page_box"],
            workbook_limit=1,
            include_histogram=True,
        )
        if page_box["workbooks"] != 1:
            raise SummaryError(code)
        page_count = int(page_box["pages"])
        semantic = _validate_ratio_output(
            case["semantic_visible_characters"],
            maximum=MAX_SEMANTIC_CODEPOINTS_PER_WORKBOOK,
            code=code,
        )
        words = _validate_text_output(
            case["poppler_words"],
            maximum=page_count * MAX_POPPLER_ITEMS_PER_PAGE,
            code=code,
        )
        lines = _validate_text_output(
            case["poppler_lines"],
            maximum=page_count * MAX_POPPLER_ITEMS_PER_PAGE,
            code=code,
        )
        parsed_raster = _validate_raster_output(
            case["raster"],
            pages=page_count,
            code=code,
        )
        parsed_cases.append(
            {
                "format": format_name,
                "pages": page_count,
                "page_box": page_box,
                "poppler_lines": lines,
                "poppler_words": words,
                "raster": parsed_raster,
                "semantic_visible_characters": semantic,
                "workbooks": 1,
            }
        )
    retained_case_counts = dict(
        sorted(
            Counter(
                str(case["format"]) for case in parsed_cases
            ).items()
        )
    )
    if not type_exact_equal(
        value["retained_cases_by_format"],
        retained_case_counts,
    ):
        raise SummaryError(code)

    retained_fidelity = _new_fidelity_accumulator()
    retained_fidelity_by_format: dict[str, dict[str, object]] = {}
    retained_page_box = _new_case_page_box_accumulator()
    retained_page_box_by_format: dict[str, dict[str, object]] = {}
    for case in parsed_cases:
        format_name = str(case["format"])
        _merge_fidelity(retained_fidelity, case)
        _merge_fidelity(
            retained_fidelity_by_format.setdefault(
                format_name, _new_fidelity_accumulator()
            ),
            case,
        )
        _merge_case_page_box(retained_page_box, case["page_box"])
        _merge_case_page_box(
            retained_page_box_by_format.setdefault(
                format_name, _new_case_page_box_accumulator()
            ),
            case["page_box"],
        )
    retained_fidelity_output = _finish_fidelity(retained_fidelity)
    retained_fidelity_formats = {
        name: _finish_fidelity(accumulator)
        for name, accumulator in retained_fidelity_by_format.items()
    }
    truncated = bool(value["truncated"])
    if not truncated:
        if not type_exact_equal(
            retained_fidelity_output, fidelity["all"]
        ):
            raise SummaryError(code)
    else:
        _require_fidelity_subset(
            retained_fidelity_output, fidelity["all"], code
        )
    empty_fidelity = _finish_fidelity(
        _new_fidelity_accumulator()
    )
    for name, total_cohort in fidelity["by_format"].items():
        cohort = retained_fidelity_formats.get(
            name, empty_fidelity
        )
        if not truncated:
            if not type_exact_equal(cohort, total_cohort):
                raise SummaryError(code)
        else:
            _require_fidelity_subset(
                cohort, total_cohort, code
            )

    total_page_box = page_box_geometry["all"]
    if not truncated:
        if not type_exact_equal(retained_page_box, total_page_box):
            raise SummaryError(code)
    else:
        _require_page_box_subset(
            retained_page_box, total_page_box, code
        )
    empty_page_box = _new_case_page_box_accumulator()
    for name, raw_total_cohort in page_box_geometry[
        "by_format"
    ].items():
        cohort = retained_page_box_by_format.get(
            name, empty_page_box
        )
        total_cohort = raw_total_cohort
        if not truncated:
            if not type_exact_equal(cohort, total_cohort):
                raise SummaryError(code)
        else:
            _require_page_box_subset(cohort, total_cohort, code)


def _validate_output(value: object) -> None:
    """Ensure no unreviewed key or path-like string reached the final JSON."""

    top = {
        "baseline_mode",
        "case_id_policy",
        "geometry_policy",
        "head_sha",
        "ingestion",
        "profile",
        "reports",
        "schema",
    }
    report_keys = {
        "by_classification",
        "by_feature",
        "by_format",
        "by_status",
        "case_diagnostics",
        "fidelity",
        "geometry",
        "label",
        "line_geometry",
        "page_box_geometry",
        "page_count_mismatches",
        "word_geometry",
        "workbooks",
    }
    if not isinstance(value, dict) or set(value) != top:
        raise SummaryError("output_contract")
    reports = value.get("reports")
    if (
        value.get("schema") != OUTPUT_SCHEMA
        or not type_exact_equal(
            value.get("case_id_policy"),
            CASE_ID_POLICY,
        )
        or not type_exact_equal(
            value.get("geometry_policy"),
            TEXT_GEOMETRY_POLICY,
        )
        or not isinstance(value.get("profile"), str)
        or value.get("profile") not in CASES
        or not isinstance(value.get("baseline_mode"), str)
        or value.get("baseline_mode") not in {"candidate", "verify"}
        or not isinstance(value.get("head_sha"), str)
        or HEAD_RE.fullmatch(str(value["head_sha"])) is None
        or not isinstance(reports, list)
        or len(reports) != len(LABELS)
        or tuple(row.get("label") for row in reports if isinstance(row, dict)) != LABELS
        or any(not isinstance(row, dict) or set(row) != report_keys for row in reports)
    ):
        raise SummaryError("output_contract")

    profile = str(value["profile"])
    ingestion = value["ingestion"]
    expected_workbooks = sum(LANES[profile].values())
    if (
        not isinstance(ingestion, dict)
        or set(ingestion) != INGESTION_KEYS
        or _integer(
            ingestion.get("expected_workbooks"),
            "output_ingestion",
            expected_workbooks,
        )
        != expected_workbooks
        or not isinstance(ingestion.get("status"), str)
        or ingestion.get("status") not in INGESTION_STATUSES
    ):
        raise SummaryError("output_ingestion")
    received_workbooks = _integer(
        ingestion.get("received_workbooks"),
        "output_ingestion",
        expected_workbooks,
    )
    allowed_features = (
        FEATURES | DIAGNOSTIC_FEATURES
        if profile == "ooxml-row-diagnostic"
        else FEATURES
    )
    allowed_page_box_features = (
        PAGE_BOX_GEOMETRY_FEATURES | DIAGNOSTIC_FEATURES
        if profile == "ooxml-row-diagnostic"
        else PAGE_BOX_GEOMETRY_FEATURES
    )
    for report in reports:
        total = _integer(
            report["workbooks"],
            "output_count",
            LANES[profile][str(report["label"])],
        )
        _count_map(report["by_status"], total, "output_status", STATUSES)
        report_classes = _count_map(
            report["by_classification"],
            total,
            "output_classification",
            OUTPUT_CLASSIFICATIONS,
        )
        point_geometry = _validate_geometry_output(
            report["geometry"], total
        )
        page_count_mismatches = report["page_count_mismatches"]
        if (
            not isinstance(page_count_mismatches, list)
            or len(page_count_mismatches) > total
        ):
            raise SummaryError("output_page_count_diagnostic")
        previous_pair: tuple[int, int] | None = None
        page_count_workbooks = 0
        for mismatch in page_count_mismatches:
            if (
                not isinstance(mismatch, dict)
                or set(mismatch)
                != {"libreoffice_pages", "rxls_pages", "workbooks"}
            ):
                raise SummaryError("output_page_count_diagnostic")
            pair = _page_count_pair(
                mismatch, "output_page_count_diagnostic"
            )
            if previous_pair is not None and pair <= previous_pair:
                raise SummaryError("output_page_count_diagnostic")
            previous_pair = pair
            workbooks = _integer(
                mismatch["workbooks"],
                "output_page_count_diagnostic",
                total,
            )
            if workbooks == 0:
                raise SummaryError("output_page_count_diagnostic")
            page_count_workbooks += workbooks
        if page_count_workbooks != report_classes.get(
            "page_count_mismatch", 0
        ):
            raise SummaryError("output_page_count_diagnostic")
        group_workbooks: dict[str, dict[str, int]] = {}
        for key, allowed in (
            ("by_format", FORMATS),
            ("by_feature", allowed_features),
        ):
            groups = report[key]
            if (
                not isinstance(groups, dict)
                or len(groups) > len(allowed)
                or any(
                    not isinstance(name, str) or name not in allowed
                    for name in groups
                )
            ):
                raise SummaryError("output_group")
            grouped_total = 0
            grouped_classes: Counter[str] = Counter()
            group_workbooks[key] = {}
            for name, group in groups.items():
                if (
                    not isinstance(group, dict)
                    or set(group) != {"by_classification", "workbooks"}
                ):
                    raise SummaryError("output_group")
                group_total = _integer(
                    group["workbooks"], "output_group", total
                )
                if group_total == 0:
                    raise SummaryError("output_group")
                group_classes = _count_map(
                    group["by_classification"],
                    group_total,
                    "output_group",
                    OUTPUT_CLASSIFICATIONS,
                )
                if key == "by_feature" and any(
                    count > report_classes.get(classification, 0)
                    for classification, count in group_classes.items()
                ):
                    raise SummaryError("output_group")
                grouped_classes.update(group_classes)
                grouped_total += group_total
                group_workbooks[key][name] = group_total
            if key == "by_format" and (
                grouped_total != total
                or dict(sorted(grouped_classes.items())) != report_classes
            ):
                raise SummaryError("output_group")
        format_workbooks = group_workbooks["by_format"]
        word_geometry = _validate_text_geometry_output(
            report["word_geometry"], total, format_workbooks
        )
        line_geometry = _validate_text_geometry_output(
            report["line_geometry"], total, format_workbooks
        )
        if (
            word_geometry["all"]["workbooks"]
            != line_geometry["all"]["workbooks"]
            or word_geometry["all"]["pages"]
            != line_geometry["all"]["pages"]
            or set(word_geometry["by_format"])
            != set(line_geometry["by_format"])
            or any(
                word_geometry["by_format"][format_name]["workbooks"]
                != line_geometry["by_format"][format_name]["workbooks"]
                or word_geometry["by_format"][format_name]["pages"]
                != line_geometry["by_format"][format_name]["pages"]
                for format_name in word_geometry["by_format"]
            )
        ):
            raise SummaryError("output_text_geometry")
        fidelity = _validate_fidelity_output(
            report["fidelity"],
            total=total,
            format_workbooks=format_workbooks,
            metric_format_cohorts=word_geometry["by_format"],
        )
        page_box_geometry = _validate_page_box_geometry_output(
            report["page_box_geometry"],
            total=total,
            metric_format_cohorts=word_geometry["by_format"],
            feature_workbooks=group_workbooks["by_feature"],
            allowed_features=allowed_page_box_features,
            point_geometry=point_geometry,
        )
        _validate_case_diagnostics(
            report["case_diagnostics"],
            fidelity=fidelity,
            page_box_geometry=page_box_geometry,
        )
    actual_received = sum(
        int(report["workbooks"]) for report in reports
    )
    if received_workbooks != actual_received:
        raise SummaryError("output_ingestion")
    status = ingestion["status"]
    if (
        (status == "complete" and actual_received != expected_workbooks)
        or (
            status == "partial"
            and not 0 < actual_received < expected_workbooks
        )
        or (status == "unavailable" and actual_received != 0)
        or (
            status == "rejected"
            and (
                actual_received != 0
                or any(report != _empty(str(report["label"])) for report in reports)
            )
        )
    ):
        raise SummaryError("output_ingestion")


def _validate_invocation(
    *,
    profile: str,
    baseline_mode: str,
    head_sha: str,
) -> None:
    if (
        not isinstance(profile, str)
        or profile not in CASES
        or not isinstance(baseline_mode, str)
        or baseline_mode not in {"candidate", "verify"}
        or (profile != "full" and baseline_mode != "verify")
        or not isinstance(head_sha, str)
        or HEAD_RE.fullmatch(head_sha) is None
    ):
        raise SummaryError("invocation")


def _summary_document(
    *,
    profile: str,
    baseline_mode: str,
    head_sha: str,
    reports: list[dict[str, object]],
    ingestion_status: str,
) -> dict[str, object]:
    expected_workbooks = sum(LANES[profile].values())
    received_workbooks = sum(
        int(report["workbooks"]) for report in reports
    )
    result = {
        "baseline_mode": baseline_mode,
        "case_id_policy": copy.deepcopy(CASE_ID_POLICY),
        "geometry_policy": copy.deepcopy(TEXT_GEOMETRY_POLICY),
        "head_sha": head_sha,
        "ingestion": {
            "expected_workbooks": expected_workbooks,
            "received_workbooks": received_workbooks,
            "status": ingestion_status,
        },
        "profile": profile,
        "reports": reports,
        "schema": OUTPUT_SCHEMA,
    }
    _validate_output(result)
    if len(_json(result)) > MAX_OUTPUT_BYTES:
        raise SummaryError("output_size")
    return result


def rejected_summary(
    *,
    profile: str,
    baseline_mode: str,
    head_sha: str,
) -> dict[str, object]:
    _validate_invocation(
        profile=profile,
        baseline_mode=baseline_mode,
        head_sha=head_sha,
    )
    return _summary_document(
        profile=profile,
        baseline_mode=baseline_mode,
        head_sha=head_sha,
        reports=[_empty(label) for label in LABELS],
        ingestion_status="rejected",
    )


def summarize(
    root: Path,
    *,
    profile: str,
    baseline_mode: str,
    head_sha: str,
    _case_id_key_for_test: bytes | None = None,
) -> dict[str, object]:
    _validate_invocation(
        profile=profile,
        baseline_mode=baseline_mode,
        head_sha=head_sha,
    )
    if root.exists() or root.is_symlink():
        metadata = root.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or root.is_symlink():
            raise SummaryError("input_root")
        _validate_namespace(root)
    case_id_key = _case_id_key(_case_id_key_for_test)
    consumed = 0
    reports = []
    for label in LABELS:
        report, size = _summarize_label(
            root,
            profile,
            label,
            MAX_TOTAL_BYTES - consumed,
            case_id_key,
        )
        consumed += size
        reports.append(report)
    received_workbooks = sum(
        int(report["workbooks"]) for report in reports
    )
    expected_workbooks = sum(LANES[profile].values())
    status = (
        "complete"
        if received_workbooks == expected_workbooks
        else "partial"
        if received_workbooks
        else "unavailable"
    )
    return _summary_document(
        profile=profile,
        baseline_mode=baseline_mode,
        head_sha=head_sha,
        reports=reports,
        ingestion_status=status,
    )


def write_atomic(path: Path, value: object) -> None:
    if path.name != OUTPUT_NAME:
        raise SummaryError("output_name")
    if path.exists() or path.is_symlink():
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
            raise SummaryError("output_type")
    _validate_output(value)
    payload = _json(value)
    if len(payload) > MAX_OUTPUT_BYTES:
        raise SummaryError("output_size")
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary: str | None = None
    try:
        with tempfile.NamedTemporaryFile(
            dir=path.parent, prefix=f".{path.name}.", delete=False
        ) as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
            temporary = output.name
        os.replace(temporary, path)
        temporary = None
    except OSError as error:
        raise SummaryError("output_write") from error
    finally:
        if temporary is not None:
            try:
                Path(temporary).unlink()
            except OSError:
                pass


def parse_args(argv: Iterable[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-root", type=Path, required=True)
    parser.add_argument("--profile", choices=sorted(CASES), required=True)
    parser.add_argument("--baseline-mode", choices=("candidate", "verify"), required=True)
    parser.add_argument("--head-sha", required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args(argv)


def main(argv: Iterable[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        result = summarize(
            args.input_root,
            profile=args.profile,
            baseline_mode=args.baseline_mode,
            head_sha=args.head_sha,
        )
    except (SummaryError, OSError) as error:
        if isinstance(error, SummaryError) and str(error) == "invocation":
            print(
                "render-oracle-failure-summary: invocation",
                file=sys.stderr,
            )
            return 1
        try:
            result = rejected_summary(
                profile=args.profile,
                baseline_mode=args.baseline_mode,
                head_sha=args.head_sha,
            )
            write_atomic(args.output, result)
        except (SummaryError, OSError) as fallback_error:
            code = (
                str(fallback_error)
                if isinstance(fallback_error, SummaryError)
                else "filesystem"
            )
            print(
                f"render-oracle-failure-summary: {code}",
                file=sys.stderr,
            )
            return 1
        print(
            "render-oracle-failure-summary: "
            "unsafe_or_incomplete_reports_rejected",
            file=sys.stderr,
        )
        return 0
    try:
        write_atomic(args.output, result)
    except (SummaryError, OSError) as error:
        code = str(error) if isinstance(error, SummaryError) else "filesystem"
        print(f"render-oracle-failure-summary: {code}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
