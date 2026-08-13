#!/usr/bin/env python3
"""Create or verify path-neutral LibreOffice parity metric ratchets."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
from typing import Any


EVIDENCE_SCHEMA = "rxls.libreoffice-render-parity.v1"
BASELINE_SCHEMA = "rxls.render-parity-baseline.v1"
SCOPED_BASELINE_SCHEMA = "rxls.render-parity-baseline.v2"
OBSERVED_CANDIDATE_SCHEMA = "rxls.render-parity-observed-candidate.v1"
RATCHET_ENVELOPE_SCHEMA = "rxls.render-parity-ratchet-envelope.v1"
CAMPAIGN_SCHEMA = "rxls.render-parity-campaign.v1"
REPORT_SCHEMA = "rxls.render-parity-baseline-check.v1"
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
CLASSIFICATION_RE = re.compile(r"[a-z][a-z0-9_]{0,95}\Z")
FEATURE_RE = re.compile(r"[a-z][a-z0-9-]{0,63}\Z")
FORMAT_RE = re.compile(r"[a-z0-9][a-z0-9]{0,15}\Z")
IDENTITY_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}\Z")
METRIC_RE = re.compile(r"[a-z][a-z0-9_]{0,127}\Z")
WARNING_RE = re.compile(r"[a-z][a-z0-9_]{0,63}\Z")
STATUS_VALUES = frozenset({"compared", "different", "dry_run", "error", "skipped"})
MAX_DOCUMENT_BYTES = 64 * 1024 * 1024
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_JSON_INTEGER_DIGITS = 128
SCORE_RATCHETS = ("p10", "mean")
DELTA_RATCHETS = ("p90", "max")
HOSTED_FULL_KIND = "project_generated_hosted_full"
PROJECT_GENERATED_KIND = "project_generated_manifest"
ACQUIRED_CORPUS_KIND = "acquired_corpus_manifest"
HOSTED_FULL_GENERATOR = "rxls-synthetic-render-corpus"
HOSTED_FULL_GENERATOR_VERSION = "1.5.0"
HOSTED_FULL_MANIFEST_SHA256 = (
    "5c6466a53e4328bb50f04cd3c63d102bf53da1a6b3478380f3724574c31b248d"
)
HOSTED_FULL_INPUT_SET_SHA256 = (
    "45dfaaac5e94e98da038c561d98eed48e8785f56749760d39bac8a720b132db9"
)
HOSTED_FULL_LATTICE_SHA256 = (
    "6d9181538349f29b1eb51bac34a3ce30a2c4ad22383186ed89b94d8eac5d1159"
)
HOSTED_FULL_GROUP_TOPOLOGY_SHA256 = (
    "559cf641df08738419af941f30c35a831ca9d000e85ab1e5753c391486f0d251"
)
HOSTED_FULL_FORMAT_COUNTS = {"ods": 200, "xls": 200, "xlsb": 200, "xlsx": 200}
HOSTED_FULL_FEATURE_COUNTS = {
    "border": 200,
    "cell-fill": 200,
    "chart": 100,
    "chinese-text": 400,
    "column-width": 400,
    "conditional-format": 100,
    "date-format": 400,
    "formula-cached": 400,
    "hidden-column": 400,
    "hidden-row": 400,
    "image-drawing": 100,
    "japanese-text": 400,
    "korean-text": 416,
    "latin-text": 800,
    "merged-cells": 400,
    "noto-ofl-font": 600,
    "number-cell": 800,
    "percent-format": 400,
    "print-settings": 400,
    "right-to-left-layout": 200,
    "row-height": 400,
    "rtl-text": 400,
    "sparkline": 100,
    "unicode-text": 752,
    "wrapped-text": 200,
}
MAX_COUNT = 1_000_000
MAX_WARNING_CODES = 256
MAX_DELTA_VALUE = (1 << 63) - 1
SCORE_MAX_PPM = 1_000_000
ADOPTION_SCORE_METRICS = frozenset(
    {
        "blurred_luma_similarity_ppm",
        "edge_f1_ppm",
        "foreground_f1_ppm",
        "similarity_ppm",
        "text_ink_f1_ppm",
    }
)
# No delta metric currently has a unit-calibrated repeatability threshold.
ADOPTION_DELTA_METRICS = frozenset()
EXPECTED_SCORE_METRICS = frozenset(
    {
        "blurred_luma_similarity_ppm",
        "edge_f1_ppm",
        "foreground_f1_ppm",
        "foreground_matched_color_similarity_ppm",
        "foreground_precision_ppm",
        "foreground_recall_ppm",
        "semantic_bigram_f1_ppm",
        "semantic_codepoint_f1_ppm",
        "semantic_token_f1_ppm",
        "similarity_ppm",
        "text_box_f1_ppm",
        "text_box_match_coverage_ppm",
        "text_box_precision_ppm",
        "text_box_recall_ppm",
        "text_ink_f1_ppm",
        "text_ink_precision_ppm",
        "text_ink_recall_ppm",
        "text_line_box_f1_ppm",
        "text_line_box_precision_ppm",
        "text_line_box_recall_ppm",
    }
)
EXPECTED_DELTA_METRICS = frozenset(
    {
        "foreground_bbox_alignment_max_delta_pixels",
        "foreground_centroid_distance_millipixels",
        "max_page_height_delta_pixels",
        "max_page_width_delta_pixels",
        "max_pdf_point_geometry_delta_millipoints",
        "max_pdf_xhtml_crosscheck_delta_micropoints",
        "page_dimension_mismatches",
        "pdf_point_geometry_mismatches",
        "semantic_page_mismatches",
        "text_box_ambiguous_items",
        "text_box_libreoffice_unmatched_items",
        "text_box_median_error_millipoints",
        "text_box_p95_error_millipoints",
        "text_box_unmatched_items",
        "text_ink_bbox_alignment_max_delta_pixels",
        "text_ink_centroid_distance_millipixels",
        "text_line_box_ambiguous_items",
        "text_line_box_libreoffice_unmatched_items",
        "text_line_box_median_error_millipoints",
        "text_line_box_p95_error_millipoints",
        "text_line_box_unmatched_items",
    }
)
ADOPTION_MAX_SCORE_DRIFT_PPM = 20_000
ADOPTION_POLICY = "rxls.repeatability-bounded-ratchet-envelope.v1"


class BaselineError(RuntimeError):
    pass


class _StrictJSONError(ValueError):
    pass


def canonical_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


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


def parse_json_bytes(payload: bytes, code: str) -> object:
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
    except (
        UnicodeDecodeError,
        RecursionError,
        ValueError,
    ) as error:
        raise BaselineError(f"{code}_invalid_json") from error


def _stat_signature(value: os.stat_result) -> tuple[int, ...]:
    return (
        value.st_dev,
        value.st_ino,
        value.st_mode,
        value.st_size,
        value.st_mtime_ns,
        value.st_ctime_ns,
    )


def _read_bounded_regular_file(path: Path, code: str) -> bytes:
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
            raise BaselineError(f"{code}_unreadable")
        remaining = MAX_DOCUMENT_BYTES + 1
        chunks: list[bytes] = []
        while remaining:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        after = os.fstat(descriptor)
        current = os.stat(path, follow_symlinks=False)
    except BaselineError:
        raise
    except OSError as error:
        raise BaselineError(f"{code}_unreadable") from error
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
        raise BaselineError(f"{code}_unreadable")
    payload = b"".join(chunks)
    if not payload or len(payload) > MAX_DOCUMENT_BYTES:
        raise BaselineError(f"{code}_limit")
    if len(payload) != after.st_size:
        raise BaselineError(f"{code}_unreadable")
    return payload


def read_json_with_identity(
    path: Path,
    code: str,
) -> tuple[dict[str, Any], dict[str, object]]:
    payload = _read_bounded_regular_file(path, code)
    document = parse_json_bytes(payload, code)
    if not isinstance(document, dict):
        raise BaselineError(f"{code}_not_object")
    return (
        document,
        {
            "bytes": len(payload),
            "sha256": hashlib.sha256(payload).hexdigest(),
        },
    )


def read_json(path: Path, code: str) -> dict[str, Any]:
    return read_json_with_identity(path, code)[0]


def sha256_json(value: object) -> str:
    return hashlib.sha256(canonical_bytes(value)).hexdigest()


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _integer_map(
    value: object,
    code: str,
    *,
    key_pattern: re.Pattern[str] | None = None,
    allowed_keys: frozenset[str] | None = None,
) -> dict[str, int]:
    if not isinstance(value, dict):
        raise BaselineError(code)
    result = {}
    for key, count in value.items():
        if (
            not isinstance(key, str)
            or not key
            or (
                key_pattern is not None
                and key_pattern.fullmatch(key) is None
            )
            or (allowed_keys is not None and key not in allowed_keys)
            or type(count) is not int
            or not 0 <= count <= MAX_COUNT
        ):
            raise BaselineError(code)
        result[key] = count
    return dict(sorted(result.items()))


def _warning_map(value: object, code: str) -> dict[str, int]:
    result = _integer_map(value, code, key_pattern=WARNING_RE)
    if (
        len(result) > MAX_WARNING_CODES
        or any(count <= 0 for count in result.values())
    ):
        raise BaselineError(code)
    return result


def _input_identity(files: object) -> tuple[str, int]:
    if not isinstance(files, list) or not files:
        raise BaselineError("evidence_files")
    identities = []
    for row in files:
        if not isinstance(row, dict):
            raise BaselineError("evidence_file")
        digest = row.get("sha256")
        format_name = row.get("format")
        if (
            not isinstance(digest, str)
            or not SHA256_RE.fullmatch(digest)
            or not isinstance(format_name, str)
            or FORMAT_RE.fullmatch(format_name) is None
        ):
            raise BaselineError("evidence_file_identity")
        features = row.get("features", [])
        if (
            not isinstance(features, list)
            or len(features) > 256
            or not all(isinstance(feature, str) and feature for feature in features)
            or any(FEATURE_RE.fullmatch(feature) is None for feature in features)
            or features != sorted(set(features))
        ):
            raise BaselineError("evidence_file_features")
        rights = row.get("rights_tier")
        if rights not in {None, "S", "U", "Q"}:
            raise BaselineError("evidence_file_rights")
        identities.append(
            {
                "features": features,
                "format": format_name,
                "rights_tier": rights,
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
    if len({row["sha256"] for row in identities}) != len(identities):
        raise BaselineError("evidence_duplicate_input")
    return sha256_json(identities), len(identities)


def _format_and_feature_counts(
    files: object,
) -> tuple[dict[str, int], dict[str, int]]:
    if not isinstance(files, list) or not files:
        raise BaselineError("campaign_files")
    format_counts: dict[str, int] = {}
    feature_counts: dict[str, int] = {}
    for row in files:
        if not isinstance(row, dict):
            raise BaselineError("campaign_file")
        format_name = row.get("format")
        features = row.get("features", [])
        if not isinstance(format_name, str) or not format_name:
            raise BaselineError("campaign_file_format")
        if (
            not isinstance(features, list)
            or len(features) > 256
            or not all(isinstance(feature, str) and feature for feature in features)
            or any(FEATURE_RE.fullmatch(feature) is None for feature in features)
            or features != sorted(set(features))
        ):
            raise BaselineError("campaign_file_features")
        if FORMAT_RE.fullmatch(format_name) is None:
            raise BaselineError("campaign_file_format")
        format_counts[format_name] = format_counts.get(format_name, 0) + 1
        for feature in features:
            feature_counts[feature] = feature_counts.get(feature, 0) + 1
    return dict(sorted(format_counts.items())), dict(sorted(feature_counts.items()))


def _manifest_lattice_sha256(files: object) -> str | None:
    if not isinstance(files, list):
        return None
    rows = []
    for value in files:
        if not isinstance(value, dict):
            return None
        case_id = value.get("case_id")
        features = value.get("features")
        format_name = value.get("format")
        generator = value.get("generator")
        generator_version = value.get("generator_version")
        seed = value.get("seed")
        if (
            not isinstance(case_id, str)
            or not case_id
            or not isinstance(features, list)
            or not all(isinstance(feature, str) and feature for feature in features)
            or features != sorted(set(features))
            or not isinstance(format_name, str)
            or not format_name
            or not isinstance(generator, str)
            or not generator
            or not isinstance(generator_version, str)
            or not generator_version
            or type(seed) is not int
            or seed < 0
        ):
            return None
        rows.append(
            {
                "case_id": case_id,
                "features": features,
                "format": format_name,
                "generator": generator,
                "generator_version": generator_version,
                "seed": seed,
            }
        )
    rows.sort(key=lambda row: row["case_id"])
    if len({row["case_id"] for row in rows}) != len(rows):
        return None
    return sha256_json(rows)


def campaign_from_manifest(
    path: Path, *, require_hosted_full_800: bool = False
) -> dict[str, Any]:
    payload = _read_bounded_regular_file(path, "campaign_manifest")
    manifest = parse_json_bytes(payload, "campaign_manifest")
    if not isinstance(manifest, dict):
        raise BaselineError("campaign_manifest_not_object")

    files = manifest.get("files")
    input_set_sha256, input_files = _input_identity(files)
    format_counts, feature_counts = _format_and_feature_counts(files)
    declared_formats = _integer_map(
        manifest.get("format_counts"),
        "campaign_manifest_format_counts",
        key_pattern=FORMAT_RE,
    )
    declared_features = _integer_map(
        manifest.get("feature_counts"),
        "campaign_manifest_feature_counts",
        key_pattern=FEATURE_RE,
    )
    if declared_formats != format_counts or declared_features != feature_counts:
        raise BaselineError("campaign_manifest_counts_mismatch")
    if manifest.get("case_count") != input_files:
        raise BaselineError("campaign_manifest_case_count")
    if sum(format_counts.values()) != input_files:
        raise BaselineError("campaign_manifest_format_coverage")

    hosted_files_are_project_owned = isinstance(files, list) and all(
        isinstance(row, dict)
        and row.get("generator") == HOSTED_FULL_GENERATOR
        and row.get("generator_version") == HOSTED_FULL_GENERATOR_VERSION
        and row.get("license") == "MIT"
        and row.get("rights_tier") == "S"
        and row.get("redistribution") == "allowed"
        and row.get("source_redistributable") is True
        and row.get("render_redistributable") is True
        for row in files
    )
    manifest_sha256 = sha256_bytes(payload)
    is_hosted_full_800 = (
        manifest_sha256 == HOSTED_FULL_MANIFEST_SHA256
        and _manifest_lattice_sha256(files) == HOSTED_FULL_LATTICE_SHA256
        and input_set_sha256 == HOSTED_FULL_INPUT_SET_SHA256
        and input_files == 800
        and format_counts == HOSTED_FULL_FORMAT_COUNTS
        and feature_counts == HOSTED_FULL_FEATURE_COUNTS
        and manifest.get("profile") == "full"
        and manifest.get("generator") == HOSTED_FULL_GENERATOR
        and manifest.get("generator_version") == HOSTED_FULL_GENERATOR_VERSION
        and manifest.get("schema_version") == 1
        and manifest.get("license") == "MIT"
        and manifest.get("rights_tier") == "S"
        and manifest.get("redistribution") == "allowed"
        and manifest.get("source_redistributable") is True
        and manifest.get("render_redistributable") is True
        and hosted_files_are_project_owned
    )
    generator = manifest.get("generator")
    if is_hosted_full_800:
        kind = HOSTED_FULL_KIND
    elif generator == HOSTED_FULL_GENERATOR:
        kind = PROJECT_GENERATED_KIND
    else:
        kind = ACQUIRED_CORPUS_KIND
    campaign = {
        "case_count": input_files,
        "feature_counts": feature_counts,
        "format_counts": format_counts,
        "generator": generator,
        "generator_version": manifest.get("generator_version"),
        "input_set_sha256": input_set_sha256,
        "kind": kind,
        "manifest_sha256": manifest_sha256,
        "profile": manifest.get("profile"),
        "schema": CAMPAIGN_SCHEMA,
    }
    if (
        not isinstance(campaign["generator"], str)
        or IDENTITY_RE.fullmatch(campaign["generator"]) is None
        or not isinstance(campaign["generator_version"], str)
        or IDENTITY_RE.fullmatch(campaign["generator_version"]) is None
        or not isinstance(campaign["profile"], str)
        or IDENTITY_RE.fullmatch(campaign["profile"]) is None
    ):
        raise BaselineError("campaign_manifest_identity")

    if require_hosted_full_800 and not is_hosted_full_800:
        raise BaselineError("campaign_not_hosted_full_800")
    return campaign


def _validate_campaign(value: object) -> dict[str, Any]:
    required = {
        "case_count",
        "feature_counts",
        "format_counts",
        "generator",
        "generator_version",
        "input_set_sha256",
        "kind",
        "manifest_sha256",
        "profile",
        "schema",
    }
    if not isinstance(value, dict) or set(value) != required:
        raise BaselineError("baseline_campaign_shape")
    if value.get("schema") != CAMPAIGN_SCHEMA or value.get("kind") not in {
        HOSTED_FULL_KIND,
        PROJECT_GENERATED_KIND,
        ACQUIRED_CORPUS_KIND,
    }:
        raise BaselineError("baseline_campaign_schema")
    if (
        type(value.get("case_count")) is not int
        or not 0 < value["case_count"] <= MAX_COUNT
        or not isinstance(value.get("generator"), str)
        or IDENTITY_RE.fullmatch(value["generator"]) is None
        or not isinstance(value.get("generator_version"), str)
        or IDENTITY_RE.fullmatch(value["generator_version"]) is None
        or not isinstance(value.get("profile"), str)
        or IDENTITY_RE.fullmatch(value["profile"]) is None
    ):
        raise BaselineError("baseline_campaign_identity")
    for key in ("input_set_sha256", "manifest_sha256"):
        if not isinstance(value.get(key), str) or not SHA256_RE.fullmatch(value[key]):
            raise BaselineError("baseline_campaign_identity")
    format_counts = _integer_map(
        value.get("format_counts"),
        "baseline_campaign_format_counts",
        key_pattern=FORMAT_RE,
    )
    feature_counts = _integer_map(
        value.get("feature_counts"),
        "baseline_campaign_feature_counts",
        key_pattern=FEATURE_RE,
    )
    if sum(format_counts.values()) != value["case_count"]:
        raise BaselineError("baseline_campaign_coverage")
    if value["kind"] == HOSTED_FULL_KIND and (
        value["case_count"] != 800
        or value["profile"] != "full"
        or value["generator"] != HOSTED_FULL_GENERATOR
        or value["generator_version"] != HOSTED_FULL_GENERATOR_VERSION
        or value["manifest_sha256"] != HOSTED_FULL_MANIFEST_SHA256
        or value["input_set_sha256"] != HOSTED_FULL_INPUT_SET_SHA256
        or format_counts != HOSTED_FULL_FORMAT_COUNTS
        or feature_counts != HOSTED_FULL_FEATURE_COUNTS
    ):
        raise BaselineError("baseline_campaign_hosted_full_identity")
    return {
        "case_count": value["case_count"],
        "feature_counts": feature_counts,
        "format_counts": format_counts,
        "generator": value["generator"],
        "generator_version": value["generator_version"],
        "input_set_sha256": value["input_set_sha256"],
        "kind": value["kind"],
        "manifest_sha256": value["manifest_sha256"],
        "profile": value["profile"],
        "schema": CAMPAIGN_SCHEMA,
    }


def _warning_counts(files: list[dict[str, Any]]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for file_row in files:
        scenes = file_row.get("scenes", [])
        if not isinstance(scenes, list):
            raise BaselineError("evidence_scenes")
        for scene in scenes:
            if not isinstance(scene, dict) or not isinstance(scene.get("warnings", []), list):
                raise BaselineError("evidence_scene")
            for warning in scene.get("warnings", []):
                if not isinstance(warning, dict):
                    raise BaselineError("evidence_warning")
                code = warning.get("code")
                occurrences = warning.get("occurrences")
                if (
                    not isinstance(code, str)
                    or WARNING_RE.fullmatch(code) is None
                    or type(occurrences) is not int
                    or not 0 < occurrences <= MAX_COUNT
                ):
                    raise BaselineError("evidence_warning")
                counts[code] = counts.get(code, 0) + occurrences
                if (
                    counts[code] > MAX_COUNT
                    or len(counts) > MAX_WARNING_CODES
                ):
                    raise BaselineError("evidence_warning")
    return dict(sorted(counts.items()))


def _raw_file_count_maps(
    files: list[dict[str, Any]],
) -> tuple[dict[str, int], dict[str, int]]:
    statuses: dict[str, int] = {}
    classifications: dict[str, int] = {}
    for row in files:
        status = row.get("status")
        classification = row.get("classification")
        if not isinstance(status, str) or status not in STATUS_VALUES:
            raise BaselineError("evidence_file_status")
        if (
            not isinstance(classification, str)
            or CLASSIFICATION_RE.fullmatch(classification) is None
        ):
            raise BaselineError("evidence_file_classification")
        statuses[status] = statuses.get(status, 0) + 1
        classifications[classification] = (
            classifications.get(classification, 0) + 1
        )
    return dict(sorted(statuses.items())), dict(sorted(classifications.items()))


def _mean_sum_interval(mean: int, count: int) -> tuple[int, int]:
    """Return every integer sum that rounds to ``mean`` in the producer."""

    half = count // 2
    return (
        count * mean - half,
        count * (mean + 1) - half - 1,
    )


def _distribution_sum_bounds(
    value: dict[str, int], *, score: bool
) -> tuple[int, int]:
    """Bound a sorted integer sample using its nearest-rank quantiles."""

    count = value["count"]
    if count == 1:
        statistics = set(value) - {"count"}
        if len({value[key] for key in statistics}) != 1:
            raise BaselineError("evidence_distribution_feasibility")
        return value["mean"], value["mean"]

    if score:
        rank_10 = (count + 9) // 10
        if rank_10 == 1 and value["p10"] != value["min"]:
            raise BaselineError("evidence_distribution_feasibility")
        minimum_sum = (
            (rank_10 - 1) * value["min"]
            + (count - rank_10) * value["p10"]
            + value["max"]
        )
        maximum_sum = (
            value["min"]
            + (rank_10 - 1) * value["p10"]
            + (count - rank_10) * value["max"]
        )
    else:
        rank_50 = (count + 1) // 2
        rank_90 = (9 * count + 9) // 10
        if (
            rank_50 == 1
            and value["p50"] != value["min"]
            or rank_90 == count
            and value["p90"] != value["max"]
        ):
            raise BaselineError("evidence_distribution_feasibility")
        minimum_sum = (
            (rank_50 - 1) * value["min"]
            + (rank_90 - rank_50) * value["p50"]
            + (count - rank_90) * value["p90"]
            + value["max"]
        )
        maximum_sum = (
            value["min"]
            + (rank_50 - 1) * value["p50"]
            + (rank_90 - rank_50) * value["p90"]
            + (count - rank_90) * value["max"]
        )
    mean_minimum, mean_maximum = _mean_sum_interval(value["mean"], count)
    possible_minimum = max(minimum_sum, mean_minimum)
    possible_maximum = min(maximum_sum, mean_maximum)
    if possible_minimum > possible_maximum:
        raise BaselineError("evidence_distribution_feasibility")
    return possible_minimum, possible_maximum


def _validate_distribution(
    value: object,
    *,
    score: bool,
    expected_count: int,
) -> dict[str, int]:
    required = (
        {"count", "max", "mean", "min", "p10"}
        if score
        else {"count", "max", "mean", "min", "p50", "p90"}
    )
    if not isinstance(value, dict) or set(value) != required:
        raise BaselineError("evidence_distribution")
    if not all(type(value[key]) is int for key in required):
        raise BaselineError("evidence_distribution")
    if value["count"] != expected_count or not 0 < value["count"] <= MAX_COUNT:
        raise BaselineError("evidence_distribution")
    maximum = SCORE_MAX_PPM if score else MAX_DELTA_VALUE
    statistics = required - {"count"}
    if any(not 0 <= value[key] <= maximum for key in statistics):
        raise BaselineError("evidence_distribution_domain")
    if not value["min"] <= value["mean"] <= value["max"]:
        raise BaselineError("evidence_distribution_order")
    if score:
        if not value["min"] <= value["p10"] <= value["max"]:
            raise BaselineError("evidence_distribution_order")
    elif not (
        value["min"]
        <= value["p50"]
        <= value["p90"]
        <= value["max"]
    ):
        raise BaselineError("evidence_distribution_order")
    result = {key: value[key] for key in sorted(required)}
    _distribution_sum_bounds(result, score=score)
    return result


def _validate_cohort(value: object) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {
        "comparable_workbooks",
        "deltas",
        "scores",
        "workbooks",
    }:
        raise BaselineError("evidence_cohort")
    workbooks = value["workbooks"]
    comparable = value["comparable_workbooks"]
    if (
        type(workbooks) is not int
        or type(comparable) is not int
        or not 0 < workbooks <= MAX_COUNT
        or not 0 <= comparable <= workbooks
    ):
        raise BaselineError("evidence_cohort")
    scores = value["scores"]
    deltas = value["deltas"]
    if not isinstance(scores, dict) or not isinstance(deltas, dict):
        raise BaselineError("evidence_cohort")
    if any(
        not isinstance(metric, str) or METRIC_RE.fullmatch(metric) is None
        for metric in (*scores, *deltas)
    ):
        raise BaselineError("evidence_metric_name")
    return {
        "comparable_workbooks": comparable,
        "deltas": {
            key: _validate_distribution(
                distribution,
                score=False,
                expected_count=comparable,
            )
            for key, distribution in sorted(deltas.items())
        },
        "scores": {
            key: _validate_distribution(
                distribution,
                score=True,
                expected_count=comparable,
            )
            for key, distribution in sorted(scores.items())
        },
        "workbooks": workbooks,
    }


def _cohorts(value: object) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {"all", "by_feature", "by_format"}:
        raise BaselineError("evidence_cohorts")
    result: dict[str, Any] = {"all": _validate_cohort(value["all"])}
    for dimension in ("by_feature", "by_format"):
        rows = value[dimension]
        pattern = FEATURE_RE if dimension == "by_feature" else FORMAT_RE
        if (
            not isinstance(rows, dict)
            or any(
                not isinstance(name, str) or pattern.fullmatch(name) is None
                for name in rows
            )
        ):
            raise BaselineError("evidence_cohorts")
        result[dimension] = {
            key: _validate_cohort(cohort) for key, cohort in sorted(rows.items())
        }
    return result


def _nearest_rank_from_histogram(
    histogram: list[list[int]], numerator: int, denominator: int
) -> int:
    count = sum(bin_count for _, bin_count in histogram)
    rank = max(1, (count * numerator + denominator - 1) // denominator)
    cumulative = 0
    for value, bin_count in histogram:
        cumulative += bin_count
        if cumulative >= rank:
            return value
    raise BaselineError("candidate_histogram_count")


def _distribution_from_histogram(
    histogram: list[list[int]], *, score: bool
) -> dict[str, int]:
    count = sum(bin_count for _, bin_count in histogram)
    total = sum(value * bin_count for value, bin_count in histogram)
    result = {
        "count": count,
        "max": histogram[-1][0],
        "mean": (total + count // 2) // count,
        "min": histogram[0][0],
    }
    if score:
        result["p10"] = _nearest_rank_from_histogram(histogram, 1, 10)
    else:
        result["p50"] = _nearest_rank_from_histogram(histogram, 1, 2)
        result["p90"] = _nearest_rank_from_histogram(histogram, 9, 10)
    return {key: result[key] for key in sorted(result)}


def _validate_histogram(
    value: object,
    *,
    score: bool,
    expected_count: int,
) -> list[list[int]]:
    if (
        not isinstance(value, list)
        or not value
        or len(value) > expected_count
        or expected_count > MAX_COUNT
    ):
        raise BaselineError("candidate_histogram")
    maximum = SCORE_MAX_PPM if score else MAX_DELTA_VALUE
    result: list[list[int]] = []
    previous: int | None = None
    count = 0
    for raw_bin in value:
        if (
            not isinstance(raw_bin, list)
            or len(raw_bin) != 2
            or type(raw_bin[0]) is not int
            or type(raw_bin[1]) is not int
        ):
            raise BaselineError("candidate_histogram")
        metric_value, bin_count = raw_bin
        if (
            not 0 <= metric_value <= maximum
            or bin_count <= 0
            or previous is not None
            and metric_value <= previous
        ):
            raise BaselineError("candidate_histogram")
        count += bin_count
        if count > expected_count:
            raise BaselineError("candidate_histogram_count")
        previous = metric_value
        result.append([metric_value, bin_count])
    if count != expected_count:
        raise BaselineError("candidate_histogram_count")
    return result


def _validate_histogram_cohort(
    value: object,
    cohort: dict[str, Any],
) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {"deltas", "scores"}:
        raise BaselineError("candidate_histogram_cohort")
    result: dict[str, Any] = {"deltas": {}, "scores": {}}
    expected_count = cohort["comparable_workbooks"]
    for metric_kind, score in (("scores", True), ("deltas", False)):
        metrics = value[metric_kind]
        if not isinstance(metrics, dict) or set(metrics) != set(cohort[metric_kind]):
            raise BaselineError("candidate_histogram_metric_coverage")
        for metric, raw_histogram in sorted(metrics.items()):
            histogram = _validate_histogram(
                raw_histogram,
                score=score,
                expected_count=expected_count,
            )
            if _distribution_from_histogram(
                histogram,
                score=score,
            ) != cohort[metric_kind][metric]:
                raise BaselineError("candidate_histogram_summary")
            result[metric_kind][metric] = histogram
    return result


def _sum_histograms(histograms: list[list[list[int]]]) -> list[list[int]]:
    counts: dict[int, int] = {}
    for histogram in histograms:
        for value, count in histogram:
            counts[value] = counts.get(value, 0) + count
    return [[value, count] for value, count in sorted(counts.items())]


def _validate_histograms(
    value: object,
    cohorts: dict[str, Any],
) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {"all", "by_feature", "by_format"}:
        raise BaselineError("candidate_histograms")
    result = {
        "all": _validate_histogram_cohort(value["all"], cohorts["all"]),
        "by_feature": {},
        "by_format": {},
    }
    for dimension in ("by_feature", "by_format"):
        raw_rows = value[dimension]
        if not isinstance(raw_rows, dict) or set(raw_rows) != set(cohorts[dimension]):
            raise BaselineError("candidate_histogram_cohort_coverage")
        result[dimension] = {
            name: _validate_histogram_cohort(raw_rows[name], cohorts[dimension][name])
            for name in sorted(raw_rows)
        }

    for metric_kind in ("scores", "deltas"):
        for metric, all_histogram in result["all"][metric_kind].items():
            format_sum = _sum_histograms(
                [
                    cohort[metric_kind][metric]
                    for cohort in result["by_format"].values()
                ]
            )
            if all_histogram != format_sum:
                raise BaselineError("candidate_histogram_format_partition")
    return result


def _histogram(values: list[int]) -> list[list[int]]:
    counts: dict[int, int] = {}
    for value in values:
        counts[value] = counts.get(value, 0) + 1
    return [[value, count] for value, count in sorted(counts.items())]


def _has_comparable_content(row: dict[str, Any]) -> bool:
    metrics = row.get("metrics")
    if (
        row.get("status") not in {"compared", "different"}
        or not isinstance(metrics, dict)
        or metrics.get("semantic_comparable", 1) != 1
    ):
        return False
    content_counts = (
        "semantic_token_rxls_items",
        "semantic_token_libreoffice_items",
        "foreground_rxls_pixels",
        "foreground_libreoffice_pixels",
    )
    return not all(
        key in metrics and type(metrics[key]) is int and metrics[key] == 0
        for key in content_counts
    )


def _histogram_cohort_from_rows(
    rows: list[dict[str, Any]],
    *,
    score_metrics: frozenset[str] = EXPECTED_SCORE_METRICS,
    delta_metrics: frozenset[str] = EXPECTED_DELTA_METRICS,
) -> dict[str, dict[str, list[list[int]]]]:
    comparable = [row for row in rows if _has_comparable_content(row)]
    result: dict[str, dict[str, list[list[int]]]] = {
        "scores": {},
        "deltas": {},
    }
    for metric_kind, metrics, score in (
        ("scores", score_metrics, True),
        ("deltas", delta_metrics, False),
    ):
        for metric in sorted(metrics):
            values = []
            for row in comparable:
                raw_metrics = row["metrics"]
                raw_value = raw_metrics.get(metric)
                if type(raw_value) is not int:
                    raise BaselineError("candidate_raw_metric")
                value = raw_value if score else abs(raw_value)
                maximum = SCORE_MAX_PPM if score else MAX_DELTA_VALUE
                if not 0 <= value <= maximum:
                    raise BaselineError("candidate_raw_metric")
                values.append(value)
            if not values:
                raise BaselineError("candidate_raw_metric")
            result[metric_kind][metric] = _histogram(values)
    return result


def _cohort_from_raw_rows(
    rows: list[dict[str, Any]],
    *,
    score_metrics: frozenset[str],
    delta_metrics: frozenset[str],
) -> dict[str, Any]:
    comparable = sum(1 for row in rows if _has_comparable_content(row))
    histograms = _histogram_cohort_from_rows(
        rows,
        score_metrics=score_metrics,
        delta_metrics=delta_metrics,
    )
    return {
        "comparable_workbooks": comparable,
        "deltas": {
            metric: _distribution_from_histogram(histogram, score=False)
            for metric, histogram in histograms["deltas"].items()
        },
        "scores": {
            metric: _distribution_from_histogram(histogram, score=True)
            for metric, histogram in histograms["scores"].items()
        },
        "workbooks": len(rows),
    }


def _derive_raw_cohorts(
    files: list[dict[str, Any]],
    *,
    score_metrics: frozenset[str],
    delta_metrics: frozenset[str],
) -> dict[str, Any]:
    by_format: dict[str, list[dict[str, Any]]] = {}
    by_feature: dict[str, list[dict[str, Any]]] = {}
    for row in files:
        by_format.setdefault(row["format"], []).append(row)
        for feature in row.get("features", []):
            by_feature.setdefault(feature, []).append(row)
    return {
        "all": _cohort_from_raw_rows(
            files,
            score_metrics=score_metrics,
            delta_metrics=delta_metrics,
        ),
        "by_feature": {
            name: _cohort_from_raw_rows(
                rows,
                score_metrics=score_metrics,
                delta_metrics=delta_metrics,
            )
            for name, rows in sorted(by_feature.items())
        },
        "by_format": {
            name: _cohort_from_raw_rows(
                rows,
                score_metrics=score_metrics,
                delta_metrics=delta_metrics,
            )
            for name, rows in sorted(by_format.items())
        },
    }


def group_topology_sha256(groups: list[dict[str, Any]]) -> str:
    return sha256_json(
        [
            {
                "features": group["features"],
                "format": group["format"],
                "workbooks": group["workbooks"],
            }
            for group in groups
        ]
    )


def derive_group_histograms(
    files: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    grouped: dict[tuple[str, tuple[str, ...]], list[dict[str, Any]]] = {}
    for row in files:
        key = (row["format"], tuple(row.get("features", [])))
        grouped.setdefault(key, []).append(row)
    result = []
    for (format_name, features), rows in sorted(grouped.items()):
        comparable = sum(1 for row in rows if _has_comparable_content(row))
        if comparable != len(rows):
            raise BaselineError("candidate_group_full_coverage")
        histograms = _histogram_cohort_from_rows(rows)
        result.append(
            {
                "comparable_workbooks": comparable,
                "deltas": histograms["deltas"],
                "features": list(features),
                "format": format_name,
                "scores": histograms["scores"],
                "workbooks": len(rows),
            }
        )
    return result


def _validate_group_metrics(
    value: object,
    *,
    score: bool,
    expected_count: int,
) -> dict[str, list[list[int]]]:
    expected_metrics = EXPECTED_SCORE_METRICS if score else EXPECTED_DELTA_METRICS
    if not isinstance(value, dict) or set(value) != expected_metrics:
        raise BaselineError("candidate_group_metric_coverage")
    return {
        metric: _validate_histogram(
            histogram,
            score=score,
            expected_count=expected_count,
        )
        for metric, histogram in sorted(value.items())
    }


def _validate_groups(value: object) -> list[dict[str, Any]]:
    if not isinstance(value, list) or not value or len(value) > 800:
        raise BaselineError("candidate_groups")
    result = []
    keys: list[tuple[str, tuple[str, ...]]] = []
    for raw_group in value:
        if not isinstance(raw_group, dict) or set(raw_group) != {
            "comparable_workbooks",
            "deltas",
            "features",
            "format",
            "scores",
            "workbooks",
        }:
            raise BaselineError("candidate_group")
        format_name = raw_group["format"]
        features = raw_group["features"]
        workbooks = raw_group["workbooks"]
        comparable = raw_group["comparable_workbooks"]
        if (
            not isinstance(format_name, str)
            or FORMAT_RE.fullmatch(format_name) is None
            or not isinstance(features, list)
            or len(features) > 256
            or not all(
                isinstance(feature, str)
                and FEATURE_RE.fullmatch(feature) is not None
                for feature in features
            )
            or features != sorted(set(features))
            or type(workbooks) is not int
            or type(comparable) is not int
            or not 0 < workbooks <= 800
            or comparable != workbooks
        ):
            raise BaselineError("candidate_group")
        key = (format_name, tuple(features))
        keys.append(key)
        result.append(
            {
                "comparable_workbooks": comparable,
                "deltas": _validate_group_metrics(
                    raw_group["deltas"],
                    score=False,
                    expected_count=comparable,
                ),
                "features": features,
                "format": format_name,
                "scores": _validate_group_metrics(
                    raw_group["scores"],
                    score=True,
                    expected_count=comparable,
                ),
                "workbooks": workbooks,
            }
        )
    if keys != sorted(set(keys)):
        raise BaselineError("candidate_group_order")
    if (
        sum(group["workbooks"] for group in result) != 800
        or group_topology_sha256(result) != HOSTED_FULL_GROUP_TOPOLOGY_SHA256
    ):
        raise BaselineError("candidate_group_topology")
    format_counts: dict[str, int] = {}
    feature_counts: dict[str, int] = {}
    for group in result:
        format_counts[group["format"]] = (
            format_counts.get(group["format"], 0) + group["workbooks"]
        )
        for feature in group["features"]:
            feature_counts[feature] = (
                feature_counts.get(feature, 0) + group["workbooks"]
            )
    if (
        dict(sorted(format_counts.items())) != HOSTED_FULL_FORMAT_COUNTS
        or dict(sorted(feature_counts.items())) != HOSTED_FULL_FEATURE_COUNTS
    ):
        raise BaselineError("candidate_group_topology")
    return result


def _aggregate_groups(
    groups: list[dict[str, Any]],
) -> tuple[dict[str, Any], dict[str, Any]]:
    workbooks = sum(group["workbooks"] for group in groups)
    comparable = sum(group["comparable_workbooks"] for group in groups)
    histograms: dict[str, Any] = {"deltas": {}, "scores": {}}
    cohort: dict[str, Any] = {
        "comparable_workbooks": comparable,
        "deltas": {},
        "scores": {},
        "workbooks": workbooks,
    }
    for metric_kind, score, expected_metrics in (
        ("scores", True, EXPECTED_SCORE_METRICS),
        ("deltas", False, EXPECTED_DELTA_METRICS),
    ):
        for metric in sorted(expected_metrics):
            histogram = _sum_histograms(
                [group[metric_kind][metric] for group in groups]
            )
            histograms[metric_kind][metric] = histogram
            cohort[metric_kind][metric] = _distribution_from_histogram(
                histogram,
                score=score,
            )
    return cohort, histograms


def _certificate_views_from_groups(
    groups: list[dict[str, Any]],
) -> tuple[dict[str, Any], dict[str, Any]]:
    formats = sorted({group["format"] for group in groups})
    features = sorted(
        {feature for group in groups for feature in group["features"]}
    )
    all_cohort, all_histograms = _aggregate_groups(groups)
    cohorts: dict[str, Any] = {
        "all": all_cohort,
        "by_feature": {},
        "by_format": {},
    }
    histograms: dict[str, Any] = {
        "all": all_histograms,
        "by_feature": {},
        "by_format": {},
    }
    for dimension, names in (("by_format", formats), ("by_feature", features)):
        for name in names:
            selected = [
                group
                for group in groups
                if (
                    group["format"] == name
                    if dimension == "by_format"
                    else name in group["features"]
                )
            ]
            cohort, histogram = _aggregate_groups(selected)
            cohorts[dimension][name] = cohort
            histograms[dimension][name] = histogram
    return cohorts, histograms


def _derive_histograms(files: list[dict[str, Any]]) -> dict[str, Any]:
    groups = derive_group_histograms(files)
    _, histograms = _certificate_views_from_groups(groups)
    return histograms


def _validate_format_partition(cohorts: dict[str, Any]) -> None:
    all_cohort = cohorts["all"]
    format_cohorts = list(cohorts["by_format"].values())
    if (
        all_cohort["workbooks"]
        != sum(cohort["workbooks"] for cohort in format_cohorts)
        or all_cohort["comparable_workbooks"]
        != sum(cohort["comparable_workbooks"] for cohort in format_cohorts)
    ):
        raise BaselineError("campaign_by_format_partition")

    for metric_kind, score in (("scores", True), ("deltas", False)):
        for metric, all_distribution in all_cohort[metric_kind].items():
            distributions = [
                cohort[metric_kind][metric] for cohort in format_cohorts
            ]
            if (
                all_distribution["count"]
                != sum(distribution["count"] for distribution in distributions)
                or all_distribution["min"]
                != min(distribution["min"] for distribution in distributions)
                or all_distribution["max"]
                != max(distribution["max"] for distribution in distributions)
            ):
                raise BaselineError("campaign_by_format_partition")
            all_minimum, all_maximum = _distribution_sum_bounds(
                all_distribution,
                score=score,
            )
            format_bounds = [
                _distribution_sum_bounds(distribution, score=score)
                for distribution in distributions
            ]
            formats_minimum = sum(bounds[0] for bounds in format_bounds)
            formats_maximum = sum(bounds[1] for bounds in format_bounds)
            if max(all_minimum, formats_minimum) > min(
                all_maximum, formats_maximum
            ):
                raise BaselineError("campaign_by_format_partition")


def _validate_campaign_cohorts(
    campaign: dict[str, Any], cohorts: dict[str, Any]
) -> None:
    dimensions = (
        ("by_format", campaign["format_counts"]),
        ("by_feature", campaign["feature_counts"]),
    )
    all_score_metrics = set(cohorts["all"]["scores"])
    all_delta_metrics = set(cohorts["all"]["deltas"])
    if not all_score_metrics or not all_delta_metrics:
        raise BaselineError("campaign_cohort_metrics")
    for dimension, expected_counts in dimensions:
        rows = cohorts[dimension]
        if set(rows) != set(expected_counts):
            raise BaselineError(f"campaign_{dimension}_coverage")
        for name, expected_count in expected_counts.items():
            cohort = rows[name]
            if (
                cohort["workbooks"] != expected_count
                or cohort["comparable_workbooks"] <= 0
                or set(cohort["scores"]) != all_score_metrics
                or set(cohort["deltas"]) != all_delta_metrics
            ):
                raise BaselineError(f"campaign_{dimension}_cohort")
    if cohorts["all"]["workbooks"] != campaign["case_count"]:
        raise BaselineError("campaign_all_cohort")
    _validate_format_partition(cohorts)


def configuration_identity_sha256(configuration: object) -> str:
    if not isinstance(configuration, dict):
        raise BaselineError("evidence_configuration")
    identity = {
        "dpi": configuration.get("dpi"),
        "font_pack": configuration.get("font_pack"),
        "locale": configuration.get("locale"),
        "measurement_toolchain": configuration.get("measurement_toolchain"),
        "metric_policy": configuration.get("metric_policy"),
        "oracle_lock": configuration.get("oracle_lock"),
        "renderer_binary": configuration.get("renderer_binary"),
    }
    return sha256_json(identity)


def derive_baseline(
    evidence: dict[str, Any], campaign: dict[str, Any] | None = None
) -> dict[str, Any]:
    if evidence.get("schema") != EVIDENCE_SCHEMA or evidence.get("mode") != "compare":
        raise BaselineError("evidence_schema_or_mode")
    configuration = evidence.get("configuration")
    summary = evidence.get("summary")
    files = evidence.get("files")
    if not isinstance(configuration, dict) or not isinstance(summary, dict) or not isinstance(files, list):
        raise BaselineError("evidence_shape")
    input_sha, input_count = _input_identity(files)
    statuses = _integer_map(
        summary.get("by_status"),
        "evidence_statuses",
        allowed_keys=STATUS_VALUES,
    )
    classifications = _integer_map(
        summary.get("by_classification"),
        "evidence_classifications",
        key_pattern=CLASSIFICATION_RE,
    )
    raw_statuses, raw_classifications = _raw_file_count_maps(files)
    if statuses != raw_statuses or classifications != raw_classifications:
        raise BaselineError("evidence_summary_file_counts")
    cohorts = _cohorts(summary.get("metric_cohorts"))
    if (
        statuses != {"compared": input_count}
        or classifications != {"within_threshold": input_count}
    ):
        raise BaselineError("evidence_not_full_success")
    raw_cohorts = _derive_raw_cohorts(
        files,
        score_metrics=frozenset(cohorts["all"]["scores"]),
        delta_metrics=frozenset(cohorts["all"]["deltas"]),
    )
    if raw_cohorts != cohorts:
        raise BaselineError("evidence_metric_cohorts")
    comparable = cohorts["all"]["comparable_workbooks"]
    if comparable <= 0:
        raise BaselineError("evidence_has_no_comparisons")
    baseline = {
        "classifications": classifications,
        "cohorts": cohorts,
        "comparable_files": comparable,
        "configuration_sha256": configuration_identity_sha256(configuration),
        "input_files": input_count,
        "input_set_sha256": input_sha,
        "schema": BASELINE_SCHEMA,
        "statuses": statuses,
        "warning_counts": _warning_counts(files),
    }
    if campaign is not None:
        campaign = _validate_campaign(campaign)
        if (
            campaign["case_count"] != input_count
            or campaign["input_set_sha256"] != input_sha
        ):
            raise BaselineError("campaign_evidence_identity_mismatch")
        _validate_campaign_cohorts(campaign, cohorts)
        baseline["campaign"] = campaign
        if campaign["kind"] == HOSTED_FULL_KIND:
            groups = _validate_groups(derive_group_histograms(files))
            certified_cohorts, histograms = _certificate_views_from_groups(groups)
            if certified_cohorts != cohorts:
                raise BaselineError("candidate_group_summary")
            baseline["groups"] = groups
            baseline["histograms"] = _validate_histograms(histograms, cohorts)
            baseline["schema"] = OBSERVED_CANDIDATE_SCHEMA
        else:
            baseline["schema"] = SCOPED_BASELINE_SCHEMA
    if baseline["schema"] == OBSERVED_CANDIDATE_SCHEMA:
        return validate_observed_candidate(baseline)
    return validate_baseline(baseline)


def validate_baseline(value: object) -> dict[str, Any]:
    required = {
        "classifications",
        "cohorts",
        "comparable_files",
        "configuration_sha256",
        "input_files",
        "input_set_sha256",
        "schema",
        "statuses",
        "warning_counts",
    }
    if not isinstance(value, dict):
        raise BaselineError("baseline_shape")
    schema = value.get("schema")
    if schema == SCOPED_BASELINE_SCHEMA:
        required.add("campaign")
    if set(value) != required:
        raise BaselineError("baseline_shape")
    if schema not in {BASELINE_SCHEMA, SCOPED_BASELINE_SCHEMA}:
        raise BaselineError("baseline_schema")
    for key in ("configuration_sha256", "input_set_sha256"):
        if not isinstance(value.get(key), str) or not SHA256_RE.fullmatch(value[key]):
            raise BaselineError("baseline_identity")
    input_files = value.get("input_files")
    comparable_files = value.get("comparable_files")
    if (
        type(input_files) is not int
        or type(comparable_files) is not int
        or not 0 < comparable_files <= input_files
        or input_files > MAX_COUNT
    ):
        raise BaselineError("baseline_counts")
    baseline = {
        "classifications": _integer_map(
            value["classifications"],
            "baseline_classifications",
            key_pattern=CLASSIFICATION_RE,
        ),
        "cohorts": _cohorts(value["cohorts"]),
        "comparable_files": comparable_files,
        "configuration_sha256": value["configuration_sha256"],
        "input_files": input_files,
        "input_set_sha256": value["input_set_sha256"],
        "schema": schema,
        "statuses": _integer_map(
            value["statuses"],
            "baseline_statuses",
            allowed_keys=STATUS_VALUES,
        ),
        "warning_counts": _warning_map(
            value["warning_counts"],
            "baseline_warning_counts",
        ),
    }
    if schema == SCOPED_BASELINE_SCHEMA:
        campaign = _validate_campaign(value["campaign"])
        if (
            campaign["case_count"] != input_files
            or campaign["input_set_sha256"] != value["input_set_sha256"]
        ):
            raise BaselineError("baseline_campaign_identity_mismatch")
        _validate_campaign_cohorts(campaign, baseline["cohorts"])
        baseline["campaign"] = campaign
    return baseline


def validate_observed_candidate(value: object) -> dict[str, Any]:
    if not isinstance(value, dict) or value.get("schema") != OBSERVED_CANDIDATE_SCHEMA:
        raise BaselineError("candidate_schema")
    if set(value) != {
        "campaign",
        "classifications",
        "cohorts",
        "comparable_files",
        "configuration_sha256",
        "groups",
        "histograms",
        "input_files",
        "input_set_sha256",
        "schema",
        "statuses",
        "warning_counts",
    }:
        raise BaselineError("candidate_shape")
    baseline_value = {
        key: raw_value
        for key, raw_value in value.items()
        if key not in {"groups", "histograms"}
    }
    baseline_value["schema"] = SCOPED_BASELINE_SCHEMA
    candidate = validate_baseline(baseline_value)
    campaign = candidate["campaign"]
    if campaign["kind"] != HOSTED_FULL_KIND:
        raise BaselineError("candidate_campaign")
    if (
        candidate["input_files"] != 800
        or candidate["comparable_files"] != 800
        or candidate["statuses"] != {"compared": 800}
        or candidate["classifications"] != {"within_threshold": 800}
    ):
        raise BaselineError("candidate_full_coverage")
    groups = _validate_groups(value["groups"])
    certified_cohorts, certified_histograms = _certificate_views_from_groups(groups)
    if certified_cohorts != candidate["cohorts"]:
        raise BaselineError("candidate_group_summary")
    candidate["histograms"] = _validate_histograms(
        value["histograms"],
        candidate["cohorts"],
    )
    if candidate["histograms"] != certified_histograms:
        raise BaselineError("candidate_group_histograms")
    candidate["groups"] = groups
    candidate["schema"] = OBSERVED_CANDIDATE_SCHEMA
    return candidate


def validate_candidate(value: object) -> dict[str, Any]:
    if isinstance(value, dict) and value.get("schema") == OBSERVED_CANDIDATE_SCHEMA:
        return validate_observed_candidate(value)
    return validate_baseline(value)


def _baseline_view(candidate: dict[str, Any]) -> dict[str, Any]:
    if candidate["schema"] != OBSERVED_CANDIDATE_SCHEMA:
        return candidate
    result = {
        key: value
        for key, value in candidate.items()
        if key not in {"groups", "histograms"}
    }
    result["schema"] = SCOPED_BASELINE_SCHEMA
    return result


def _validate_envelope_distribution(
    value: object,
    *,
    score: bool,
    expected_count: int,
) -> dict[str, int]:
    required = {"count", "mean", "p10"} if score else {"count", "max", "p90"}
    if (
        not isinstance(value, dict)
        or set(value) != required
        or any(type(value[key]) is not int for key in required)
        or value["count"] != expected_count
    ):
        raise BaselineError("envelope_distribution")
    maximum = SCORE_MAX_PPM if score else MAX_DELTA_VALUE
    if any(
        not 0 <= value[key] <= maximum for key in required if key != "count"
    ):
        raise BaselineError("envelope_distribution")
    if not score and value["p90"] > value["max"]:
        raise BaselineError("envelope_distribution")
    return {key: value[key] for key in sorted(required)}


def _validate_envelope_cohort(value: object) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {
        "comparable_workbooks",
        "deltas",
        "scores",
        "workbooks",
    }:
        raise BaselineError("envelope_cohort")
    workbooks = value["workbooks"]
    comparable = value["comparable_workbooks"]
    if (
        type(workbooks) is not int
        or type(comparable) is not int
        or not 0 < workbooks <= MAX_COUNT
        or not 0 < comparable <= workbooks
        or not isinstance(value["scores"], dict)
        or not isinstance(value["deltas"], dict)
    ):
        raise BaselineError("envelope_cohort")
    return {
        "comparable_workbooks": comparable,
        "deltas": {
            metric: _validate_envelope_distribution(
                distribution,
                score=False,
                expected_count=comparable,
            )
            for metric, distribution in sorted(value["deltas"].items())
        },
        "scores": {
            metric: _validate_envelope_distribution(
                distribution,
                score=True,
                expected_count=comparable,
            )
            for metric, distribution in sorted(value["scores"].items())
        },
        "workbooks": workbooks,
    }


def _validate_envelope_cohorts(
    value: object,
    campaign: dict[str, Any],
) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {"all", "by_feature", "by_format"}:
        raise BaselineError("envelope_cohorts")
    result = {"all": _validate_envelope_cohort(value["all"])}
    for dimension in ("by_feature", "by_format"):
        raw_rows = value[dimension]
        expected_counts = (
            campaign["feature_counts"]
            if dimension == "by_feature"
            else campaign["format_counts"]
        )
        if not isinstance(raw_rows, dict) or set(raw_rows) != set(expected_counts):
            raise BaselineError(f"envelope_{dimension}_coverage")
        result[dimension] = {
            name: _validate_envelope_cohort(raw_rows[name])
            for name in sorted(raw_rows)
        }
        for name, expected_count in expected_counts.items():
            cohort = result[dimension][name]
            if (
                cohort["workbooks"] != expected_count
                or cohort["comparable_workbooks"] != expected_count
            ):
                raise BaselineError(f"envelope_{dimension}_cohort")
    all_cohort = result["all"]
    if (
        all_cohort["workbooks"] != campaign["case_count"]
        or all_cohort["comparable_workbooks"] != campaign["case_count"]
        or set(all_cohort["scores"]) != EXPECTED_SCORE_METRICS
        or set(all_cohort["deltas"]) != EXPECTED_DELTA_METRICS
    ):
        raise BaselineError("envelope_all_cohort")
    for dimension in ("by_feature", "by_format"):
        for cohort in result[dimension].values():
            if (
                set(cohort["scores"]) != EXPECTED_SCORE_METRICS
                or set(cohort["deltas"]) != EXPECTED_DELTA_METRICS
            ):
                raise BaselineError("envelope_metric_coverage")
    if (
        all_cohort["workbooks"]
        != sum(cohort["workbooks"] for cohort in result["by_format"].values())
    ):
        raise BaselineError("envelope_by_format_partition")
    return result


def _validate_source_policy(value: object) -> dict[str, Any]:
    required = {
        "candidate_count",
        "candidate_schema",
        "candidate_sha256s",
        "delta_bounds",
        "delta_combination",
        "delta_drift_policy",
        "id",
        "observed_score_drift_maximum_ppm",
        "score_bounds",
        "score_combination",
        "score_drift_ceiling_ppm",
    }
    if not isinstance(value, dict) or set(value) != required:
        raise BaselineError("envelope_source_policy")
    candidate_sha256s = value["candidate_sha256s"]
    if (
        value["id"] != ADOPTION_POLICY
        or value["candidate_count"] != 2
        or value["candidate_schema"] != OBSERVED_CANDIDATE_SCHEMA
        or not isinstance(candidate_sha256s, list)
        or len(candidate_sha256s) != 2
        or candidate_sha256s != sorted(candidate_sha256s)
        or not all(
            isinstance(digest, str) and SHA256_RE.fullmatch(digest)
            for digest in candidate_sha256s
        )
        or value["score_bounds"] != list(SCORE_RATCHETS)
        or value["delta_bounds"] != list(DELTA_RATCHETS)
        or value["score_combination"] != "minimum_across_authenticated_candidates"
        or value["delta_combination"] != "maximum_across_authenticated_candidates"
        or value["delta_drift_policy"] != "exact_histograms"
        or value["score_drift_ceiling_ppm"] != ADOPTION_MAX_SCORE_DRIFT_PPM
    ):
        raise BaselineError("envelope_source_policy")
    limits = _integer_map(
        value["observed_score_drift_maximum_ppm"],
        "envelope_source_policy",
    )
    if (
        set(limits) != ADOPTION_SCORE_METRICS
        or any(limit > ADOPTION_MAX_SCORE_DRIFT_PPM for limit in limits.values())
    ):
        raise BaselineError("envelope_source_policy")
    return {
        "candidate_count": 2,
        "candidate_schema": OBSERVED_CANDIDATE_SCHEMA,
        "candidate_sha256s": candidate_sha256s,
        "delta_bounds": list(DELTA_RATCHETS),
        "delta_combination": "maximum_across_authenticated_candidates",
        "delta_drift_policy": "exact_histograms",
        "id": ADOPTION_POLICY,
        "observed_score_drift_maximum_ppm": limits,
        "score_bounds": list(SCORE_RATCHETS),
        "score_combination": "minimum_across_authenticated_candidates",
        "score_drift_ceiling_ppm": ADOPTION_MAX_SCORE_DRIFT_PPM,
    }


def validate_ratchet_envelope(value: object) -> dict[str, Any]:
    required = {
        "campaign",
        "classifications",
        "cohorts",
        "comparable_files",
        "configuration_sha256",
        "input_files",
        "input_set_sha256",
        "schema",
        "source_policy",
        "statuses",
        "warning_counts",
    }
    if (
        not isinstance(value, dict)
        or set(value) != required
        or value.get("schema") != RATCHET_ENVELOPE_SCHEMA
    ):
        raise BaselineError("envelope_shape")
    campaign = _validate_campaign(value["campaign"])
    if campaign["kind"] != HOSTED_FULL_KIND:
        raise BaselineError("envelope_campaign")
    for key in ("configuration_sha256", "input_set_sha256"):
        if not isinstance(value.get(key), str) or not SHA256_RE.fullmatch(value[key]):
            raise BaselineError("envelope_identity")
    if (
        type(value["input_files"]) is not int
        or type(value["comparable_files"]) is not int
        or value["input_files"] != campaign["case_count"]
        or value["comparable_files"] != campaign["case_count"]
        or value["input_set_sha256"] != campaign["input_set_sha256"]
    ):
        raise BaselineError("envelope_counts")
    envelope = {
        "campaign": campaign,
        "classifications": _integer_map(
            value["classifications"],
            "envelope_classifications",
            key_pattern=CLASSIFICATION_RE,
        ),
        "cohorts": _validate_envelope_cohorts(value["cohorts"], campaign),
        "comparable_files": value["comparable_files"],
        "configuration_sha256": value["configuration_sha256"],
        "input_files": value["input_files"],
        "input_set_sha256": value["input_set_sha256"],
        "schema": RATCHET_ENVELOPE_SCHEMA,
        "source_policy": _validate_source_policy(value["source_policy"]),
        "statuses": _integer_map(
            value["statuses"],
            "envelope_statuses",
            allowed_keys=STATUS_VALUES,
        ),
        "warning_counts": _warning_map(
            value["warning_counts"],
            "envelope_warning_counts",
        ),
    }
    if (
        envelope["statuses"] != {"compared": campaign["case_count"]}
        or envelope["classifications"]
        != {"within_threshold": campaign["case_count"]}
    ):
        raise BaselineError("envelope_full_coverage")
    return envelope


def validate_reviewed_ratchet(value: object) -> dict[str, Any]:
    if isinstance(value, dict) and value.get("schema") == RATCHET_ENVELOPE_SCHEMA:
        return validate_ratchet_envelope(value)
    return validate_baseline(value)


def _compare_count_map(
    baseline: dict[str, int], candidate: dict[str, int], label: str, failures: list[str]
) -> None:
    for key, count in candidate.items():
        if key not in baseline and count:
            failures.append(f"{label}:new:{key}:{count}")
    for key, baseline_count in baseline.items():
        candidate_count = candidate.get(key, 0)
        if candidate_count > baseline_count:
            failures.append(
                f"{label}:increased:{key}:{baseline_count}->{candidate_count}"
            )


def _compare_cohort(
    path: str,
    baseline: dict[str, Any],
    candidate: dict[str, Any],
    failures: list[str],
) -> None:
    if candidate["workbooks"] != baseline["workbooks"]:
        failures.append(
            f"{path}:workbooks:{baseline['workbooks']}->"
            f"{candidate['workbooks']}"
        )
    if candidate["comparable_workbooks"] < baseline["comparable_workbooks"]:
        failures.append(
            f"{path}:coverage:{baseline['comparable_workbooks']}->"
            f"{candidate['comparable_workbooks']}"
        )
    baseline_score_metrics = set(baseline["scores"])
    candidate_score_metrics = set(candidate["scores"])
    for metric in sorted(baseline_score_metrics - candidate_score_metrics):
        failures.append(f"{path}:missing_score:{metric}")
    for metric in sorted(candidate_score_metrics - baseline_score_metrics):
        failures.append(f"{path}:new_score:{metric}")
    for metric, baseline_distribution in baseline["scores"].items():
        candidate_distribution = candidate["scores"].get(metric)
        if candidate_distribution is None:
            continue
        if candidate_distribution["count"] != baseline_distribution["count"]:
            failures.append(
                f"{path}:score_count:{metric}:"
                f"{baseline_distribution['count']}->"
                f"{candidate_distribution['count']}"
            )
        for statistic in SCORE_RATCHETS:
            if candidate_distribution[statistic] < baseline_distribution[statistic]:
                failures.append(
                    f"{path}:score_regression:{metric}:{statistic}:"
                    f"{baseline_distribution[statistic]}->"
                    f"{candidate_distribution[statistic]}"
                )
    baseline_delta_metrics = set(baseline["deltas"])
    candidate_delta_metrics = set(candidate["deltas"])
    for metric in sorted(baseline_delta_metrics - candidate_delta_metrics):
        failures.append(f"{path}:missing_delta:{metric}")
    for metric in sorted(candidate_delta_metrics - baseline_delta_metrics):
        failures.append(f"{path}:new_delta:{metric}")
    for metric, baseline_distribution in baseline["deltas"].items():
        candidate_distribution = candidate["deltas"].get(metric)
        if candidate_distribution is None:
            continue
        if candidate_distribution["count"] != baseline_distribution["count"]:
            failures.append(
                f"{path}:delta_count:{metric}:"
                f"{baseline_distribution['count']}->"
                f"{candidate_distribution['count']}"
            )
        for statistic in DELTA_RATCHETS:
            if candidate_distribution[statistic] > baseline_distribution[statistic]:
                failures.append(
                    f"{path}:delta_regression:{metric}:{statistic}:"
                    f"{baseline_distribution[statistic]}->"
                    f"{candidate_distribution[statistic]}"
                )


def compare(baseline: dict[str, Any], candidate: dict[str, Any]) -> dict[str, Any]:
    baseline = validate_reviewed_ratchet(baseline)
    candidate_document = validate_candidate(candidate)
    if baseline["schema"] == RATCHET_ENVELOPE_SCHEMA:
        if candidate_document["schema"] != OBSERVED_CANDIDATE_SCHEMA:
            raise BaselineError("candidate_observed_schema_required")
        candidate = _baseline_view(candidate_document)
    else:
        candidate = _baseline_view(candidate_document)
    failures: list[str] = []
    identity_keys = ["configuration_sha256", "input_set_sha256", "input_files"]
    if baseline["schema"] != RATCHET_ENVELOPE_SCHEMA:
        identity_keys.insert(0, "schema")
    for key in identity_keys:
        if candidate.get(key) != baseline.get(key):
            failures.append(f"identity_mismatch:{key}")
    if candidate.get("campaign") != baseline.get("campaign"):
        failures.append("identity_mismatch:campaign")
    if candidate.get("comparable_files", 0) < baseline.get("comparable_files", 0):
        failures.append(
            f"coverage:{baseline.get('comparable_files', 0)}->"
            f"{candidate.get('comparable_files', 0)}"
        )
    _compare_count_map(
        baseline.get("statuses", {}), candidate.get("statuses", {}), "status", failures
    )
    _compare_count_map(
        baseline.get("classifications", {}),
        candidate.get("classifications", {}),
        "classification",
        failures,
    )
    _compare_count_map(
        baseline.get("warning_counts", {}),
        candidate.get("warning_counts", {}),
        "warning",
        failures,
    )
    unclassified_warnings = sorted(
        code
        for code, count in candidate.get("warning_counts", {}).items()
        if count and code not in baseline.get("warning_counts", {})
    )
    failures.extend(
        f"warning:unclassified:{code}:"
        f"{candidate['warning_counts'][code]}"
        for code in unclassified_warnings
    )
    baseline_cohorts = baseline.get("cohorts", {})
    candidate_cohorts = candidate.get("cohorts", {})
    _compare_cohort("all", baseline_cohorts["all"], candidate_cohorts["all"], failures)
    for dimension in ("by_format", "by_feature"):
        for name in sorted(
            set(candidate_cohorts[dimension]) - set(baseline_cohorts[dimension])
        ):
            failures.append(f"{dimension}:new:{name}")
        for name, baseline_cohort in baseline_cohorts[dimension].items():
            candidate_cohort = candidate_cohorts[dimension].get(name)
            if candidate_cohort is None:
                failures.append(f"{dimension}:missing:{name}")
                continue
            _compare_cohort(
                f"{dimension}:{name}", baseline_cohort, candidate_cohort, failures
            )
    report = {
        "baseline_sha256": sha256_json(baseline),
        "candidate_sha256": sha256_json(candidate_document),
        "failures": sorted(failures),
        "passed": not failures,
        "schema": REPORT_SCHEMA,
        "warning_policy": {
            "candidate_code_count": len(candidate.get("warning_counts", {})),
            "candidate_counts_sha256": sha256_json(
                candidate.get("warning_counts", {})
            ),
            "reviewed_code_count": len(baseline.get("warning_counts", {})),
            "reviewed_counts_sha256": sha256_json(
                baseline.get("warning_counts", {})
            ),
            "reviewed_codes_sha256": sha256_json(
                sorted(baseline.get("warning_counts", {}))
            ),
            "unclassified_codes": unclassified_warnings,
        },
    }
    campaign = candidate.get("campaign")
    if isinstance(campaign, dict):
        report["campaign"] = {
            "case_count": campaign["case_count"],
            "kind": campaign["kind"],
            "manifest_sha256": campaign["manifest_sha256"],
            "sha256": sha256_json(campaign),
        }
    return report


def _validate_adoption_candidate(value: object) -> dict[str, Any]:
    candidate = validate_observed_candidate(value)
    campaign = candidate.get("campaign")
    all_cohort = candidate["cohorts"]["all"]
    if (
        candidate["schema"] != OBSERVED_CANDIDATE_SCHEMA
        or not isinstance(campaign, dict)
        or campaign["kind"] != HOSTED_FULL_KIND
        or campaign["profile"] != "full"
        or campaign["generator"] != HOSTED_FULL_GENERATOR
        or campaign["generator_version"] != HOSTED_FULL_GENERATOR_VERSION
        or campaign["case_count"] != 800
        or campaign["format_counts"] != HOSTED_FULL_FORMAT_COUNTS
        or campaign["feature_counts"] != HOSTED_FULL_FEATURE_COUNTS
        or candidate["input_files"] != 800
        or candidate["comparable_files"] != 800
        or all_cohort["workbooks"] != 800
        or all_cohort["comparable_workbooks"] != 800
        or candidate["statuses"] != {"compared": 800}
        or candidate["classifications"] != {"within_threshold": 800}
    ):
        raise BaselineError("adoption_full_coverage")
    for dimension in ("all", "by_feature", "by_format"):
        rows = (
            {"all": candidate["cohorts"]["all"]}
            if dimension == "all"
            else candidate["cohorts"][dimension]
        )
        for name, cohort in rows.items():
            if (
                cohort["comparable_workbooks"] != cohort["workbooks"]
                or set(cohort["scores"]) != EXPECTED_SCORE_METRICS
                or set(cohort["deltas"]) != EXPECTED_DELTA_METRICS
            ):
                raise BaselineError(
                    f"adoption_metric_coverage:{dimension}:{name}"
                )
    return candidate


def _dimension_rows(
    value: dict[str, Any],
    dimension: str,
    *,
    histograms: bool = False,
) -> dict[str, Any]:
    source = value["histograms"] if histograms else value["cohorts"]
    return {"all": source["all"]} if dimension == "all" else source[dimension]


def _histogram_ordered_maximum_delta(
    left: list[list[int]],
    right: list[list[int]],
) -> int:
    if sum(count for _, count in left) != sum(count for _, count in right):
        raise BaselineError("adoption_histogram_count")
    left_index = 0
    right_index = 0
    left_remaining = left[0][1]
    right_remaining = right[0][1]
    maximum = 0
    while left_index < len(left) and right_index < len(right):
        maximum = max(
            maximum,
            abs(left[left_index][0] - right[right_index][0]),
        )
        consumed = min(left_remaining, right_remaining)
        left_remaining -= consumed
        right_remaining -= consumed
        if left_remaining == 0:
            left_index += 1
            if left_index < len(left):
                left_remaining = left[left_index][1]
        if right_remaining == 0:
            right_index += 1
            if right_index < len(right):
                right_remaining = right[right_index][1]
    if left_index != len(left) or right_index != len(right):
        raise BaselineError("adoption_histogram_count")
    return maximum


def conservative_adoption_baseline(
    first: dict[str, Any],
    second: dict[str, Any],
    *,
    max_score_drift_ppm: dict[str, int],
) -> dict[str, Any]:
    """Return the order-independent reviewed envelope for two full candidates.

    Only the five score metrics covered by the paired repeatability gate may
    differ, and each statistic is bounded by that run's authenticated observed
    maximum as well as the fixed policy ceiling. All other score distributions
    and every delta distribution remain exact until a unit-calibrated
    repeatability threshold exists for them.
    """

    if (
        not isinstance(max_score_drift_ppm, dict)
        or set(max_score_drift_ppm) != ADOPTION_SCORE_METRICS
        or any(
            type(limit) is not int
            or not 0 <= limit <= ADOPTION_MAX_SCORE_DRIFT_PPM
            for limit in max_score_drift_ppm.values()
        )
    ):
        raise BaselineError("adoption_repeatability_limits")
    left = _validate_adoption_candidate(first)
    right = _validate_adoption_candidate(second)
    invariant_keys = {
        "campaign",
        "classifications",
        "comparable_files",
        "configuration_sha256",
        "input_files",
        "input_set_sha256",
        "schema",
        "statuses",
        "warning_counts",
    }
    for key in sorted(invariant_keys):
        if left.get(key) != right.get(key):
            raise BaselineError(f"adoption_invariant:{key}")

    for left_group, right_group in zip(
        left["groups"],
        right["groups"],
        strict=True,
    ):
        for key in (
            "comparable_workbooks",
            "features",
            "format",
            "workbooks",
        ):
            if left_group[key] != right_group[key]:
                raise BaselineError(f"adoption_group_invariant:{key}")
        for metric_kind, allowed_metrics, score in (
            ("scores", ADOPTION_SCORE_METRICS, True),
            ("deltas", ADOPTION_DELTA_METRICS, False),
        ):
            for metric in sorted(left_group[metric_kind]):
                left_histogram = left_group[metric_kind][metric]
                right_histogram = right_group[metric_kind][metric]
                if left_histogram == right_histogram:
                    continue
                if metric not in allowed_metrics:
                    raise BaselineError(
                        f"adoption_unbounded_group_drift:{metric_kind}:{metric}"
                    )
                if (
                    score
                    and _histogram_ordered_maximum_delta(
                        left_histogram,
                        right_histogram,
                    )
                    > max_score_drift_ppm[metric]
                ):
                    raise BaselineError(
                        f"adoption_group_drift_threshold:{metric_kind}:{metric}"
                    )

    adopted_cohorts: dict[str, Any] = {
        "all": None,
        "by_feature": {},
        "by_format": {},
    }
    for dimension in ("all", "by_feature", "by_format"):
        left_rows = _dimension_rows(left, dimension)
        right_rows = _dimension_rows(right, dimension)
        left_histogram_rows = _dimension_rows(left, dimension, histograms=True)
        right_histogram_rows = _dimension_rows(right, dimension, histograms=True)
        if set(left_rows) != set(right_rows):
            raise BaselineError(f"adoption_topology:{dimension}")
        adopted_rows: dict[str, Any] = {}
        for name in sorted(left_rows):
            left_cohort = left_rows[name]
            right_cohort = right_rows[name]
            for key in ("workbooks", "comparable_workbooks"):
                if left_cohort[key] != right_cohort[key]:
                    raise BaselineError(
                        f"adoption_cohort_invariant:{dimension}:{name}:{key}"
                    )
            adopted_cohort = {
                "comparable_workbooks": left_cohort["comparable_workbooks"],
                "deltas": {},
                "scores": {},
                "workbooks": left_cohort["workbooks"],
            }
            for metric_kind, allowed_metrics, score in (
                ("scores", ADOPTION_SCORE_METRICS, True),
                ("deltas", ADOPTION_DELTA_METRICS, False),
            ):
                left_metrics = left_cohort[metric_kind]
                right_metrics = right_cohort[metric_kind]
                if set(left_metrics) != set(right_metrics):
                    raise BaselineError(
                        f"adoption_metric_topology:{dimension}:{name}:{metric_kind}"
                    )
                for metric in sorted(left_metrics):
                    left_distribution = left_metrics[metric]
                    right_distribution = right_metrics[metric]
                    left_histogram = left_histogram_rows[name][metric_kind][metric]
                    right_histogram = right_histogram_rows[name][metric_kind][metric]
                    histogram_changed = (
                        left_histogram != right_histogram
                    )
                    if (
                        left_distribution != right_distribution
                        or histogram_changed
                    ) and metric not in allowed_metrics:
                        raise BaselineError(
                            f"adoption_unbounded_drift:{dimension}:{name}:{metric}"
                        )
                    if score and metric in allowed_metrics and any(
                        abs(left_distribution[key] - right_distribution[key])
                        > max_score_drift_ppm[metric]
                        for key in set(left_distribution) - {"count"}
                    ):
                        raise BaselineError(
                            f"adoption_drift_threshold:{dimension}:{name}:{metric}"
                        )
                    if (
                        score
                        and metric in allowed_metrics
                        and _histogram_ordered_maximum_delta(
                            left_histogram,
                            right_histogram,
                        )
                        > max_score_drift_ppm[metric]
                    ):
                        raise BaselineError(
                            f"adoption_drift_threshold:{dimension}:{name}:{metric}"
                        )
                    if score:
                        adopted_cohort[metric_kind][metric] = {
                            "count": left_distribution["count"],
                            "mean": min(
                                left_distribution["mean"],
                                right_distribution["mean"],
                            ),
                            "p10": min(
                                left_distribution["p10"],
                                right_distribution["p10"],
                            ),
                        }
                    else:
                        adopted_cohort[metric_kind][metric] = {
                            "count": left_distribution["count"],
                            "max": max(
                                left_distribution["max"],
                                right_distribution["max"],
                            ),
                            "p90": max(
                                left_distribution["p90"],
                                right_distribution["p90"],
                            ),
                        }
            adopted_rows[name] = adopted_cohort
        if dimension == "all":
            adopted_cohorts["all"] = adopted_rows["all"]
        else:
            adopted_cohorts[dimension] = adopted_rows

    adopted = validate_ratchet_envelope(
        {
            "campaign": left["campaign"],
            "classifications": left["classifications"],
            "cohorts": adopted_cohorts,
            "comparable_files": left["comparable_files"],
            "configuration_sha256": left["configuration_sha256"],
            "input_files": left["input_files"],
            "input_set_sha256": left["input_set_sha256"],
            "schema": RATCHET_ENVELOPE_SCHEMA,
            "source_policy": {
                "candidate_count": 2,
                "candidate_schema": OBSERVED_CANDIDATE_SCHEMA,
                "candidate_sha256s": sorted(
                    [sha256_json(left), sha256_json(right)]
                ),
                "delta_bounds": list(DELTA_RATCHETS),
                "delta_combination": "maximum_across_authenticated_candidates",
                "delta_drift_policy": "exact_histograms",
                "id": ADOPTION_POLICY,
                "observed_score_drift_maximum_ppm": dict(
                    sorted(max_score_drift_ppm.items())
                ),
                "score_bounds": list(SCORE_RATCHETS),
                "score_combination": "minimum_across_authenticated_candidates",
                "score_drift_ceiling_ppm": ADOPTION_MAX_SCORE_DRIFT_PPM,
            },
            "statuses": left["statuses"],
            "warning_counts": left["warning_counts"],
        }
    )
    for label, candidate in (("first", left), ("second", right)):
        ratchet = compare(adopted, candidate)
        if not ratchet["passed"]:
            raise BaselineError(
                f"adoption_candidate_rejected:{label}:"
                + ",".join(ratchet["failures"])
            )
    return adopted


def write_atomic(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--campaign-manifest", type=Path)
    parser.add_argument("--candidate-baseline", type=Path)
    parser.add_argument("--create", action="store_true")
    parser.add_argument("--require-hosted-full-800", action="store_true")
    parser.add_argument("--report", type=Path)
    return parser.parse_args()


def _validate_cli_paths(args: argparse.Namespace) -> None:
    inputs = {
        "evidence": args.evidence,
        **(
            {"campaign_manifest": args.campaign_manifest}
            if args.campaign_manifest is not None
            else {}
        ),
        **({"baseline": args.baseline} if not args.create else {}),
    }
    outputs = {
        **({"baseline": args.baseline} if args.create else {}),
        **(
            {"candidate_baseline": args.candidate_baseline}
            if args.candidate_baseline is not None
            else {}
        ),
        **({"report": args.report} if args.report is not None else {}),
    }
    try:
        resolved_inputs = {name: path.resolve() for name, path in inputs.items()}
        resolved_outputs = {name: path.resolve() for name, path in outputs.items()}
    except (OSError, RuntimeError) as error:
        raise BaselineError("path_resolution") from error
    if len(set(resolved_outputs.values())) != len(resolved_outputs):
        raise BaselineError("output_path_alias")
    for output_path in resolved_outputs.values():
        if output_path in resolved_inputs.values():
            raise BaselineError("input_output_path_alias")


def main() -> int:
    args = parse_args()
    candidate: dict[str, Any] | None = None
    source_evidence: dict[str, object] | None = None
    paths_valid = False
    try:
        _validate_cli_paths(args)
        paths_valid = True
        if args.require_hosted_full_800 and args.campaign_manifest is None:
            raise BaselineError("campaign_manifest_required")
        campaign = (
            campaign_from_manifest(
                args.campaign_manifest,
                require_hosted_full_800=args.require_hosted_full_800,
            )
            if args.campaign_manifest is not None
            else None
        )
        evidence, source_evidence = read_json_with_identity(
            args.evidence,
            "evidence",
        )
        candidate = derive_baseline(evidence, campaign)
        if args.candidate_baseline is not None:
            write_atomic(args.candidate_baseline, canonical_bytes(candidate))
        if args.create:
            write_atomic(args.baseline, canonical_bytes(candidate))
            report = {
                "baseline_sha256": sha256_json(candidate),
                "created": True,
                "passed": True,
                "schema": REPORT_SCHEMA,
                "source_evidence": source_evidence,
            }
        else:
            baseline = validate_reviewed_ratchet(
                read_json(args.baseline, "baseline")
            )
            report = compare(baseline, candidate)
            report["source_evidence"] = source_evidence
        rendered = canonical_bytes(report)
        if args.report is not None and paths_valid:
            write_atomic(args.report, rendered)
        else:
            sys.stdout.buffer.write(rendered)
        return 0 if report["passed"] else 1
    except BaselineError as error:
        if args.report is not None and paths_valid:
            report = {
                "failures": [f"error:{error}"],
                "passed": False,
                "schema": REPORT_SCHEMA,
            }
            if source_evidence is not None:
                report["source_evidence"] = source_evidence
            if candidate is not None:
                report["candidate_sha256"] = sha256_json(candidate)
                campaign = candidate.get("campaign")
                if isinstance(campaign, dict):
                    report["campaign"] = {
                        "case_count": campaign["case_count"],
                        "kind": campaign["kind"],
                        "manifest_sha256": campaign["manifest_sha256"],
                        "sha256": sha256_json(campaign),
                    }
            write_atomic(args.report, canonical_bytes(report))
        print(f"check-render-parity-baseline: {error}", file=sys.stderr)
        return 2
    except OSError:
        print("check-render-parity-baseline: filesystem_error", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
