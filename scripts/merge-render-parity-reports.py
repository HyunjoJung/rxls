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
from pathlib import Path
import re
import sys
from typing import Any, Sequence


ROOT = Path(__file__).resolve().parents[1]
HARNESS_PATH = ROOT / "scripts" / "libreoffice-render-parity.py"
EVIDENCE_SCHEMA = "rxls.libreoffice-render-parity.v1"
MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_TOTAL_BYTES = 1024 * 1024 * 1024
MAX_SHARDS = 256
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")


class MergeError(RuntimeError):
    """A shard set is malformed, incomplete, overlapping, or inconsistent."""


def _load_harness() -> Any:
    spec = importlib.util.spec_from_file_location("rxls_render_parity_merge", HARNESS_PATH)
    if spec is None or spec.loader is None:
        raise MergeError("harness_unavailable")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


HARNESS = _load_harness()


def canonical_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def canonical_sha256(value: object) -> str:
    return hashlib.sha256(canonical_bytes(value)).hexdigest()


def read_report(path: Path, remaining_bytes: int) -> tuple[dict[str, Any], int]:
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise MergeError("report_unreadable") from error
    if len(payload) > MAX_REPORT_BYTES or len(payload) > remaining_bytes:
        raise MergeError("report_bytes_limit")
    try:
        document = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise MergeError("report_invalid_json") from error
    if not isinstance(document, dict):
        raise MergeError("report_not_object")
    return document, len(payload)


def _nonnegative_integer(value: object, code: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise MergeError(code)
    return value


def validate_report(report: dict[str, Any]) -> tuple[int, int, list[dict[str, Any]]]:
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
    shard_candidates = _nonnegative_integer(
        discovery["shard_candidate_count"], "shard_candidate_count"
    )
    if selected != shard_candidates or selected != len(files):
        raise MergeError("shard_coverage")
    if summary.get("files") != len(files):
        raise MergeError("summary_file_count")
    for row in files:
        if not isinstance(row, dict):
            raise MergeError("file_row")
        digest = row.get("sha256")
        if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
            raise MergeError("file_identity")
    return shard_count, shard_index, files


def merge_reports(reports: Sequence[dict[str, Any]]) -> dict[str, Any]:
    if len(reports) < 2 or len(reports) > MAX_SHARDS:
        raise MergeError("report_count")
    first = reports[0]
    first_count, _, _ = validate_report(first)
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
    for report in reports:
        shard_count, shard_index, shard_files = validate_report(report)
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
        input_bytes_considered += summary_bytes
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
    return {
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
            "by_classification": dict(sorted(classifications.items())),
            "by_status": dict(sorted(statuses.items())),
            "files": len(files),
            "input_bytes_considered": input_bytes_considered,
            "metric_cohorts": HARNESS.metric_cohorts(files),
        },
    }


def validate_complete_campaign(
    report: dict[str, Any],
) -> tuple[int, int, list[dict[str, Any]]]:
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
    if (
        discovery.get("shard_count") != 1
        or discovery.get("shard_index") != 0
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
    if candidates < selected or summary.get("files") != len(files):
        raise MergeError("campaign_coverage")
    for row in files:
        if not isinstance(row, dict):
            raise MergeError("file_row")
        digest = row.get("sha256")
        if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
            raise MergeError("file_identity")
    return candidates, selected, files


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
    return {
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
            "by_classification": dict(sorted(classifications.items())),
            "by_status": dict(sorted(statuses.items())),
            "files": len(files),
            "input_bytes_considered": input_bytes_considered,
            "metric_cohorts": HARNESS.metric_cohorts(files),
        },
    }


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
        candidates, _, campaign_files = validate_complete_campaign(report)
        if canonical_sha256(report["configuration"]) != configuration_sha:
            raise MergeError("configuration_mismatch")
        if canonical_sha256(report["preflight"]) != preflight_sha:
            raise MergeError("preflight_mismatch")
        candidate_count += candidates
        input_bytes_considered += _nonnegative_integer(
            report["summary"].get("input_bytes_considered"),
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
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_bytes(payload)
    temporary.replace(path)


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
