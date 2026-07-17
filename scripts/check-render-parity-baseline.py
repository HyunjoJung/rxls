#!/usr/bin/env python3
"""Create or verify path-neutral LibreOffice parity metric ratchets."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import sys
from typing import Any


EVIDENCE_SCHEMA = "rxls.libreoffice-render-parity.v1"
BASELINE_SCHEMA = "rxls.render-parity-baseline.v1"
SCOPED_BASELINE_SCHEMA = "rxls.render-parity-baseline.v2"
CAMPAIGN_SCHEMA = "rxls.render-parity-campaign.v1"
REPORT_SCHEMA = "rxls.render-parity-baseline-check.v1"
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
MAX_DOCUMENT_BYTES = 64 * 1024 * 1024
SCORE_RATCHETS = ("p10", "mean")
DELTA_RATCHETS = ("p90", "max")
HOSTED_FULL_KIND = "project_generated_hosted_full"
PROJECT_GENERATED_KIND = "project_generated_manifest"
ACQUIRED_CORPUS_KIND = "acquired_corpus_manifest"
HOSTED_FULL_FORMAT_COUNTS = {"ods": 200, "xls": 200, "xlsb": 200, "xlsx": 200}


class BaselineError(RuntimeError):
    pass


def canonical_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def read_json(path: Path, code: str) -> dict[str, Any]:
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise BaselineError(f"{code}_unreadable") from error
    if len(payload) > MAX_DOCUMENT_BYTES:
        raise BaselineError(f"{code}_limit")
    try:
        document = json.loads(payload)
    except json.JSONDecodeError as error:
        raise BaselineError(f"{code}_invalid_json") from error
    if not isinstance(document, dict):
        raise BaselineError(f"{code}_not_object")
    return document


def sha256_json(value: object) -> str:
    return hashlib.sha256(canonical_bytes(value)).hexdigest()


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _integer_map(value: object, code: str) -> dict[str, int]:
    if not isinstance(value, dict):
        raise BaselineError(code)
    result = {}
    for key, count in value.items():
        if (
            not isinstance(key, str)
            or not key
            or not isinstance(count, int)
            or count < 0
        ):
            raise BaselineError(code)
        result[key] = count
    return dict(sorted(result.items()))


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
            or not format_name
        ):
            raise BaselineError("evidence_file_identity")
        features = row.get("features", [])
        if (
            not isinstance(features, list)
            or not all(isinstance(feature, str) and feature for feature in features)
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
            or not all(isinstance(feature, str) and feature for feature in features)
            or features != sorted(set(features))
        ):
            raise BaselineError("campaign_file_features")
        format_counts[format_name] = format_counts.get(format_name, 0) + 1
        for feature in features:
            feature_counts[feature] = feature_counts.get(feature, 0) + 1
    return dict(sorted(format_counts.items())), dict(sorted(feature_counts.items()))


def campaign_from_manifest(
    path: Path, *, require_hosted_full_800: bool = False
) -> dict[str, Any]:
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise BaselineError("campaign_manifest_unreadable") from error
    if len(payload) > MAX_DOCUMENT_BYTES:
        raise BaselineError("campaign_manifest_limit")
    try:
        manifest = json.loads(payload)
    except json.JSONDecodeError as error:
        raise BaselineError("campaign_manifest_invalid_json") from error
    if not isinstance(manifest, dict):
        raise BaselineError("campaign_manifest_not_object")

    files = manifest.get("files")
    input_set_sha256, input_files = _input_identity(files)
    format_counts, feature_counts = _format_and_feature_counts(files)
    declared_formats = _integer_map(
        manifest.get("format_counts"), "campaign_manifest_format_counts"
    )
    declared_features = _integer_map(
        manifest.get("feature_counts"), "campaign_manifest_feature_counts"
    )
    if declared_formats != format_counts or declared_features != feature_counts:
        raise BaselineError("campaign_manifest_counts_mismatch")
    if manifest.get("case_count") != input_files:
        raise BaselineError("campaign_manifest_case_count")
    if sum(format_counts.values()) != input_files:
        raise BaselineError("campaign_manifest_format_coverage")

    hosted_files_are_project_owned = isinstance(files, list) and all(
        isinstance(row, dict)
        and row.get("generator") == "rxls-synthetic-render-corpus"
        and row.get("license") == "MIT"
        and row.get("rights_tier") == "S"
        and row.get("redistribution") == "allowed"
        and row.get("source_redistributable") is True
        and row.get("render_redistributable") is True
        for row in files
    )
    is_hosted_full_800 = (
        input_files == 800
        and format_counts == HOSTED_FULL_FORMAT_COUNTS
        and manifest.get("profile") == "full"
        and manifest.get("generator") == "rxls-synthetic-render-corpus"
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
    elif generator == "rxls-synthetic-render-corpus":
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
        "manifest_sha256": sha256_bytes(payload),
        "profile": manifest.get("profile"),
        "schema": CAMPAIGN_SCHEMA,
    }
    if (
        not isinstance(campaign["generator"], str)
        or not campaign["generator"]
        or not isinstance(campaign["generator_version"], str)
        or not campaign["generator_version"]
        or not isinstance(campaign["profile"], str)
        or not campaign["profile"]
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
        not isinstance(value.get("case_count"), int)
        or value["case_count"] <= 0
        or not isinstance(value.get("generator"), str)
        or not value["generator"]
        or not isinstance(value.get("generator_version"), str)
        or not value["generator_version"]
        or not isinstance(value.get("profile"), str)
        or not value["profile"]
    ):
        raise BaselineError("baseline_campaign_identity")
    for key in ("input_set_sha256", "manifest_sha256"):
        if not isinstance(value.get(key), str) or not SHA256_RE.fullmatch(value[key]):
            raise BaselineError("baseline_campaign_identity")
    format_counts = _integer_map(
        value.get("format_counts"), "baseline_campaign_format_counts"
    )
    feature_counts = _integer_map(
        value.get("feature_counts"), "baseline_campaign_feature_counts"
    )
    if sum(format_counts.values()) != value["case_count"]:
        raise BaselineError("baseline_campaign_coverage")
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
                    or not re.fullmatch(r"[a-z][a-z0-9_]{0,63}", code)
                    or not isinstance(occurrences, int)
                    or occurrences <= 0
                ):
                    raise BaselineError("evidence_warning")
                counts[code] = counts.get(code, 0) + occurrences
    return dict(sorted(counts.items()))


def _validate_distribution(value: object, *, score: bool) -> dict[str, int]:
    required = {"count", "max", "mean", "min", "p10" if score else "p50", "p90" if not score else "p10"}
    if not isinstance(value, dict) or set(value) != required:
        raise BaselineError("evidence_distribution")
    if not all(isinstance(value[key], int) for key in required):
        raise BaselineError("evidence_distribution")
    if value["count"] <= 0:
        raise BaselineError("evidence_distribution")
    return {key: value[key] for key in sorted(required)}


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
        not isinstance(workbooks, int)
        or not isinstance(comparable, int)
        or not 0 <= comparable <= workbooks
    ):
        raise BaselineError("evidence_cohort")
    scores = value["scores"]
    deltas = value["deltas"]
    if not isinstance(scores, dict) or not isinstance(deltas, dict):
        raise BaselineError("evidence_cohort")
    return {
        "comparable_workbooks": comparable,
        "deltas": {
            key: _validate_distribution(distribution, score=False)
            for key, distribution in sorted(deltas.items())
        },
        "scores": {
            key: _validate_distribution(distribution, score=True)
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
        if not isinstance(rows, dict):
            raise BaselineError("evidence_cohorts")
        result[dimension] = {
            key: _validate_cohort(cohort) for key, cohort in sorted(rows.items())
        }
    return result


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
    statuses = _integer_map(summary.get("by_status"), "evidence_statuses")
    classifications = _integer_map(
        summary.get("by_classification"), "evidence_classifications"
    )
    cohorts = _cohorts(summary.get("metric_cohorts"))
    comparable = cohorts["all"]["comparable_workbooks"]
    if comparable <= 0:
        raise BaselineError("evidence_has_no_comparisons")
    if statuses.get("error", 0) or statuses.get("different", 0):
        raise BaselineError("evidence_has_failure")
    identity_configuration = {
        "dpi": configuration.get("dpi"),
        "font_pack": configuration.get("font_pack"),
        "locale": configuration.get("locale"),
        "metric_policy": configuration.get("metric_policy"),
        "oracle_lock": configuration.get("oracle_lock"),
    }
    baseline = {
        "classifications": classifications,
        "cohorts": cohorts,
        "comparable_files": comparable,
        "configuration_sha256": sha256_json(identity_configuration),
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
        baseline["schema"] = SCOPED_BASELINE_SCHEMA
    return baseline


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
        not isinstance(input_files, int)
        or not isinstance(comparable_files, int)
        or not 0 < comparable_files <= input_files
    ):
        raise BaselineError("baseline_counts")
    baseline = {
        "classifications": _integer_map(
            value["classifications"], "baseline_classifications"
        ),
        "cohorts": _cohorts(value["cohorts"]),
        "comparable_files": comparable_files,
        "configuration_sha256": value["configuration_sha256"],
        "input_files": input_files,
        "input_set_sha256": value["input_set_sha256"],
        "schema": schema,
        "statuses": _integer_map(value["statuses"], "baseline_statuses"),
        "warning_counts": _integer_map(
            value["warning_counts"], "baseline_warning_counts"
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
    if candidate["comparable_workbooks"] < baseline["comparable_workbooks"]:
        failures.append(
            f"{path}:coverage:{baseline['comparable_workbooks']}->"
            f"{candidate['comparable_workbooks']}"
        )
    for metric, baseline_distribution in baseline["scores"].items():
        candidate_distribution = candidate["scores"].get(metric)
        if candidate_distribution is None:
            failures.append(f"{path}:missing_score:{metric}")
            continue
        for statistic in SCORE_RATCHETS:
            if candidate_distribution[statistic] < baseline_distribution[statistic]:
                failures.append(
                    f"{path}:score_regression:{metric}:{statistic}:"
                    f"{baseline_distribution[statistic]}->"
                    f"{candidate_distribution[statistic]}"
                )
    for metric, baseline_distribution in baseline["deltas"].items():
        candidate_distribution = candidate["deltas"].get(metric)
        if candidate_distribution is None:
            failures.append(f"{path}:missing_delta:{metric}")
            continue
        for statistic in DELTA_RATCHETS:
            if candidate_distribution[statistic] > baseline_distribution[statistic]:
                failures.append(
                    f"{path}:delta_regression:{metric}:{statistic}:"
                    f"{baseline_distribution[statistic]}->"
                    f"{candidate_distribution[statistic]}"
                )


def compare(baseline: dict[str, Any], candidate: dict[str, Any]) -> dict[str, Any]:
    baseline = validate_baseline(baseline)
    candidate = validate_baseline(candidate)
    failures: list[str] = []
    for key in ("schema", "configuration_sha256", "input_set_sha256", "input_files"):
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
        "candidate_sha256": sha256_json(candidate),
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


def write_atomic(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_bytes(payload)
    temporary.replace(path)


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


def main() -> int:
    args = parse_args()
    candidate: dict[str, Any] | None = None
    try:
        if (
            args.candidate_baseline is not None
            and args.candidate_baseline.resolve() == args.baseline.resolve()
        ):
            raise BaselineError("candidate_baseline_overwrites_reviewed_baseline")
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
        evidence = read_json(args.evidence, "evidence")
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
            }
        else:
            baseline = validate_baseline(read_json(args.baseline, "baseline"))
            report = compare(baseline, candidate)
        rendered = canonical_bytes(report)
        if args.report is not None:
            write_atomic(args.report, rendered)
        else:
            sys.stdout.buffer.write(rendered)
        return 0 if report["passed"] else 1
    except BaselineError as error:
        if args.report is not None:
            report = {
                "failures": [f"error:{error}"],
                "passed": False,
                "schema": REPORT_SCHEMA,
            }
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
