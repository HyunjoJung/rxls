#!/usr/bin/env python3
"""Tests for public-corpus manifest selection helpers."""

from __future__ import annotations

import importlib.util
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import redirect_stdout
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

from public_corpus_manifest import (
    corpus_files,
    emit_parity_provenance,
    manifest_sha256,
    manifest_files,
    report_path,
    report_reason,
    resolve_binary,
)
from oracle_timeout import _read_result, run_with_timeout

ROOT = Path(__file__).resolve().parents[1]


class PublicCorpusManifestTests(unittest.TestCase):
    def test_parity_provenance_emits_reader_version_and_exact_manifest_digest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest_path = Path(tmp) / "manifest.json"
            payload = b'{"schema":"fixture","files":[]}\n'
            manifest_path.write_bytes(payload)
            output = io.StringIO()
            with redirect_stdout(output):
                emit_parity_provenance(
                    manifest_path,
                    oracle_reader="fixture-reader",
                )
            expected_digest = hashlib.sha256(payload).hexdigest()
            self.assertEqual(manifest_sha256(manifest_path), expected_digest)

        self.assertIn(
            "provenance: oracle_reader=fixture-reader oracle_version=python-",
            output.getvalue(),
        )
        self.assertIn(
            f"provenance: input_manifest_sha256={expected_digest}",
            output.getvalue(),
        )

    def test_parity_provenance_marks_non_manifest_corpus_runs(self) -> None:
        output = io.StringIO()
        with redirect_stdout(output):
            emit_parity_provenance(None, oracle_reader="fixture-reader")
        self.assertIn("provenance: input_manifest_sha256=none", output.getvalue())

    def test_parity_provenance_reads_installed_oracle_distribution_version(self) -> None:
        output = io.StringIO()
        with mock.patch(
            "public_corpus_manifest.importlib.metadata.version",
            return_value="1.2.3",
        ), redirect_stdout(output):
            emit_parity_provenance(
                None,
                oracle_reader="fixture-reader",
                package_distribution="fixture-distribution",
            )
        self.assertIn(
            "provenance: oracle_reader=fixture-reader oracle_version=1.2.3",
            output.getvalue(),
        )

    def test_report_paths_and_oracle_reasons_are_machine_independent(self) -> None:
        runner_home = "/" + "home" + "/runner"
        selected = f"{runner_home}/work/rxls/rxls/local/public-corpus/source/book.xls"
        root = f"{runner_home}/work/rxls/rxls/local/public-corpus"

        self.assertEqual(report_path(selected, root), "corpus/source/book.xls")
        self.assertEqual(
            report_reason(f"failed to open {selected}", selected, root),
            "failed to open corpus/source/book.xls",
        )
        self.assertNotIn(runner_home, report_path(selected, root))

        checkout_root = ROOT / "local" / "public-corpus"
        checkout_selected = checkout_root / "source" / "book.xls"
        self.assertEqual(
            report_path(checkout_selected, checkout_root), "corpus/source/book.xls"
        )

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

    def test_fetch_expectations_annotate_open_and_reject_entries(self) -> None:
        module = _load_fetch_public_corpus()
        with tempfile.TemporaryDirectory() as tmp:
            expectations = Path(tmp) / "expectations.json"
            expectations.write_text(
                json.dumps(
                    {
                        "schema": "rxls.public-corpus-expectations.v1",
                        "default_outcome": "open",
                        "rejects": [
                            {
                                "source": "fixture",
                                "path": "bad.xls",
                                "kind": "malformed_biff",
                                "decision": "excluded_malformed_workbook",
                                "evidence": "biff_structure_invalid",
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )

            annotated = module.apply_expectations(
                [
                    {"source": "fixture", "path": "good.xls"},
                    {"source": "fixture", "path": "bad.xls"},
                ],
                expectations,
            )

        self.assertEqual(annotated[0]["expected"], {"outcome": "open"})
        self.assertEqual(
            annotated[1]["expected"],
            {
                "outcome": "reject",
                "kind": "malformed_biff",
                "decision": "excluded_malformed_workbook",
                "evidence": "biff_structure_invalid",
            },
        )

    def test_fetch_expectations_reject_stale_entries(self) -> None:
        module = _load_fetch_public_corpus()
        with tempfile.TemporaryDirectory() as tmp:
            expectations = Path(tmp) / "expectations.json"
            expectations.write_text(
                json.dumps(
                    {
                        "schema": "rxls.public-corpus-expectations.v1",
                        "default_outcome": "open",
                        "rejects": [
                            {
                                "source": "fixture",
                                "path": "missing.xls",
                                "kind": "malformed_biff",
                                "decision": "excluded_malformed_workbook",
                                "evidence": "biff_structure_invalid",
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(RuntimeError, "not present in pinned sources"):
                module.apply_expectations([], expectations)

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
            Path(result["local_path"]),
            rel_dest / "apache-poi" / "test-data" / "spreadsheet" / "demo.xlsm",
        )

    def test_fetch_replaces_same_size_file_with_wrong_blob_hash(self) -> None:
        module = _load_fetch_public_corpus()
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            source = base / "source.xls"
            source.write_bytes(b"good")
            destination = base / "corpus"
            cached = destination / "calamine" / "tests" / "sample.xls"
            cached.parent.mkdir(parents=True)
            cached.write_bytes(b"evil")

            result = module.download_one(
                {
                    "source": "calamine",
                    "path": "tests/sample.xls",
                    "size": source.stat().st_size,
                    "git_blob_sha": module.git_blob_sha(source),
                    "url": source.as_uri(),
                },
                destination,
                force=False,
            )

            self.assertEqual(result["status"], "downloaded")
            self.assertEqual(cached.read_bytes(), b"good")

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

    def test_timeout_helper_classifies_broken_result_pipe(self) -> None:
        result = _read_result(_BrokenQueue(), 1)

        self.assertEqual(result.status, "error")
        self.assertIsNone(result.value)
        self.assertEqual(result.error, "oracle exited with code 1")

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


class _BrokenQueue:
    def get_nowait(self) -> None:
        raise EOFError


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
