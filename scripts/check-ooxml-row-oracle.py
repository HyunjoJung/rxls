#!/usr/bin/env python3
"""Reduce one locked OOXML row campaign report to path-neutral geometry facts."""

from __future__ import annotations

import argparse
from fractions import Fraction
from hashlib import sha256
import json
import os
from pathlib import Path
import re
import tempfile
from typing import Iterable


REPORT_SCHEMA = "rxls.libreoffice-render-parity.v1"
OUTPUT_SCHEMA = "rxls.ooxml-row-oracle.v1"
MANIFEST_BINDING_SCHEMA = "rxls.render-parity-manifest-binding.v1"
METRIC_CONTRACT_SCHEMA = "rxls.render-parity-metrics.v2"
CONTAINER_IDENTITY_SCHEMA = "rxls.render-oracle-container-identity.v2"
PROFILE = "ooxml-row-diagnostic"
GENERATOR = "rxls-ooxml-row-diagnostic"
GENERATOR_VERSION = "1.0.0"
CASE_COUNT = 12
PRINT_MODE_SINGLE_PAGE = "single-page-sheets"
LIBREOFFICE_ARTIFACT_SHA256 = (
    "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5"
)

MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_MANIFEST_BYTES = 256 * 1024
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_INTEGER_DIGITS = 128
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
TOGGLE_FEATURES = {
    "explicit-row-height": "explicit_row_height",
    "hidden-row": "hidden_row",
    "image-drawing": "image_drawing",
    "right-to-left-layout": "right_to_left_layout",
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


def _load_json(path: Path, maximum: int, code: str) -> tuple[dict[str, object], bytes]:
    try:
        _require(path.is_file() and not path.is_symlink(), code)
        size = path.stat().st_size
        _require(0 < size <= maximum, f"{code}_limit")
        payload = path.read_bytes()
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
        manifest.get("schema_version") == 1
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
        set(value) == {"cohorts", "coverage", "identities", "passed", "schema"}
        and value.get("schema") == OUTPUT_SCHEMA
        and value.get("passed") is True,
        "output_contract",
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
        and coverage.get("page_count") == CASE_COUNT,
        "output_coverage_contract",
    )
    _require(
        coverage.get("normal_font_counts") == {"carlito": 4, "noto": 8}
        and coverage.get("normal_size_point_counts") == {"11": 8, "12": 4}
        and coverage.get("sheet_format_counts") == {"missing": 8, "present": 4}
        and coverage.get("toggle_counts")
        == {
            "explicit_row_height": 1,
            "hidden_row": 1,
            "image_drawing": 1,
            "none": 8,
            "right_to_left_layout": 1,
        },
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
                "workbook_count",
            }
            and row.get("page_count") == 1
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
    _require(
        observed_dimensions == _expected_dimensions(),
        "output_dimension_contract",
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

    reject_pathful(value)


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
        and discovery.get("shard_count") == 1
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
        rxls_height, libreoffice_height, delta = _extract_height(pages[0])
        cohorts.append(
            {
                "dimensions": expected["dimension"],
                "height_delta_millipoints": delta,
                "libreoffice_height_millipoints": libreoffice_height,
                "page_count": 1,
                "rxls_height_millipoints": rxls_height,
                "workbook_count": 1,
            }
        )
        seen.add(path)
    _require(seen == set(manifest_paths), "report_file_coverage")
    cohorts.sort(key=lambda row: _dimension_key(row["dimensions"]))
    dimensions = [row["dimensions"] for row in cohorts]
    output = {
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
