#!/usr/bin/env python3
"""Compare two clean rxls release bundles and explain every byte difference."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import math
import re
import sys
from pathlib import Path


SCHEMA = "rxls.release-reproducibility.v1"
MANIFEST_SCHEMA = "rxls.release-manifest.v1"
MANIFEST_NAME = "rxls-release-manifest.json"
PERFORMANCE_EXPECTATIONS = {
    "release-performance-small.json": ("diagnose", {"small"}),
    "release-performance-medium.json": ("diagnose", {"medium"}),
    "release-performance-edit.json": ("edit-save", {"medium-edit"}),
    "release-performance-largest.json": ("diagnose", {"largest-corpus"}),
}
PERFORMANCE_NAMES = set(PERFORMANCE_EXPECTATIONS)
TEST_EVIDENCE_NAMES = {
    "release-formula-evidence.txt",
    "release-evaluation-evidence.txt",
    "release-edit-unit-evidence.txt",
    "release-edit-integration-evidence.txt",
}
FUZZ_NAMES = {
    "fuzz-build.log",
    "fuzz-parse.log",
    "fuzz-author.log",
    "fuzz-edit.log",
    "fuzz-formula.log",
}
EXPECTED_VARIABLE_NAMES = PERFORMANCE_NAMES | TEST_EVIDENCE_NAMES | FUZZ_NAMES
FUZZ_FAILURE_MARKERS = (
    "ERROR: libFuzzer",
    "SUMMARY: AddressSanitizer",
    "SUMMARY: UndefinedBehaviorSanitizer",
    "deadly signal",
    "panicked at",
)
WALL_TIME_INCREASE_LIMIT = 0.20
PEAK_RSS_INCREASE_LIMIT = 0.15
EDIT_OUTPUT_INCREASE_LIMIT = 0.10
WALL_TIME_ABSOLUTE_NOISE_SECONDS = 0.250
PEAK_RSS_ABSOLUTE_NOISE_BYTES = 16 * 1024 * 1024
ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _files(root: Path) -> dict[str, Path]:
    if not root.is_dir():
        raise ValueError(f"bundle is not a directory: {root}")
    entries = list(root.iterdir())
    non_files = [path.name for path in entries if not path.is_file()]
    if non_files:
        raise ValueError(f"bundle must be flat; non-files found: {sorted(non_files)}")
    files = {path.name: path for path in entries}
    return files


def _load_manifest(path: Path) -> dict[str, object]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("schema") != MANIFEST_SCHEMA:
        raise ValueError(f"unexpected release manifest schema in {path}")
    return payload


def _manifest_records(payload: dict[str, object]) -> dict[str, dict[str, object]]:
    records = payload.get("artifacts")
    if not isinstance(records, list):
        raise ValueError("release manifest artifacts must be an array")
    result: dict[str, dict[str, object]] = {}
    for record in records:
        if not isinstance(record, dict) or not isinstance(record.get("name"), str):
            raise ValueError("release manifest has an invalid artifact record")
        name = record["name"]
        if name in result:
            raise ValueError(f"release manifest repeats artifact {name}")
        result[name] = record
    return result


def _verify_manifest_files(
    payload: dict[str, object], files: dict[str, Path], label: str
) -> list[str]:
    errors: list[str] = []
    records = _manifest_records(payload)
    evidence = payload.get("evidence")
    if not isinstance(evidence, dict):
        return [f"{label}: manifest has no valid evidence object"]
    hygiene = evidence.get("public_hygiene")
    if not isinstance(hygiene, dict) or not isinstance(hygiene.get("name"), str):
        return [f"{label}: manifest has no valid public-hygiene record"]
    covered = set(records) | {hygiene["name"], MANIFEST_NAME}
    if covered != set(files):
        errors.append(
            f"{label}: manifest coverage differs: "
            f"missing={sorted(set(files) - covered)} extra={sorted(covered - set(files))}"
        )
    for record in [*records.values(), hygiene]:
        name = record["name"]
        path = files.get(name)
        if path is None:
            continue
        if record.get("bytes") != path.stat().st_size:
            errors.append(f"{label}: size record is wrong for {name}")
        if record.get("sha256") != sha256_file(path):
            errors.append(f"{label}: checksum record is wrong for {name}")
    hygiene_path = files.get(hygiene["name"])
    if hygiene_path is not None:
        try:
            hygiene_payload = json.loads(hygiene_path.read_text(encoding="utf-8"))
            if (
                hygiene_payload.get("schema") != "rxls.public-hygiene-audit.v1"
                or hygiene_payload.get("passed") is not True
                or hygiene_payload.get("findings") != []
            ):
                errors.append(f"{label}: public-hygiene evidence did not pass")
        except (OSError, json.JSONDecodeError):
            errors.append(f"{label}: public-hygiene evidence is unreadable")
    return errors


def _positive_metric(value: object, field: str, path: Path) -> float:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value <= 0
    ):
        raise ValueError(f"invalid or missing positive {field} in {path}")
    return float(value)


def _load_performance(path: Path) -> dict[str, object]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("schema") != "rxls.performance-evidence.v1":
        raise ValueError(f"unexpected performance schema in {path}")
    if payload.get("passed") is not True:
        raise ValueError(f"performance evidence did not pass: {path}")
    operation = payload.get("command")
    if operation not in {"diagnose", "edit-save"}:
        raise ValueError(f"invalid or missing performance operation in {path}")
    cases = payload.get("cases")
    if not isinstance(cases, list) or not cases:
        raise ValueError(f"performance evidence has no cases: {path}")
    labels: set[str] = set()
    for case in cases:
        if not isinstance(case, dict):
            raise ValueError(f"invalid performance case in {path}")
        label = case.get("label")
        if not isinstance(label, str) or not label or label in labels:
            raise ValueError(f"invalid or repeated performance case label in {path}")
        labels.add(label)
        if case.get("rss_sampling_complete") is not True:
            raise ValueError(f"incomplete RSS sampling in {path}")
        if case.get("output_size_consistent") is not True:
            raise ValueError(f"inconsistent output sizes in {path}")
        _positive_metric(case.get("median_seconds"), "median_seconds", path)
        _positive_metric(case.get("max_peak_rss_bytes"), "max_peak_rss_bytes", path)
        _positive_metric(case.get("output_bytes"), "output_bytes", path)
        samples = case.get("samples")
        if not isinstance(samples, list) or not samples:
            raise ValueError(f"performance case has no samples in {path}")
        for sample in samples:
            if not isinstance(sample, dict) or sample.get("timed_out") is not False:
                raise ValueError(f"timed-out or invalid performance sample in {path}")
            _positive_metric(sample.get("seconds"), "sample seconds", path)
            _positive_metric(sample.get("peak_rss_bytes"), "sample peak RSS", path)
            _positive_metric(sample.get("output_bytes"), "sample output bytes", path)
    return payload


def _normalize_performance_payload(
    payload: dict[str, object], path: Path
) -> dict[str, object]:
    normalized = copy.deepcopy(payload)
    operation = normalized["command"]
    cases = normalized["cases"]
    assert isinstance(cases, list)
    for case in cases:
        assert isinstance(case, dict)
        for key in ("max_peak_rss_bytes", "max_seconds", "median_seconds"):
            case.pop(key, None)
        if operation == "edit-save":
            case.pop("output_bytes", None)
        samples = case["samples"]
        assert isinstance(samples, list)
        for sample in samples:
            assert isinstance(sample, dict)
            for key in ("peak_rss_bytes", "rss_source", "seconds"):
                sample.pop(key, None)
            if operation == "edit-save":
                sample.pop("output_bytes", None)
    return normalized


def _normalized_performance(path: Path) -> dict[str, object]:
    return _normalize_performance_payload(_load_performance(path), path)


def _case_map(payload: dict[str, object]) -> dict[str, dict[str, object]]:
    cases = payload["cases"]
    assert isinstance(cases, list)
    return {str(case["label"]): case for case in cases if isinstance(case, dict)}


def _increase_error(
    name: str,
    label: str,
    description: str,
    baseline: float,
    candidate: float,
    limit: float,
    absolute_noise: float = 0.0,
) -> str | None:
    increase = candidate / baseline - 1.0
    absolute_increase = candidate - baseline
    if (
        increase <= limit
        or math.isclose(increase, limit, rel_tol=1e-12, abs_tol=1e-12)
        or absolute_increase <= absolute_noise
        or math.isclose(
            absolute_increase, absolute_noise, rel_tol=1e-12, abs_tol=1e-12
        )
    ):
        return None
    message = (
        f"{name} case {label!r}: same-SHA {description} increased {increase:.2%} "
        f"(reference={baseline:g}, candidate={candidate:g}); "
        f"reproducibility limit is {limit:.0%}"
    )
    if absolute_noise > 0:
        message += f" beyond an absolute noise allowance of {absolute_noise:g}"
    return message


def _validate_performance_pair(first: Path, second: Path, name: str) -> str:
    baseline = _load_performance(first)
    candidate = _load_performance(second)
    expected_operation, expected_labels = PERFORMANCE_EXPECTATIONS[name]
    for role, payload in (("baseline", baseline), ("candidate", candidate)):
        if payload["command"] != expected_operation:
            raise ValueError(
                f"{role} performance operation for {name} must be "
                f"{expected_operation!r}, got {payload['command']!r}"
            )
        actual_labels = set(_case_map(payload))
        if actual_labels != expected_labels:
            raise ValueError(
                f"{role} performance case labels differ from the release contract "
                f"for {name}: expected={sorted(expected_labels)} "
                f"actual={sorted(actual_labels)}"
            )
    if baseline["command"] != candidate["command"]:
        raise ValueError(f"performance operation differs for {name}")
    baseline_cases = _case_map(baseline)
    candidate_cases = _case_map(candidate)
    if set(baseline_cases) != set(candidate_cases):
        raise ValueError(
            f"performance case labels differ for {name}: "
            f"baseline_only={sorted(set(baseline_cases) - set(candidate_cases))} "
            f"candidate_only={sorted(set(candidate_cases) - set(baseline_cases))}"
        )
    if _normalize_performance_payload(
        baseline, first
    ) != _normalize_performance_payload(candidate, second):
        raise ValueError(
            f"performance evidence changed outside permitted measurement fields: {name}"
        )

    errors: list[str] = []
    for label in sorted(baseline_cases):
        baseline_case = baseline_cases[label]
        candidate_case = candidate_cases[label]
        metrics = [
            (
                "median wall time",
                "median_seconds",
                WALL_TIME_INCREASE_LIMIT,
                WALL_TIME_ABSOLUTE_NOISE_SECONDS,
            ),
            (
                "peak RSS",
                "max_peak_rss_bytes",
                PEAK_RSS_INCREASE_LIMIT,
                PEAK_RSS_ABSOLUTE_NOISE_BYTES,
            ),
        ]
        if baseline["command"] == "edit-save":
            metrics.append(
                ("edited output size", "output_bytes", EDIT_OUTPUT_INCREASE_LIMIT, 0.0)
            )
        for description, field, limit, absolute_noise in metrics:
            baseline_value = _positive_metric(baseline_case.get(field), field, first)
            candidate_value = _positive_metric(candidate_case.get(field), field, second)
            error = _increase_error(
                name,
                label,
                description,
                baseline_value,
                candidate_value,
                limit,
                absolute_noise,
            )
            if error is not None:
                errors.append(error)
    if errors:
        raise ValueError("; ".join(errors))
    if baseline["command"] == "edit-save":
        return "runtime/RSS and edited output within same-SHA reproducibility limits"
    return "runtime timing/RSS within same-SHA reproducibility limits"


def _normalized_test_evidence(path: Path) -> str:
    text = path.read_text(encoding="utf-8")
    if "test result: ok." not in text or " 0 failed;" not in text:
        raise ValueError(f"test evidence did not pass: {path}")
    return re.sub(r"finished in [0-9]+(?:[.][0-9]+)?s", "finished in <time>s", text)


def _validate_fuzz(path: Path, name: str) -> None:
    text = path.read_text(encoding="utf-8", errors="replace")
    for marker in FUZZ_FAILURE_MARKERS:
        if marker.casefold() in text.casefold():
            raise ValueError(f"fuzz failure marker {marker!r} in {path}")
    if name == "fuzz-build.log":
        if "Finished `release` profile" not in ANSI_ESCAPE_RE.sub("", text):
            raise ValueError(f"fuzz build did not finish: {path}")
        return
    matches = re.findall(r"Done [0-9]+ runs in ([0-9]+) second", text)
    if not matches or max(int(seconds) for seconds in matches) < 120:
        raise ValueError(f"fuzz target lacks a completed 120-second campaign: {path}")


def _validate_expected_difference(first: Path, second: Path, name: str) -> str:
    if name in PERFORMANCE_NAMES:
        return _validate_performance_pair(first, second, name)
    if name in TEST_EVIDENCE_NAMES:
        if _normalized_test_evidence(first) != _normalized_test_evidence(second):
            raise ValueError(f"test evidence changed outside elapsed duration: {name}")
        return "test elapsed duration"
    if name in FUZZ_NAMES:
        _validate_fuzz(first, name)
        _validate_fuzz(second, name)
        return "fuzz seed, coverage, throughput, corpus growth, or elapsed diagnostics"
    raise ValueError(f"no difference policy for {name}")


def compare_bundles(first_root: Path, second_root: Path) -> dict[str, object]:
    errors: list[str] = []
    identical: list[str] = []
    expected_differences: list[dict[str, str]] = []
    version: object = None
    git_rev: object = None
    try:
        first = _files(first_root)
        second = _files(second_root)
        if set(first) != set(second):
            errors.append(
                "bundle file sets differ: "
                f"first_only={sorted(set(first) - set(second))} "
                f"second_only={sorted(set(second) - set(first))}"
            )
        if MANIFEST_NAME not in first or MANIFEST_NAME not in second:
            raise ValueError("both bundles must contain rxls-release-manifest.json")
        first_manifest = _load_manifest(first[MANIFEST_NAME])
        second_manifest = _load_manifest(second[MANIFEST_NAME])
        version = first_manifest.get("version")
        git_rev = first_manifest.get("git_rev")
        for field in ("schema", "version", "git_rev"):
            if first_manifest.get(field) != second_manifest.get(field):
                errors.append(f"release manifest {field} differs")
        errors.extend(_verify_manifest_files(first_manifest, first, "first"))
        errors.extend(_verify_manifest_files(second_manifest, second, "second"))

        performance_validation: dict[str, str | None] = {}
        for name in sorted(PERFORMANCE_NAMES):
            missing = []
            if name not in first:
                missing.append("baseline")
            if name not in second:
                missing.append("candidate")
            if missing:
                errors.append(
                    f"missing required performance evidence {name} from "
                    + " and ".join(missing)
                )
                performance_validation[name] = None
                continue
            try:
                performance_validation[name] = _validate_performance_pair(
                    first[name], second[name], name
                )
            except (OSError, ValueError, json.JSONDecodeError) as error:
                errors.append(str(error))
                performance_validation[name] = None

        first_records = _manifest_records(first_manifest)
        second_records = _manifest_records(second_manifest)
        if set(first_records) != set(second_records):
            errors.append("release manifest artifact sets differ")
        for name in sorted(set(first_records) & set(second_records)):
            left = first_records[name]
            right = second_records[name]
            if name in EXPECTED_VARIABLE_NAMES:
                stable_left = {
                    key: value
                    for key, value in left.items()
                    if key not in {"bytes", "sha256"}
                }
                stable_right = {
                    key: value
                    for key, value in right.items()
                    if key not in {"bytes", "sha256"}
                }
                if stable_left != stable_right:
                    errors.append(f"stable manifest fields differ for {name}")
            elif left != right:
                errors.append(f"deterministic manifest record differs for {name}")
        if first_manifest.get("evidence") != second_manifest.get("evidence"):
            errors.append("public-hygiene manifest evidence differs")

        for name in sorted(set(first) & set(second)):
            left_hash = sha256_file(first[name])
            right_hash = sha256_file(second[name])
            if left_hash == right_hash:
                identical.append(name)
                continue
            if name == MANIFEST_NAME:
                expected_differences.append(
                    {
                        "name": name,
                        "reason": "derived checksums for validated variable evidence",
                        "first_sha256": left_hash,
                        "second_sha256": right_hash,
                    }
                )
                continue
            if name in PERFORMANCE_NAMES:
                reason = performance_validation.get(name)
                if reason is not None:
                    expected_differences.append(
                        {
                            "name": name,
                            "reason": reason,
                            "first_sha256": left_hash,
                            "second_sha256": right_hash,
                        }
                    )
                continue
            try:
                reason = _validate_expected_difference(first[name], second[name], name)
                expected_differences.append(
                    {
                        "name": name,
                        "reason": reason,
                        "first_sha256": left_hash,
                        "second_sha256": right_hash,
                    }
                )
            except (OSError, ValueError, json.JSONDecodeError) as error:
                errors.append(str(error))
    except (OSError, ValueError, json.JSONDecodeError) as error:
        errors.append(str(error))

    return {
        "schema": SCHEMA,
        "passed": not errors,
        "version": version,
        "git_rev": git_rev,
        "first_bundle": first_root.as_posix(),
        "second_bundle": second_root.as_posix(),
        "identical": identical,
        "expected_differences": expected_differences,
        "errors": errors,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("first", type=Path)
    parser.add_argument("second", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    result = compare_bundles(args.first, args.second)
    rendered = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
    else:
        sys.stdout.write(rendered)
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
