#!/usr/bin/env python3
"""Tests for deterministic renderer performance gates."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("render-performance-gates.py")


def load_module():
    spec = importlib.util.spec_from_file_location("render_performance_gates", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def metrics(case: str = "wrapped-cjk") -> dict[str, object]:
    return {
        "artifact_sha256": "a" * 64,
        "backend_commands": 10,
        "case": case,
        "disposition": "rendered",
        "limit_kind": None,
        "output_bytes": 100,
        "pages": 1,
        "schema": "rxls.render-performance-driver.v1",
    }


def sample(case: str = "wrapped-cjk") -> dict[str, object]:
    return {
        "wall_seconds": 0.1,
        "peak_rss_bytes": 1024,
        "rss_source": "sampled",
        "timed_out": False,
        "returncode": 0,
        "stdout_bytes": 100,
        "stderr_bytes": 0,
        "capture_complete": True,
        "metrics": metrics(case),
    }


class RenderPerformanceGateTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.module = load_module()

    def test_strict_driver_schema_rejects_path_fields(self) -> None:
        payload = metrics()
        payload["source_path"] = "/private/runner/work/repository"
        with self.assertRaises(self.module.GateError):
            self.module.parse_driver_payload(json.dumps(payload).encode(), "wrapped-cjk")

    def test_malformed_driver_output_becomes_failed_evidence(self) -> None:
        measured = {
            "returncode": 0,
            "timed_out": False,
            "wall_seconds": 0.01,
            "peak_rss_bytes": 1024,
            "rss_source": "sampled",
            "stdout": b"not-json",
            "stderr": b"",
            "stdout_truncated": False,
            "stderr_truncated": False,
        }
        with mock.patch.object(self.module, "measure_process", return_value=measured):
            result = self.module.run_sample(
                Path("rxls-render-perf"),
                "wrapped-cjk",
                self.module.DEFAULT_BUDGETS["wrapped-cjk"],
            )
        self.assertNotIn("metrics", result)
        self.assertIn("driver_error", result)

    def test_every_cap_fails_independently(self) -> None:
        budget = dict(self.module.DEFAULT_BUDGETS["wrapped-cjk"])
        mutations = {
            "wall_seconds": lambda value: value.update(
                wall_seconds=float(budget["max_wall_seconds"]) + 1
            ),
            "rss_bytes": lambda value: value.update(
                peak_rss_bytes=int(budget["max_rss_bytes"]) + 1
            ),
            "pages": lambda value: value["metrics"].update(
                pages=int(budget["max_pages"]) + 1
            ),
            "backend_commands": lambda value: value["metrics"].update(
                backend_commands=int(budget["max_backend_commands"]) + 1
            ),
            "output_bytes": lambda value: value["metrics"].update(
                output_bytes=int(budget["max_output_bytes"]) + 1
            ),
        }
        for expected, mutate in mutations.items():
            with self.subTest(expected=expected):
                first = sample()
                second = sample()
                mutate(first)
                if expected in {"pages", "backend_commands", "output_bytes"}:
                    mutate(second)
                record, passed = self.module.evaluate_case(
                    "wrapped-cjk", budget, [first, second]
                )
                self.assertFalse(passed)
                self.assertIn(expected, record["violations"])

    def test_missing_rss_fails_closed(self) -> None:
        first = sample()
        second = sample()
        first["peak_rss_bytes"] = None
        second["peak_rss_bytes"] = None
        record, passed = self.module.evaluate_case(
            "wrapped-cjk",
            self.module.DEFAULT_BUDGETS["wrapped-cjk"],
            [first, second],
        )
        self.assertFalse(passed)
        self.assertIn("rss_unavailable", record["violations"])

    def test_repeated_domain_metrics_must_be_identical(self) -> None:
        first = sample()
        second = sample()
        second["metrics"]["artifact_sha256"] = "b" * 64
        record, passed = self.module.evaluate_case(
            "wrapped-cjk",
            self.module.DEFAULT_BUDGETS["wrapped-cjk"],
            [first, second],
        )
        self.assertFalse(passed)
        self.assertFalse(record["deterministic_metrics"])
        self.assertIn("nondeterministic_metrics", record["violations"])

    def test_evidence_never_records_absolute_driver_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            driver = root / "private" / "bin" / "rxls-render-perf"
            driver.parent.mkdir(parents=True)
            driver.write_bytes(b"driver")
            output = root / "evidence.json"
            with mock.patch.object(
                self.module,
                "driver_identity",
                return_value={
                    "name": "rxls-render-perf",
                    "sha256": "c" * 64,
                    "version": "rxls-render-perf 0.1.0",
                },
            ), mock.patch.object(
                self.module,
                "font_pack_identity",
                return_value=(
                    root / "font-pack" / "manifest.json",
                    {
                        "face_count": 1,
                        "faces": [
                            {
                                "family": "Fixture Sans",
                                "sha256": "d" * 64,
                                "style": "normal",
                                "weight": 400,
                            }
                        ],
                        "manifest_sha256": "e" * 64,
                        "pack_sha256": "f" * 64,
                    },
                ),
            ), mock.patch.object(self.module, "run_sample", return_value=sample()):
                status = self.module.main(
                    [
                        "--driver",
                        str(driver),
                        "--case",
                        "wrapped-cjk",
                        "--font-pack-manifest",
                        str(root / "font-pack" / "manifest.json"),
                        "--repeat",
                        "2",
                        "--output",
                        str(output),
                    ]
                )
            rendered = output.read_text(encoding="utf-8")
            payload = json.loads(rendered)
            self.assertEqual(status, 0)
            self.assertNotIn(str(root), rendered)
            self.assertEqual(payload["driver"]["name"], "rxls-render-perf")
            self.assertEqual(payload["font_pack"]["pack_sha256"], "f" * 64)

    def test_shaped_workloads_require_a_verified_font_pack(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            driver = Path(tmp) / "driver"
            driver.write_bytes(b"driver")
            with mock.patch.object(
                self.module,
                "driver_identity",
                return_value={
                    "name": "driver",
                    "sha256": "a" * 64,
                    "version": "fixture",
                },
            ):
                status = self.module.main(
                    ["--driver", str(driver), "--case", "wrapped-cjk"]
                )
        self.assertEqual(status, 2)

    def test_real_timeout_kills_the_process_group(self) -> None:
        started = time.monotonic()
        measured = self.module.measure_process(
            [sys.executable, "-c", "import time; time.sleep(10)"], 0.05
        )
        self.assertTrue(measured["timed_out"])
        self.assertLess(time.monotonic() - started, 2.0)

    def test_default_budget_digest_is_stable_and_complete(self) -> None:
        budgets, digest = self.module.load_budgets(None)
        self.assertEqual(set(budgets), set(self.module.DEFAULT_BUDGETS))
        self.assertRegex(digest, r"^[0-9a-f]{64}$")
        for budget in budgets.values():
            self.assertEqual(
                set(budget),
                {
                    "max_wall_seconds",
                    "max_rss_bytes",
                    "min_pages",
                    "max_pages",
                    "max_backend_commands",
                    "max_output_bytes",
                },
            )


if __name__ == "__main__":
    unittest.main()
