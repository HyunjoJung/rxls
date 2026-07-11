#!/usr/bin/env python3
"""Tests for public-corpus manifest selection helpers."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

from public_corpus_manifest import corpus_files, manifest_files, resolve_binary
from oracle_timeout import run_with_timeout

ROOT = Path(__file__).resolve().parents[1]


class PublicCorpusManifestTests(unittest.TestCase):
    def test_manifest_files_selects_ready_entries_by_extension_and_limit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            xls_path = base / "nested" / "a.xls"
            xlsx_path = base / "nested" / "b.xlsx"
            ods_path = base / "nested" / "c.ods"
            failed_path = base / "nested" / "d.xls"
            for path in [xls_path, xlsx_path, ods_path, failed_path]:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"fixture")

            manifest_path = base / "manifest.json"
            manifest_path.write_text(
                json.dumps(
                    {
                        "files": [
                            {
                                "path": "nested/b.xlsx",
                                "local_path": str(xlsx_path),
                                "status": "downloaded",
                            },
                            {
                                "path": "nested/a.xls",
                                "local_path": str(xls_path),
                                "status": "cached",
                            },
                            {
                                "path": "nested/c.ods",
                                "local_path": str(ods_path),
                            },
                            {
                                "path": "nested/d.xls",
                                "local_path": str(failed_path),
                                "status": "failed",
                            },
                        ]
                    }
                ),
                encoding="utf-8",
            )

            selected = manifest_files(manifest_path, {".xls", ".xlsx"}, limit=10)
            self.assertEqual(selected, sorted([str(xls_path), str(xlsx_path)]))

            limited = manifest_files(manifest_path, {".xls", ".xlsx", ".ods"}, limit=2)
            self.assertEqual(limited, sorted([str(xls_path), str(xlsx_path), str(ods_path)])[:2])

    def test_manifest_files_accepts_manifest_relative_local_paths(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            ods_path = base / "files" / "reader.ods"
            ods_path.parent.mkdir()
            ods_path.write_bytes(b"ods")
            manifest_path = base / "manifest.json"
            manifest_path.write_text(
                json.dumps(
                    {
                        "files": [
                            {
                                "path": "fixtures/reader.ods",
                                "local_path": "files/reader.ods",
                                "status": "downloaded",
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )

            old_cwd = os.getcwd()
            try:
                os.chdir("/")
                self.assertEqual(manifest_files(manifest_path, {".ods"}), [str(ods_path)])
            finally:
                os.chdir(old_cwd)

    def test_corpus_files_selects_flat_directory_files_by_extension(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            for name in ["b.xlsx", "a.xls", "ignored.txt", "c.xlsm"]:
                (base / name).write_bytes(b"fixture")

            self.assertEqual(
                [Path(path).name for path in corpus_files(base, {".xlsx", ".xlsm"})],
                ["b.xlsx", "c.xlsm"],
            )

    def test_fetch_recipe_includes_macro_enabled_ooxml_extensions(self) -> None:
        module = _load_fetch_public_corpus()

        self.assertIn(".xlsm", module.SUPPORTED_EXTS)

    def test_fetch_download_one_accepts_relative_repo_destinations(self) -> None:
        module = _load_fetch_public_corpus()
        rel_dest = Path("target") / f"fetch_relative_dest_{os.getpid()}_{time.time_ns()}"
        payload = b"macro workbook placeholder"
        local_file = (
            ROOT
            / rel_dest
            / "apache-poi"
            / "test-data"
            / "spreadsheet"
            / "demo.xlsm"
        )
        local_file.parent.mkdir(parents=True, exist_ok=True)
        local_file.write_bytes(payload)

        old_cwd = os.getcwd()
        try:
            os.chdir(ROOT)
            result = module.download_one(
                {
                    "source": "apache-poi",
                    "path": "test-data/spreadsheet/demo.xlsm",
                    "size": len(payload),
                },
                rel_dest,
                force=False,
            )
        finally:
            os.chdir(old_cwd)
            shutil.rmtree(ROOT / rel_dest, ignore_errors=True)

        self.assertEqual(result["status"], "cached")
        self.assertEqual(
            result["local_path"],
            str(rel_dest / "apache-poi" / "test-data" / "spreadsheet" / "demo.xlsm"),
        )

    def test_resolve_binary_accepts_windows_exe_suffix(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "extract.exe"
            binary.write_bytes(b"")
            self.assertEqual(resolve_binary(str(Path(tmp) / "extract")), str(binary))

    def test_oracle_script_help_does_not_require_oracle_packages(self) -> None:
        for script in [
            "xls-xlrd-parity.py",
            "xlsx-openpyxl-parity.py",
            "xlsb-pyxlsb-parity.py",
            "ods-odfpy-parity.py",
        ]:
            with self.subTest(script=script):
                output = subprocess.run(
                    [sys.executable, str(ROOT / "scripts" / script), "--help"],
                    check=True,
                    capture_output=True,
                    text=True,
                )
                self.assertIn("--manifest", output.stdout)
                self.assertIn("--limit", output.stdout)

    def test_timeout_helper_marks_slow_oracle_calls_as_timed_out(self) -> None:
        result = run_with_timeout(_slow_oracle_value, ("late", 0.5), timeout_seconds=0.05)

        self.assertEqual(result.status, "timeout")
        self.assertIsNone(result.value)
        self.assertIsNone(result.error)

    def test_timeout_helper_returns_fast_oracle_values(self) -> None:
        result = run_with_timeout(_fast_oracle_value, ("ready",), timeout_seconds=1.0)

        self.assertEqual(result.status, "ok")
        self.assertEqual(result.value, ["ready"])
        self.assertIsNone(result.error)

    def test_ods_harness_help_exposes_oracle_timeout(self) -> None:
        output = subprocess.run(
            [sys.executable, str(ROOT / "scripts" / "ods-odfpy-parity.py"), "--help"],
            check=True,
            capture_output=True,
            text=True,
        )

        self.assertIn("--oracle-timeout-seconds", output.stdout)

    def test_xlsb_harness_help_exposes_expected_values(self) -> None:
        output = subprocess.run(
            [sys.executable, str(ROOT / "scripts" / "xlsb-pyxlsb-parity.py"), "--help"],
            check=True,
            capture_output=True,
            text=True,
        )

        self.assertIn("--expected-values", output.stdout)


def _fast_oracle_value(value: str) -> list[str]:
    return [value]


def _slow_oracle_value(value: str, delay: float) -> list[str]:
    time.sleep(delay)
    return [value]


def _load_fetch_public_corpus():
    spec = importlib.util.spec_from_file_location(
        "fetch_public_corpus", ROOT / "scripts" / "fetch-public-corpus.py"
    )
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


if __name__ == "__main__":
    unittest.main()
