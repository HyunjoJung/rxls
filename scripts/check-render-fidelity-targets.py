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
from fractions import Fraction
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
from typing import Any, Sequence

try:
    from render_parity_geometry_gate import (
        GeometryContractError,
        validate_report_geometry,
    )
except ModuleNotFoundError:  # Imported as ``scripts.*`` by unit tests.
    from scripts.render_parity_geometry_gate import (
        GeometryContractError,
        validate_report_geometry,
    )
try:
    from strict_json_contract import type_exact_equal
except ModuleNotFoundError:  # Imported as ``scripts.*`` by unit tests.
    from scripts.strict_json_contract import type_exact_equal


EVIDENCE_SCHEMA = "rxls.libreoffice-render-parity.v1"
OUTPUT_SCHEMA = "rxls.render-fidelity-targets.v1"
MANIFEST_BINDING_SCHEMA = "rxls.render-parity-manifest-binding.v1"
METRIC_CONTRACT_SCHEMA = "rxls.render-parity-metrics.v2"
CONTAINER_EXECUTION_SCHEMA = "rxls.render-oracle-container-execution.v3"
CONTAINER_IDENTITY_SCHEMA = "rxls.render-oracle-container-identity.v2"
CONTAINER_LIBREOFFICE_ARTIFACT_SHA256 = (
    "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5"
)
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
CLASSIFICATION_RE = re.compile(r"[a-z][a-z0-9_]{0,95}\Z")
MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_JSON_INTEGER_DIGITS = 128
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
HARD_FEATURE_COHORTS = {
    "chart": frozenset({"chart"}),
    "conditional_format": frozenset({"conditional-format"}),
    "image_drawing": frozenset({"image-drawing"}),
    "print_settings": frozenset({"print-settings"}),
    "rtl": frozenset({"right-to-left-layout", "rtl-text"}),
    "sparkline": frozenset({"sparkline"}),
    "wrapped_text": frozenset({"wrapped-text"}),
}

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
MIN_HARD_FEATURE_WORKBOOKS = 1
PDF_POINT_DELTA_KEYS = frozenset(
    {
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
    }
)
PDF_DIRECT_POINT_DELTA_KEYS = frozenset(
    {
        "crop_box_height",
        "crop_box_width",
        "media_box_height",
        "media_box_width",
    }
)
PDF_XHTML_CROSSCHECK_DELTA_KEYS = (
    PDF_POINT_DELTA_KEYS - PDF_DIRECT_POINT_DELTA_KEYS
)
PDF_XHTML_CROSSCHECK_MAX_POINTS = Fraction(1, 1000)
PDF_XHTML_CROSSCHECK_MAX_MICROPOINTS = 1_000
UNIQUE_TEXT_GEOMETRY_POLICY = {
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
            "nearest_width_multiple_half_away_from_zero_with_nonzero_sign_preserved"
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


class GateError(RuntimeError):
    """The input evidence is malformed or violates the gate contract."""


class _StrictJSONError(ValueError):
    pass


def _reject_duplicate_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise GateError("duplicate_json_key")
        result[key] = value
    return result


def _reject_json_constant(_value: str) -> object:
    raise _StrictJSONError("non_finite_number")


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


def _stat_signature(value: os.stat_result) -> tuple[int, ...]:
    return (
        value.st_dev,
        value.st_ino,
        value.st_mode,
        value.st_size,
        value.st_mtime_ns,
        value.st_ctime_ns,
    )


def _read_bounded_regular_file(path: Path, maximum: int) -> bytes:
    descriptor: int | None = None
    try:
        descriptor = os.open(
            path,
            os.O_RDONLY
            | getattr(os, "O_CLOEXEC", 0)
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_NONBLOCK", 0),
        )
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            raise GateError("report_unreadable")
        remaining = maximum + 1
        chunks: list[bytes] = []
        while remaining:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        after = os.fstat(descriptor)
        current = os.stat(path, follow_symlinks=False)
    except GateError:
        raise
    except OSError as error:
        raise GateError("report_unreadable") from error
    finally:
        if descriptor is not None:
            try:
                os.close(descriptor)
            except OSError:
                pass
    if (
        not stat.S_ISREG(current.st_mode)
        or _stat_signature(before) != _stat_signature(after)
        or _stat_signature(after) != _stat_signature(current)
    ):
        raise GateError("report_unreadable")
    payload = b"".join(chunks)
    if not payload or len(payload) > maximum:
        raise GateError("report_size_limit")
    if len(payload) != after.st_size:
        raise GateError("report_unreadable")
    return payload


def _read_report(path: Path) -> tuple[dict[str, Any], str, int]:
    payload = _read_bounded_regular_file(path, MAX_REPORT_BYTES)
    size = len(payload)
    try:
        text = payload.decode("utf-8")
        _preflight_json_text(text)
        value = json.loads(
            text,
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_json_constant,
            parse_float=_reject_json_number,
            parse_int=_parse_json_integer,
        )
    except GateError:
        raise
    except (
        UnicodeDecodeError,
        json.JSONDecodeError,
        _StrictJSONError,
        RecursionError,
        ValueError,
    ) as error:
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


def _validate_complete_discovery(value: object, file_count: int) -> None:
    required = {
        "candidate_count",
        "pre_shard_selected_count",
        "selected_count",
        "shard_candidate_count",
        "shard_count",
        "shard_index",
        "truncated",
    }
    if not isinstance(value, dict) or set(value) != required:
        raise GateError("discovery_shape")
    shard_count = _integer(
        value.get("shard_count"),
        "campaign_incomplete",
        minimum=1,
        maximum=256,
    )
    shard_index = _integer(
        value.get("shard_index"),
        "campaign_incomplete",
        maximum=255,
    )
    if (
        shard_count != 1
        or shard_index != 0
        or value.get("truncated") is not False
    ):
        raise GateError("campaign_incomplete")
    selected = _integer(
        value.get("selected_count"),
        "campaign_coverage",
        minimum=1,
        maximum=MAX_FILES,
    )
    pre_shard = _integer(
        value.get("pre_shard_selected_count"),
        "campaign_coverage",
        minimum=1,
        maximum=MAX_FILES,
    )
    shard_candidates = _integer(
        value.get("shard_candidate_count"),
        "campaign_coverage",
        minimum=1,
        maximum=MAX_FILES,
    )
    candidates = _integer(
        value.get("candidate_count"),
        "campaign_coverage",
        minimum=1,
        maximum=MAX_FILES,
    )
    if (
        selected != file_count
        or pre_shard != selected
        or shard_candidates != selected
        or candidates < selected
    ):
        raise GateError("campaign_coverage")


def _ppm(value: object, code: str) -> int:
    return _integer(value, code, maximum=1_000_000)


def _point(value: object, code: str, *, positive: bool) -> Fraction:
    if not isinstance(value, str) or re.fullmatch(
        r"-?[0-9]+/[1-9][0-9]*", value
    ) is None:
        raise GateError(code)
    result = Fraction(value)
    if positive and not 0 < result <= 1_000_000:
        raise GateError(code)
    return result


def _point_side(value: object, code: str) -> dict[str, tuple[Fraction, Fraction]]:
    row = _exact_object(
        value,
        {"crop_box", "media_box", "page_size"},
        code,
    )
    result: dict[str, tuple[Fraction, Fraction]] = {}
    for name in ("page_size", "media_box", "crop_box"):
        dimensions = _exact_object(
            row[name],
            {"height_points", "width_points"},
            code,
        )
        result[name] = (
            _point(dimensions["width_points"], code, positive=True),
            _point(dimensions["height_points"], code, positive=True),
        )
    return result


def _page_point_geometry(
    page: dict[str, Any],
) -> tuple[int, int, bool, bool]:
    """Return exact box and bounded XHTML crosscheck results independently."""

    evidence = _exact_object(
        page.get("pdf_point_geometry"),
        {"deltas_points", "libreoffice", "rxls", "xhtml"},
        "page_point_geometry",
    )
    rxls = _point_side(evidence["rxls"], "page_point_geometry")
    libreoffice = _point_side(
        evidence["libreoffice"], "page_point_geometry"
    )
    xhtml = _exact_object(
        evidence["xhtml"], {"libreoffice", "rxls"}, "page_point_geometry"
    )
    xhtml_values: dict[str, tuple[Fraction, Fraction]] = {}
    for side in ("rxls", "libreoffice"):
        dimensions = _exact_object(
            xhtml[side],
            {"height_points", "width_points"},
            "page_point_geometry",
        )
        xhtml_values[side] = (
            _point(
                dimensions["width_points"], "page_point_geometry", positive=True
            ),
            _point(
                dimensions["height_points"], "page_point_geometry", positive=True
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
                xhtml_values[side][offset] - geometry["page_size"][offset]
            )
    for offset, axis in enumerate(("width", "height")):
        expected[f"xhtml_{axis}"] = (
            xhtml_values["rxls"][offset] - xhtml_values["libreoffice"][offset]
        )
    deltas = _exact_object(
        evidence["deltas_points"],
        set(PDF_POINT_DELTA_KEYS),
        "page_point_geometry",
    )
    parsed = {
        key: _point(value, "page_point_geometry", positive=False)
        for key, value in deltas.items()
    }
    if parsed != expected:
        raise GateError("page_point_geometry_delta")
    max_direct_delta = max(
        (abs(parsed[key]) for key in PDF_DIRECT_POINT_DELTA_KEYS),
        default=Fraction(),
    )
    max_crosscheck_delta = max(
        (abs(parsed[key]) for key in PDF_XHTML_CROSSCHECK_DELTA_KEYS),
        default=Fraction(),
    )
    millipoints = (
        max_direct_delta.numerator * 1000
        + max_direct_delta.denominator
        - 1
    ) // max_direct_delta.denominator
    crosscheck_micropoints = (
        max_crosscheck_delta.numerator * 1_000_000
        + max_crosscheck_delta.denominator
        - 1
    ) // max_crosscheck_delta.denominator
    return (
        millipoints,
        crosscheck_micropoints,
        max_direct_delta == 0,
        max_crosscheck_delta <= PDF_XHTML_CROSSCHECK_MAX_POINTS,
    )


def _mapping_tuple(row: object) -> tuple[int, int, int]:
    if not isinstance(row, dict):
        raise GateError("page_mapping")
    return (
        _integer(row.get("source_sheet_index"), "page_mapping"),
        _integer(row.get("source_pdf_page_index"), "page_mapping"),
        _integer(row.get("oracle_output_page_index"), "page_mapping"),
    )


def _validate_page_mapping(
    pages: Sequence[object], scenes: Sequence[object]
) -> bool:
    page_mapping = [_mapping_tuple(row) for row in pages]
    scene_mapping = [_mapping_tuple(row) for row in scenes]
    if scene_mapping != page_mapping or [
        row[2] for row in page_mapping
    ] != list(range(len(page_mapping))):
        return False
    seen_sheets: set[int] = set()
    current_sheet: int | None = None
    next_local = 0
    for source_sheet, source_pdf_page, _ in page_mapping:
        if source_sheet != current_sheet:
            if source_sheet in seen_sheets:
                return False
            if current_sheet is not None and source_sheet <= current_sheet:
                return False
            seen_sheets.add(source_sheet)
            current_sheet = source_sheet
            next_local = 0
        if source_pdf_page != next_local:
            return False
        next_local += 1
    return True


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


def _canonical_json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def _mapping_binding(
    rows: Sequence[dict[str, Any]],
    *,
    manifest_sha256: str,
) -> dict[str, object]:
    input_sha256: list[str] = []
    feature_mapping: list[dict[str, object]] = []
    seen: set[str] = set()
    for row in rows:
        digest = _sha256(row.get("sha256"), "manifest_binding")
        format_name = row.get("format")
        features = row.get("features")
        if (
            digest in seen
            or format_name not in ORACLE_FORMATS
            or not isinstance(features, list)
            or features != sorted(set(features))
            or any(not isinstance(feature, str) or not feature for feature in features)
        ):
            raise GateError("manifest_binding")
        seen.add(digest)
        input_sha256.append(digest)
        feature_mapping.append(
            {
                "features": list(features),
                "format": format_name,
                "sha256": digest,
            }
        )
    input_sha256.sort()
    feature_mapping.sort(
        key=lambda row: (
            str(row["sha256"]),
            str(row["format"]),
            tuple(row["features"]),
        )
    )
    return {
        "feature_map_sha256": hashlib.sha256(
            _canonical_json_bytes(feature_mapping)
        ).hexdigest(),
        "input_set_sha256": hashlib.sha256(
            _canonical_json_bytes(input_sha256)
        ).hexdigest(),
        "manifest_sha256": _sha256(manifest_sha256, "manifest_binding"),
        "schema": MANIFEST_BINDING_SCHEMA,
        "selected_case_count": len(rows),
    }


def _configuration_manifest_binding(
    configuration: dict[str, Any],
    files: Sequence[dict[str, Any]],
    *,
    expected: dict[str, object] | None,
) -> dict[str, object]:
    binding = configuration.get("manifest_binding")
    if not isinstance(binding, dict) or set(binding) != {
        "feature_map_sha256",
        "input_set_sha256",
        "manifest_sha256",
        "schema",
        "selected_case_count",
    }:
        raise GateError("manifest_binding")
    for key in ("feature_map_sha256", "input_set_sha256", "manifest_sha256"):
        _sha256(binding.get(key), "manifest_binding")
    if (
        binding.get("schema") != MANIFEST_BINDING_SCHEMA
        or binding.get("selected_case_count") != len(files)
    ):
        raise GateError("manifest_binding")
    derived = _mapping_binding(
        files,
        manifest_sha256=str(binding["manifest_sha256"]),
    )
    if binding != derived or (expected is not None and binding != expected):
        raise GateError("manifest_binding")
    return binding


def _campaign_manifest_binding(path: Path) -> dict[str, object]:
    document, digest, _ = _read_report(path)
    rows = document.get("files")
    if not isinstance(rows, list):
        raise GateError("campaign_manifest")
    selected = [
        row
        for row in rows
        if isinstance(row, dict) and row.get("format") in ORACLE_FORMATS
    ]
    if len(selected) != len(rows) or not selected:
        raise GateError("campaign_manifest")
    return _mapping_binding(selected, manifest_sha256=digest)


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
    page: dict[str, Any],
    *,
    prefix: str = "text_box",
) -> tuple[int, int, int, int, int, int, Counter[int]]:
    if prefix not in {"text_box", "text_line_box"}:
        raise GateError("text_box_prefix")
    code = prefix
    rxls_items = _integer(
        page.get(f"{prefix}_rxls_items"),
        code,
        maximum=1_000_000,
    )
    candidates = _integer(
        page.get(f"{prefix}_candidate_items"),
        code,
        maximum=1_000_000,
    )
    libreoffice_items = _integer(
        page.get(f"{prefix}_libreoffice_items"),
        code,
        maximum=1_000_000,
    )
    matched = _integer(
        page.get(f"{prefix}_matched_items"),
        code,
        maximum=candidates,
    )
    ambiguous = _integer(
        page.get(f"{prefix}_ambiguous_items"),
        code,
        maximum=candidates,
    )
    rxls_unmatched = _integer(
        page.get(f"{prefix}_rxls_unmatched_items"),
        code,
        maximum=candidates,
    )
    unmatched = _integer(
        page.get(f"{prefix}_unmatched_items"),
        code,
        maximum=candidates,
    )
    libreoffice_unmatched = _integer(
        page.get(f"{prefix}_libreoffice_unmatched_items"),
        code,
        maximum=libreoffice_items,
    )
    if (
        candidates != rxls_items
        or unmatched != rxls_unmatched
        or rxls_items != matched + ambiguous + rxls_unmatched
        or libreoffice_items != matched + libreoffice_unmatched
    ):
        raise GateError(f"{prefix}_partition")
    rows = page.get(f"{prefix}_error_histogram_millipoints")
    if not isinstance(rows, list) or len(rows) > MAX_HISTOGRAM_BUCKETS:
        raise GateError(f"{prefix}_histogram")
    histogram: Counter[int] = Counter()
    previous = -1
    for row in rows:
        if not isinstance(row, dict) or set(row) != {"count", "error_millipoints"}:
            raise GateError(f"{prefix}_histogram")
        error = _integer(
            row["error_millipoints"],
            f"{prefix}_histogram",
            maximum=1_000_000_000,
        )
        count = _integer(
            row["count"],
            f"{prefix}_histogram",
            minimum=1,
            maximum=1_000_000,
        )
        if error <= previous:
            raise GateError(f"{prefix}_histogram_order")
        previous = error
        histogram[error] = count
    if sum(histogram.values()) != matched:
        raise GateError(f"{prefix}_histogram_count")
    both_empty = rxls_items == 0 and libreoffice_items == 0
    precision = _ratio_ppm(
        matched, rxls_items, empty=1_000_000 if both_empty else 0
    )
    recall = _ratio_ppm(
        matched, libreoffice_items, empty=1_000_000 if both_empty else 0
    )
    f1 = _ratio_ppm(
        2 * matched, rxls_items + libreoffice_items, empty=1_000_000
    )
    if (
        _ppm(page.get(f"{prefix}_match_coverage_ppm"), code)
        != precision
        or _ppm(page.get(f"{prefix}_precision_ppm"), code)
        != precision
        or _ppm(page.get(f"{prefix}_recall_ppm"), code) != recall
        or _ppm(page.get(f"{prefix}_f1_ppm"), code) != f1
    ):
        raise GateError(f"{prefix}_coverage_inconsistent")
    expected_median = (
        _histogram_quantile(histogram, 1, 2) if histogram else None
    )
    expected_p95 = (
        _histogram_quantile(histogram, 95, 100) if histogram else None
    )
    if (
        page.get(f"{prefix}_median_error_millipoints") != expected_median
        or page.get(f"{prefix}_p95_error_millipoints") != expected_p95
    ):
        raise GateError(f"{prefix}_quantile_inconsistent")
    return (
        rxls_items,
        libreoffice_items,
        matched,
        ambiguous,
        rxls_unmatched,
        libreoffice_unmatched,
        histogram,
    )


def _edge_f1(rows: Sequence[dict[str, Any]]) -> tuple[int, int, int]:
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
        return 1_000_000, rxls_pixels, lo_pixels
    if denominator == 0:
        return 0, rxls_pixels, lo_pixels
    return (
        _ratio_ppm(2 * rxls_matched * lo_matched, denominator),
        rxls_pixels,
        lo_pixels,
    )


def _semantic_codepoint(
    rows: Sequence[dict[str, Any]],
) -> tuple[int, int, int, int]:
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
        rxls,
        libreoffice,
    )


def _aggregate_text_boxes(
    rows: Sequence[dict[str, Any]],
    *,
    prefix: str = "text_box",
) -> dict[str, object]:
    rxls_items = 0
    libreoffice_items = 0
    matched = 0
    ambiguous = 0
    rxls_unmatched = 0
    libreoffice_unmatched = 0
    histogram: Counter[int] = Counter()
    for page in rows:
        (
            page_rxls,
            page_libreoffice,
            page_matched,
            page_ambiguous,
            page_rxls_unmatched,
            page_libreoffice_unmatched,
            page_histogram,
        ) = _text_box_histogram(page, prefix=prefix)
        rxls_items += page_rxls
        libreoffice_items += page_libreoffice
        matched += page_matched
        ambiguous += page_ambiguous
        rxls_unmatched += page_rxls_unmatched
        libreoffice_unmatched += page_libreoffice_unmatched
        histogram.update(page_histogram)
    both_empty = rxls_items == 0 and libreoffice_items == 0
    return {
        "ambiguous": ambiguous,
        "f1_ppm": _ratio_ppm(
            2 * matched,
            rxls_items + libreoffice_items,
            empty=1_000_000,
        ),
        "libreoffice_items": libreoffice_items,
        "libreoffice_unmatched": libreoffice_unmatched,
        "matched": matched,
        "median_error_millipoints": (
            _histogram_quantile(histogram, 1, 2) if histogram else None
        ),
        "p95_error_millipoints": (
            _histogram_quantile(histogram, 95, 100) if histogram else None
        ),
        "precision_ppm": _ratio_ppm(
            matched,
            rxls_items,
            empty=1_000_000 if both_empty else 0,
        ),
        "recall_ppm": _ratio_ppm(
            matched,
            libreoffice_items,
            empty=1_000_000 if both_empty else 0,
        ),
        "rxls_items": rxls_items,
        "rxls_unmatched": rxls_unmatched,
    }


def _absolute_cohort_metrics(
    rows: Sequence[dict[str, Any]],
    similarities: Sequence[int],
) -> dict[str, object]:
    semantic_precision, semantic_recall, semantic_rxls, semantic_libreoffice = (
        _semantic_codepoint(rows)
    )
    edge_f1, edge_rxls, edge_libreoffice = _edge_f1(rows)
    return {
        "edge_f1_ppm": edge_f1,
        "edge_libreoffice_pixels": edge_libreoffice,
        "edge_rxls_pixels": edge_rxls,
        "semantic_codepoint_libreoffice_items": semantic_libreoffice,
        "semantic_codepoint_precision_ppm": semantic_precision,
        "semantic_codepoint_recall_ppm": semantic_recall,
        "semantic_codepoint_rxls_items": semantic_rxls,
        "similarity_mean_ppm": _mean(similarities),
        "text_box": _aggregate_text_boxes(rows),
        "text_line_box": _aggregate_text_boxes(
            rows,
            prefix="text_line_box",
        ),
    }


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
            "expected_manifest_digest",
            "identity_status",
            "manifest_digest",
        },
        "configuration_container_image",
    )
    config_digest = image.get("config_digest")
    manifest_digest = image.get("manifest_digest")
    if (
        image.get("architecture") != "linux/amd64"
        or not isinstance(config_digest, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", config_digest) is None
        or image.get("expected_config_digest") != config_digest
        or not isinstance(manifest_digest, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", manifest_digest) is None
        or image.get("expected_manifest_digest") != manifest_digest
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
        {
            "host_tools_identity_sha256",
            "kind",
            "pdffonts_sha256",
            "pdfinfo_sha256",
            "pdftoppm_sha256",
            "pdftotext_sha256",
        },
        "configuration_container_pdffonts",
    )
    if inspector.get("kind") != "poppler":
        raise GateError("configuration_container_pdffonts")
    for key in (
        "host_tools_identity_sha256",
        "pdffonts_sha256",
        "pdfinfo_sha256",
        "pdftoppm_sha256",
        "pdftotext_sha256",
    ):
        _sha256(inspector.get(key), "configuration_container_pdffonts")
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
        {
            "architecture",
            "expected_id",
            "expected_manifest_digest",
            "id",
            "identity_status",
            "manifest_digest",
        },
        "file_oracle_adapter_image",
    )
    expected_image = aggregate["image"]
    if (
        row.get("schema") != CONTAINER_EXECUTION_SCHEMA
        or row.get("font_pack_sha256") != aggregate["font_pack_sha256"]
        or image.get("architecture") != expected_image["architecture"]
        or image.get("id") != expected_image["config_digest"]
        or image.get("expected_id") != expected_image["expected_config_digest"]
        or image.get("manifest_digest") != expected_image["manifest_digest"]
        or image.get("expected_manifest_digest")
        != expected_image["expected_manifest_digest"]
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


def _native_pdf_attestation(value: object) -> tuple[int, int]:
    row = _exact_object(
        value,
        {
            "actual_text_documents",
            "charprocs_documents",
            "documents",
            "embedded_font_objects",
            "font_objects",
            "identity_set_sha256",
            "subset_font_objects",
            "type3_documents",
            "type3_font_objects",
            "unicode_font_objects",
        },
        "native_pdf_attestation",
    )
    documents = _integer(
        row["documents"],
        "native_pdf_attestation",
        minimum=1,
        maximum=MAX_PAGES,
    )
    objects = _integer(
        row["font_objects"],
        "native_pdf_attestation",
        minimum=1,
        maximum=1_000_000,
    )
    for key in (
        "actual_text_documents",
        "charprocs_documents",
        "type3_documents",
    ):
        if _integer(
            row[key], "native_pdf_attestation", maximum=documents
        ) != documents:
            raise GateError("native_pdf_attestation")
    for key in (
        "embedded_font_objects",
        "subset_font_objects",
        "type3_font_objects",
        "unicode_font_objects",
    ):
        if _integer(
            row[key], "native_pdf_attestation", maximum=objects
        ) != objects:
            raise GateError("native_pdf_attestation")
    _sha256(row["identity_set_sha256"], "native_pdf_attestation")
    return documents, objects


def _configuration(
    report: dict[str, Any],
) -> tuple[int, dict[str, str], str, dict[str, Any] | None]:
    configuration = report.get("configuration")
    if not isinstance(configuration, dict):
        raise GateError("configuration")
    if configuration.get("lane_filter") != {
        "formats": [],
        "required_features": [],
    }:
        raise GateError("configuration_lane_filter")
    dpi = _integer(configuration.get("dpi"), "configuration_dpi", minimum=36, maximum=1200)
    font_pack = configuration.get("font_pack")
    oracle_lock = configuration.get("oracle_lock")
    renderer = configuration.get("renderer_binary")
    measurement_toolchain = configuration.get("measurement_toolchain")
    if not isinstance(font_pack, dict) or not isinstance(oracle_lock, dict) or not isinstance(renderer, dict):
        raise GateError("configuration_identity")
    measurement_keys = {
        "kind",
        "pdffonts_sha256",
        "pdfinfo_sha256",
        "pdftoppm_sha256",
        "pdftotext_sha256",
    }
    if not isinstance(measurement_toolchain, dict) or (
        frozenset(measurement_toolchain)
        not in {
            frozenset(measurement_keys),
            frozenset({*measurement_keys, "host_tools_identity_sha256"}),
        }
    ) or measurement_toolchain.get("kind") != "poppler":
        raise GateError("configuration_measurement_toolchain")
    measurement_hashes = {
        key: measurement_toolchain.get(key)
        for key in (
            "pdffonts_sha256",
            "pdfinfo_sha256",
            "pdftoppm_sha256",
            "pdftotext_sha256",
        )
    }
    if not all(
        isinstance(value, str) and SHA256_RE.fullmatch(value)
        for value in measurement_hashes.values()
    ):
        raise GateError("configuration_measurement_toolchain")
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
    oracle_schema = oracle_lock.get("schema")
    if (
        isinstance(oracle_schema, str)
        and oracle_schema.startswith("rxls.render-oracle-container-identity.")
        and oracle_schema != CONTAINER_IDENTITY_SCHEMA
    ):
        raise GateError("configuration_container_identity")
    if oracle_schema == CONTAINER_IDENTITY_SCHEMA:
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
                "oracle_image_manifest_digest": container_identity["image"]["manifest_digest"],
                "oracle_lock_file_sha256": container_identity["lock_file_sha256"],
                "oracle_libreoffice_artifact_sha256": container_identity["libreoffice"]["artifact_sha256"],
            }
        )
        inspector = container_identity["pdf_font_inspector"]
        if measurement_toolchain != inspector:
            raise GateError("configuration_measurement_toolchain")
        identities.update(measurement_hashes)
        identities["host_tools_identity_sha256"] = _sha256(
            measurement_toolchain.get("host_tools_identity_sha256"),
            "configuration_measurement_toolchain",
        )
        python_identity = None
    else:
        oracle_mode = "direct"
        if set(measurement_toolchain) != measurement_keys:
            raise GateError("configuration_measurement_toolchain")
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
        if any(
            measurement_hashes[key] != locked_hashes[key]
            for key in measurement_hashes
        ):
            raise GateError("configuration_measurement_toolchain")
        identities.update(locked_hashes)
    policy = configuration.get("metric_policy")
    implementation = policy.get("implementation") if isinstance(policy, dict) else None
    if (
        not isinstance(policy, dict)
        or policy.get("contract_schema") != METRIC_CONTRACT_SCHEMA
        or policy.get("contract_version") != 2
        or type(policy.get("mask_match_tolerance_pixels")) is not int
        or policy.get("mask_match_tolerance_pixels") != 1
        or policy.get("edge_luma_delta") != 32
        or policy.get("semantic_content_retained") is not False
        or policy.get("semantic_text_source")
        != "svg_data-rxls-visible-label_vs_pdftotext_layout"
        or policy.get("raster_source")
        != "rxls_native_print_pdf_vs_libreoffice_calc_pdf"
        or policy.get("rasterizer")
        != "same_locked_poppler_pdftoppm_both_sides"
        or policy.get("text_ink_source")
        != "thresholded_common_poppler_rasters"
        or policy.get("text_box_content_retained") is not False
        or policy.get("text_box_error_units") != "millipoints"
        or policy.get("text_box_source")
        != "pdftotext_bbox_layout_word_boxes_both_native_pdfs"
        or policy.get("text_line_box_source")
        != "pdftotext_bbox_layout_line_boxes_both_native_pdfs"
        or policy.get("text_box_matching")
        != "exact_normalized_tokens_nearest_unique_one_to_one_same_bbox_level_symmetric_counts"
        or policy.get("text_box_geometry")
        != "nominal_poppler_layout_not_ink_bounds"
        or not type_exact_equal(
            policy.get("unique_text_geometry"),
            UNIQUE_TEXT_GEOMETRY_POLICY,
        )
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


def evaluate(
    report: dict[str, Any],
    evidence_sha256: str,
    evidence_bytes: int,
    expected_manifest_binding: dict[str, object] | None = None,
) -> dict[str, Any]:
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
    _validate_complete_discovery(report.get("discovery"), len(files))
    summary = report.get("summary")
    if (
        not isinstance(summary, dict)
        or type(summary.get("files")) is not int
        or summary.get("files") != len(files)
    ):
        raise GateError("summary_files")
    try:
        validate_report_geometry(files)
    except GeometryContractError as error:
        raise GateError(str(error)) from error
    manifest_binding = _configuration_manifest_binding(
        report["configuration"],
        files,
        expected=expected_manifest_binding,
    )

    broad_rows: list[dict[str, Any]] = []
    core_rows: list[dict[str, Any]] = []
    broad_similarities: list[int] = []
    core_similarities: list[int] = []
    broad_files: list[dict[str, Any]] = []
    core_files: list[dict[str, Any]] = []
    hard_rows: dict[str, list[dict[str, Any]]] = {
        name: [] for name in HARD_FEATURE_COHORTS
    }
    hard_similarities: dict[str, list[int]] = {
        name: [] for name in HARD_FEATURE_COHORTS
    }
    hard_files: dict[str, list[dict[str, Any]]] = {
        name: [] for name in HARD_FEATURE_COHORTS
    }
    format_counts: Counter[str] = Counter()
    status_counts: Counter[str] = Counter()
    classification_counts: Counter[str] = Counter()
    page_errors_millipoints: list[int] = []
    total_pages = 0
    font_objects = 0
    native_pdf_documents = 0
    native_pdf_font_objects = 0
    point_geometry_mismatches = 0
    xhtml_crosscheck_max_micropoints = 0
    failures: set[str] = set()

    for item in files:
        if not isinstance(item, dict):
            raise GateError("file_row")
        format_name = item.get("format")
        status = item.get("status")
        classification = item.get("classification")
        if (
            not isinstance(format_name, str)
            or not isinstance(status, str)
            or not isinstance(classification, str)
            or CLASSIFICATION_RE.fullmatch(classification) is None
        ):
            raise GateError("file_identity")
        if status not in {"compared", "different", "skipped", "error"}:
            raise GateError("file_status")
        status_counts[status] += 1
        classification_counts[classification] += 1
        if format_name not in ORACLE_FORMATS:
            raise GateError("file_format")
        if status != "compared" or classification != "within_threshold":
            failures.add("broad_coverage_incomplete")
            continue
        font_objects += _font_attestation(item.get("font_attestation"))
        native_documents, native_objects = _native_pdf_attestation(
            item.get("native_pdf_attestation")
        )
        native_pdf_documents += native_documents
        native_pdf_font_objects += native_objects
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
        if (
            page_count == 0
            or rxls_pages != page_count
            or libreoffice_pages != page_count
            or not _validate_page_mapping(pages, scenes)
        ):
            failures.add("sheet_page_mapping_not_exact")
        file_point_mismatches = 0
        file_point_max = 0
        file_crosscheck_max = 0
        for page in pages:
            if not isinstance(page, dict):
                raise GateError("page_row")
            rxls_size = page.get("rxls_size")
            libreoffice_size = page.get("libreoffice_size")
            if not isinstance(rxls_size, dict) or not isinstance(libreoffice_size, dict):
                raise GateError("page_geometry")
            rxls_width = _integer(
                rxls_size.get("width"), "page_geometry", minimum=1
            )
            rxls_height = _integer(
                rxls_size.get("height"), "page_geometry", minimum=1
            )
            libreoffice_width = _integer(
                libreoffice_size.get("width"), "page_geometry", minimum=1
            )
            libreoffice_height = _integer(
                libreoffice_size.get("height"), "page_geometry", minimum=1
            )
            if (rxls_width, rxls_height) != (
                libreoffice_width,
                libreoffice_height,
            ):
                failures.add("raster_page_box_mismatch")
            (
                point_error,
                crosscheck_error,
                exact_point_geometry,
                exact_xhtml_crosscheck,
            ) = _page_point_geometry(page)
            page_errors_millipoints.append(point_error)
            file_point_max = max(file_point_max, point_error)
            file_crosscheck_max = max(
                file_crosscheck_max, crosscheck_error
            )
            file_point_mismatches += int(not exact_point_geometry)
            if not exact_point_geometry:
                failures.add("pdf_point_geometry_mismatch")
            if not exact_xhtml_crosscheck:
                failures.add("pdf_xhtml_crosscheck_above_tolerance")
        point_geometry_mismatches += file_point_mismatches
        xhtml_crosscheck_max_micropoints = max(
            xhtml_crosscheck_max_micropoints,
            file_crosscheck_max,
        )
        if (
            _integer(
                metrics.get("pdf_point_geometry_mismatches"),
                "page_point_geometry_aggregate",
                maximum=page_count,
            )
            != file_point_mismatches
            or _integer(
                metrics.get("max_pdf_point_geometry_delta_millipoints"),
                "page_point_geometry_aggregate",
            )
            != file_point_max
            or _integer(
                metrics.get(
                    "max_pdf_xhtml_crosscheck_delta_micropoints"
                ),
                "page_point_geometry_aggregate",
            )
            != file_crosscheck_max
        ):
            raise GateError("page_point_geometry_aggregate")

        broad_rows.extend(pages)
        broad_similarities.append(file_similarity)
        broad_files.append(item)
        features = _features(item.get("features"))
        if features is None:
            raise GateError("file_features")
        _, _, semantic_rxls, semantic_libreoffice = _semantic_codepoint(pages)
        _, edge_rxls, edge_libreoffice = _edge_f1(pages)
        if semantic_rxls == 0 or semantic_libreoffice == 0:
            failures.add("semantic_population_empty")
        if edge_rxls == 0 or edge_libreoffice == 0:
            failures.add("edge_population_empty")
        if features is not None and not CORE_EXCLUDED_FEATURES.intersection(features):
            core_rows.extend(pages)
            core_similarities.append(file_similarity)
            core_files.append(item)
        for name, cohort_features in HARD_FEATURE_COHORTS.items():
            if cohort_features.intersection(features):
                hard_rows[name].extend(pages)
                hard_similarities[name].append(file_similarity)
                hard_files[name].append(item)

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
    by_classification = summary.get("by_classification")
    if (
        not isinstance(by_classification, dict)
        or any(
            not isinstance(key, str)
            or CLASSIFICATION_RE.fullmatch(key) is None
            or isinstance(value, bool)
            or not isinstance(value, int)
            or value < 0
            for key, value in by_classification.items()
        )
        or by_classification
        != dict(sorted(classification_counts.items()))
    ):
        raise GateError("summary_classification_counts")

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

    core_metrics = _absolute_cohort_metrics(core_rows, core_similarities)
    core_precision = int(core_metrics["semantic_codepoint_precision_ppm"])
    core_recall = int(core_metrics["semantic_codepoint_recall_ppm"])
    core_edge_f1 = int(core_metrics["edge_f1_ppm"])
    core_similarity = int(core_metrics["similarity_mean_ppm"])
    core_text_box = core_metrics["text_box"]
    core_line_box = core_metrics["text_line_box"]
    if not isinstance(core_text_box, dict) or not isinstance(core_line_box, dict):
        raise GateError("text_box_metric")
    broad_similarity = _mean(broad_similarities)
    if int(core_text_box["matched"]) < MIN_CORE_TEXT_BOXES:
        failures.add("text_box_coverage_below_minimum")
    text_box_precision = int(core_text_box["precision_ppm"])
    text_box_recall = int(core_text_box["recall_ppm"])
    text_box_f1 = int(core_text_box["f1_ppm"])
    text_box_median = core_text_box["median_error_millipoints"]
    text_box_p95 = core_text_box["p95_error_millipoints"]
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
    if text_box_precision < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_box_match_coverage_below_target")
        failures.add("text_box_precision_below_target")
    if text_box_recall < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_box_recall_below_target")
    if text_box_f1 < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_box_f1_below_target")
    if int(core_text_box["ambiguous"]) != 0:
        failures.add("text_box_mapping_ambiguous")
    if int(core_text_box["rxls_unmatched"]) != 0:
        failures.add("text_box_mapping_unmatched")
    if int(core_text_box["libreoffice_unmatched"]) != 0:
        failures.add("text_box_reference_unmatched")
    if text_box_median is None or text_box_median > TEXT_BOX_MEDIAN_MAX_MILLIPOINTS:
        failures.add("text_box_median_error_above_target")
    if text_box_p95 is None or text_box_p95 > TEXT_BOX_P95_MAX_MILLIPOINTS:
        failures.add("text_box_p95_error_above_target")
    if int(core_line_box["matched"]) <= 0:
        failures.add("text_line_box_coverage_below_minimum")
    if int(core_line_box["precision_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_line_box_precision_below_target")
    if int(core_line_box["recall_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_line_box_recall_below_target")
    if int(core_line_box["f1_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_line_box_f1_below_target")
    if int(core_line_box["ambiguous"]) != 0:
        failures.add("text_line_box_mapping_ambiguous")
    if int(core_line_box["rxls_unmatched"]) != 0:
        failures.add("text_line_box_mapping_unmatched")
    if int(core_line_box["libreoffice_unmatched"]) != 0:
        failures.add("text_line_box_reference_unmatched")
    if (
        core_line_box["median_error_millipoints"] is None
        or int(core_line_box["median_error_millipoints"])
        > TEXT_BOX_MEDIAN_MAX_MILLIPOINTS
    ):
        failures.add("text_line_box_median_error_above_target")
    if (
        core_line_box["p95_error_millipoints"] is None
        or int(core_line_box["p95_error_millipoints"])
        > TEXT_BOX_P95_MAX_MILLIPOINTS
    ):
        failures.add("text_line_box_p95_error_above_target")
    if page_median > PAGE_BOX_MEDIAN_MAX_MILLIPOINTS:
        failures.add("page_box_median_error_above_target")
    if page_p95 > PAGE_BOX_P95_MAX_MILLIPOINTS:
        failures.add("page_box_p95_error_above_target")
    if page_max > PAGE_BOX_MAX_MILLIPOINTS:
        failures.add("page_box_max_error_above_target")

    hard_feature_metrics: dict[str, dict[str, object]] = {}
    for name in sorted(HARD_FEATURE_COHORTS):
        workbook_count = len(hard_files[name])
        if workbook_count < MIN_HARD_FEATURE_WORKBOOKS:
            failures.add(f"hard_feature_coverage_below_minimum:{name}")
        if not hard_rows[name]:
            hard_feature_metrics[name] = {"workbooks": workbook_count}
            continue
        metrics = _absolute_cohort_metrics(
            hard_rows[name],
            hard_similarities[name],
        )
        text_box = metrics.get("text_box")
        line_box = metrics.get("text_line_box")
        if not isinstance(text_box, dict) or not isinstance(line_box, dict):
            raise GateError("hard_feature_text_box")
        hard_feature_metrics[name] = {
            "workbooks": workbook_count,
            **metrics,
        }
        if (
            int(metrics["semantic_codepoint_rxls_items"]) == 0
            or int(metrics["semantic_codepoint_libreoffice_items"]) == 0
        ):
            failures.add(f"hard_feature_semantic_population_empty:{name}")
        if (
            int(metrics["edge_rxls_pixels"]) == 0
            or int(metrics["edge_libreoffice_pixels"]) == 0
        ):
            failures.add(f"hard_feature_edge_population_empty:{name}")
        if int(metrics["semantic_codepoint_precision_ppm"]) < SEMANTIC_CODEPOINT_MIN_PPM:
            failures.add(f"hard_feature_semantic_precision_below_target:{name}")
        if int(metrics["semantic_codepoint_recall_ppm"]) < SEMANTIC_CODEPOINT_MIN_PPM:
            failures.add(f"hard_feature_semantic_recall_below_target:{name}")
        if int(metrics["edge_f1_ppm"]) < EDGE_F1_MIN_PPM:
            failures.add(f"hard_feature_edge_f1_below_target:{name}")
        if int(metrics["similarity_mean_ppm"]) < BROAD_SIMILARITY_MIN_PPM:
            failures.add(f"hard_feature_similarity_below_target:{name}")
        if int(text_box["matched"]) <= 0:
            failures.add(f"hard_feature_text_box_coverage_empty:{name}")
        if int(text_box["precision_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
            failures.add(f"hard_feature_text_box_precision_below_target:{name}")
        if int(text_box["recall_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
            failures.add(f"hard_feature_text_box_recall_below_target:{name}")
        if int(text_box["f1_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
            failures.add(f"hard_feature_text_box_f1_below_target:{name}")
        if int(text_box["ambiguous"]) != 0:
            failures.add(f"hard_feature_text_box_ambiguous:{name}")
        if int(text_box["rxls_unmatched"]) != 0:
            failures.add(f"hard_feature_text_box_rxls_unmatched:{name}")
        if int(text_box["libreoffice_unmatched"]) != 0:
            failures.add(f"hard_feature_text_box_reference_unmatched:{name}")
        if (
            text_box["median_error_millipoints"] is None
            or int(text_box["median_error_millipoints"])
            > TEXT_BOX_MEDIAN_MAX_MILLIPOINTS
        ):
            failures.add(f"hard_feature_text_box_median_above_target:{name}")
        if (
            text_box["p95_error_millipoints"] is None
            or int(text_box["p95_error_millipoints"])
            > TEXT_BOX_P95_MAX_MILLIPOINTS
        ):
            failures.add(f"hard_feature_text_box_p95_above_target:{name}")
        if int(line_box["matched"]) <= 0:
            failures.add(f"hard_feature_text_line_box_coverage_empty:{name}")
        if int(line_box["precision_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
            failures.add(
                f"hard_feature_text_line_box_precision_below_target:{name}"
            )
        if int(line_box["recall_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
            failures.add(
                f"hard_feature_text_line_box_recall_below_target:{name}"
            )
        if int(line_box["f1_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
            failures.add(f"hard_feature_text_line_box_f1_below_target:{name}")
        if int(line_box["ambiguous"]) != 0:
            failures.add(f"hard_feature_text_line_box_ambiguous:{name}")
        if int(line_box["rxls_unmatched"]) != 0:
            failures.add(
                f"hard_feature_text_line_box_rxls_unmatched:{name}"
            )
        if int(line_box["libreoffice_unmatched"]) != 0:
            failures.add(
                f"hard_feature_text_line_box_reference_unmatched:{name}"
            )
        if (
            line_box["median_error_millipoints"] is None
            or int(line_box["median_error_millipoints"])
            > TEXT_BOX_MEDIAN_MAX_MILLIPOINTS
        ):
            failures.add(f"hard_feature_text_line_box_median_above_target:{name}")
        if (
            line_box["p95_error_millipoints"] is None
            or int(line_box["p95_error_millipoints"])
            > TEXT_BOX_P95_MAX_MILLIPOINTS
        ):
            failures.add(f"hard_feature_text_line_box_p95_above_target:{name}")

    thresholds = {
        "broad_similarity_min_ppm": BROAD_SIMILARITY_MIN_PPM,
        "core_similarity_min_ppm": CORE_SIMILARITY_MIN_PPM,
        "edge_f1_min_ppm": EDGE_F1_MIN_PPM,
        "page_box_max_millipoints": PAGE_BOX_MAX_MILLIPOINTS,
        "page_box_median_max_millipoints": PAGE_BOX_MEDIAN_MAX_MILLIPOINTS,
        "page_box_p95_max_millipoints": PAGE_BOX_P95_MAX_MILLIPOINTS,
        "pdf_point_geometry_exact": True,
        "pdf_xhtml_crosscheck_max_micropoints": (
            PDF_XHTML_CROSSCHECK_MAX_MICROPOINTS
        ),
        "semantic_codepoint_precision_min_ppm": SEMANTIC_CODEPOINT_MIN_PPM,
        "semantic_codepoint_recall_min_ppm": SEMANTIC_CODEPOINT_MIN_PPM,
        "text_box_match_min_ppm": TEXT_BOX_MATCH_MIN_PPM,
        "text_box_median_max_millipoints": TEXT_BOX_MEDIAN_MAX_MILLIPOINTS,
        "text_box_p95_max_millipoints": TEXT_BOX_P95_MAX_MILLIPOINTS,
    }
    return {
        "coverage": {
            "broad_workbooks": len(broad_files),
            "core_text_box_candidates": core_text_box["rxls_items"],
            "core_text_box_libreoffice_items": core_text_box[
                "libreoffice_items"
            ],
            "core_text_box_matches": core_text_box["matched"],
            "core_text_box_ambiguous": core_text_box["ambiguous"],
            "core_text_box_unmatched": core_text_box["rxls_unmatched"],
            "core_text_box_libreoffice_unmatched": core_text_box[
                "libreoffice_unmatched"
            ],
            "core_text_line_box_candidates": core_line_box["rxls_items"],
            "core_text_line_box_libreoffice_items": core_line_box[
                "libreoffice_items"
            ],
            "core_text_line_box_matches": core_line_box["matched"],
            "core_text_line_box_ambiguous": core_line_box["ambiguous"],
            "core_text_line_box_unmatched": core_line_box["rxls_unmatched"],
            "core_text_line_box_libreoffice_unmatched": core_line_box[
                "libreoffice_unmatched"
            ],
            "core_workbooks": len(core_files),
            "format_workbooks": dict(sorted(format_counts.items())),
            "libreoffice_pdf_font_objects": font_objects,
            "native_pdf_documents": native_pdf_documents,
            "native_pdf_font_objects": native_pdf_font_objects,
            "pages": total_pages,
            "report_workbooks": len(files),
            "status_counts": dict(sorted(status_counts.items())),
            "hard_feature_workbooks": {
                name: len(hard_files[name]) for name in sorted(hard_files)
            },
        },
        "evidence": {
            "bytes": evidence_bytes,
            "feature_map_sha256": manifest_binding["feature_map_sha256"],
            "input_set_sha256": manifest_binding["input_set_sha256"],
            "manifest_sha256": manifest_binding["manifest_sha256"],
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
            "hard_feature_cohorts": hard_feature_metrics,
            "page_box_max_millipoints": page_max,
            "page_box_median_millipoints": page_median,
            "page_box_p95_millipoints": page_p95,
            "pdf_point_geometry_mismatches": point_geometry_mismatches,
            "pdf_xhtml_crosscheck_max_micropoints": (
                xhtml_crosscheck_max_micropoints
            ),
            "text_box_f1_ppm": text_box_f1,
            "text_box_match_coverage_ppm": text_box_precision,
            "text_box_median_error_millipoints": text_box_median,
            "text_box_p95_error_millipoints": text_box_p95,
            "text_box_precision_ppm": text_box_precision,
            "text_box_recall_ppm": text_box_recall,
            "text_line_box_f1_ppm": core_line_box["f1_ppm"],
            "text_line_box_median_error_millipoints": core_line_box[
                "median_error_millipoints"
            ],
            "text_line_box_p95_error_millipoints": core_line_box[
                "p95_error_millipoints"
            ],
            "text_line_box_precision_ppm": core_line_box["precision_ppm"],
            "text_line_box_recall_ppm": core_line_box["recall_ppm"],
        },
        "passed": not failures,
        "policy": {
            "core_excluded_features": sorted(CORE_EXCLUDED_FEATURES),
            "minimum_broad_workbooks": MIN_BROAD_WORKBOOKS,
            "minimum_core_text_boxes": MIN_CORE_TEXT_BOXES,
            "minimum_core_workbooks": MIN_CORE_WORKBOOKS,
            "minimum_hard_feature_workbooks": MIN_HARD_FEATURE_WORKBOOKS,
            "hard_feature_cohorts": {
                name: sorted(features)
                for name, features in sorted(HARD_FEATURE_COHORTS.items())
            },
            "oracle_formats": list(ORACLE_FORMATS),
        },
        "schema": OUTPUT_SCHEMA,
        "thresholds": thresholds,
    }


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path, help="complete parity report")
    parser.add_argument("--campaign-manifest", type=Path)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        report, digest, size = _read_report(args.report)
        expected_manifest_binding = (
            _campaign_manifest_binding(args.campaign_manifest)
            if args.campaign_manifest is not None
            else None
        )
        result = evaluate(
            report,
            digest,
            size,
            expected_manifest_binding=expected_manifest_binding,
        )
    except GateError as error:
        print(f"check-render-fidelity-targets: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
