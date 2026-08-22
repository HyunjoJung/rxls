#!/usr/bin/env python3
"""Tests for exact release toolchain evidence."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "record_toolchain_versions.py"


def _load():
    spec = importlib.util.spec_from_file_location("record_toolchain_versions", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ToolchainVersionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.versions = _load()

    @staticmethod
    def _runner(command, **_kwargs):
        outputs = {
            ("rustc", "+1.85.0", "--version"): "rustc 1.85.0 (4d91de4e4 2025-02-17)\n",
            (
                "rustc",
                "+nightly-2026-07-10",
                "--version",
            ): "rustc 1.99.0-nightly (375b1431b 2026-07-10)\n",
            ("cargo", "fuzz", "--version"): "cargo-fuzz 0.13.2\n",
        }
        return subprocess.CompletedProcess(command, 0, stdout=outputs[tuple(command)])

    def test_records_exact_versions_deterministically(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "versions.json"
            payload = self.versions.record_versions(
                "1.85.0",
                "nightly-2026-07-10",
                "0.13.2",
                output,
                self._runner,
            )
            persisted = json.loads(output.read_text(encoding="utf-8"))

        self.assertEqual(payload, persisted)
        self.assertEqual(payload["schema"], "rxls.release-toolchain-versions.v1")
        self.assertEqual(payload["cargo_fuzz"], "cargo-fuzz 0.13.2")

    def test_rejects_wrong_cargo_fuzz_version(self) -> None:
        def runner(command, **kwargs):
            completed = self._runner(command, **kwargs)
            if command == ["cargo", "fuzz", "--version"]:
                completed.stdout = "cargo-fuzz 0.13.1\n"
            return completed

        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(ValueError, "cargo-fuzz differs"):
                self.versions.record_versions(
                    "1.85.0",
                    "nightly-2026-07-10",
                    "0.13.2",
                    Path(tmp) / "versions.json",
                    runner,
                )

    def test_rejects_non_nightly_fuzz_toolchain(self) -> None:
        def runner(command, **kwargs):
            completed = self._runner(command, **kwargs)
            if command == ["rustc", "+nightly-2026-07-10", "--version"]:
                completed.stdout = "rustc 1.99.0 (deadbeef0 2026-07-10)\n"
            return completed

        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(ValueError, "not a nightly"):
                self.versions.record_versions(
                    "1.85.0",
                    "nightly-2026-07-10",
                    "0.13.2",
                    Path(tmp) / "versions.json",
                    runner,
                )


if __name__ == "__main__":
    unittest.main()
