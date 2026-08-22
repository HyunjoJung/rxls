#!/usr/bin/env python3
"""Gate repeat LibreOffice parity runs without publishing corpus paths.

The two inputs must be complete single-page ``rxls.libreoffice-render-parity.v1``
campaign reports produced with exactly the same configuration, preflight, and
renderer binary.  Authored-print evidence has a separate page-map gate.  Input
workbooks are paired only by their SHA-256 identity; host paths are deliberately
ignored and never copied to the result.

Everything owned by rxls (renderer metadata, scene hashes, page mapping,
semantic counts, page dimensions, and content-private unique-text geometry)
must be exact.  The only tolerated variation is integer visual evidence derived
from the LibreOffice oracle.  The gate publishes sorted, path-neutral
distributions of absolute PPM deltas for plain similarity, blurred-luma
similarity, and the three mask F1 scores.  The diagnostic unique-text geometry
is validated and compared exactly, but is not part of the acceptance score.
The 20,000 PPM defaults are deliberately bounded just above the clean locked
40-workbook profile maxima (11,447 visual PPM and 16,828 mask PPM).

Exit status is 0 for a pass, 1 for an identity/stability/threshold failure, and
2 for malformed, incomplete, duplicate, oversized, or unreadable evidence.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
from typing import Any, Sequence

try:
    from strict_json_contract import type_exact_equal
except ModuleNotFoundError:  # Imported as ``scripts.*`` by unit tests.
    from scripts.strict_json_contract import type_exact_equal


INPUT_SCHEMA = "rxls.libreoffice-render-parity.v1"
OUTPUT_SCHEMA = "rxls.libreoffice-render-repeatability.v2"
HARNESS_PATH = Path(__file__).with_name("libreoffice-render-parity.py")
MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_TOTAL_REPORT_BYTES = 512 * 1024 * 1024
MAX_FILES = 1_000_000
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_JSON_INTEGER_DIGITS = 128
DEFAULT_MAX_DRIFT_PPM = 20_000
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
CLASSIFICATION_RE = re.compile(r"[a-z][a-z0-9_]{0,95}\Z")
REPORT_STATUSES = frozenset({"compared", "different", "error", "skipped"})

SIMILARITY_METRIC = "similarity_ppm"
BLUR_METRIC = "blurred_luma_similarity_ppm"
MASK_METRICS = ("edge_f1_ppm", "foreground_f1_ppm", "text_ink_f1_ppm")
DRIFT_METRICS = (SIMILARITY_METRIC, BLUR_METRIC, *MASK_METRICS)
UNIQUE_TEXT_GEOMETRY_METRICS = (
    "text_box_unique_geometry",
    "text_line_box_unique_geometry",
)
UNIQUE_TEXT_GEOMETRY_AXES = (
    "x_min",
    "x_max",
    "y_min",
    "y_max",
    "center_x",
    "center_y",
    "width",
    "height",
)
UNIQUE_TEXT_GEOMETRY_PAGE_KEYS = frozenset(
    {
        "delta_histograms_millipoints",
        "exact_delta_summaries_millipoints",
        "libreoffice_unique_items",
        "matched_items",
        "rxls_unique_items",
    }
)
UNIQUE_TEXT_GEOMETRY_EXACT_SUMMARY_KEYS = frozenset(
    {
        "count",
        "max_delta_millipoints",
        "min_delta_millipoints",
        "negative_overflow_items",
        "positive_overflow_items",
        "sum_delta_millipoints",
    }
)
UNIQUE_TEXT_GEOMETRY_BUCKET_KEYS = frozenset(
    {"count", "delta_millipoints"}
)
MAX_UNIQUE_TEXT_GEOMETRY_ITEMS = 250_000
MAX_UNIQUE_TEXT_GEOMETRY_DELTA_MILLIPOINTS = 1_000_000_000
MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES = 2_000
MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS = 50_000
UNIQUE_TEXT_GEOMETRY_EXACT_LIMIT_MILLIPOINTS = 2
UNIQUE_TEXT_GEOMETRY_MIDDLE_LIMIT_MILLIPOINTS = 1_000
UNIQUE_TEXT_GEOMETRY_MIDDLE_BUCKET_MILLIPOINTS = 500
UNIQUE_TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS = 10_000
UNIQUE_TEXT_GEOMETRY_OUTER_BUCKET_MILLIPOINTS = 2_000
UNIQUE_TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS = 12_000
MAX_UNIQUE_TEXT_GEOMETRY_BUCKETS = 21
UNIQUE_TEXT_GEOMETRY_ALLOWED_BUCKETS = frozenset(
    range(-UNIQUE_TEXT_GEOMETRY_EXACT_LIMIT_MILLIPOINTS,
          UNIQUE_TEXT_GEOMETRY_EXACT_LIMIT_MILLIPOINTS + 1)
) | frozenset(
    value
    for magnitude in (500, 1_000)
    for value in (-magnitude, magnitude)
) | frozenset(
    value
    for magnitude in range(
        UNIQUE_TEXT_GEOMETRY_OUTER_BUCKET_MILLIPOINTS,
        UNIQUE_TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS + 1,
        UNIQUE_TEXT_GEOMETRY_OUTER_BUCKET_MILLIPOINTS,
    )
    for value in (-magnitude, magnitude)
) | {
    -UNIQUE_TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS,
    UNIQUE_TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS,
}
UNIQUE_TEXT_GEOMETRY_POLICY = {
    "content_retained": False,
    "coordinates": "pdf_points_y_down",
    "delta_direction": "rxls_minus_libreoffice",
    "diagnostic_only": True,
    "exact_delta_absolute_limit_millipoints": (
        MAX_UNIQUE_TEXT_GEOMETRY_DELTA_MILLIPOINTS
    ),
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
    "max_geometry_pages_per_report": (
        MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES
    ),
    "max_histogram_buckets_per_report": (
        MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS
    ),
    "max_items_per_side_per_page": MAX_UNIQUE_TEXT_GEOMETRY_ITEMS,
    "matching": "exact_normalized_token_tuple_unique_on_both_sides",
    "rounding": "nearest_millipoint_half_away_from_zero_exact_rational",
    "shard_budget": "equal_floor_partition_by_declared_shard_count",
    "units": "millipoints",
}

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


def _load_metric_cohort_contract() -> Any:
    """Load the producer's canonical bounded cohort reducer once."""
    name = "rxls_render_parity_repeatability_metric_contract"
    existing = sys.modules.get(name)
    if existing is not None:
        return existing
    spec = importlib.util.spec_from_file_location(name, HARNESS_PATH)
    if spec is None or spec.loader is None:
        raise MalformedReport("metric_cohorts_contract")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    try:
        spec.loader.exec_module(module)
    except (ImportError, OSError, RuntimeError) as error:
        sys.modules.pop(name, None)
        raise MalformedReport("metric_cohorts_contract") from error
    return module


def _recompute_metric_cohorts(
    rows: Sequence[dict[str, Any]],
) -> dict[str, object]:
    """Recompute producer-owned cohort summaries without comparing oracle scores."""
    contract = _load_metric_cohort_contract()
    try:
        return contract.metric_cohorts(rows)
    except (
        KeyError,
        TypeError,
        ValueError,
        contract.HarnessError,
    ) as error:
        raise MalformedReport("summary_metric_cohorts") from error


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


def _reject_json_number(_value: str) -> object:
    raise MalformedReport("report_nonintegral_number")


def _parse_json_integer(token: str) -> int:
    digits = token[1:] if token.startswith("-") else token
    if len(digits) > MAX_JSON_INTEGER_DIGITS:
        raise MalformedReport("report_integer_limit")
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
                raise MalformedReport("report_json_complexity")
            closers.append("]" if character == "[" else "}")
            if len(closers) > MAX_JSON_DEPTH:
                raise MalformedReport("report_json_depth")
        elif character in "]}":
            if not closers or closers.pop() != character:
                raise MalformedReport("report_invalid_json")
        elif character == ",":
            structural_nodes += 1
            if structural_nodes > MAX_JSON_NODES:
                raise MalformedReport("report_json_complexity")
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
                raise MalformedReport("report_integer_limit")
            if index < len(text) and text[index] in ".eE":
                raise MalformedReport("report_nonintegral_number")
            continue
        index += 1
    if closers:
        raise MalformedReport("report_invalid_json")


def _strict_json_loads(payload: bytes) -> object:
    try:
        text = payload.decode("utf-8")
        _preflight_json_text(text)
        return json.loads(
            text,
            object_pairs_hook=_strict_object,
            parse_constant=_reject_json_constant,
            parse_float=_reject_json_number,
            parse_int=_parse_json_integer,
        )
    except MalformedReport:
        raise
    except (RecursionError, ValueError, UnicodeDecodeError) as error:
        raise MalformedReport("report_invalid_json") from error


def _integer(value: object, code: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise MalformedReport(code)
    return value


def _bounded_signed_integer(value: object, code: str, *, maximum: int) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or abs(value) > maximum
    ):
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
    descriptor = -1
    try:
        metadata = path.lstat()
        if (
            path.is_symlink()
            or not stat.S_ISREG(metadata.st_mode)
            or not 0 < metadata.st_size <= byte_limit
        ):
            raise MalformedReport("report_bytes_limit")
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
                raise MalformedReport("report_unreadable")
            payload = source.read(byte_limit + 1)
    except OSError as error:
        raise MalformedReport("report_unreadable") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    if len(payload) != metadata.st_size or len(payload) > byte_limit:
        raise MalformedReport("report_bytes_limit")
    document = _strict_json_loads(payload)
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
    if not type_exact_equal(rxls_command.get("binary_identity"), identity):
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


def _unique_text_geometry_bucket(delta_millipoints: int) -> int:
    magnitude = abs(delta_millipoints)
    if magnitude <= UNIQUE_TEXT_GEOMETRY_EXACT_LIMIT_MILLIPOINTS:
        return delta_millipoints
    if magnitude <= UNIQUE_TEXT_GEOMETRY_MIDDLE_LIMIT_MILLIPOINTS:
        width = UNIQUE_TEXT_GEOMETRY_MIDDLE_BUCKET_MILLIPOINTS
        bucket = max(width, (magnitude + width // 2) // width * width)
    elif magnitude <= UNIQUE_TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS:
        width = UNIQUE_TEXT_GEOMETRY_OUTER_BUCKET_MILLIPOINTS
        bucket = (magnitude + width // 2) // width * width
    else:
        bucket = UNIQUE_TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS
    return -bucket if delta_millipoints < 0 else bucket


def _unique_text_geometry_bucket_interval(
    bucket_millipoints: int,
) -> tuple[int, int]:
    magnitude = abs(bucket_millipoints)
    if magnitude <= UNIQUE_TEXT_GEOMETRY_EXACT_LIMIT_MILLIPOINTS:
        lower = magnitude
        upper = magnitude
    elif magnitude <= UNIQUE_TEXT_GEOMETRY_MIDDLE_LIMIT_MILLIPOINTS:
        width = UNIQUE_TEXT_GEOMETRY_MIDDLE_BUCKET_MILLIPOINTS
        lower = (
            UNIQUE_TEXT_GEOMETRY_EXACT_LIMIT_MILLIPOINTS + 1
            if magnitude == width
            else magnitude - width // 2
        )
        upper = min(
            UNIQUE_TEXT_GEOMETRY_MIDDLE_LIMIT_MILLIPOINTS,
            magnitude + width // 2 - 1,
        )
    elif magnitude <= UNIQUE_TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS:
        width = UNIQUE_TEXT_GEOMETRY_OUTER_BUCKET_MILLIPOINTS
        lower = max(
            UNIQUE_TEXT_GEOMETRY_MIDDLE_LIMIT_MILLIPOINTS + 1,
            magnitude - width // 2,
        )
        upper = min(
            UNIQUE_TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS,
            magnitude + width // 2 - 1,
        )
    elif magnitude == UNIQUE_TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS:
        lower = UNIQUE_TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS + 1
        upper = MAX_UNIQUE_TEXT_GEOMETRY_DELTA_MILLIPOINTS
    else:
        raise MalformedReport("page_unique_text_geometry")
    return (-upper, -lower) if bucket_millipoints < 0 else (lower, upper)


def _unique_text_geometry_sum_bounds(
    histogram: dict[int, int],
    minimum: int,
    maximum: int,
    code: str,
) -> tuple[int, int]:
    minimum_bucket = _unique_text_geometry_bucket(minimum)
    maximum_bucket = _unique_text_geometry_bucket(maximum)
    if (
        minimum < maximum
        and minimum_bucket == maximum_bucket
        and histogram[minimum_bucket] < 2
    ):
        raise MalformedReport(code)
    lower_total = 0
    upper_total = 0
    effective: dict[int, tuple[int, int]] = {}
    for bucket, count in histogram.items():
        bucket_lower, bucket_upper = (
            _unique_text_geometry_bucket_interval(bucket)
        )
        lower = max(bucket_lower, minimum)
        upper = min(bucket_upper, maximum)
        if lower > upper:
            raise MalformedReport(code)
        effective[bucket] = (lower, upper)
        lower_total += lower * count
        upper_total += upper * count
    lower_total += maximum - effective[maximum_bucket][0]
    upper_total -= effective[minimum_bucket][1] - minimum
    if lower_total > upper_total:
        raise MalformedReport(code)
    return lower_total, upper_total


def _validate_unique_text_geometry(value: object) -> dict[str, Any]:
    code = "page_unique_text_geometry"
    if (
        not isinstance(value, dict)
        or set(value) != UNIQUE_TEXT_GEOMETRY_PAGE_KEYS
    ):
        raise MalformedReport(code)
    rxls_unique = _integer(
        value["rxls_unique_items"],
        code,
    )
    libreoffice_unique = _integer(
        value["libreoffice_unique_items"],
        code,
    )
    matched = _integer(value["matched_items"], code)
    if (
        rxls_unique > MAX_UNIQUE_TEXT_GEOMETRY_ITEMS
        or libreoffice_unique > MAX_UNIQUE_TEXT_GEOMETRY_ITEMS
        or matched > MAX_UNIQUE_TEXT_GEOMETRY_ITEMS
        or matched > min(rxls_unique, libreoffice_unique)
    ):
        raise MalformedReport(code)

    raw_histograms = value["delta_histograms_millipoints"]
    raw_summaries = value["exact_delta_summaries_millipoints"]
    if (
        not isinstance(raw_histograms, dict)
        or set(raw_histograms) != set(UNIQUE_TEXT_GEOMETRY_AXES)
        or not isinstance(raw_summaries, dict)
        or set(raw_summaries) != set(UNIQUE_TEXT_GEOMETRY_AXES)
    ):
        raise MalformedReport(code)

    exact_sums: dict[str, int] = {}
    for axis in UNIQUE_TEXT_GEOMETRY_AXES:
        raw_histogram = raw_histograms[axis]
        if (
            not isinstance(raw_histogram, list)
            or len(raw_histogram)
            > min(matched, MAX_UNIQUE_TEXT_GEOMETRY_BUCKETS)
        ):
            raise MalformedReport(code)
        previous_delta: int | None = None
        population = 0
        histogram: dict[int, int] = {}
        for raw_bucket in raw_histogram:
            if (
                not isinstance(raw_bucket, dict)
                or set(raw_bucket) != UNIQUE_TEXT_GEOMETRY_BUCKET_KEYS
            ):
                raise MalformedReport(code)
            delta = raw_bucket["delta_millipoints"]
            if (
                isinstance(delta, bool)
                or not isinstance(delta, int)
                or delta not in UNIQUE_TEXT_GEOMETRY_ALLOWED_BUCKETS
                or (previous_delta is not None and delta <= previous_delta)
            ):
                raise MalformedReport(code)
            count = _integer(raw_bucket["count"], code, minimum=1)
            if count > matched:
                raise MalformedReport(code)
            population += count
            if population > matched:
                raise MalformedReport(code)
            histogram[delta] = count
            previous_delta = delta
        if population != matched:
            raise MalformedReport(code)

        raw_summary = raw_summaries[axis]
        if (
            not isinstance(raw_summary, dict)
            or set(raw_summary) != UNIQUE_TEXT_GEOMETRY_EXACT_SUMMARY_KEYS
        ):
            raise MalformedReport(code)
        if _integer(raw_summary["count"], code) != matched:
            raise MalformedReport(code)
        total = _bounded_signed_integer(
            raw_summary["sum_delta_millipoints"],
            code,
            maximum=(
                matched * MAX_UNIQUE_TEXT_GEOMETRY_DELTA_MILLIPOINTS
            ),
        )
        negative_overflow = _integer(
            raw_summary["negative_overflow_items"], code
        )
        positive_overflow = _integer(
            raw_summary["positive_overflow_items"], code
        )
        if (
            negative_overflow > matched
            or positive_overflow > matched
            or negative_overflow + positive_overflow > matched
            or histogram.get(
                -UNIQUE_TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS, 0
            )
            != negative_overflow
            or histogram.get(
                UNIQUE_TEXT_GEOMETRY_OVERFLOW_MILLIPOINTS, 0
            )
            != positive_overflow
        ):
            raise MalformedReport(code)

        raw_minimum = raw_summary["min_delta_millipoints"]
        raw_maximum = raw_summary["max_delta_millipoints"]
        if matched == 0:
            if (
                raw_minimum is not None
                or raw_maximum is not None
                or total != 0
                or negative_overflow != 0
                or positive_overflow != 0
            ):
                raise MalformedReport(code)
            exact_sums[axis] = total
            continue

        minimum = _bounded_signed_integer(
            raw_minimum,
            code,
            maximum=MAX_UNIQUE_TEXT_GEOMETRY_DELTA_MILLIPOINTS,
        )
        maximum = _bounded_signed_integer(
            raw_maximum,
            code,
            maximum=MAX_UNIQUE_TEXT_GEOMETRY_DELTA_MILLIPOINTS,
        )
        if (
            minimum > maximum
            or (matched == 1 and not minimum == maximum == total)
            or (negative_overflow > 0)
            != (minimum < -UNIQUE_TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS)
            or (positive_overflow > 0)
            != (maximum > UNIQUE_TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS)
            or _unique_text_geometry_bucket(minimum)
            != raw_histogram[0]["delta_millipoints"]
            or _unique_text_geometry_bucket(maximum)
            != raw_histogram[-1]["delta_millipoints"]
        ):
            raise MalformedReport(code)
        sum_lower, sum_upper = _unique_text_geometry_sum_bounds(
            histogram, minimum, maximum, code
        )
        if not sum_lower <= total <= sum_upper:
            raise MalformedReport(code)
        exact_sums[axis] = total
    if (
        abs(
            exact_sums["width"]
            - (exact_sums["x_max"] - exact_sums["x_min"])
        )
        > matched
        or abs(
            2 * exact_sums["center_x"]
            - exact_sums["x_min"]
            - exact_sums["x_max"]
        )
        > matched
        or abs(
            exact_sums["height"]
            - (exact_sums["y_max"] - exact_sums["y_min"])
        )
        > matched
        or abs(
            2 * exact_sums["center_y"]
            - exact_sums["y_min"]
            - exact_sums["y_max"]
        )
        > matched
    ):
        raise MalformedReport(code)
    return value


def _validate_page(page: object) -> dict[str, Any]:
    if not isinstance(page, dict):
        raise MalformedReport("page_not_object")
    for key in (
        "source_sheet_index",
        "source_pdf_page_index",
        "oracle_output_page_index",
    ):
        _integer(page.get(key), "page_mapping")
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
    if any(key not in page for key in UNIQUE_TEXT_GEOMETRY_METRICS):
        raise MalformedReport("page_unique_text_geometry_pair")
    for key, prefix in (
        ("text_box_unique_geometry", "text_box"),
        ("text_line_box_unique_geometry", "text_line_box"),
    ):
        geometry = _validate_unique_text_geometry(page[key])
        rxls_items = _integer(
            page.get(f"{prefix}_rxls_items"),
            "page_unique_text_geometry",
        )
        libreoffice_items = _integer(
            page.get(f"{prefix}_libreoffice_items"),
            "page_unique_text_geometry",
        )
        paired_items = _integer(
            page.get(f"{prefix}_matched_items"),
            "page_unique_text_geometry",
        )
        if (
            rxls_items > MAX_UNIQUE_TEXT_GEOMETRY_ITEMS
            or libreoffice_items > MAX_UNIQUE_TEXT_GEOMETRY_ITEMS
            or paired_items > MAX_UNIQUE_TEXT_GEOMETRY_ITEMS
            or geometry["rxls_unique_items"] > rxls_items
            or geometry["libreoffice_unique_items"] > libreoffice_items
            or geometry["matched_items"] > paired_items
        ):
            raise MalformedReport("page_unique_text_geometry")
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


def _validate_comparable_row(row: dict[str, Any]) -> tuple[int, int]:
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
    page_mapping: list[tuple[int, int, int]] = []
    histogram_buckets = 0
    for raw_page in pages:
        page = _validate_page(raw_page)
        histogram_buckets += sum(
            len(
                page[key]["delta_histograms_millipoints"][axis]
            )
            for key in UNIQUE_TEXT_GEOMETRY_METRICS
            for axis in UNIQUE_TEXT_GEOMETRY_AXES
        )
        page_mapping.append(
            (
                int(page["source_sheet_index"]),
                int(page["source_pdf_page_index"]),
                int(page["oracle_output_page_index"]),
            )
        )
    scene_mapping = []
    for scene in scenes:
        if not isinstance(scene, dict):
            raise MalformedReport("scene_evidence")
        scene_mapping.append(
            (
                _integer(scene.get("source_sheet_index"), "page_mapping"),
                _integer(scene.get("source_pdf_page_index"), "page_mapping"),
                _integer(scene.get("oracle_output_page_index"), "page_mapping"),
            )
        )
    if (
        scene_mapping != page_mapping
        or [row[2] for row in page_mapping] != list(range(len(page_mapping)))
    ):
        raise MalformedReport("page_mapping")
    seen_sheets: set[int] = set()
    current_sheet: int | None = None
    local_index = 0
    for source_sheet, source_pdf_page, _ in page_mapping:
        if source_sheet != current_sheet:
            if source_sheet in seen_sheets or (
                current_sheet is not None and source_sheet <= current_sheet
            ):
                raise MalformedReport("page_mapping")
            seen_sheets.add(source_sheet)
            current_sheet = source_sheet
            local_index = 0
        if source_pdf_page != local_index:
            raise MalformedReport("page_mapping")
        local_index += 1
    _validate_aggregate(row.get("metrics"), len(pages))
    return len(pages), histogram_buckets


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
    metric_policy = configuration.get("metric_policy")
    if (
        not isinstance(metric_policy, dict)
        or not type_exact_equal(
            metric_policy.get("unique_text_geometry"),
            UNIQUE_TEXT_GEOMETRY_POLICY,
        )
    ):
        raise MalformedReport("metric_policy_unique_text_geometry")

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
        "authored_print",
        "by_classification",
        "by_status",
        "files",
        "input_bytes_considered",
        "metric_cohorts",
    }:
        raise MalformedReport("summary_shape")
    if (
        configuration.get("print_mode") != "single-page-sheets"
        or summary.get("authored_print") is not None
    ):
        raise MalformedReport("summary_authored_print")
    if _integer(summary.get("files"), "summary_file_count") != selected:
        raise MalformedReport("summary_file_count")
    _integer(summary.get("input_bytes_considered"), "summary_input_bytes")
    if not isinstance(summary.get("metric_cohorts"), dict):
        raise MalformedReport("summary_metric_cohorts")

    files: dict[str, dict[str, Any]] = {}
    statuses: dict[str, int] = {}
    classifications: dict[str, int] = {}
    page_count = 0
    geometry_histogram_buckets = 0
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
        if (
            status not in REPORT_STATUSES
            or CLASSIFICATION_RE.fullmatch(classification) is None
        ):
            raise MalformedReport("file_status_or_classification")
        if status in {"compared", "different"}:
            row_pages, row_buckets = _validate_comparable_row(row)
            page_count += row_pages
            geometry_histogram_buckets += row_buckets
            if (
                page_count > MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES
                or geometry_histogram_buckets
                > MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS
            ):
                raise MalformedReport(
                    "unique_text_geometry_report_limit"
                )
        elif "metrics" in row or "pages" in row:
            raise MalformedReport("incomparable_row_metrics")
        files[digest] = row
        statuses[status] = statuses.get(status, 0) + 1
        classifications[classification] = classifications.get(classification, 0) + 1

    if not type_exact_equal(
        summary.get("by_status"),
        dict(sorted(statuses.items())),
    ):
        raise MalformedReport("summary_status_counts")
    if not type_exact_equal(
        summary.get("by_classification"),
        dict(sorted(classifications.items())),
    ):
        raise MalformedReport("summary_classification_counts")
    if not type_exact_equal(
        summary.get("metric_cohorts"),
        _recompute_metric_cohorts(rows),
    ):
        raise MalformedReport("summary_metric_cohorts")
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
        and key not in UNIQUE_TEXT_GEOMETRY_METRICS
    }


def _unique_text_geometry_subset(metrics: dict[str, Any]) -> dict[str, Any]:
    """Return validated path-neutral geometry that must repeat exactly."""
    return {key: metrics[key] for key in UNIQUE_TEXT_GEOMETRY_METRICS}


def _distribution(values: list[int]) -> dict[str, Any]:
    ordered = sorted(values)
    return {
        "absolute_deltas_ppm": ordered,
        "count": len(ordered),
        "max_absolute_delta_ppm": max(ordered) if ordered else None,
    }


def _baseline_configuration_sha256(configuration: dict[str, Any]) -> str:
    identity = {
        "dpi": configuration.get("dpi"),
        "font_pack": configuration.get("font_pack"),
        "locale": configuration.get("locale"),
        "measurement_toolchain": configuration.get("measurement_toolchain"),
        "metric_policy": configuration.get("metric_policy"),
        "oracle_lock": configuration.get("oracle_lock"),
        "renderer_binary": configuration.get("renderer_binary"),
    }
    return canonical_sha256(identity)


def _baseline_input_identity(files: dict[str, dict[str, Any]]) -> tuple[str, int]:
    identities = []
    for digest, row in files.items():
        features = row.get("features", [])
        format_name = row.get("format")
        rights_tier = row.get("rights_tier")
        if (
            not isinstance(features, list)
            or not all(isinstance(feature, str) and feature for feature in features)
            or features != sorted(set(features))
            or not isinstance(format_name, str)
            or not format_name
            or rights_tier not in {None, "S", "U", "Q"}
        ):
            raise MalformedReport("baseline_input_identity")
        identities.append(
            {
                "features": features,
                "format": format_name,
                "rights_tier": rights_tier,
                "sha256": digest,
            }
        )
    identities.sort(
        key=lambda row: (
            row["sha256"],
            row["format"],
            row["rights_tier"] or "",
            row["features"],
        )
    )
    return canonical_sha256(identities), len(identities)


def _identity_result(
    baseline: ValidatedReport, candidate: ValidatedReport
) -> dict[str, Any]:
    left = baseline.loaded.document
    right = candidate.loaded.document
    left_inputs = sorted(baseline.files)
    right_inputs = sorted(candidate.files)
    left_configuration_sha256 = canonical_sha256(left["configuration"])
    right_configuration_sha256 = canonical_sha256(right["configuration"])
    left_preflight_sha256 = canonical_sha256(left["preflight"])
    right_preflight_sha256 = canonical_sha256(right["preflight"])
    left_input_set_sha256 = canonical_sha256(left_inputs)
    right_input_set_sha256 = canonical_sha256(right_inputs)
    left_baseline_configuration_sha256 = (
        _baseline_configuration_sha256(left["configuration"])
    )
    right_baseline_configuration_sha256 = (
        _baseline_configuration_sha256(right["configuration"])
    )
    left_baseline_input_sha256, left_baseline_input_count = (
        _baseline_input_identity(baseline.files)
    )
    right_baseline_input_sha256, right_baseline_input_count = (
        _baseline_input_identity(candidate.files)
    )
    return {
        "baseline_contract": {
            "configuration": {
                "baseline_sha256": left_baseline_configuration_sha256,
                "candidate_sha256": right_baseline_configuration_sha256,
                "equal": (
                    left_baseline_configuration_sha256
                    == right_baseline_configuration_sha256
                ),
            },
            "input_set": {
                "baseline_count": left_baseline_input_count,
                "baseline_sha256": left_baseline_input_sha256,
                "candidate_count": right_baseline_input_count,
                "candidate_sha256": right_baseline_input_sha256,
                "equal": (
                    left_baseline_input_count == right_baseline_input_count
                    and left_baseline_input_sha256
                    == right_baseline_input_sha256
                ),
            },
        },
        "configuration": {
            "baseline_sha256": left_configuration_sha256,
            "candidate_sha256": right_configuration_sha256,
            "equal": left_configuration_sha256 == right_configuration_sha256,
        },
        "input_set": {
            "baseline_count": len(left_inputs),
            "baseline_sha256": left_input_set_sha256,
            "candidate_count": len(right_inputs),
            "candidate_sha256": right_input_set_sha256,
            "equal": (
                len(left_inputs) == len(right_inputs)
                and left_input_set_sha256 == right_input_set_sha256
            ),
        },
        "preflight": {
            "baseline_sha256": left_preflight_sha256,
            "candidate_sha256": right_preflight_sha256,
            "equal": left_preflight_sha256 == right_preflight_sha256,
        },
        "renderer_binary": {
            "baseline": left["configuration"]["renderer_binary"],
            "candidate": right["configuration"]["renderer_binary"],
            "equal": type_exact_equal(
                left["configuration"]["renderer_binary"],
                right["configuration"]["renderer_binary"],
            ),
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
    if not type_exact_equal(
        baseline.loaded.document["summary"]["input_bytes_considered"],
        candidate.loaded.document["summary"]["input_bytes_considered"],
    ):
        failures.add("summary_input_bytes_mismatch")

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
                if not type_exact_equal(left.get(key), right.get(key)):
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
            if not type_exact_equal(left_evidence, right_evidence):
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
            if not type_exact_equal(
                _semantic_subset(left_metrics),
                _semantic_subset(right_metrics),
            ):
                failures.add("semantic_counts_mismatch")
            if not type_exact_equal(
                _metric_subset(left_metrics, AGGREGATE_DIMENSION_KEYS),
                _metric_subset(right_metrics, AGGREGATE_DIMENSION_KEYS),
            ):
                failures.add("page_dimensions_mismatch")
            if not type_exact_equal(
                _metric_subset(left_metrics, RENDERER_METRIC_KEYS),
                _metric_subset(right_metrics, RENDERER_METRIC_KEYS),
            ):
                failures.add("renderer_metric_evidence_mismatch")
            if not type_exact_equal(
                _non_oracle_subset(left_metrics),
                _non_oracle_subset(right_metrics),
            ):
                failures.add("non_oracle_metric_evidence_mismatch")
            for key in DRIFT_METRICS:
                deltas[key].append(abs(int(left_metrics[key]) - int(right_metrics[key])))

            for left_page, right_page in zip(left_pages, right_pages):
                if set(left_page) != set(right_page):
                    failures.add("page_metric_shape_mismatch")
                if any(
                    left_page.get(key) != right_page.get(key)
                    for key in (
                        "source_sheet_index",
                        "source_pdf_page_index",
                        "oracle_output_page_index",
                    )
                ):
                    failures.add("page_mapping_mismatch")
                if not type_exact_equal(
                    _semantic_subset(left_page),
                    _semantic_subset(right_page),
                ):
                    failures.add("semantic_counts_mismatch")
                if not type_exact_equal(
                    _metric_subset(left_page, PAGE_DIMENSION_KEYS),
                    _metric_subset(right_page, PAGE_DIMENSION_KEYS),
                ):
                    failures.add("page_dimensions_mismatch")
                if not type_exact_equal(
                    _metric_subset(left_page, RENDERER_METRIC_KEYS),
                    _metric_subset(right_page, RENDERER_METRIC_KEYS),
                ):
                    failures.add("renderer_metric_evidence_mismatch")
                if not type_exact_equal(
                    _unique_text_geometry_subset(left_page),
                    _unique_text_geometry_subset(right_page),
                ):
                    failures.add("unique_text_geometry_evidence_mismatch")
                if not type_exact_equal(
                    _non_oracle_subset(left_page),
                    _non_oracle_subset(right_page),
                ):
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
            "unique_text_geometry": (
                "schema_validated_exact_same_sha_diagnostic_non_scoring"
            ),
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
