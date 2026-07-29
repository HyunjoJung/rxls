#!/usr/bin/env python3
"""Reduce one locked OOXML row campaign report to path-neutral geometry facts."""

from __future__ import annotations

import argparse
import copy
from fractions import Fraction
from hashlib import sha256
import json
import os
from pathlib import Path
import re
import stat
import tempfile
from typing import Iterable

try:
    from strict_json_contract import type_exact_equal
except ModuleNotFoundError:  # Imported as ``scripts.*`` by unit tests.
    from scripts.strict_json_contract import type_exact_equal


REPORT_SCHEMA = "rxls.libreoffice-render-parity.v1"
OUTPUT_SCHEMA = "rxls.ooxml-row-oracle.v3"
MANIFEST_BINDING_SCHEMA = "rxls.render-parity-manifest-binding.v1"
METRIC_CONTRACT_SCHEMA = "rxls.render-parity-metrics.v2"
CONTAINER_IDENTITY_SCHEMA = "rxls.render-oracle-container-identity.v2"
PROFILE = "ooxml-row-diagnostic"
GENERATOR = "rxls-ooxml-row-diagnostic"
GENERATOR_VERSION = "1.1.0"
CASE_COUNT = 24
BASELINE_CASE_COUNT = 12
BASELINE_MAX_ABSOLUTE_HEIGHT_DELTA_MILLIPOINTS = 50
PRINT_MODE_SINGLE_PAGE = "single-page-sheets"
LIBREOFFICE_ARTIFACT_SHA256 = (
    "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5"
)

MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_MANIFEST_BYTES = 256 * 1024
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_INTEGER_DIGITS = 128
MAX_UNIQUE_GEOMETRY_ITEMS = 250_000
MAX_UNIQUE_GEOMETRY_EXACT_DELTA_MILLIPOINTS = 1_000_000_000
MAX_UNIQUE_GEOMETRY_REPORT_PAGES = 2_000
MAX_UNIQUE_GEOMETRY_REPORT_HISTOGRAM_BUCKETS = 50_000
UNIQUE_GEOMETRY_OUTER_LIMIT_MILLIPOINTS = 10_000
UNIQUE_GEOMETRY_OVERFLOW_MILLIPOINTS = 12_000
UNIQUE_GEOMETRY_BUCKETS = frozenset(
    {
        *range(-2, 3),
        *(
            sign * magnitude
            for sign in (-1, 1)
            for magnitude in (500, 1_000)
        ),
        *(
            sign * magnitude
            for sign in (-1, 1)
            for magnitude in range(2_000, 10_001, 2_000)
        ),
        -UNIQUE_GEOMETRY_OVERFLOW_MILLIPOINTS,
        UNIQUE_GEOMETRY_OVERFLOW_MILLIPOINTS,
    }
)
MAX_UNIQUE_GEOMETRY_BUCKETS = 21
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
IMAGE_DIGEST_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
POINT_RE = re.compile(r"^-?[0-9]+/[1-9][0-9]*$")
FORBIDDEN_OUTPUT_KEY_RE = re.compile(
    r"(?:^|_)(?:path|label|text|content|command|url|file)(?:_|$)"
)
FORBIDDEN_OUTPUT_VALUE_RE = re.compile(
    r"(?:[\\/]|(?:^|[.])(?:xlsx?|xlsb|xlsm|ods|fods|pdf|png|svg)$)",
    re.IGNORECASE,
)

MANIFEST_KEYS = frozenset(
    {
        "case_count",
        "feature_counts",
        "files",
        "format_counts",
        "format_feature_counts",
        "generator",
        "generator_version",
        "license",
        "profile",
        "redistribution",
        "render_redistributable",
        "rights_tier",
        "schema_version",
        "source_redistributable",
        "total_bytes",
    }
)
MANIFEST_FILE_KEYS = frozenset(
    {
        "byte_length",
        "case_id",
        "features",
        "format",
        "generator",
        "generator_version",
        "license",
        "path",
        "redistribution",
        "render_redistributable",
        "rights_tier",
        "seed",
        "sha256",
        "source_redistributable",
    }
)
REPORT_CONFIGURATION_KEYS = frozenset(
    {
        "caps",
        "dpi",
        "font_pack",
        "lane_filter",
        "locale",
        "manifest_binding",
        "measurement_toolchain",
        "metric_policy",
        "min_similarity_ppm",
        "oracle_lock",
        "print_mode",
        "renderer_binary",
    }
)
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
UNIQUE_GEOMETRY_AXES = (
    "x_min",
    "x_max",
    "y_min",
    "y_max",
    "center_x",
    "center_y",
    "width",
    "height",
)
UNIQUE_GEOMETRY_KEYS = frozenset(
    {
        "rxls_unique_items",
        "libreoffice_unique_items",
        "matched_items",
        "delta_histograms_millipoints",
        "exact_delta_summaries_millipoints",
    }
)
UNIQUE_GEOMETRY_POLICY = {
    "content_retained": False,
    "coordinates": "pdf_points_y_down",
    "delta_direction": "rxls_minus_libreoffice",
    "diagnostic_only": True,
    "exact_delta_absolute_limit_millipoints": (
        MAX_UNIQUE_GEOMETRY_EXACT_DELTA_MILLIPOINTS
    ),
    "exact_summary": "count_sum_min_max_and_signed_overflow_counts",
    "histogram": {
        "exact_absolute_limit_millipoints": 2,
        "max_buckets_per_axis": MAX_UNIQUE_GEOMETRY_BUCKETS,
        "middle_absolute_limit_millipoints": 1_000,
        "middle_bucket_width_millipoints": 500,
        "outer_absolute_limit_millipoints": (
            UNIQUE_GEOMETRY_OUTER_LIMIT_MILLIPOINTS
        ),
        "outer_bucket_width_millipoints": 2_000,
        "overflow_bucket_absolute_millipoints": (
            UNIQUE_GEOMETRY_OVERFLOW_MILLIPOINTS
        ),
        "rounding": (
            "nearest_width_multiple_half_away_from_zero_"
            "with_nonzero_sign_preserved"
        ),
    },
    "max_geometry_pages_per_report": (
        MAX_UNIQUE_GEOMETRY_REPORT_PAGES
    ),
    "max_histogram_buckets_per_report": (
        MAX_UNIQUE_GEOMETRY_REPORT_HISTOGRAM_BUCKETS
    ),
    "max_items_per_side_per_page": MAX_UNIQUE_GEOMETRY_ITEMS,
    "matching": "exact_normalized_token_tuple_unique_on_both_sides",
    "rounding": "nearest_millipoint_half_away_from_zero_exact_rational",
    "shard_budget": "equal_floor_partition_by_declared_shard_count",
    "units": "millipoints",
}
TOGGLE_FEATURES = {
    "auto-bold-font": "auto_bold_font",
    "auto-bold-font-wrapped": "auto_bold_font_wrapped",
    "auto-large-font": "auto_large_font",
    "auto-long-unwrapped": "auto_long_unwrapped",
    "auto-wrapped-explicit": "auto_wrapped_explicit",
    "auto-wrapped-hidden": "auto_wrapped_hidden",
    "auto-wrapped-image": "auto_wrapped_image",
    "auto-wrapped-long": "auto_wrapped_long",
    "auto-wrapped-long-anchor": "auto_wrapped_long_anchor",
    "auto-wrapped-merged": "auto_wrapped_merged",
    "auto-wrapped-rtl": "auto_wrapped_rtl",
    "auto-wrapped-wide": "auto_wrapped_wide",
    "explicit-row-height": "explicit_row_height",
    "hidden-row": "hidden_row",
    "image-drawing": "image_drawing",
    "right-to-left-layout": "right_to_left_layout",
}
BASELINE_TOGGLE_VALUES = frozenset(
    {
        "explicit_row_height",
        "hidden_row",
        "image_drawing",
        "none",
        "right_to_left_layout",
    }
)
EXPECTED_TOGGLE_COUNTS = {
    "none": 8,
    **{value: 1 for value in TOGGLE_FEATURES.values()},
}


class DiagnosticError(ValueError):
    """Raised when diagnostic evidence is absent, ambiguous, or pathful."""


class _StrictJSONError(ValueError):
    pass


def _require(condition: bool, code: str) -> None:
    if not condition:
        raise DiagnosticError(code)


def _object_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise DiagnosticError("duplicate_json_key")
        result[key] = value
    return result


def _reject_json_constant(_value: str) -> object:
    raise _StrictJSONError("invalid_json_constant")


def _reject_json_number(_value: str) -> object:
    raise _StrictJSONError("non_integral_number")


def _parse_json_integer(token: str) -> int:
    digits = token[1:] if token.startswith("-") else token
    if len(digits) > MAX_INTEGER_DIGITS:
        raise _StrictJSONError("integer_limit")
    return int(token)


def _preflight_json_text(text: str) -> None:
    closers: list[str] = []
    nodes = 0
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
            nodes += 1
            _require(nodes <= MAX_JSON_NODES, "json_complexity")
            closers.append("]" if character == "[" else "}")
            _require(len(closers) <= MAX_JSON_DEPTH, "json_depth")
        elif character in "]}":
            _require(bool(closers) and closers.pop() == character, "json_structure")
        elif character == ",":
            nodes += 1
            _require(nodes <= MAX_JSON_NODES, "json_complexity")
        elif character == "-" or character.isdigit():
            start = index
            if character == "-":
                index += 1
            digit_start = index
            while index < len(text) and text[index].isdigit():
                index += 1
            if index == digit_start:
                index = start + 1
                continue
            _require(index - digit_start <= MAX_INTEGER_DIGITS, "integer_limit")
            if index < len(text) and text[index] in ".eE":
                raise DiagnosticError("non_integral_number")
            continue
        index += 1
    _require(not closers, "json_structure")


def _stat_signature(value: os.stat_result) -> tuple[int, ...]:
    return (
        value.st_dev,
        value.st_ino,
        value.st_mode,
        value.st_size,
        value.st_mtime_ns,
        value.st_ctime_ns,
    )


def _read_bounded_regular_file(
    path: Path,
    maximum: int,
    code: str,
) -> bytes:
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
        _require(stat.S_ISREG(before.st_mode), code)
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
    except DiagnosticError:
        raise
    except OSError as error:
        raise DiagnosticError(code) from error
    finally:
        if descriptor is not None:
            try:
                os.close(descriptor)
            except OSError:
                pass
    _require(
        stat.S_ISREG(current.st_mode)
        and _stat_signature(before) == _stat_signature(after)
        and _stat_signature(after) == _stat_signature(current),
        code,
    )
    payload = b"".join(chunks)
    _require(0 < len(payload) <= maximum, f"{code}_limit")
    _require(len(payload) == after.st_size, code)
    return payload


def _load_json(path: Path, maximum: int, code: str) -> tuple[dict[str, object], bytes]:
    payload = _read_bounded_regular_file(path, maximum, code)
    try:
        text = payload.decode("utf-8")
        _preflight_json_text(text)
        value = json.loads(
            text,
            object_pairs_hook=_object_pairs,
            parse_constant=_reject_json_constant,
            parse_float=_reject_json_number,
            parse_int=_parse_json_integer,
        )
    except DiagnosticError:
        raise
    except (OSError, UnicodeDecodeError, ValueError, RecursionError) as error:
        raise DiagnosticError(code) from error
    _require(isinstance(value, dict), code)
    return value, payload


def _canonical_json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def _sha(value: object, code: str) -> str:
    _require(isinstance(value, str) and SHA256_RE.fullmatch(value) is not None, code)
    return value


def _image_digest(value: object, code: str) -> str:
    _require(
        isinstance(value, str) and IMAGE_DIGEST_RE.fullmatch(value) is not None,
        code,
    )
    return value


def _positive_int(value: object, code: str, maximum: int = 2**63 - 1) -> int:
    _require(
        isinstance(value, int)
        and not isinstance(value, bool)
        and 0 < value <= maximum,
        code,
    )
    return value


def _nonnegative_int(
    value: object, code: str, maximum: int = 2**63 - 1
) -> int:
    _require(
        isinstance(value, int)
        and not isinstance(value, bool)
        and 0 <= value <= maximum,
        code,
    )
    return value


def _dimension_from_features(features: Iterable[str]) -> dict[str, object]:
    values = frozenset(features)
    _require("ooxml-implicit-row" in values, "manifest_feature_contract")
    sheet_states = values & {"sheet-format-missing", "sheet-format-present"}
    fonts = values & {"normal-font-noto", "normal-font-carlito"}
    sizes = values & {"normal-size-11", "normal-size-12"}
    toggles = values & set(TOGGLE_FEATURES)
    _require(
        len(sheet_states) == len(fonts) == len(sizes) == 1
        and len(toggles) <= 1
        and values
        == {
            "ooxml-implicit-row",
            *sheet_states,
            *fonts,
            *sizes,
            *toggles,
        },
        "manifest_feature_contract",
    )
    sheet_state = next(iter(sheet_states))
    font = next(iter(fonts))
    size = next(iter(sizes))
    toggle = next(iter(toggles), None)
    dimension = {
        "normal_font": font.removeprefix("normal-font-"),
        "normal_size_points": int(size.removeprefix("normal-size-")),
        "sheet_format": sheet_state.removeprefix("sheet-format-"),
        "toggle": TOGGLE_FEATURES[toggle] if toggle is not None else "none",
    }
    if toggle is not None:
        _require(
            dimension
            == {
                "normal_font": "noto",
                "normal_size_points": 11,
                "sheet_format": "missing",
                "toggle": TOGGLE_FEATURES[toggle],
            },
            "manifest_stress_contract",
        )
    return dimension


def _expected_dimensions() -> list[dict[str, object]]:
    rows = [
        {
            "normal_font": font,
            "normal_size_points": size,
            "sheet_format": state,
            "toggle": "none",
        }
        for state in ("missing", "present")
        for font in ("noto", "carlito")
        for size in (11, 12)
    ]
    rows.extend(
        {
            "normal_font": "noto",
            "normal_size_points": 11,
            "sheet_format": "missing",
            "toggle": toggle,
        }
        for toggle in sorted(TOGGLE_FEATURES.values())
    )
    return sorted(rows, key=_dimension_key)


def _dimension_key(row: dict[str, object]) -> tuple[object, ...]:
    return (
        row["sheet_format"],
        row["normal_font"],
        row["normal_size_points"],
        row["toggle"],
    )


def _validate_manifest(
    manifest: dict[str, object], payload: bytes
) -> tuple[dict[str, dict[str, object]], dict[str, object]]:
    _require(set(manifest) == MANIFEST_KEYS, "manifest_keys")
    _require(
        type(manifest.get("schema_version")) is int
        and manifest.get("schema_version") == 1
        and manifest.get("profile") == PROFILE
        and manifest.get("generator") == GENERATOR
        and manifest.get("generator_version") == GENERATOR_VERSION
        and manifest.get("case_count") == CASE_COUNT
        and manifest.get("format_counts") == {"xlsx": CASE_COUNT}
        and manifest.get("license") == "MIT"
        and manifest.get("redistribution") == "allowed"
        and manifest.get("render_redistributable") is True
        and manifest.get("rights_tier") == "S"
        and manifest.get("source_redistributable") is True,
        "manifest_contract",
    )
    rows = manifest.get("files")
    _require(isinstance(rows, list) and len(rows) == CASE_COUNT, "manifest_files")
    paths: dict[str, dict[str, object]] = {}
    hashes: set[str] = set()
    dimensions: list[dict[str, object]] = []
    feature_counts: dict[str, int] = {}
    total_bytes = 0
    feature_mapping: list[dict[str, object]] = []
    for index, raw in enumerate(rows):
        _require(isinstance(raw, dict) and set(raw) == MANIFEST_FILE_KEYS, "manifest_file")
        case_id = raw.get("case_id")
        path = raw.get("path")
        features = raw.get("features")
        digest = raw.get("sha256")
        byte_length = raw.get("byte_length")
        _require(
            isinstance(case_id, str)
            and re.fullmatch(r"[a-z0-9-]{1,96}", case_id) is not None
            and path == f"payload/xlsx/{case_id}.xlsx"
            and isinstance(features, list)
            and all(
                isinstance(feature, str)
                and re.fullmatch(r"[a-z][a-z0-9-]{0,63}", feature) is not None
                for feature in features
            )
            and features == sorted(set(features))
            and isinstance(digest, str)
            and SHA256_RE.fullmatch(digest) is not None
            and isinstance(byte_length, int)
            and not isinstance(byte_length, bool)
            and 0 < byte_length <= 256 * 1024
            and raw.get("format") == "xlsx"
            and raw.get("generator") == GENERATOR
            and raw.get("generator_version") == GENERATOR_VERSION
            and raw.get("license") == "MIT"
            and raw.get("redistribution") == "allowed"
            and raw.get("render_redistributable") is True
            and raw.get("rights_tier") == "S"
            and raw.get("seed") == 550_000 + index
            and raw.get("source_redistributable") is True,
            "manifest_file_contract",
        )
        _require(path not in paths and digest not in hashes, "manifest_file_identity")
        dimension = _dimension_from_features(features)
        dimensions.append(dimension)
        total_bytes += byte_length
        for feature in features:
            feature_counts[feature] = feature_counts.get(feature, 0) + 1
        paths[path] = {
            "byte_length": byte_length,
            "dimension": dimension,
            "features": features,
            "sha256": digest,
        }
        hashes.add(digest)
        feature_mapping.append(
            {"features": features, "format": "xlsx", "sha256": digest}
        )
    _require(
        sorted(dimensions, key=_dimension_key) == _expected_dimensions(),
        "manifest_matrix",
    )
    feature_counts = dict(sorted(feature_counts.items()))
    _require(
        manifest.get("total_bytes") == total_bytes
        and manifest.get("feature_counts") == feature_counts
        and manifest.get("format_feature_counts") == {"xlsx": feature_counts},
        "manifest_counts",
    )
    input_hashes = sorted(hashes)
    feature_mapping.sort(
        key=lambda row: (
            str(row["sha256"]),
            str(row["format"]),
            tuple(row["features"]),
        )
    )
    binding = {
        "feature_map_sha256": sha256(
            _canonical_json_bytes(feature_mapping)
        ).hexdigest(),
        "input_set_sha256": sha256(_canonical_json_bytes(input_hashes)).hexdigest(),
        "manifest_sha256": sha256(payload).hexdigest(),
        "schema": MANIFEST_BINDING_SCHEMA,
        "selected_case_count": CASE_COUNT,
    }
    return paths, binding


def _validate_toolchain(value: object) -> dict[str, str]:
    keys = {
        "host_tools_identity_sha256",
        "kind",
        "pdffonts_sha256",
        "pdfinfo_sha256",
        "pdftoppm_sha256",
        "pdftotext_sha256",
    }
    _require(isinstance(value, dict) and set(value) == keys, "toolchain_identity")
    _require(value.get("kind") == "poppler", "toolchain_identity")
    return {
        key: _sha(value.get(key), "toolchain_identity")
        for key in sorted(keys - {"kind"})
    }


def _validate_oracle_identity(
    value: object, toolchain: dict[str, str], font_pack_sha256: str
) -> dict[str, str]:
    keys = {
        "build_contract_sha256",
        "font_pack_sha256",
        "image",
        "libreoffice",
        "lock_file_sha256",
        "pdf_font_inspector",
        "runtime",
        "schema",
    }
    _require(isinstance(value, dict) and set(value) == keys, "oracle_identity")
    _require(
        value.get("schema") == CONTAINER_IDENTITY_SCHEMA
        and value.get("font_pack_sha256") == font_pack_sha256
        and value.get("runtime") in {"docker", "podman"},
        "oracle_identity",
    )
    image = value.get("image")
    _require(
        isinstance(image, dict)
        and set(image)
        == {
            "architecture",
            "config_digest",
            "expected_config_digest",
            "expected_manifest_digest",
            "identity_status",
            "manifest_digest",
        }
        and image.get("architecture") == "linux/amd64"
        and image.get("identity_status") == "pinned_match",
        "oracle_image_identity",
    )
    config_digest = _image_digest(image.get("config_digest"), "oracle_image_identity")
    manifest_digest = _image_digest(
        image.get("manifest_digest"), "oracle_image_identity"
    )
    _require(
        image.get("expected_config_digest") == config_digest
        and image.get("expected_manifest_digest") == manifest_digest,
        "oracle_image_identity",
    )
    libreoffice = value.get("libreoffice")
    _require(
        isinstance(libreoffice, dict)
        and set(libreoffice) == {"artifact_sha256", "name", "version"}
        and libreoffice.get("artifact_sha256") == LIBREOFFICE_ARTIFACT_SHA256
        and libreoffice.get("name") == "LibreOffice"
        and libreoffice.get("version") == "26.2.3.2",
        "libreoffice_identity",
    )
    inspector = _validate_toolchain(value.get("pdf_font_inspector"))
    _require(inspector == toolchain, "oracle_toolchain_identity")
    return {
        "container_build_contract_sha256": _sha(
            value.get("build_contract_sha256"), "oracle_identity"
        ),
        "container_config_digest": config_digest,
        "container_execution_lock_sha256": _sha(
            value.get("lock_file_sha256"), "oracle_identity"
        ),
        "container_manifest_digest": manifest_digest,
        "libreoffice_artifact_sha256": LIBREOFFICE_ARTIFACT_SHA256,
    }


def _fraction(value: object, code: str, *, positive: bool = False) -> Fraction:
    _require(isinstance(value, str) and POINT_RE.fullmatch(value) is not None, code)
    result = Fraction(value)
    if positive:
        _require(0 < result <= 1_000_000, code)
    return result


def _geometry(value: object) -> dict[str, tuple[Fraction, Fraction]]:
    _require(
        isinstance(value, dict)
        and set(value) == {"crop_box", "media_box", "page_size"},
        "pdf_point_geometry",
    )
    result: dict[str, tuple[Fraction, Fraction]] = {}
    for name in ("crop_box", "media_box", "page_size"):
        row = value.get(name)
        _require(
            isinstance(row, dict)
            and set(row) == {"height_points", "width_points"},
            "pdf_point_geometry",
        )
        result[name] = (
            _fraction(row.get("width_points"), "pdf_point_geometry", positive=True),
            _fraction(row.get("height_points"), "pdf_point_geometry", positive=True),
        )
    return result


def _millipoints(value: Fraction) -> int:
    converted = value * 1000
    _require(converted.denominator == 1, "non_integral_millipoints")
    _require(abs(converted.numerator) <= 1_000_000_000, "millipoint_limit")
    return converted.numerator


def _unique_geometry_bucket(delta_millipoints: int) -> int:
    magnitude = abs(delta_millipoints)
    if magnitude <= 2:
        return delta_millipoints
    if magnitude <= 1_000:
        width = 500
        bucket = max(width, (magnitude + width // 2) // width * width)
        return -bucket if delta_millipoints < 0 else bucket
    if magnitude <= UNIQUE_GEOMETRY_OUTER_LIMIT_MILLIPOINTS:
        width = 2_000
        bucket = (magnitude + width // 2) // width * width
        return -bucket if delta_millipoints < 0 else bucket
    return (
        -UNIQUE_GEOMETRY_OVERFLOW_MILLIPOINTS
        if delta_millipoints < 0
        else UNIQUE_GEOMETRY_OVERFLOW_MILLIPOINTS
    )


def _unique_geometry_bucket_interval(
    bucket_millipoints: int,
) -> tuple[int, int]:
    magnitude = abs(bucket_millipoints)
    if magnitude <= 2:
        lower = magnitude
        upper = magnitude
    elif magnitude <= 1_000:
        width = 500
        lower = 3 if magnitude == width else magnitude - width // 2
        upper = min(1_000, magnitude + width // 2 - 1)
    elif magnitude <= UNIQUE_GEOMETRY_OUTER_LIMIT_MILLIPOINTS:
        width = 2_000
        lower = max(1_001, magnitude - width // 2)
        upper = min(
            UNIQUE_GEOMETRY_OUTER_LIMIT_MILLIPOINTS,
            magnitude + width // 2 - 1,
        )
    else:
        _require(
            magnitude == UNIQUE_GEOMETRY_OVERFLOW_MILLIPOINTS,
            "unique_geometry_bucket",
        )
        lower = UNIQUE_GEOMETRY_OUTER_LIMIT_MILLIPOINTS + 1
        upper = MAX_UNIQUE_GEOMETRY_EXACT_DELTA_MILLIPOINTS
    return (-upper, -lower) if bucket_millipoints < 0 else (lower, upper)


def _unique_geometry_sum_bounds(
    histogram: dict[int, int],
    minimum: int,
    maximum: int,
    code: str,
) -> tuple[int, int]:
    minimum_bucket = _unique_geometry_bucket(minimum)
    maximum_bucket = _unique_geometry_bucket(maximum)
    _require(
        not (
            minimum < maximum
            and minimum_bucket == maximum_bucket
            and histogram[minimum_bucket] < 2
        ),
        code,
    )
    lower_total = 0
    upper_total = 0
    effective: dict[int, tuple[int, int]] = {}
    for bucket, count in histogram.items():
        bucket_lower, bucket_upper = _unique_geometry_bucket_interval(bucket)
        lower = max(bucket_lower, minimum)
        upper = min(bucket_upper, maximum)
        _require(lower <= upper, code)
        effective[bucket] = (lower, upper)
        lower_total += lower * count
        upper_total += upper * count
    lower_total += maximum - effective[maximum_bucket][0]
    upper_total -= effective[minimum_bucket][1] - minimum
    _require(lower_total <= upper_total, code)
    return lower_total, upper_total


def _validate_unique_geometry_axis_identities(
    summaries: dict[str, dict[str, int | None]],
    matched: int,
    code: str,
) -> None:
    sums = {
        axis: int(summary["sum_delta_millipoints"])
        for axis, summary in summaries.items()
    }
    _require(
        abs(sums["width"] - (sums["x_max"] - sums["x_min"]))
        <= matched
        and abs(
            2 * sums["center_x"] - sums["x_min"] - sums["x_max"]
        )
        <= matched
        and abs(sums["height"] - (sums["y_max"] - sums["y_min"]))
        <= matched
        and abs(
            2 * sums["center_y"] - sums["y_min"] - sums["y_max"]
        )
        <= matched,
        code,
    )


def _unique_geometry(value: object, code: str) -> dict[str, object]:
    _require(
        isinstance(value, dict) and set(value) == UNIQUE_GEOMETRY_KEYS,
        f"{code}_contract",
    )
    rxls_unique = _nonnegative_int(
        value.get("rxls_unique_items"),
        f"{code}_count",
        MAX_UNIQUE_GEOMETRY_ITEMS,
    )
    libreoffice_unique = _nonnegative_int(
        value.get("libreoffice_unique_items"),
        f"{code}_count",
        MAX_UNIQUE_GEOMETRY_ITEMS,
    )
    matched = _nonnegative_int(
        value.get("matched_items"),
        f"{code}_count",
        MAX_UNIQUE_GEOMETRY_ITEMS,
    )
    _require(
        matched <= min(rxls_unique, libreoffice_unique),
        f"{code}_count",
    )
    raw_histograms = value.get("delta_histograms_millipoints")
    raw_summaries = value.get("exact_delta_summaries_millipoints")
    _require(
        isinstance(raw_histograms, dict)
        and set(raw_histograms) == set(UNIQUE_GEOMETRY_AXES),
        f"{code}_axes",
    )
    _require(
        isinstance(raw_summaries, dict)
        and set(raw_summaries) == set(UNIQUE_GEOMETRY_AXES),
        f"{code}_exact_summary",
    )
    histograms: dict[str, list[dict[str, int]]] = {}
    summaries: dict[str, dict[str, int | None]] = {}
    for axis in UNIQUE_GEOMETRY_AXES:
        raw_rows = raw_histograms.get(axis)
        _require(
            isinstance(raw_rows, list)
            and len(raw_rows)
            <= min(matched, MAX_UNIQUE_GEOMETRY_BUCKETS),
            f"{code}_histogram",
        )
        rows: list[dict[str, int]] = []
        previous: int | None = None
        population = 0
        for raw_row in raw_rows:
            _require(
                isinstance(raw_row, dict)
                and set(raw_row) == {"delta_millipoints", "count"},
                f"{code}_histogram",
            )
            delta = raw_row.get("delta_millipoints")
            _require(
                isinstance(delta, int)
                and not isinstance(delta, bool)
                and delta in UNIQUE_GEOMETRY_BUCKETS,
                f"{code}_histogram",
            )
            count = _positive_int(
                raw_row.get("count"),
                f"{code}_histogram",
                MAX_UNIQUE_GEOMETRY_ITEMS,
            )
            _require(
                previous is None or delta > previous,
                f"{code}_histogram_order",
            )
            population += count
            _require(population <= matched, f"{code}_histogram_population")
            previous = delta
            rows.append({"delta_millipoints": delta, "count": count})
        _require(population == matched, f"{code}_histogram_population")
        histograms[axis] = rows

        raw_summary = raw_summaries.get(axis)
        _require(
            isinstance(raw_summary, dict)
            and set(raw_summary)
            == {
                "count",
                "max_delta_millipoints",
                "min_delta_millipoints",
                "negative_overflow_items",
                "positive_overflow_items",
                "sum_delta_millipoints",
            },
            f"{code}_exact_summary",
        )
        count = _nonnegative_int(
            raw_summary.get("count"),
            f"{code}_exact_summary",
            MAX_UNIQUE_GEOMETRY_ITEMS,
        )
        negative_overflow = _nonnegative_int(
            raw_summary.get("negative_overflow_items"),
            f"{code}_exact_summary",
            MAX_UNIQUE_GEOMETRY_ITEMS,
        )
        positive_overflow = _nonnegative_int(
            raw_summary.get("positive_overflow_items"),
            f"{code}_exact_summary",
            MAX_UNIQUE_GEOMETRY_ITEMS,
        )
        total = raw_summary.get("sum_delta_millipoints")
        minimum = raw_summary.get("min_delta_millipoints")
        maximum = raw_summary.get("max_delta_millipoints")
        _require(
            count == matched
            and isinstance(total, int)
            and not isinstance(total, bool)
            and abs(total)
            <= matched * MAX_UNIQUE_GEOMETRY_EXACT_DELTA_MILLIPOINTS
            and negative_overflow + positive_overflow <= matched,
            f"{code}_exact_summary",
        )
        if matched == 0:
            _require(
                minimum is None
                and maximum is None
                and total == 0
                and negative_overflow == 0
                and positive_overflow == 0,
                f"{code}_exact_summary",
            )
        else:
            _require(
                isinstance(minimum, int)
                and not isinstance(minimum, bool)
                and isinstance(maximum, int)
                and not isinstance(maximum, bool)
                and -MAX_UNIQUE_GEOMETRY_EXACT_DELTA_MILLIPOINTS
                <= minimum
                <= maximum
                <= MAX_UNIQUE_GEOMETRY_EXACT_DELTA_MILLIPOINTS
                and matched * minimum <= total <= matched * maximum,
                f"{code}_exact_summary",
            )
            _require(
                (negative_overflow > 0)
                == (minimum < -UNIQUE_GEOMETRY_OUTER_LIMIT_MILLIPOINTS)
                and (positive_overflow > 0)
                == (maximum > UNIQUE_GEOMETRY_OUTER_LIMIT_MILLIPOINTS),
                f"{code}_exact_summary",
            )
            if matched == 1:
                _require(
                    minimum == maximum == total,
                    f"{code}_exact_summary",
                )

        histogram_counts = {
            row["delta_millipoints"]: row["count"] for row in rows
        }
        _require(
            histogram_counts.get(
                -UNIQUE_GEOMETRY_OVERFLOW_MILLIPOINTS, 0
            )
            == negative_overflow
            and histogram_counts.get(
                UNIQUE_GEOMETRY_OVERFLOW_MILLIPOINTS, 0
            )
            == positive_overflow,
            f"{code}_exact_summary",
        )
        if matched > 0:
            _require(
                _unique_geometry_bucket(minimum)
                == rows[0]["delta_millipoints"]
                and _unique_geometry_bucket(maximum)
                == rows[-1]["delta_millipoints"],
                f"{code}_exact_summary",
            )
            sum_lower, sum_upper = _unique_geometry_sum_bounds(
                histogram_counts,
                minimum,
                maximum,
                f"{code}_exact_summary",
            )
            _require(
                sum_lower <= total <= sum_upper,
                f"{code}_exact_summary",
            )
        summaries[axis] = {
            "count": count,
            "max_delta_millipoints": maximum,
            "min_delta_millipoints": minimum,
            "negative_overflow_items": negative_overflow,
            "positive_overflow_items": positive_overflow,
            "sum_delta_millipoints": total,
        }
    _validate_unique_geometry_axis_identities(
        summaries, matched, f"{code}_exact_summary"
    )
    return {
        "rxls_unique_items": rxls_unique,
        "libreoffice_unique_items": libreoffice_unique,
        "matched_items": matched,
        "delta_histograms_millipoints": histograms,
        "exact_delta_summaries_millipoints": summaries,
    }


def _extract_height(page: object) -> tuple[int, int, int]:
    _require(isinstance(page, dict), "page_geometry")
    point = page.get("pdf_point_geometry")
    _require(
        isinstance(point, dict)
        and set(point) == {"deltas_points", "libreoffice", "rxls", "xhtml"},
        "pdf_point_geometry",
    )
    rxls = _geometry(point.get("rxls"))
    libreoffice = _geometry(point.get("libreoffice"))
    deltas = point.get("deltas_points")
    _require(
        isinstance(deltas, dict) and set(deltas) == PDF_POINT_DELTA_KEYS,
        "pdf_point_deltas",
    )
    parsed_deltas = {
        key: _fraction(value, "pdf_point_deltas")
        for key, value in deltas.items()
    }
    expected = rxls["media_box"][1] - libreoffice["media_box"][1]
    _require(parsed_deltas["media_box_height"] == expected, "height_delta_identity")
    return (
        _millipoints(rxls["media_box"][1]),
        _millipoints(libreoffice["media_box"][1]),
        _millipoints(expected),
    )


def _count_dimensions(
    dimensions: Iterable[dict[str, object]], key: str
) -> dict[str, int]:
    counts: dict[str, int] = {}
    for row in dimensions:
        value = str(row[key])
        counts[value] = counts.get(value, 0) + 1
    return dict(sorted(counts.items()))


def _validate_output(value: dict[str, object]) -> None:
    _require(
        set(value)
        == {
            "baseline",
            "cohorts",
            "coverage",
            "geometry_policy",
            "identities",
            "passed",
            "schema",
        }
        and value.get("schema") == OUTPUT_SCHEMA
        and value.get("passed") is True,
        "output_contract",
    )
    _require(
        type_exact_equal(
            value.get("geometry_policy"),
            UNIQUE_GEOMETRY_POLICY,
        ),
        "output_geometry_policy",
    )
    identities = value.get("identities")
    _require(
        isinstance(identities, dict)
        and set(identities)
        == {
            "container_build_contract_sha256",
            "container_config_digest",
            "container_execution_lock_sha256",
            "container_manifest_digest",
            "feature_map_sha256",
            "font_pack_sha256",
            "host_tools_identity_sha256",
            "libreoffice_artifact_sha256",
            "manifest_byte_count",
            "manifest_sha256",
            "pdffonts_sha256",
            "pdfinfo_sha256",
            "pdftoppm_sha256",
            "pdftotext_sha256",
            "renderer_byte_count",
            "renderer_sha256",
            "report_byte_count",
            "report_sha256",
            "selected_input_set_sha256",
        },
        "output_identity_contract",
    )
    for key, item in identities.items():
        if key.endswith("_byte_count"):
            _positive_int(item, "output_identity_contract", MAX_REPORT_BYTES)
        elif key.endswith("_digest"):
            _image_digest(item, "output_identity_contract")
        else:
            _sha(item, "output_identity_contract")
    coverage = value.get("coverage")
    _require(
        isinstance(coverage, dict)
        and set(coverage)
        == {
            "case_count",
            "normal_font_counts",
            "normal_size_point_counts",
            "page_count",
            "sheet_format_counts",
            "toggle_counts",
        }
        and coverage.get("case_count") == CASE_COUNT
        and type(coverage.get("page_count")) is int
        and coverage.get("page_count") == CASE_COUNT,
        "output_coverage_contract",
    )
    _require(
        coverage.get("normal_font_counts") == {"carlito": 4, "noto": 20}
        and coverage.get("normal_size_point_counts") == {"11": 20, "12": 4}
        and coverage.get("sheet_format_counts") == {"missing": 20, "present": 4}
        and coverage.get("toggle_counts") == EXPECTED_TOGGLE_COUNTS,
        "output_coverage_contract",
    )
    cohorts = value.get("cohorts")
    _require(isinstance(cohorts, list) and len(cohorts) == CASE_COUNT, "output_cohorts")
    observed_dimensions: list[dict[str, object]] = []
    for row in cohorts:
        _require(
            isinstance(row, dict)
            and set(row)
            == {
                "dimensions",
                "height_delta_millipoints",
                "libreoffice_height_millipoints",
                "page_count",
                "rxls_height_millipoints",
                "unique_line_geometry",
                "unique_word_geometry",
                "workbook_count",
            }
            and type(row.get("page_count")) is int
            and row.get("page_count") == 1
            and type(row.get("workbook_count")) is int
            and row.get("workbook_count") == 1,
            "output_cohort_contract",
        )
        dimension = row.get("dimensions")
        _require(
            isinstance(dimension, dict)
            and set(dimension)
            == {
                "normal_font",
                "normal_size_points",
                "sheet_format",
                "toggle",
            },
            "output_dimension_contract",
        )
        observed_dimensions.append(dimension)
        for key in (
            "height_delta_millipoints",
            "libreoffice_height_millipoints",
            "rxls_height_millipoints",
        ):
            _require(
                isinstance(row.get(key), int) and not isinstance(row.get(key), bool),
                "output_height_contract",
            )
        _require(
            0 < row["rxls_height_millipoints"] <= 1_000_000_000
            and 0 < row["libreoffice_height_millipoints"] <= 1_000_000_000
            and row["height_delta_millipoints"]
            == (
                row["rxls_height_millipoints"]
                - row["libreoffice_height_millipoints"]
            ),
            "output_height_contract",
        )
        _unique_geometry(row.get("unique_word_geometry"), "unique_word_geometry")
        _unique_geometry(row.get("unique_line_geometry"), "unique_line_geometry")
    _require(
        observed_dimensions == _expected_dimensions(),
        "output_dimension_contract",
    )
    baseline_cohorts = [
        row
        for row in cohorts
        if row["dimensions"]["toggle"] in BASELINE_TOGGLE_VALUES
    ]
    _require(
        len(baseline_cohorts) == BASELINE_CASE_COUNT,
        "output_baseline_contract",
    )
    maximum_baseline_delta = max(
        abs(row["height_delta_millipoints"]) for row in baseline_cohorts
    )
    baseline = value.get("baseline")
    _require(
        isinstance(baseline, dict)
        and set(baseline)
        == {
            "case_count",
            "max_absolute_height_delta_millipoints",
            "passed",
            "threshold_max_absolute_height_delta_millipoints",
        }
        and type(baseline.get("case_count")) is int
        and baseline.get("case_count") == BASELINE_CASE_COUNT
        and type(
            baseline.get("max_absolute_height_delta_millipoints")
        )
        is int
        and baseline.get("max_absolute_height_delta_millipoints")
        == maximum_baseline_delta
        and baseline.get("passed") is True
        and type(
            baseline.get(
                "threshold_max_absolute_height_delta_millipoints"
            )
        )
        is int
        and baseline.get("threshold_max_absolute_height_delta_millipoints")
        == BASELINE_MAX_ABSOLUTE_HEIGHT_DELTA_MILLIPOINTS
        and maximum_baseline_delta
        <= BASELINE_MAX_ABSOLUTE_HEIGHT_DELTA_MILLIPOINTS,
        "output_baseline_contract",
    )

    def reject_pathful(item: object) -> None:
        if isinstance(item, dict):
            for key, child in item.items():
                _require(
                    FORBIDDEN_OUTPUT_KEY_RE.search(key) is None,
                    "pathful_output_key",
                )
                reject_pathful(child)
        elif isinstance(item, list):
            for child in item:
                reject_pathful(child)
        elif isinstance(item, str):
            _require(
                FORBIDDEN_OUTPUT_VALUE_RE.search(item) is None,
                "pathful_output_value",
            )

    reject_pathful(
        {
            key: item
            for key, item in value.items()
            if key != "geometry_policy"
        }
    )


def reduce_report(
    report: dict[str, object],
    report_payload: bytes,
    manifest: dict[str, object],
    manifest_payload: bytes,
) -> dict[str, object]:
    """Validate one unsharded report and return its minimal safe aggregate."""

    manifest_paths, binding = _validate_manifest(manifest, manifest_payload)
    _require(
        set(report)
        == {"configuration", "discovery", "files", "mode", "preflight", "schema", "summary"}
        and report.get("schema") == REPORT_SCHEMA
        and report.get("mode") == "compare",
        "report_contract",
    )
    configuration = report.get("configuration")
    _require(
        isinstance(configuration, dict)
        and set(configuration) == REPORT_CONFIGURATION_KEYS
        and configuration.get("print_mode") == PRINT_MODE_SINGLE_PAGE
        and configuration.get("min_similarity_ppm") is None
        and configuration.get("lane_filter")
        == {
            "formats": ["xlsx"],
            "required_features": ["ooxml-implicit-row"],
        }
        and configuration.get("manifest_binding") == binding,
        "report_configuration",
    )
    metric_policy = configuration.get("metric_policy")
    _require(
        isinstance(metric_policy, dict)
        and metric_policy.get("contract_schema") == METRIC_CONTRACT_SCHEMA
        and metric_policy.get("contract_version") == 2
        and metric_policy.get("semantic_content_retained") is False
        and metric_policy.get("text_box_content_retained") is False,
        "metric_policy",
    )
    _require(
        type_exact_equal(
            metric_policy.get("unique_text_geometry"),
            UNIQUE_GEOMETRY_POLICY,
        ),
        "metric_policy_unique_text_geometry",
    )
    renderer = configuration.get("renderer_binary")
    _require(
        isinstance(renderer, dict)
        and set(renderer) == {"bytes", "sha256"},
        "renderer_identity",
    )
    renderer_bytes = _positive_int(
        renderer.get("bytes"), "renderer_identity", 512 * 1024 * 1024
    )
    renderer_sha256 = _sha(renderer.get("sha256"), "renderer_identity")
    font_pack = configuration.get("font_pack")
    _require(
        isinstance(font_pack, dict)
        and set(font_pack)
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
        and font_pack.get("attestation_required") is True
        and font_pack.get("configured") is True,
        "font_pack_identity",
    )
    font_pack_sha256 = _sha(font_pack.get("pack_sha256"), "font_pack_identity")
    toolchain = _validate_toolchain(configuration.get("measurement_toolchain"))
    oracle = _validate_oracle_identity(
        configuration.get("oracle_lock"), toolchain, font_pack_sha256
    )
    discovery = report.get("discovery")
    _require(
        isinstance(discovery, dict)
        and set(discovery)
        == {
            "candidate_count",
            "pre_shard_selected_count",
            "selected_count",
            "shard_candidate_count",
            "shard_count",
            "shard_index",
            "truncated",
        }
        and discovery.get("candidate_count") == CASE_COUNT
        and discovery.get("pre_shard_selected_count") == CASE_COUNT
        and discovery.get("selected_count") == CASE_COUNT
        and discovery.get("shard_candidate_count") == CASE_COUNT
        and type(discovery.get("shard_count")) is int
        and discovery.get("shard_count") == 1
        and type(discovery.get("shard_index")) is int
        and discovery.get("shard_index") == 0
        and discovery.get("truncated") is False,
        "discovery_contract",
    )
    summary = report.get("summary")
    _require(
        isinstance(summary, dict)
        and summary.get("files") == CASE_COUNT
        and summary.get("input_bytes_considered") == manifest.get("total_bytes")
        and summary.get("by_status") == {"compared": CASE_COUNT}
        and summary.get("by_classification") == {"within_threshold": CASE_COUNT}
        and summary.get("authored_print") is None,
        "summary_contract",
    )
    files = report.get("files")
    _require(isinstance(files, list) and len(files) == CASE_COUNT, "report_files")
    seen: set[str] = set()
    cohorts: list[dict[str, object]] = []
    geometry_pages = 0
    geometry_histogram_buckets = 0
    for raw in files:
        _require(isinstance(raw, dict), "report_file")
        path = raw.get("path")
        _require(isinstance(path, str) and path in manifest_paths, "report_file_identity")
        expected = manifest_paths[path]
        _require(
            path not in seen
            and raw.get("bytes") == expected["byte_length"]
            and raw.get("format") == "xlsx"
            and raw.get("rights_tier") == "S"
            and raw.get("features") == expected["features"]
            and raw.get("status") == "compared"
            and raw.get("classification") == "within_threshold",
            "report_file_contract",
        )
        pages = raw.get("pages")
        _require(isinstance(pages, list) and len(pages) == 1, "page_count")
        page = pages[0]
        rxls_height, libreoffice_height, delta = _extract_height(page)
        _require(isinstance(page, dict), "page_geometry")
        unique_word_geometry = _unique_geometry(
            page.get("text_box_unique_geometry"),
            "text_box_unique_geometry",
        )
        unique_line_geometry = _unique_geometry(
            page.get("text_line_box_unique_geometry"),
            "text_line_box_unique_geometry",
        )
        geometry_pages += 1
        geometry_histogram_buckets += sum(
            len(geometry["delta_histograms_millipoints"][axis])
            for geometry in (
                unique_word_geometry,
                unique_line_geometry,
            )
            for axis in UNIQUE_GEOMETRY_AXES
        )
        _require(
            geometry_pages <= MAX_UNIQUE_GEOMETRY_REPORT_PAGES
            and geometry_histogram_buckets
            <= MAX_UNIQUE_GEOMETRY_REPORT_HISTOGRAM_BUCKETS,
            "unique_geometry_report_limit",
        )
        for geometry, prefix in (
            (unique_word_geometry, "text_box"),
            (unique_line_geometry, "text_line_box"),
        ):
            rxls_items = _nonnegative_int(
                page.get(f"{prefix}_rxls_items"),
                f"{prefix}_unique_geometry_count",
                MAX_UNIQUE_GEOMETRY_ITEMS,
            )
            libreoffice_items = _nonnegative_int(
                page.get(f"{prefix}_libreoffice_items"),
                f"{prefix}_unique_geometry_count",
                MAX_UNIQUE_GEOMETRY_ITEMS,
            )
            paired_items = _nonnegative_int(
                page.get(f"{prefix}_matched_items"),
                f"{prefix}_unique_geometry_count",
                MAX_UNIQUE_GEOMETRY_ITEMS,
            )
            _require(
                geometry["rxls_unique_items"] <= rxls_items
                and geometry["libreoffice_unique_items"]
                <= libreoffice_items
                and geometry["matched_items"] <= paired_items,
                f"{prefix}_unique_geometry_count",
            )
        cohorts.append(
            {
                "dimensions": expected["dimension"],
                "height_delta_millipoints": delta,
                "libreoffice_height_millipoints": libreoffice_height,
                "page_count": 1,
                "rxls_height_millipoints": rxls_height,
                "unique_line_geometry": unique_line_geometry,
                "unique_word_geometry": unique_word_geometry,
                "workbook_count": 1,
            }
        )
        seen.add(path)
    _require(seen == set(manifest_paths), "report_file_coverage")
    cohorts.sort(key=lambda row: _dimension_key(row["dimensions"]))
    dimensions = [row["dimensions"] for row in cohorts]
    baseline_cohorts = [
        row
        for row in cohorts
        if row["dimensions"]["toggle"] in BASELINE_TOGGLE_VALUES
    ]
    _require(
        len(baseline_cohorts) == BASELINE_CASE_COUNT,
        "baseline_height_delta",
    )
    maximum_baseline_delta = max(
        abs(row["height_delta_millipoints"]) for row in baseline_cohorts
    )
    _require(
        maximum_baseline_delta
        <= BASELINE_MAX_ABSOLUTE_HEIGHT_DELTA_MILLIPOINTS,
        "baseline_height_delta",
    )
    output = {
        "baseline": {
            "case_count": BASELINE_CASE_COUNT,
            "max_absolute_height_delta_millipoints": maximum_baseline_delta,
            "passed": True,
            "threshold_max_absolute_height_delta_millipoints": (
                BASELINE_MAX_ABSOLUTE_HEIGHT_DELTA_MILLIPOINTS
            ),
        },
        "cohorts": cohorts,
        "coverage": {
            "case_count": CASE_COUNT,
            "normal_font_counts": _count_dimensions(dimensions, "normal_font"),
            "normal_size_point_counts": _count_dimensions(
                dimensions, "normal_size_points"
            ),
            "page_count": CASE_COUNT,
            "sheet_format_counts": _count_dimensions(dimensions, "sheet_format"),
            "toggle_counts": _count_dimensions(dimensions, "toggle"),
        },
        "geometry_policy": copy.deepcopy(UNIQUE_GEOMETRY_POLICY),
        "identities": {
            **oracle,
            "feature_map_sha256": binding["feature_map_sha256"],
            "font_pack_sha256": font_pack_sha256,
            **toolchain,
            "manifest_byte_count": len(manifest_payload),
            "manifest_sha256": binding["manifest_sha256"],
            "renderer_byte_count": renderer_bytes,
            "renderer_sha256": renderer_sha256,
            "report_byte_count": len(report_payload),
            "report_sha256": sha256(report_payload).hexdigest(),
            "selected_input_set_sha256": binding["input_set_sha256"],
        },
        "passed": True,
        "schema": OUTPUT_SCHEMA,
    }
    _validate_output(output)
    return output


def _atomic_write(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary_path = Path(temporary)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary_path, 0o600)
        os.replace(temporary_path, path)
    except BaseException:
        temporary_path.unlink(missing_ok=True)
        raise


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--campaign-manifest", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        report, report_payload = _load_json(
            args.report, MAX_REPORT_BYTES, "report_unreadable"
        )
        manifest, manifest_payload = _load_json(
            args.campaign_manifest, MAX_MANIFEST_BYTES, "manifest_unreadable"
        )
        output = reduce_report(report, report_payload, manifest, manifest_payload)
        payload = _canonical_json_bytes(output)
        _atomic_write(args.output, payload)
    except DiagnosticError as error:
        print(f"ooxml-row-diagnostic:{error}", file=os.sys.stderr)
        return 1
    print(_canonical_json_bytes(output).decode("utf-8"), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
