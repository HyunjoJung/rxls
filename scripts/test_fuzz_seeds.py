#!/usr/bin/env python3
"""Tests for deterministic fuzz seed materialization and replay."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from zipfile import ZipFile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "fuzz_seeds.py"
MANIFEST = ROOT / "fuzz" / "seeds" / "manifest.json"


def _load():
    spec = importlib.util.spec_from_file_location("fuzz_seeds", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FuzzSeedTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.seeds = _load()

    def test_manifest_covers_formats_formula_fixtures_and_editable_xlsm(self) -> None:
        _, records = self.seeds.load_manifest(MANIFEST, ROOT)
        by_target = {
            record["target"]: {seed["name"] for seed in record["seeds"]}
            for record in records
        }

        self.assertTrue(
            {"reader-basic.xls", "korean-cp949-biff5.xls", "formula-source.xls"}
            <= by_target["parse"]
        )
        self.assertTrue(
            {"reader-structural.xlsx", "formula-source.xlsx"}
            <= by_target["parse"]
        )
        self.assertTrue(
            {"reader-basic.xlsb", "repeated-hidden.ods", "reader-structural.xlsm"}
            <= by_target["parse"]
        )
        self.assertTrue(
            {"formula-source.xls", "formula-source.xlsx", "formula-expression.bin"}
            <= by_target["formula"]
        )
        self.assertEqual(
            by_target["edit"], {"reader-structural.xlsx", "reader-structural.xlsm"}
        )

        xlsm = next(
            seed
            for record in records
            for seed in record["seeds"]
            if seed["name"] == "reader-structural.xlsm"
        )
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "derived.xlsm"
            path.write_bytes(xlsm["data"])
            with ZipFile(path) as package:
                content_types = package.read("[Content_Types].xml")
        self.assertIn(self.seeds.XLSM_CONTENT_TYPE, content_types)
        self.assertNotIn(self.seeds.XLSX_CONTENT_TYPE, content_types)

    def test_materialization_report_and_files_are_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as first_tmp, tempfile.TemporaryDirectory() as second_tmp:
            first = Path(first_tmp)
            second = Path(second_tmp)
            first_report = first / "manifest.json"
            second_report = second / "manifest.json"

            self.seeds.materialize(MANIFEST, ROOT, first / "corpus", first_report)
            self.seeds.materialize(MANIFEST, ROOT, second / "corpus", second_report)

            self.assertEqual(first_report.read_bytes(), second_report.read_bytes())
            first_files = {
                path.relative_to(first / "corpus"): path.read_bytes()
                for path in (first / "corpus").glob("*/*")
            }
            second_files = {
                path.relative_to(second / "corpus"): path.read_bytes()
                for path in (second / "corpus").glob("*/*")
            }
            self.assertEqual(first_files, second_files)
            self.assertEqual(
                first_files[Path("parse/reader-structural.xlsm")],
                first_files[Path("edit/reader-structural.xlsm")],
            )

    def test_manifest_rejects_empty_required_target_mapping(self) -> None:
        payload = json.loads(MANIFEST.read_text(encoding="utf-8"))
        next(
            target for target in payload["targets"] if target["target"] == "author"
        )["seeds"] = []
        with tempfile.TemporaryDirectory() as tmp:
            manifest = Path(tmp) / "manifest.json"
            manifest.write_text(
                json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
            with self.assertRaisesRegex(ValueError, "at least one seed"):
                self.seeds.load_manifest(manifest, ROOT)

    def test_manifest_rejects_tampered_seed_bytes(self) -> None:
        payload = json.loads(MANIFEST.read_text(encoding="utf-8"))
        next(seed for seed in payload["seeds"] if seed["id"] == "bounded-authoring")[
            "hex"
        ] = "ff"
        with tempfile.TemporaryDirectory() as tmp:
            manifest = Path(tmp) / "manifest.json"
            manifest.write_text(
                json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
            with self.assertRaisesRegex(ValueError, "digest differs"):
                self.seeds.load_manifest(manifest, ROOT)

    def test_replay_runs_every_materialized_seed_once(self) -> None:
        calls: list[list[str]] = []

        def runner(command, **_kwargs):
            calls.append(command)
            return subprocess.CompletedProcess(command, 0, stdout="seed replay passed\n")

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            corpus = root / "corpus"
            self.seeds.materialize(MANIFEST, ROOT, corpus, root / "manifest.json")
            with contextlib.redirect_stdout(io.StringIO()):
                payload = self.seeds.replay(
                    MANIFEST,
                    ROOT,
                    corpus,
                    root / "replay.json",
                    "nightly-2026-07-10",
                    "0.13.2",
                    runner,
                )
            persisted = json.loads((root / "replay.json").read_text(encoding="utf-8"))

        self.assertTrue(payload["passed"])
        self.assertEqual(payload, persisted)
        expected_count = sum(
            len(record["seeds"])
            for record in payload["targets"]
        )
        self.assertEqual(len(calls), expected_count)
        self.assertTrue(all("-runs=1" in command for command in calls))
        self.assertTrue(
            all(command[1] == "+nightly-2026-07-10" for command in calls)
        )

    def test_replay_failure_is_recorded(self) -> None:
        def runner(command, **_kwargs):
            return subprocess.CompletedProcess(command, 1, stdout="failed\n")

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            corpus = root / "corpus"
            self.seeds.materialize(MANIFEST, ROOT, corpus, root / "manifest.json")
            with contextlib.redirect_stdout(io.StringIO()):
                payload = self.seeds.replay(
                    MANIFEST,
                    ROOT,
                    corpus,
                    root / "replay.json",
                    "nightly-2026-07-10",
                    "0.13.2",
                    runner,
                )

        self.assertFalse(payload["passed"])


if __name__ == "__main__":
    unittest.main()
