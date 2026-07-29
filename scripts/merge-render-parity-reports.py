#!/usr/bin/env python3
"""Merge deterministic LibreOffice parity shards or complete campaigns.

The merger is deliberately fail-closed.  Every shard must use the same
renderer binary, oracle, font pack, metric policy, limits, and preflight
identity.  Exactly one report for every shard index is required, input
workbooks may not overlap, and capped/truncated shard sets are rejected.  The
result has the same evidence schema and metric distributions as one unsharded
run over the combined files.  ``--combine-campaigns`` accepts only already
complete, unsharded reports and is used to join independently manifested corpus
lanes without creating a path-bearing local super-manifest.
"""

from __future__ import annotations

import argparse
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

try:
    from render_parity_geometry_gate import (
        GeometryContractError,
        validate_report_identity,
        validate_report_rows,
    )
except ModuleNotFoundError:  # Imported as ``scripts.*`` by unit tests.
    from scripts.render_parity_geometry_gate import (
        GeometryContractError,
        validate_report_identity,
        validate_report_rows,
    )


ROOT = Path(__file__).resolve().parents[1]
HARNESS_PATH = ROOT / "scripts" / "libreoffice-render-parity.py"
EVIDENCE_SCHEMA = "rxls.libreoffice-render-parity.v1"
MAX_REPORT_BYTES = 64 * 1024 * 1024
MAX_TOTAL_BYTES = 1024 * 1024 * 1024
MAX_SHARDS = 256
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_JSON_INTEGER_DIGITS = 128
MAX_JSON_INTEGER = 10**MAX_JSON_INTEGER_DIGITS - 1
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")


class MergeError(RuntimeError):
    """A shard set is malformed, incomplete, overlapping, or inconsistent."""


class _StrictJSONError(ValueError):
    pass


def _load_harness() -> Any:
    spec = importlib.util.spec_from_file_location("rxls_render_parity_merge", HARNESS_PATH)
    if spec is None or spec.loader is None:
        raise MergeError("harness_unavailable")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


HARNESS = _load_harness()
AUTHORED_PRINT_KEYS = {
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


def _validate_geometry_policy(report: dict[str, Any]) -> None:
    configuration = report.get("configuration")
    metric_policy = (
        configuration.get("metric_policy")
        if isinstance(configuration, dict)
        else None
    )
    if (
        not isinstance(metric_policy, dict)
        or not type_exact_equal(
            metric_policy.get("unique_text_geometry"),
            HARNESS.UNIQUE_TEXT_GEOMETRY_POLICY,
        )
    ):
        raise MergeError("metric_policy_unique_text_geometry")


def _validate_authored_print_contract(
    report: dict[str, Any],
    files: Sequence[dict[str, Any]],
) -> None:
    configuration = report["configuration"]
    summary = report["summary"]
    print_mode = configuration.get("print_mode")
    if "authored_print" not in summary:
        raise MergeError("authored_print_summary")
    if print_mode not in HARNESS.PRINT_MODES:
        raise MergeError("print_mode")
    if print_mode == HARNESS.PRINT_MODE_SINGLE_PAGE:
        if (
            summary.get("authored_print") is not None
            or any("authored_print" in row for row in files)
        ):
            raise MergeError("authored_print_contract")
        return

    for row in files:
        value = row.get("authored_print")
        if value is None:
            if row.get("status") in {"compared", "different"}:
                raise MergeError("authored_print_contract")
            continue
        if (
            not isinstance(value, dict)
            or set(value) != AUTHORED_PRINT_KEYS
            or value.get("expected_page_height_pixels") != 1056
            or value.get("expected_page_width_pixels") != 816
            or value.get("header_footer") is not True
            or type(value.get("manual_col_breaks")) is not int
            or value.get("manual_col_breaks") != 1
            or type(value.get("manual_row_breaks")) is not int
            or value.get("manual_row_breaks") != 1
            or value.get("margins") is not True
            or type(value.get("paper_code")) is not int
            or value.get("paper_code") != 1
            or value.get("print_area") is not True
            or value.get("repeated_cols") is not True
            or value.get("repeated_rows") is not True
            or value.get("scale_mode")
            not in HARNESS.AUTHORED_SCALE_MODES
        ):
            raise MergeError("authored_print_contract")
    expected = HARNESS.authored_print_summary(files, str(print_mode))
    if not type_exact_equal(summary.get("authored_print"), expected):
        raise MergeError("authored_print_summary")


def _safe_metric_cohorts(
    files: Sequence[dict[str, Any]],
) -> dict[str, object]:
    try:
        return HARNESS.metric_cohorts(files)
    except (KeyError, TypeError, ValueError, HARNESS.HarnessError) as error:
        raise MergeError("metric_cohorts") from error


def _safe_authored_print_summary(
    files: Sequence[dict[str, Any]],
    print_mode: object,
) -> dict[str, object] | None:
    try:
        return HARNESS.authored_print_summary(files, str(print_mode))
    except (KeyError, TypeError, ValueError, HARNESS.HarnessError) as error:
        raise MergeError("authored_print_summary") from error


def _geometry_complexity(
    files: Sequence[dict[str, Any]],
    *,
    max_pages: int | None = None,
    max_histogram_buckets: int | None = None,
) -> tuple[int, int]:
    if max_pages is None:
        max_pages = HARNESS.MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES
    if max_histogram_buckets is None:
        max_histogram_buckets = (
            HARNESS.MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS
        )
    try:
        return validate_report_rows(
            files,
            max_pages=max_pages,
            max_histogram_buckets=max_histogram_buckets,
        )
    except GeometryContractError as error:
        code = str(error)
        if code in {
            "file_status_or_classification",
            "incomparable_row_metrics",
            "unique_text_geometry_report_limit",
        }:
            pass
        elif code.startswith("page_unique_text_geometry") or code in {
            "page_evidence",
            "unique_text_geometry_pages",
            "unique_text_geometry_pair",
            "unique_text_geometry_page",
            "unique_text_geometry_count",
        }:
            code = "unique_text_geometry_report_shape"
        else:
            code = "report_row_contract"
        raise MergeError(code) from error


def _finalize_merged_report(
    report: dict[str, Any],
) -> dict[str, Any]:
    _geometry_complexity(report["files"])
    try:
        HARNESS.validate_evidence_report_limits(report)
    except HARNESS.HarnessError as error:
        raise MergeError(str(error)) from error
    return report


def canonical_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def canonical_sha256(value: object) -> str:
    return hashlib.sha256(canonical_bytes(value)).hexdigest()


def _object_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise _StrictJSONError("report_duplicate_json_key")
        result[key] = value
    return result


def _reject_json_constant(_value: str) -> object:
    raise _StrictJSONError("report_nonfinite_number")


def _reject_json_number(_value: str) -> object:
    raise _StrictJSONError("report_nonintegral_number")


def _parse_json_integer(token: str) -> int:
    digits = token[1:] if token.startswith("-") else token
    if len(digits) > MAX_JSON_INTEGER_DIGITS:
        raise _StrictJSONError("report_integer_limit")
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
            if nodes > MAX_JSON_NODES:
                raise _StrictJSONError("report_json_complexity")
            closers.append("]" if character == "[" else "}")
            if len(closers) > MAX_JSON_DEPTH:
                raise _StrictJSONError("report_json_depth")
        elif character in "]}":
            if not closers or closers.pop() != character:
                raise _StrictJSONError("report_invalid_json")
        elif character == ",":
            nodes += 1
            if nodes > MAX_JSON_NODES:
                raise _StrictJSONError("report_json_complexity")
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
                raise _StrictJSONError("report_integer_limit")
            if index < len(text) and text[index] in ".eE":
                raise _StrictJSONError("report_nonintegral_number")
            continue
        index += 1
    if closers:
        raise _StrictJSONError("report_invalid_json")


def read_report(path: Path, remaining_bytes: int) -> tuple[dict[str, Any], int]:
    byte_limit = min(MAX_REPORT_BYTES, remaining_bytes)
    if byte_limit <= 0:
        raise MergeError("report_bytes_limit")
    descriptor = -1
    try:
        metadata = path.lstat()
        if (
            path.is_symlink()
            or not stat.S_ISREG(metadata.st_mode)
            or not 0 < metadata.st_size <= byte_limit
        ):
            raise MergeError("report_bytes_limit")
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
                raise MergeError("report_unreadable")
            payload = source.read(byte_limit + 1)
    except OSError as error:
        raise MergeError("report_unreadable") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    if len(payload) != metadata.st_size or len(payload) > byte_limit:
        raise MergeError("report_bytes_limit")
    try:
        text = payload.decode("utf-8")
        _preflight_json_text(text)
        document = json.loads(
            text,
            object_pairs_hook=_object_pairs,
            parse_constant=_reject_json_constant,
            parse_float=_reject_json_number,
            parse_int=_parse_json_integer,
        )
    except _StrictJSONError as error:
        raise MergeError(str(error)) from error
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise MergeError("report_invalid_json") from error
    if not isinstance(document, dict):
        raise MergeError("report_not_object")
    return document, len(payload)


def _nonnegative_integer(value: object, code: str) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or value < 0
        or value > MAX_JSON_INTEGER
    ):
        raise MergeError(code)
    return value


def _bounded_sum(left: int, right: int, code: str) -> int:
    return _nonnegative_integer(left + right, code)


def _validate_report_summary(
    summary: dict[str, Any],
    files: Sequence[dict[str, Any]],
) -> int:
    if set(summary) != {
        "authored_print",
        "by_classification",
        "by_status",
        "files",
        "input_bytes_considered",
        "metric_cohorts",
    }:
        raise MergeError("summary_shape")
    if (
        _nonnegative_integer(
            summary.get("files"),
            "summary_file_count",
        )
        != len(files)
    ):
        raise MergeError("summary_file_count")
    statuses: dict[str, int] = {}
    classifications: dict[str, int] = {}
    input_bytes_considered = 0
    for row in files:
        status = str(row["status"])
        classification = str(row["classification"])
        statuses[status] = statuses.get(status, 0) + 1
        classifications[classification] = (
            classifications.get(classification, 0) + 1
        )
        input_bytes_considered = _bounded_sum(
            input_bytes_considered,
            _nonnegative_integer(
                row["bytes"],
                "input_bytes_considered",
            ),
            "input_bytes_considered",
        )
    if not type_exact_equal(
        summary.get("by_status"),
        dict(sorted(statuses.items())),
    ):
        raise MergeError("summary_status")
    if not type_exact_equal(
        summary.get("by_classification"),
        dict(sorted(classifications.items())),
    ):
        raise MergeError("summary_classification")
    if (
        _nonnegative_integer(
            summary.get("input_bytes_considered"),
            "input_bytes_considered",
        )
        != input_bytes_considered
    ):
        raise MergeError("input_bytes_considered")
    if not type_exact_equal(
        summary.get("metric_cohorts"),
        _safe_metric_cohorts(files),
    ):
        raise MergeError("metric_cohorts")
    return input_bytes_considered


def validate_report(
    report: dict[str, Any],
) -> tuple[int, int, list[dict[str, Any]], int, int]:
    if set(report) != {
        "configuration",
        "discovery",
        "files",
        "mode",
        "preflight",
        "schema",
        "summary",
    }:
        raise MergeError("report_shape")
    if report.get("schema") != EVIDENCE_SCHEMA or report.get("mode") != "compare":
        raise MergeError("report_schema_or_mode")
    if not isinstance(report.get("configuration"), dict) or not isinstance(
        report.get("preflight"), dict
    ):
        raise MergeError("report_identity")
    try:
        validate_report_identity(
            report["configuration"],
            report["preflight"],
        )
    except GeometryContractError as error:
        raise MergeError("report_identity") from error
    _validate_geometry_policy(report)
    discovery = report.get("discovery")
    summary = report.get("summary")
    files = report.get("files")
    if not isinstance(discovery, dict) or not isinstance(summary, dict) or not isinstance(files, list):
        raise MergeError("report_payload")
    required_discovery = {
        "candidate_count",
        "pre_shard_selected_count",
        "selected_count",
        "shard_candidate_count",
        "shard_count",
        "shard_index",
        "truncated",
    }
    if set(discovery) != required_discovery:
        raise MergeError("discovery_shape")
    shard_count = _nonnegative_integer(discovery["shard_count"], "shard_count")
    shard_index = _nonnegative_integer(discovery["shard_index"], "shard_index")
    if not 2 <= shard_count <= MAX_SHARDS or shard_index >= shard_count:
        raise MergeError("shard_identity")
    if discovery.get("truncated") is not False:
        raise MergeError("shard_truncated")
    selected = _nonnegative_integer(discovery["selected_count"], "selected_count")
    candidate_count = _nonnegative_integer(
        discovery["candidate_count"],
        "candidate_count",
    )
    pre_shard_selected_count = _nonnegative_integer(
        discovery["pre_shard_selected_count"],
        "pre_shard_selected_count",
    )
    shard_candidates = _nonnegative_integer(
        discovery["shard_candidate_count"], "shard_candidate_count"
    )
    if (
        candidate_count < pre_shard_selected_count
        or selected > pre_shard_selected_count
        or selected != shard_candidates
        or selected != len(files)
    ):
        raise MergeError("shard_coverage")
    for row in files:
        if not isinstance(row, dict):
            raise MergeError("file_row")
        digest = row.get("sha256")
        if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
            raise MergeError("file_identity")
    _validate_authored_print_contract(report, files)
    geometry_pages, histogram_buckets = _geometry_complexity(
        files,
        max_pages=(
            HARNESS.MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES
            // shard_count
        ),
        max_histogram_buckets=(
            HARNESS.MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS
            // shard_count
        ),
    )
    _validate_report_summary(summary, files)
    return (
        shard_count,
        shard_index,
        files,
        geometry_pages,
        histogram_buckets,
    )


def merge_reports(reports: Sequence[dict[str, Any]]) -> dict[str, Any]:
    if len(reports) < 2 or len(reports) > MAX_SHARDS:
        raise MergeError("report_count")
    first = reports[0]
    first_count, _, _, _, _ = validate_report(first)
    if len(reports) != first_count:
        raise MergeError("incomplete_shard_set")
    configuration_sha = canonical_sha256(first["configuration"])
    preflight_sha = canonical_sha256(first["preflight"])
    base_discovery = first["discovery"]
    candidate_count = _nonnegative_integer(
        base_discovery["candidate_count"], "candidate_count"
    )
    pre_shard_count = _nonnegative_integer(
        base_discovery["pre_shard_selected_count"], "pre_shard_selected_count"
    )
    seen_indexes: set[int] = set()
    seen_inputs: set[str] = set()
    files: list[dict[str, Any]] = []
    input_bytes_considered = 0
    geometry_pages = 0
    geometry_histogram_buckets = 0
    for report in reports:
        (
            shard_count,
            shard_index,
            shard_files,
            shard_geometry_pages,
            shard_histogram_buckets,
        ) = validate_report(report)
        discovery = report["discovery"]
        if shard_count != first_count:
            raise MergeError("shard_count_mismatch")
        if shard_index in seen_indexes:
            raise MergeError("duplicate_shard_index")
        seen_indexes.add(shard_index)
        if (
            discovery["candidate_count"] != candidate_count
            or discovery["pre_shard_selected_count"] != pre_shard_count
        ):
            raise MergeError("discovery_identity_mismatch")
        if canonical_sha256(report["configuration"]) != configuration_sha:
            raise MergeError("configuration_mismatch")
        if canonical_sha256(report["preflight"]) != preflight_sha:
            raise MergeError("preflight_mismatch")
        summary_bytes = _nonnegative_integer(
            report["summary"].get("input_bytes_considered"),
            "input_bytes_considered",
        )
        input_bytes_considered = _bounded_sum(
            input_bytes_considered,
            summary_bytes,
            "input_bytes_considered",
        )
        geometry_pages += shard_geometry_pages
        geometry_histogram_buckets += shard_histogram_buckets
        if (
            geometry_pages
            > HARNESS.MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES
            or geometry_histogram_buckets
            > HARNESS.MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS
        ):
            raise MergeError("unique_text_geometry_report_limit")
        for row in shard_files:
            digest = row["sha256"]
            if digest in seen_inputs:
                raise MergeError("overlapping_input")
            seen_inputs.add(digest)
            files.append(row)
    if seen_indexes != set(range(first_count)):
        raise MergeError("incomplete_shard_indexes")
    if len(files) != pre_shard_count:
        raise MergeError("combined_coverage")
    files.sort(
        key=lambda row: (
            str(row.get("sha256", "")),
            str(row.get("format", "")),
            str(row.get("path", "")),
        )
    )
    statuses: dict[str, int] = {}
    classifications: dict[str, int] = {}
    for row in files:
        status = row.get("status")
        classification = row.get("classification")
        if not isinstance(status, str) or not status:
            raise MergeError("file_status")
        if not isinstance(classification, str) or not classification:
            raise MergeError("file_classification")
        statuses[status] = statuses.get(status, 0) + 1
        classifications[classification] = classifications.get(classification, 0) + 1
    return _finalize_merged_report({
        "configuration": first["configuration"],
        "discovery": {
            "candidate_count": candidate_count,
            "pre_shard_selected_count": pre_shard_count,
            "selected_count": len(files),
            "shard_candidate_count": len(files),
            "shard_count": 1,
            "shard_index": 0,
            "truncated": False,
        },
        "files": files,
        "mode": "compare",
        "preflight": first["preflight"],
        "schema": EVIDENCE_SCHEMA,
        "summary": {
            "authored_print": _safe_authored_print_summary(
                files,
                first["configuration"].get("print_mode"),
            ),
            "by_classification": dict(sorted(classifications.items())),
            "by_status": dict(sorted(statuses.items())),
            "files": len(files),
            "input_bytes_considered": input_bytes_considered,
            "metric_cohorts": _safe_metric_cohorts(files),
        },
    })


def validate_complete_campaign(
    report: dict[str, Any],
) -> tuple[int, int, list[dict[str, Any]], int, int]:
    """Validate one complete unsharded report produced directly or by this merger."""
    if set(report) != {
        "configuration",
        "discovery",
        "files",
        "mode",
        "preflight",
        "schema",
        "summary",
    }:
        raise MergeError("report_shape")
    if report.get("schema") != EVIDENCE_SCHEMA or report.get("mode") != "compare":
        raise MergeError("report_schema_or_mode")
    if not isinstance(report.get("configuration"), dict) or not isinstance(
        report.get("preflight"), dict
    ):
        raise MergeError("report_identity")
    try:
        validate_report_identity(
            report["configuration"],
            report["preflight"],
        )
    except GeometryContractError as error:
        raise MergeError("report_identity") from error
    _validate_geometry_policy(report)
    discovery = report.get("discovery")
    summary = report.get("summary")
    files = report.get("files")
    if not isinstance(discovery, dict) or not isinstance(summary, dict) or not isinstance(files, list):
        raise MergeError("report_payload")
    if set(discovery) != {
        "candidate_count",
        "pre_shard_selected_count",
        "selected_count",
        "shard_candidate_count",
        "shard_count",
        "shard_index",
        "truncated",
    }:
        raise MergeError("discovery_shape")
    shard_count = _nonnegative_integer(
        discovery.get("shard_count"),
        "campaign_incomplete",
    )
    shard_index = _nonnegative_integer(
        discovery.get("shard_index"),
        "campaign_incomplete",
    )
    if (
        shard_count != 1
        or shard_index != 0
        or discovery.get("truncated") is not False
    ):
        raise MergeError("campaign_incomplete")
    selected = _nonnegative_integer(discovery.get("selected_count"), "selected_count")
    pre_shard = _nonnegative_integer(
        discovery.get("pre_shard_selected_count"), "pre_shard_selected_count"
    )
    shard_candidates = _nonnegative_integer(
        discovery.get("shard_candidate_count"), "shard_candidate_count"
    )
    candidates = _nonnegative_integer(
        discovery.get("candidate_count"), "candidate_count"
    )
    if selected != pre_shard or selected != shard_candidates or selected != len(files):
        raise MergeError("campaign_coverage")
    if candidates < selected:
        raise MergeError("campaign_coverage")
    for row in files:
        if not isinstance(row, dict):
            raise MergeError("file_row")
        digest = row.get("sha256")
        if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
            raise MergeError("file_identity")
    _validate_authored_print_contract(report, files)
    geometry_pages, histogram_buckets = _geometry_complexity(files)
    _validate_report_summary(summary, files)
    return (
        candidates,
        selected,
        files,
        geometry_pages,
        histogram_buckets,
    )


def _combined_report(
    first: dict[str, Any],
    files: list[dict[str, Any]],
    *,
    candidate_count: int,
    input_bytes_considered: int,
) -> dict[str, Any]:
    files.sort(
        key=lambda row: (
            str(row.get("sha256", "")),
            str(row.get("format", "")),
            str(row.get("path", "")),
        )
    )
    statuses: dict[str, int] = {}
    classifications: dict[str, int] = {}
    for row in files:
        status = row.get("status")
        classification = row.get("classification")
        if not isinstance(status, str) or not status:
            raise MergeError("file_status")
        if not isinstance(classification, str) or not classification:
            raise MergeError("file_classification")
        statuses[status] = statuses.get(status, 0) + 1
        classifications[classification] = classifications.get(classification, 0) + 1
    return _finalize_merged_report({
        "configuration": first["configuration"],
        "discovery": {
            "candidate_count": candidate_count,
            "pre_shard_selected_count": len(files),
            "selected_count": len(files),
            "shard_candidate_count": len(files),
            "shard_count": 1,
            "shard_index": 0,
            "truncated": False,
        },
        "files": files,
        "mode": "compare",
        "preflight": first["preflight"],
        "schema": EVIDENCE_SCHEMA,
        "summary": {
            "authored_print": _safe_authored_print_summary(
                files,
                first["configuration"].get("print_mode"),
            ),
            "by_classification": dict(sorted(classifications.items())),
            "by_status": dict(sorted(statuses.items())),
            "files": len(files),
            "input_bytes_considered": input_bytes_considered,
            "metric_cohorts": _safe_metric_cohorts(files),
        },
    })


def combine_campaigns(reports: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Combine complete corpus lanes under one exact renderer/oracle identity."""
    if len(reports) < 2 or len(reports) > MAX_SHARDS:
        raise MergeError("report_count")
    first = reports[0]
    configuration_sha = canonical_sha256(first.get("configuration"))
    preflight_sha = canonical_sha256(first.get("preflight"))
    candidate_count = 0
    input_bytes_considered = 0
    seen_inputs: set[str] = set()
    files: list[dict[str, Any]] = []
    for report in reports:
        candidates, _, campaign_files, _, _ = (
            validate_complete_campaign(report)
        )
        if canonical_sha256(report["configuration"]) != configuration_sha:
            raise MergeError("configuration_mismatch")
        if canonical_sha256(report["preflight"]) != preflight_sha:
            raise MergeError("preflight_mismatch")
        candidate_count = _bounded_sum(
            candidate_count,
            candidates,
            "candidate_count",
        )
        input_bytes_considered = _bounded_sum(
            input_bytes_considered,
            _nonnegative_integer(
                report["summary"].get("input_bytes_considered"),
                "input_bytes_considered",
            ),
            "input_bytes_considered",
        )
        for row in campaign_files:
            if row["sha256"] in seen_inputs:
                raise MergeError("overlapping_input")
            seen_inputs.add(row["sha256"])
            files.append(row)
    return _combined_report(
        first,
        files,
        candidate_count=candidate_count,
        input_bytes_considered=input_bytes_considered,
    )


def write_atomic(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="wb",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as target:
            temporary = Path(target.name)
            target.write(payload)
            target.flush()
            os.fsync(target.fileno())
        os.replace(temporary, path)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", nargs="+", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--combine-campaigns",
        action="store_true",
        help="combine complete unsharded corpus-lane reports instead of shards",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        total = 0
        reports = []
        for path in args.reports:
            report, consumed = read_report(path, MAX_TOTAL_BYTES - total)
            total += consumed
            reports.append(report)
        merged = combine_campaigns(reports) if args.combine_campaigns else merge_reports(reports)
        write_atomic(args.output, canonical_bytes(merged))
        return 0
    except MergeError as error:
        print(f"merge-render-parity-reports: {error}", file=sys.stderr)
        return 2
    except OSError:
        print("merge-render-parity-reports: filesystem_error", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
