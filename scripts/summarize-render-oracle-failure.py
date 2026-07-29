#!/usr/bin/env python3
"""Reduce failed Render Oracle reports to a bounded path-neutral summary."""

from __future__ import annotations

import argparse
from collections import Counter
import copy
from fractions import Fraction
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
from typing import Any, Iterable, Sequence

try:
    from strict_json_contract import type_exact_equal
except ModuleNotFoundError:  # Imported as ``scripts.*`` by unit tests.
    from scripts.strict_json_contract import type_exact_equal


INPUT_SCHEMA = "rxls.libreoffice-render-parity.v1"
OUTPUT_SCHEMA = "rxls.render-oracle-failure-summary.v6"
OUTPUT_NAME = "render-oracle-failure-summary.json"
MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_TOTAL_BYTES = 768 * 1024 * 1024
MAX_OUTPUT_BYTES = 1024 * 1024
MAX_ROOT_ENTRIES = 128
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_JSON_INTEGER_DIGITS = 128
MAX_PAGE_COUNT = 64
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
CASES = {"full": 800, "ooxml-row-diagnostic": 12, "pilot": 40}
LANES = {
    "full": {"authored-print": 100, "parity-a": 800, "parity-b": 800},
    "ooxml-row-diagnostic": {
        "authored-print": 0,
        "parity-a": 12,
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
        "xhtml_height",
        "xhtml_width",
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
    ) or any(
        abs(parsed[key]) > PDF_XHTML_CROSSCHECK_MAX_POINTS
        for key in PDF_XHTML_CROSSCHECK_DELTA_KEYS
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
                not isinstance(feature, str) or feature not in FEATURES
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
        "geometry": _empty_geometry(),
        "label": label,
        "line_geometry": _empty_text_geometry(),
        "page_count_mismatches": [],
        "word_geometry": _empty_text_geometry(),
        "workbooks": 0,
    }


def _summarize_label(
    root: Path, profile: str, label: str, remaining: int
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
    page_count_mismatches: Counter[tuple[int, int]] = Counter()
    geometry = _empty_geometry()
    word_geometry_all = _new_text_geometry_accumulator()
    line_geometry_all = _new_text_geometry_accumulator()
    word_geometry_by_format: dict[str, dict[str, object]] = {}
    line_geometry_by_format: dict[str, dict[str, object]] = {}
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
        for feature in row["features"]:
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
        else:
            row_geometry = None
            text_geometry = None
        if status in METRIC_BEARING_STATUSES and (
            row_geometry is None or text_geometry is None
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


def _validate_geometry_output(value: object, total: int) -> None:
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
    over_limit_crosscheck_counts = [
        parsed[key][0]
        for key in PDF_XHTML_CROSSCHECK_DELTA_KEYS
        if parsed[key][1] > 1000
    ]
    minimum_mismatches = max(
        [*direct_counts, int(bool(over_limit_crosscheck_counts))],
        default=0,
    )
    maximum_mismatches = min(
        pages,
        sum(direct_counts) + sum(over_limit_crosscheck_counts),
    )
    if not minimum_mismatches <= mismatch_pages <= maximum_mismatches:
        raise SummaryError(code)


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


def _validate_output(value: object) -> None:
    """Ensure no unreviewed key or path-like string reached the final JSON."""

    top = {
        "baseline_mode",
        "geometry_policy",
        "head_sha",
        "profile",
        "reports",
        "schema",
    }
    report_keys = {
        "by_classification",
        "by_feature",
        "by_format",
        "by_status",
        "geometry",
        "label",
        "line_geometry",
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
        _validate_geometry_output(report["geometry"], total)
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
        for key, allowed in (("by_format", FORMATS), ("by_feature", FEATURES)):
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
            for group in groups.values():
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
            if key == "by_format" and (
                grouped_total != total
                or dict(sorted(grouped_classes.items())) != report_classes
            ):
                raise SummaryError("output_group")
        format_workbooks = {
            format_name: int(group["workbooks"])
            for format_name, group in report["by_format"].items()
        }
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


def summarize(
    root: Path, *, profile: str, baseline_mode: str, head_sha: str
) -> dict[str, object]:
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
    if root.exists() or root.is_symlink():
        metadata = root.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or root.is_symlink():
            raise SummaryError("input_root")
        _validate_namespace(root)
    consumed = 0
    reports = []
    for label in LABELS:
        report, size = _summarize_label(root, profile, label, MAX_TOTAL_BYTES - consumed)
        consumed += size
        reports.append(report)
    result = {
        "baseline_mode": baseline_mode,
        "geometry_policy": copy.deepcopy(TEXT_GEOMETRY_POLICY),
        "head_sha": head_sha,
        "profile": profile,
        "reports": reports,
        "schema": OUTPUT_SCHEMA,
    }
    _validate_output(result)
    if len(_json(result)) > MAX_OUTPUT_BYTES:
        raise SummaryError("output_size")
    return result


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
        write_atomic(
            args.output,
            summarize(
                args.input_root,
                profile=args.profile,
                baseline_mode=args.baseline_mode,
                head_sha=args.head_sha,
            ),
        )
        return 0
    except (SummaryError, OSError) as error:
        code = str(error) if isinstance(error, SummaryError) else "filesystem"
        print(f"render-oracle-failure-summary: {code}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
