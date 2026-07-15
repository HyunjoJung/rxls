#!/usr/bin/env python3
"""Regression tests for bounded performance evidence collection."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("measure-performance.py")


def load_module():
    spec = importlib.util.spec_from_file_location("measure_performance", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class MeasurePerformanceTests(unittest.TestCase):
    def test_conservative_rss_combines_polling_and_child_rusage(self) -> None:
        module = load_module()
        self.assertEqual(
            module.conservative_peak_rss_kib(512, 50_000),
            (50_000, "sampled+child-rusage"),
        )
        self.assertEqual(
            module.conservative_peak_rss_kib(50_000, 512),
            (50_000, "sampled+child-rusage"),
        )
        self.assertEqual(
            module.conservative_peak_rss_kib(None, 50_000),
            (50_000, "child-rusage-fallback"),
        )

    def test_inconsistent_output_sizes_fail_the_gate(self) -> None:
        module = load_module()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            case = root / "book.xlsx"
            case.write_bytes(b"fixture")
            output = root / "performance.json"
            calls = 0

            def fake_measure(command, timeout=None):
                nonlocal calls
                calls += 1
                Path(command[2]).write_bytes(b"x" * calls)
                return {
                    "seconds": 0.01,
                    "peak_rss_bytes": 4096,
                    "rss_source": "sampled",
                    "stdout_bytes": 0,
                    "timed_out": False,
                }

            with mock.patch.object(module, "measure", side_effect=fake_measure):
                status = module.main(
                    [
                        "--bin",
                        "edit-save-benchmark",
                        "--operation",
                        "edit-save",
                        "--case",
                        f"medium-edit={case}",
                        "--repeat",
                        "2",
                        "--output",
                        str(output),
                    ]
                )

            payload = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(status, 1)
            self.assertFalse(payload["passed"])
            self.assertFalse(payload["cases"][0]["output_size_consistent"])

    def test_edit_save_records_output_size_and_portable_input_path(self) -> None:
        module = load_module()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            case = root / "book.xlsx"
            case.write_bytes(b"fixture")
            output = root / "performance.json"

            def fake_measure(command, timeout=None):
                self.assertEqual(timeout, 10)
                Path(command[2]).write_bytes(b"edited-workbook")
                return {
                    "seconds": 0.01,
                    "peak_rss_bytes": 4096,
                    "rss_source": "sampled",
                    "stdout_bytes": 0,
                    "timed_out": False,
                }

            with mock.patch.object(module, "measure", side_effect=fake_measure):
                status = module.main(
                    [
                        "--bin",
                        "edit-save-benchmark",
                        "--operation",
                        "edit-save",
                        "--case",
                        f"medium-edit={case}",
                        "--repeat",
                        "2",
                        "--max-seconds",
                        "10",
                        "--max-rss-mib",
                        "768",
                        "--output",
                        str(output),
                    ]
                )

            payload = json.loads(output.read_text(encoding="utf-8"))
            measured = payload["cases"][0]
            self.assertEqual(status, 0)
            self.assertEqual(payload["command"], "edit-save")
            self.assertEqual(measured["path"], "book.xlsx")
            self.assertEqual(measured["input_bytes"], len(b"fixture"))
            self.assertEqual(measured["output_bytes"], len(b"edited-workbook"))
            self.assertTrue(measured["output_size_consistent"])
            self.assertEqual(
                [sample["output_bytes"] for sample in measured["samples"]],
                [len(b"edited-workbook"), len(b"edited-workbook")],
            )

    def test_measure_enforces_a_real_timeout(self) -> None:
        module = load_module()
        started = time.monotonic()
        sample = module.measure(
            [sys.executable, "-c", "import time; time.sleep(10)"], timeout=0.05
        )
        self.assertTrue(sample["timed_out"])
        self.assertLess(time.monotonic() - started, 2.0)

    def test_requested_rss_budget_fails_closed_without_a_sample(self) -> None:
        module = load_module()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            case = root / "book.xlsx"
            case.write_bytes(b"fixture")
            output = root / "performance.json"
            missing_rss = {
                "seconds": 0.001,
                "peak_rss_bytes": None,
                "stdout_bytes": 0,
                "timed_out": False,
            }
            with mock.patch.object(module, "measure", return_value=missing_rss):
                status = module.main(
                    [
                        "--bin",
                        sys.executable,
                        "--case",
                        f"fast={case}",
                        "--repeat",
                        "2",
                        "--max-rss-mib",
                        "1",
                        "--output",
                        str(output),
                    ]
                )

            payload = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(status, 1)
            self.assertFalse(payload["passed"])
            self.assertFalse(payload["cases"][0]["rss_sampling_complete"])


if __name__ == "__main__":
    unittest.main()
