#!/usr/bin/env python3
"""Tests for clean release-bundle reproducibility comparison."""

from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("compare_release_bundles.py")


def _load():
    spec = importlib.util.spec_from_file_location("compare_release_bundles", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _record(path: Path) -> dict[str, object]:
    data = path.read_bytes()
    return {
        "name": path.name,
        "path": f"dist/{path.name}",
        "bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    }


def _write_manifest(root: Path) -> None:
    hygiene = root / "public-hygiene.json"
    artifacts = [
        _record(path)
        for path in sorted(root.iterdir())
        if path.is_file() and path.name != "public-hygiene.json"
    ]
    payload = {
        "schema": "rxls.release-manifest.v1",
        "version": "0.1.2",
        "git_rev": "a" * 40,
        "artifacts": artifacts,
        "evidence": {"public_hygiene": _record(hygiene)},
    }
    (root / "rxls-release-manifest.json").write_text(
        json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def _performance(
    seconds: float = 1.0,
    *,
    peak_rss_bytes: int = 1_000,
    output_bytes: int = 100,
    operation: str = "diagnose",
    label: str = "small",
    passed: bool = True,
) -> str:
    return json.dumps(
        {
            "schema": "rxls.performance-evidence.v1",
            "passed": passed,
            "command": operation,
            "budgets": {"max_seconds": 1.0, "max_rss_mib": 128.0},
            "cases": [
                {
                    "label": label,
                    "path": "fixture.xlsx",
                    "input_bytes": 1,
                    "output_bytes": output_bytes,
                    "output_size_consistent": True,
                    "rss_sampling_complete": True,
                    "repeats": 1,
                    "max_seconds": seconds,
                    "median_seconds": seconds,
                    "max_peak_rss_bytes": peak_rss_bytes,
                    "samples": [
                        {
                            "seconds": seconds,
                            "peak_rss_bytes": peak_rss_bytes,
                            "rss_source": "sampled",
                            "timed_out": False,
                            "output_bytes": output_bytes,
                            "stdout_bytes": 0 if operation == "edit-save" else output_bytes,
                        }
                    ],
                }
            ],
        },
        sort_keys=True,
    )


def _write_performance_set(root: Path) -> None:
    specifications = {
        "release-performance-small.json": ("diagnose", "small", 100),
        "release-performance-medium.json": ("diagnose", "medium", 200),
        "release-performance-edit.json": ("edit-save", "medium-edit", 1_000),
        "release-performance-largest.json": ("diagnose", "largest-corpus", 300),
    }
    for name, (operation, label, output_bytes) in specifications.items():
        (root / name).write_text(
            _performance(
                operation=operation,
                label=label,
                output_bytes=output_bytes,
            ),
            encoding="utf-8",
        )


class CompareReleaseBundlesTests(unittest.TestCase):
    def _bundle_pair(self, base: Path) -> tuple[Path, Path]:
        first = base / "first"
        second = base / "second"
        for root in (first, second):
            root.mkdir()
            (root / "rxls-0.1.2.crate").write_bytes(b"crate")
            _write_performance_set(root)
            (root / "public-hygiene.json").write_text(
                '{"findings": [], "passed": true, "schema": "rxls.public-hygiene-audit.v1"}\n',
                encoding="utf-8",
            )
        return first, second

    def test_identical_bundles_pass(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertTrue(result["passed"], result["errors"])
        self.assertEqual(result["expected_differences"], [])

    def test_performance_measurements_are_explained(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (first / "release-performance-small.json").write_text(
                _performance(1.0, peak_rss_bytes=1_000), encoding="utf-8"
            )
            (second / "release-performance-small.json").write_text(
                _performance(1.19, peak_rss_bytes=1_149), encoding="utf-8"
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertTrue(result["passed"], result["errors"])
        self.assertEqual(
            [item["name"] for item in result["expected_differences"]],
            ["release-performance-small.json", "rxls-release-manifest.json"],
        )

    def test_sub_resolution_same_sha_measurement_drift_passes(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (first / "release-performance-small.json").write_text(
                _performance(0.001365, peak_rss_bytes=516_096), encoding="utf-8"
            )
            (second / "release-performance-small.json").write_text(
                _performance(0.001886, peak_rss_bytes=913_408), encoding="utf-8"
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertTrue(result["passed"], result["errors"])

    def test_hosted_subsecond_scheduling_drift_passes(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (first / "release-performance-edit.json").write_text(
                _performance(
                    0.418578,
                    peak_rss_bytes=69_197_824,
                    operation="edit-save",
                    label="medium-edit",
                    output_bytes=12_755_614,
                ),
                encoding="utf-8",
            )
            (second / "release-performance-edit.json").write_text(
                _performance(
                    0.529258,
                    peak_rss_bytes=69_201_920,
                    operation="edit-save",
                    label="medium-edit",
                    output_bytes=12_755_614,
                ),
                encoding="utf-8",
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertTrue(result["passed"], result["errors"])

    def test_exact_same_sha_performance_increase_limits_pass(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (first / "release-performance-small.json").write_text(
                _performance(1.0, peak_rss_bytes=1_000), encoding="utf-8"
            )
            (second / "release-performance-small.json").write_text(
                _performance(1.2, peak_rss_bytes=1_150), encoding="utf-8"
            )
            (first / "release-performance-edit.json").write_text(
                _performance(
                    operation="edit-save",
                    label="medium-edit",
                    output_bytes=1_000,
                ),
                encoding="utf-8",
            )
            (second / "release-performance-edit.json").write_text(
                _performance(
                    operation="edit-save",
                    label="medium-edit",
                    output_bytes=1_100,
                ),
                encoding="utf-8",
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertTrue(result["passed"], result["errors"])

    def test_same_sha_wall_time_increase_over_twenty_percent_fails(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (first / "release-performance-small.json").write_text(
                _performance(2.0), encoding="utf-8"
            )
            (second / "release-performance-small.json").write_text(
                _performance(2.402), encoding="utf-8"
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertTrue(
            any(
                "same-SHA median wall time increased 20.10%" in error
                for error in result["errors"]
            ),
            result["errors"],
        )

    def test_same_sha_peak_rss_increase_over_fifteen_percent_fails(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (first / "release-performance-small.json").write_text(
                _performance(peak_rss_bytes=128 * 1024 * 1024), encoding="utf-8"
            )
            (second / "release-performance-small.json").write_text(
                _performance(peak_rss_bytes=(128 * 1024 * 1024 * 1_151) // 1_000),
                encoding="utf-8",
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertTrue(
            any(
                "same-SHA peak RSS increased 15.10%" in error
                for error in result["errors"]
            ),
            result["errors"],
        )

    def test_same_sha_edited_output_increase_over_ten_percent_fails(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (first / "release-performance-edit.json").write_text(
                _performance(
                    operation="edit-save",
                    label="medium-edit",
                    output_bytes=1_000,
                ),
                encoding="utf-8",
            )
            (second / "release-performance-edit.json").write_text(
                _performance(
                    operation="edit-save",
                    label="medium-edit",
                    output_bytes=1_101,
                ),
                encoding="utf-8",
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertTrue(
            any(
                "same-SHA edited output size increased 10.10%" in error
                for error in result["errors"]
            ),
            result["errors"],
        )

    def test_identical_nonpassing_performance_evidence_fails(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            for root in (first, second):
                (root / "release-performance-small.json").write_text(
                    _performance(passed=False), encoding="utf-8"
                )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertTrue(
            any("performance evidence did not pass" in error for error in result["errors"]),
            result["errors"],
        )

    def test_zero_baseline_performance_metric_fails(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (first / "release-performance-small.json").write_text(
                _performance(0.0), encoding="utf-8"
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertTrue(
            any("positive median_seconds" in error for error in result["errors"]),
            result["errors"],
        )

    def test_missing_candidate_performance_metric_fails(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            payload = json.loads(
                (second / "release-performance-small.json").read_text(encoding="utf-8")
            )
            del payload["cases"][0]["median_seconds"]
            (second / "release-performance-small.json").write_text(
                json.dumps(payload), encoding="utf-8"
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertTrue(
            any("positive median_seconds" in error for error in result["errors"]),
            result["errors"],
        )

    def test_mismatched_performance_cases_fail(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (second / "release-performance-small.json").write_text(
                _performance(label="different"), encoding="utf-8"
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertTrue(
            any("performance case labels differ" in error for error in result["errors"]),
            result["errors"],
        )

    def test_missing_required_performance_artifact_fails(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            for root in (first, second):
                (root / "release-performance-largest.json").unlink()
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertIn(
            "missing required performance evidence release-performance-largest.json "
            "from baseline and candidate",
            result["errors"],
        )

    def test_unclassified_artifact_difference_fails(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (second / "rxls-0.1.2.crate").write_bytes(b"different")
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertIn(
            "deterministic manifest record differs for rxls-0.1.2.crate",
            result["errors"],
        )

    def test_short_fuzz_campaign_is_rejected(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (first / "fuzz-parse.log").write_text(
                "Done 10 runs in 20 second(s)\n", encoding="utf-8"
            )
            (second / "fuzz-parse.log").write_text(
                "Done 11 runs in 21 second(s)\n", encoding="utf-8"
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertTrue(
            any("120-second campaign" in error for error in result["errors"])
        )

    def test_ansi_colored_finished_fuzz_build_is_accepted(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            for root in (first, second):
                (root / "fuzz-build.log").write_text(
                    "\x1b[1m\x1b[92m    Finished\x1b[0m `release` profile "
                    "[optimized + debuginfo] target(s) in 1m 24s\n",
                    encoding="utf-8",
                )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertTrue(result["passed"], result["errors"])

    def test_unfinished_fuzz_build_is_rejected(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            (first / "fuzz-build.log").write_text(
                "Compiling rxls-fuzz v0.0.0\n", encoding="utf-8"
            )
            (second / "fuzz-build.log").write_text(
                "Finished `release` profile\n", encoding="utf-8"
            )
            _write_manifest(first)
            _write_manifest(second)
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertTrue(any("fuzz build did not finish" in error for error in result["errors"]))

    def test_nested_bundle_content_is_rejected(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            first, second = self._bundle_pair(Path(tmp))
            _write_manifest(first)
            _write_manifest(second)
            (second / "nested").mkdir()
            result = module.compare_bundles(first, second)
        self.assertFalse(result["passed"])
        self.assertTrue(any("bundle must be flat" in error for error in result["errors"]))


if __name__ == "__main__":
    unittest.main()
