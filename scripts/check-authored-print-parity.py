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
OUTPUT_SCHEMA = "rxls.authored-print-parity.v1"
MANIFEST_BINDING_SCHEMA = "rxls.render-parity-manifest-binding.v1"
METRIC_CONTRACT_SCHEMA = "rxls.render-parity-metrics.v2"
CONTAINER_IDENTITY_SCHEMA = "rxls.render-oracle-container-identity.v2"
CONTAINER_EXECUTION_SCHEMA = "rxls.render-oracle-container-execution.v3"
CONTAINER_LIBREOFFICE_ARTIFACT_SHA256 = (
    "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5"
)
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_JSON_INTEGER_DIGITS = 128
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
EXPECTED_PAGE_WIDTH_POINTS = Fraction(612)
EXPECTED_PAGE_HEIGHT_POINTS = Fraction(792)
EXPECTED_PAGES_PER_WORKBOOK = 4
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
        "xhtml_height",
        "xhtml_width",
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
    """The report is malformed or violates the authored-print contract."""


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
        raise GateError("report_size")
    if len(payload) != after.st_size:
        raise GateError("report_unreadable")
    return payload


def _read(path: Path) -> tuple[dict[str, Any], str, int]:
    payload = _read_bounded_regular_file(path, MAX_REPORT_BYTES)
    size = len(payload)
    try:
        text = payload.decode("utf-8")
        _preflight_json_text(text)
        document = json.loads(
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
        maximum=MAX_WORKBOOKS,
    )
    pre_shard = _integer(
        value.get("pre_shard_selected_count"),
        "campaign_coverage",
        minimum=1,
        maximum=MAX_WORKBOOKS,
    )
    shard_candidates = _integer(
        value.get("shard_candidate_count"),
        "campaign_coverage",
        minimum=1,
        maximum=MAX_WORKBOOKS,
    )
    candidates = _integer(
        value.get("candidate_count"),
        "campaign_coverage",
        minimum=1,
        maximum=MAX_WORKBOOKS,
    )
    if (
        selected != file_count
        or pre_shard != selected
        or shard_candidates != selected
        or candidates < selected
    ):
        raise GateError("campaign_coverage")


def _sha(value: object, code: str) -> str:
    if not isinstance(value, str) or SHA256_RE.fullmatch(value) is None:
        raise GateError(code)
    return value


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
    if not isinstance(value, dict) or set(value) != {
        "crop_box",
        "media_box",
        "page_size",
    }:
        raise GateError(code)
    result: dict[str, tuple[Fraction, Fraction]] = {}
    for name in ("page_size", "media_box", "crop_box"):
        row = value[name]
        if not isinstance(row, dict) or set(row) != {
            "height_points",
            "width_points",
        }:
            raise GateError(code)
        result[name] = (
            _point(row["width_points"], code, positive=True),
            _point(row["height_points"], code, positive=True),
        )
    return result


def _page_point_geometry(page: dict[str, Any]) -> tuple[int, int, bool]:
    evidence = page.get("pdf_point_geometry")
    if not isinstance(evidence, dict) or set(evidence) != {
        "deltas_points",
        "libreoffice",
        "rxls",
        "xhtml",
    }:
        raise GateError("page_point_geometry")
    rxls = _point_side(evidence["rxls"], "page_point_geometry")
    libreoffice = _point_side(
        evidence["libreoffice"], "page_point_geometry"
    )
    xhtml = evidence["xhtml"]
    if not isinstance(xhtml, dict) or set(xhtml) != {"libreoffice", "rxls"}:
        raise GateError("page_point_geometry")
    xhtml_values: dict[str, tuple[Fraction, Fraction]] = {}
    for side in ("rxls", "libreoffice"):
        row = xhtml[side]
        if not isinstance(row, dict) or set(row) != {
            "height_points",
            "width_points",
        }:
            raise GateError("page_point_geometry")
        xhtml_values[side] = (
            _point(row["width_points"], "page_point_geometry", positive=True),
            _point(row["height_points"], "page_point_geometry", positive=True),
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
    deltas = evidence["deltas_points"]
    if not isinstance(deltas, dict) or set(deltas) != PDF_POINT_DELTA_KEYS:
        raise GateError("page_point_geometry")
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
    max_millipoints = (
        max_direct_delta.numerator * 1000
        + max_direct_delta.denominator
        - 1
    ) // max_direct_delta.denominator
    max_crosscheck_micropoints = (
        max_crosscheck_delta.numerator * 1_000_000
        + max_crosscheck_delta.denominator
        - 1
    ) // max_crosscheck_delta.denominator
    exact_expected = (
        EXPECTED_PAGE_WIDTH_POINTS,
        EXPECTED_PAGE_HEIGHT_POINTS,
    )
    renderer_expected = all(
        rxls[name] == exact_expected
        for name in ("page_size", "media_box", "crop_box")
    )
    return (
        max_millipoints,
        max_crosscheck_micropoints,
        renderer_expected
        and max_direct_delta == 0
        and max_crosscheck_delta <= PDF_XHTML_CROSSCHECK_MAX_POINTS,
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
) -> None:
    page_mapping = [_mapping_tuple(row) for row in pages]
    scene_mapping = [_mapping_tuple(row) for row in scenes]
    if scene_mapping != page_mapping or [
        row[2] for row in page_mapping
    ] != list(range(len(page_mapping))):
        raise GateError("page_mapping")
    seen_sheets: set[int] = set()
    current_sheet: int | None = None
    next_local = 0
    for source_sheet, source_pdf_page, _ in page_mapping:
        if source_sheet != current_sheet:
            if source_sheet in seen_sheets:
                raise GateError("page_mapping")
            if current_sheet is not None and source_sheet <= current_sheet:
                raise GateError("page_mapping")
            seen_sheets.add(source_sheet)
            current_sheet = source_sheet
            next_local = 0
        if source_pdf_page != next_local:
            raise GateError("page_mapping")
        next_local += 1


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
        digest = _sha(row.get("sha256"), "manifest_binding")
        format_name = row.get("format")
        features = row.get("features")
        if (
            digest in seen
            or not isinstance(format_name, str)
            or not format_name
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
        "manifest_sha256": _sha(manifest_sha256, "manifest_binding"),
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
        _sha(binding.get(key), "manifest_binding")
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
    document, digest, _ = _read(path)
    rows = document.get("files")
    if not isinstance(rows, list):
        raise GateError("campaign_manifest")
    selected = []
    for row in rows:
        if not isinstance(row, dict):
            raise GateError("campaign_manifest")
        features = row.get("features")
        if (
            row.get("format") == "xlsx"
            and isinstance(features, list)
            and "print-settings" in features
        ):
            selected.append(row)
    if not selected:
        raise GateError("campaign_manifest")
    return _mapping_binding(selected, manifest_sha256=digest)


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


def _edge_f1(rows: Sequence[dict[str, Any]]) -> tuple[int, int, int]:
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
        return 1_000_000, rxls_pixels, libreoffice_pixels
    denominator = rxls_matched * libreoffice_pixels + libreoffice_matched * rxls_pixels
    if denominator == 0:
        return 0, rxls_pixels, libreoffice_pixels
    return (
        _ratio_ppm(2 * rxls_matched * libreoffice_matched, denominator),
        rxls_pixels,
        libreoffice_pixels,
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


def _text_box_histogram(
    page: dict[str, Any],
    *,
    prefix: str = "text_box",
) -> tuple[int, int, int, int, int, int, Counter[int]]:
    if prefix not in {"text_box", "text_line_box"}:
        raise GateError("text_box_prefix")
    rxls_items = _integer(
        page.get(f"{prefix}_rxls_items"),
        prefix,
        minimum=1,
        maximum=1_000_000,
    )
    candidates = _integer(
        page.get(f"{prefix}_candidate_items"),
        prefix,
        minimum=1,
        maximum=1_000_000,
    )
    libreoffice_items = _integer(
        page.get(f"{prefix}_libreoffice_items"),
        prefix,
        minimum=1,
        maximum=1_000_000,
    )
    matched = _integer(
        page.get(f"{prefix}_matched_items"),
        prefix,
        maximum=candidates,
    )
    ambiguous = _integer(
        page.get(f"{prefix}_ambiguous_items"),
        prefix,
        maximum=candidates,
    )
    rxls_unmatched = _integer(
        page.get(f"{prefix}_rxls_unmatched_items"),
        prefix,
        maximum=candidates,
    )
    unmatched = _integer(
        page.get(f"{prefix}_unmatched_items"),
        prefix,
        maximum=candidates,
    )
    libreoffice_unmatched = _integer(
        page.get(f"{prefix}_libreoffice_unmatched_items"),
        prefix,
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
    if not isinstance(rows, list) or len(rows) > 1_000_000:
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
        _ppm(page.get(f"{prefix}_match_coverage_ppm"), prefix)
        != precision
        or _ppm(page.get(f"{prefix}_precision_ppm"), prefix)
        != precision
        or _ppm(page.get(f"{prefix}_recall_ppm"), prefix) != recall
        or _ppm(page.get(f"{prefix}_f1_ppm"), prefix) != f1
    ):
        raise GateError(f"{prefix}_metric_inconsistent")
    median = _histogram_quantile(histogram, 1, 2) if histogram else None
    p95 = _histogram_quantile(histogram, 95, 100) if histogram else None
    if (
        page.get(f"{prefix}_median_error_millipoints") != median
        or page.get(f"{prefix}_p95_error_millipoints") != p95
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


def _aggregate_text_boxes(
    rows: Sequence[dict[str, Any]],
    *,
    prefix: str,
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


def _container_identity(configuration: dict[str, Any]) -> dict[str, Any]:
    identity = configuration.get("oracle_lock")
    if (
        not isinstance(identity, dict)
        or set(identity)
        != {
            "build_contract_sha256",
            "font_pack_sha256",
            "image",
            "libreoffice",
            "lock_file_sha256",
            "pdf_font_inspector",
            "runtime",
            "schema",
        }
        or identity.get("schema") != CONTAINER_IDENTITY_SCHEMA
    ):
        raise GateError("oracle_identity")
    image = identity.get("image")
    if not isinstance(image, dict) or set(image) != {
        "architecture",
        "config_digest",
        "expected_config_digest",
        "expected_manifest_digest",
        "identity_status",
        "manifest_digest",
    }:
        raise GateError("oracle_image")
    config_digest = image.get("config_digest")
    manifest_digest = image.get("manifest_digest")
    if (
        not isinstance(config_digest, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", config_digest) is None
        or image.get("expected_config_digest") != config_digest
        or not isinstance(manifest_digest, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", manifest_digest) is None
        or image.get("expected_manifest_digest") != manifest_digest
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
        or set(pdf_font_inspector)
        != {
            "host_tools_identity_sha256",
            "kind",
            "pdffonts_sha256",
            "pdfinfo_sha256",
            "pdftoppm_sha256",
            "pdftotext_sha256",
        }
        or pdf_font_inspector.get("kind") != "poppler"
    ):
        raise GateError("oracle_identity")
    for key in (
        "host_tools_identity_sha256",
        "pdffonts_sha256",
        "pdfinfo_sha256",
        "pdftoppm_sha256",
        "pdftotext_sha256",
    ):
        _sha(pdf_font_inspector.get(key), "oracle_identity")
    if identity.get("runtime") != "docker":
        raise GateError("oracle_identity")
    return identity


def _metric_policy(configuration: dict[str, Any]) -> None:
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
        or type(evidence.get("paper_code")) is not int
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
    if not isinstance(evidence, dict) or set(evidence) != {
        "embedded_font_objects",
        "font_objects",
        "matched_font_objects",
        "normalized_identities_sha256",
        "subset_font_objects",
        "unicode_font_objects",
        "unique_font_identities",
    }:
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


def _native_pdf_attestation(row: dict[str, Any]) -> tuple[int, int]:
    evidence = row.get("native_pdf_attestation")
    if not isinstance(evidence, dict) or set(evidence) != {
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
    }:
        raise GateError("native_pdf_attestation")
    documents = _integer(
        evidence["documents"], "native_pdf_attestation", minimum=1
    )
    objects = _integer(
        evidence["font_objects"], "native_pdf_attestation", minimum=1
    )
    for key in (
        "actual_text_documents",
        "charprocs_documents",
        "type3_documents",
    ):
        if _integer(
            evidence[key], "native_pdf_attestation", maximum=documents
        ) != documents:
            raise GateError("native_pdf_attestation")
    for key in (
        "embedded_font_objects",
        "subset_font_objects",
        "type3_font_objects",
        "unicode_font_objects",
    ):
        if _integer(
            evidence[key], "native_pdf_attestation", maximum=objects
        ) != objects:
            raise GateError("native_pdf_attestation")
    _sha(evidence["identity_set_sha256"], "native_pdf_attestation")
    return documents, objects


def _adapter(row: dict[str, Any], identity: dict[str, Any]) -> None:
    adapter = row.get("oracle_adapter")
    image = adapter.get("image") if isinstance(adapter, dict) else None
    expected_config = identity["image"]["config_digest"]
    expected_manifest = identity["image"]["manifest_digest"]
    if (
        not isinstance(adapter, dict)
        or set(adapter)
        != {
            "font_pack_sha256",
            "image",
            "lock_file_sha256",
            "lock_sha256",
            "oracle",
            "runtime",
            "schema",
        }
        or adapter.get("schema") != CONTAINER_EXECUTION_SCHEMA
        or not isinstance(image, dict)
        or set(image)
        != {
            "architecture",
            "expected_id",
            "expected_manifest_digest",
            "id",
            "identity_status",
            "manifest_digest",
        }
        or image.get("id") != expected_config
        or image.get("expected_id") != expected_config
        or image.get("manifest_digest") != expected_manifest
        or image.get("expected_manifest_digest") != expected_manifest
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
    expected_manifest_binding: dict[str, object] | None = None,
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
    measurement_toolchain = configuration.get("measurement_toolchain")
    if (
        not isinstance(measurement_toolchain, dict)
        or set(measurement_toolchain)
        != {
            "host_tools_identity_sha256",
            "kind",
            "pdffonts_sha256",
            "pdfinfo_sha256",
            "pdftoppm_sha256",
            "pdftotext_sha256",
        }
        or measurement_toolchain.get("kind") != "poppler"
    ):
        raise GateError("measurement_toolchain")
    for key in (
        "pdffonts_sha256",
        "pdfinfo_sha256",
        "pdftoppm_sha256",
        "pdftotext_sha256",
        "host_tools_identity_sha256",
    ):
        _sha(measurement_toolchain.get(key), "measurement_toolchain")
    if measurement_toolchain != identity["pdf_font_inspector"]:
        raise GateError("measurement_toolchain")
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
        or not type_exact_equal(summary.get("files"), len(files))
        or not type_exact_equal(
            summary.get("by_status"),
            {"compared": len(files)},
        )
        or not type_exact_equal(
            summary.get("by_classification"),
            {"within_threshold": len(files)},
        )
    ):
        raise GateError("workbook_coverage")
    _validate_complete_discovery(report.get("discovery"), len(files))
    try:
        validate_report_geometry(files)
    except GeometryContractError as error:
        raise GateError(str(error)) from error
    manifest_binding = _configuration_manifest_binding(
        configuration,
        files,
        expected=expected_manifest_binding,
    )

    failures: set[str] = set()
    page_errors: list[int] = []
    metric_pages: list[dict[str, Any]] = []
    file_similarities: list[int] = []
    text_box_histogram: Counter[int] = Counter()
    text_box_rxls_items = 0
    text_box_libreoffice_items = 0
    text_box_matched = 0
    text_box_ambiguous = 0
    text_box_rxls_unmatched = 0
    text_box_libreoffice_unmatched = 0
    scale_modes: Counter[str] = Counter()
    font_objects = 0
    native_pdf_documents = 0
    native_pdf_font_objects = 0
    point_geometry_mismatches = 0
    xhtml_crosscheck_max_micropoints = 0
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
        native_documents, native_objects = _native_pdf_attestation(row)
        native_pdf_documents += native_documents
        native_pdf_font_objects += native_objects
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
        _, _, semantic_rxls, semantic_libreoffice = _semantic_codepoint(pages)
        _, edge_rxls, edge_libreoffice = _edge_f1(pages)
        if semantic_rxls == 0 or semantic_libreoffice == 0:
            failures.add("semantic_population_empty")
        if edge_rxls == 0 or edge_libreoffice == 0:
            failures.add("edge_population_empty")
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
        _validate_page_mapping(pages, scenes)
        file_point_mismatches = 0
        file_point_max = 0
        file_crosscheck_max = 0
        for page in pages:
            if not isinstance(page, dict):
                raise GateError("page_mapping")
            metric_pages.append(page)
            (
                rxls_items,
                libreoffice_items,
                matched,
                ambiguous,
                rxls_unmatched,
                libreoffice_unmatched,
                histogram,
            ) = (
                _text_box_histogram(page)
            )
            text_box_rxls_items += rxls_items
            text_box_libreoffice_items += libreoffice_items
            text_box_matched += matched
            text_box_ambiguous += ambiguous
            text_box_rxls_unmatched += rxls_unmatched
            text_box_libreoffice_unmatched += libreoffice_unmatched
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
            if (rxls_width, rxls_height) != (lo_width, lo_height):
                failures.add("raster_page_box_mismatch")
            (
                point_error,
                crosscheck_error,
                exact_point_geometry,
            ) = _page_point_geometry(page)
            page_errors.append(point_error)
            file_point_max = max(file_point_max, point_error)
            file_crosscheck_max = max(
                file_crosscheck_max, crosscheck_error
            )
            file_point_mismatches += int(not exact_point_geometry)
            if not exact_point_geometry:
                failures.add("pdf_point_geometry_mismatch")
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
    edge_f1, edge_rxls_pixels, edge_libreoffice_pixels = _edge_f1(metric_pages)
    (
        semantic_precision,
        semantic_recall,
        semantic_rxls_items,
        semantic_libreoffice_items,
    ) = _semantic_codepoint(metric_pages)
    text_box_precision = _ratio_ppm(
        text_box_matched,
        text_box_rxls_items,
        empty=1_000_000,
    )
    text_box_recall = _ratio_ppm(
        text_box_matched,
        text_box_libreoffice_items,
        empty=1_000_000,
    )
    text_box_f1 = _ratio_ppm(
        2 * text_box_matched,
        text_box_rxls_items + text_box_libreoffice_items,
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
    text_line_box = _aggregate_text_boxes(
        metric_pages,
        prefix="text_line_box",
    )
    if similarity_mean < SIMILARITY_MEAN_MIN_PPM:
        failures.add("similarity_mean_below_target")
    if edge_f1 < EDGE_F1_MIN_PPM:
        failures.add("edge_f1_below_target")
    if semantic_precision < SEMANTIC_CODEPOINT_MIN_PPM:
        failures.add("semantic_codepoint_precision_below_target")
    if semantic_recall < SEMANTIC_CODEPOINT_MIN_PPM:
        failures.add("semantic_codepoint_recall_below_target")
    if text_box_precision < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_box_match_coverage_below_target")
        failures.add("text_box_precision_below_target")
    if text_box_recall < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_box_recall_below_target")
    if text_box_f1 < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_box_f1_below_target")
    if text_box_ambiguous != 0:
        failures.add("text_box_mapping_ambiguous")
    if text_box_rxls_unmatched != 0:
        failures.add("text_box_mapping_unmatched")
    if text_box_libreoffice_unmatched != 0:
        failures.add("text_box_reference_unmatched")
    if (
        text_box_median is None
        or text_box_median > TEXT_BOX_MEDIAN_MAX_MILLIPOINTS
    ):
        failures.add("text_box_median_error_above_target")
    if text_box_p95 is None or text_box_p95 > TEXT_BOX_P95_MAX_MILLIPOINTS:
        failures.add("text_box_p95_error_above_target")
    if int(text_line_box["matched"]) <= 0:
        failures.add("text_line_box_coverage_below_minimum")
    if int(text_line_box["precision_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_line_box_precision_below_target")
    if int(text_line_box["recall_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_line_box_recall_below_target")
    if int(text_line_box["f1_ppm"]) < TEXT_BOX_MATCH_MIN_PPM:
        failures.add("text_line_box_f1_below_target")
    if int(text_line_box["ambiguous"]) != 0:
        failures.add("text_line_box_mapping_ambiguous")
    if int(text_line_box["rxls_unmatched"]) != 0:
        failures.add("text_line_box_mapping_unmatched")
    if int(text_line_box["libreoffice_unmatched"]) != 0:
        failures.add("text_line_box_reference_unmatched")
    if (
        text_line_box["median_error_millipoints"] is None
        or int(text_line_box["median_error_millipoints"])
        > TEXT_BOX_MEDIAN_MAX_MILLIPOINTS
    ):
        failures.add("text_line_box_median_error_above_target")
    if (
        text_line_box["p95_error_millipoints"] is None
        or int(text_line_box["p95_error_millipoints"])
        > TEXT_BOX_P95_MAX_MILLIPOINTS
    ):
        failures.add("text_line_box_p95_error_above_target")
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
            "native_pdf_documents": native_pdf_documents,
            "native_pdf_font_objects": native_pdf_font_objects,
            "page_count_histogram": {
                str(key): value for key, value in sorted(page_count_histogram.items())
            },
            "pages": total_pages,
            "edge_libreoffice_pixels": edge_libreoffice_pixels,
            "edge_rxls_pixels": edge_rxls_pixels,
            "semantic_codepoint_libreoffice_items": semantic_libreoffice_items,
            "semantic_codepoint_rxls_items": semantic_rxls_items,
            "text_box_candidates": text_box_rxls_items,
            "text_box_libreoffice_items": text_box_libreoffice_items,
            "text_box_matches": text_box_matched,
            "text_line_box_candidates": text_line_box["rxls_items"],
            "text_line_box_libreoffice_items": text_line_box[
                "libreoffice_items"
            ],
            "text_line_box_matches": text_line_box["matched"],
            "workbooks": len(files),
        },
        "evidence": {
            "font_pack_sha256": font_pack_sha,
            "oracle_build_contract_sha256": identity["build_contract_sha256"],
            "oracle_image_config_digest": identity["image"]["config_digest"],
            "oracle_image_manifest_digest": identity["image"]["manifest_digest"],
            "oracle_lock_file_sha256": identity["lock_file_sha256"],
            "oracle_libreoffice_artifact_sha256": identity["libreoffice"][
                "artifact_sha256"
            ],
            "feature_map_sha256": manifest_binding["feature_map_sha256"],
            "host_tools_identity_sha256": measurement_toolchain[
                "host_tools_identity_sha256"
            ],
            "input_set_sha256": manifest_binding["input_set_sha256"],
            "manifest_sha256": manifest_binding["manifest_sha256"],
            "pdffonts_sha256": identity["pdf_font_inspector"]["pdffonts_sha256"],
            "pdfinfo_sha256": measurement_toolchain["pdfinfo_sha256"],
            "pdftoppm_sha256": measurement_toolchain["pdftoppm_sha256"],
            "pdftotext_sha256": measurement_toolchain["pdftotext_sha256"],
            "renderer_sha256": renderer_sha,
            "report_bytes": report_bytes,
            "report_sha256": report_sha256,
        },
        "expected": {
            "page_box_pixels": {
                "height": EXPECTED_PAGE_HEIGHT,
                "width": EXPECTED_PAGE_WIDTH,
            },
            "page_box_points": {
                "height": "792/1",
                "width": "612/1",
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
            "pdf_point_geometry_mismatches": point_geometry_mismatches,
            "pdf_xhtml_crosscheck_max_micropoints": (
                xhtml_crosscheck_max_micropoints
            ),
            "semantic_codepoint_precision_ppm": semantic_precision,
            "semantic_codepoint_recall_ppm": semantic_recall,
            "similarity_mean_ppm": similarity_mean,
            "text_box_ambiguous": text_box_ambiguous,
            "text_box_f1_ppm": text_box_f1,
            "text_box_match_coverage_ppm": text_box_precision,
            "text_box_median_error_millipoints": text_box_median,
            "text_box_p95_error_millipoints": text_box_p95,
            "text_box_precision_ppm": text_box_precision,
            "text_box_recall_ppm": text_box_recall,
            "text_box_unmatched": text_box_rxls_unmatched,
            "text_box_libreoffice_unmatched": text_box_libreoffice_unmatched,
            "text_line_box_f1_ppm": text_line_box["f1_ppm"],
            "text_line_box_median_error_millipoints": text_line_box[
                "median_error_millipoints"
            ],
            "text_line_box_p95_error_millipoints": text_line_box[
                "p95_error_millipoints"
            ],
            "text_line_box_precision_ppm": text_line_box["precision_ppm"],
            "text_line_box_recall_ppm": text_line_box["recall_ppm"],
            "text_line_box_ambiguous": text_line_box["ambiguous"],
            "text_line_box_unmatched": text_line_box["rxls_unmatched"],
            "text_line_box_libreoffice_unmatched": text_line_box[
                "libreoffice_unmatched"
            ],
        },
        "passed": not failures,
        "schema": OUTPUT_SCHEMA,
        "thresholds": {
            "edge_f1_min_ppm": EDGE_F1_MIN_PPM,
            "page_box_max_millipoints": PAGE_MAX_MILLIPOINTS,
            "page_box_median_max_millipoints": PAGE_MEDIAN_MAX_MILLIPOINTS,
            "page_box_p95_max_millipoints": PAGE_P95_MAX_MILLIPOINTS,
            "pdf_point_geometry_exact": True,
            "pdf_xhtml_crosscheck_max_micropoints": (
                PDF_XHTML_CROSSCHECK_MAX_MICROPOINTS
            ),
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
    parser.add_argument("--campaign-manifest", type=Path)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    if not 1 <= args.expected_workbooks <= MAX_WORKBOOKS:
        print("check-authored-print-parity: expected_workbooks", file=sys.stderr)
        return 2
    try:
        report, digest, size = _read(args.report)
        expected_manifest_binding = (
            _campaign_manifest_binding(args.campaign_manifest)
            if args.campaign_manifest is not None
            else None
        )
        result = evaluate(
            report,
            report_sha256=digest,
            report_bytes=size,
            expected_workbooks=args.expected_workbooks,
            expected_manifest_binding=expected_manifest_binding,
        )
    except GateError as error:
        print(f"check-authored-print-parity: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
