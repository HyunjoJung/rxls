#!/usr/bin/env python3
"""Tests for the manifest-aware `.xls` xlrd oracle harness."""

from __future__ import annotations

import contextlib
import io
import importlib.util
from pathlib import Path
from types import SimpleNamespace
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT_DIR = ROOT / "scripts"
SCRIPT = SCRIPT_DIR / "xls-xlrd-parity.py"


def load_xls_xlrd_parity_module():
    sys.path.insert(0, str(SCRIPT_DIR))
    spec = importlib.util.spec_from_file_location("xls_xlrd_parity", SCRIPT)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class XlsXlrdParityTests(unittest.TestCase):
    def test_module_import_does_not_keep_devnull_handle_open(self) -> None:
        module = load_xls_xlrd_parity_module()

        devnull = getattr(module, "_DEVNULL", None)
        self.assertTrue(devnull is None or devnull.closed)

    def test_skip_classification_maps_rows_to_decisions_and_evidence(self) -> None:
        module = load_xls_xlrd_parity_module()
        cases = [
            (
                "xlrd-unreadable",
                "XLRDError: Workbook is encrypted",
                ("unsupported_encrypted", "xlrd_encrypted_workbook", None),
            ),
            (
                "oversized-comparison",
                "combined text length 207099 exceeds 200000",
                ("needs_bounded_oracle", "comparison_budget_exceeded", None),
            ),
            (
                "xlrd-unreadable",
                "ValueError: unknown xlrd failure",
                ("needs_oracle_triage", "xlrd_exception", None),
            ),
            (
                "xlrd-unreadable",
                "FormulaError: ERROR *** Token 0x2d (AreaN) found in NAME formula",
                (
                    "documented_oracle_limitation",
                    "xlrd_formula_parser_limitation",
                    None,
                ),
            ),
            (
                "xlrd-unreadable",
                "TypeError: 'NoneType' object is not iterable",
                (
                    "documented_oracle_limitation",
                    "xlrd_parser_type_limitation",
                    None,
                ),
            ),
            (
                "xlrd-unreadable",
                "AssertionError: ",
                (
                    "documented_oracle_limitation",
                    "xlrd_assertion_limitation",
                    None,
                ),
            ),
            (
                "xlrd-unreadable",
                "ValueError: cannot convert float NaN to integer",
                (
                    "documented_oracle_limitation",
                    "xlrd_nan_number_limitation",
                    None,
                ),
            ),
            (
                "xlrd-unreadable",
                "XLRDError: Can't determine file's BIFF version",
                (
                    "documented_oracle_limitation",
                    "xlrd_biff_header_limitation",
                    None,
                ),
            ),
            (
                "xlrd-unreadable",
                "XLRDError: Unsupported format, or corrupt file: Expected BOF record; found b'\\x00'",
                (
                    "documented_oracle_limitation",
                    "xlrd_biff_header_limitation",
                    None,
                ),
            ),
            (
                "xlrd-unreadable",
                "CompDocError: Workbook corruption: seen[44] == 3",
                ("excluded_malformed_container", "xlrd_compdoc_error", None),
            ),
            (
                "xlrd-unreadable",
                "error: unpack requires a buffer of 2 bytes",
                ("excluded_malformed_workbook", "xlrd_truncated_record", None),
            ),
            (
                "xlrd-unreadable",
                "UnicodeDecodeError: 'utf-16-le' codec can't decode byte 0x3b in position 32: truncated data",
                ("excluded_malformed_workbook", "xlrd_truncated_unicode", None),
            ),
            (
                "unknown-kind",
                "unknown reason",
                ("needs_oracle_triage", "unknown_skip_kind", None),
            ),
        ]

        for kind, reason, expected in cases:
            with self.subTest(kind=kind, reason=reason):
                self.assertEqual(module.skip_classification(kind, reason), expected)

    def test_corpus_report_refines_xlrd_unreadable_skip_classification(self) -> None:
        module = load_xls_xlrd_parity_module()

        with tempfile.TemporaryDirectory() as tmp:
            corpus_report = Path(tmp) / "corpus-report.txt"
            corpus_report.write_text(
                "failure: .xls test-data/spreadsheet/fuzzer.xls "
                "kind=invalid_cfb decision=excluded_malformed_container "
                "evidence=ole2_signature_corrupt_container container=ole2 "
                "extension_mismatch=false parse: invalid CFB package\n",
                encoding="utf-8",
            )

            failures = module.parse_corpus_report(corpus_report)
            full_path = Path(tmp) / "local" / "public-corpus" / "test-data" / "spreadsheet" / "fuzzer.xls"

            self.assertEqual(
                module.skip_classification(
                    "xlrd-unreadable",
                    "CompDocError: MSAT: invalid sector id: -8",
                    path=full_path,
                    corpus_failures=failures,
                ),
                (
                    "excluded_malformed_container",
                    "ole2_signature_corrupt_container",
                    "invalid_cfb",
                ),
            )

    def test_worst_records_sorts_lowest_parity_first_and_limits(self) -> None:
        module = load_xls_xlrd_parity_module()

        records = [
            (1.0, "/tmp/perfect.xls", 10, 10),
            (0.25, "/tmp/bad.xls", 10, 2),
            (0.75, "/tmp/medium.xls", 10, 8),
        ]

        self.assertEqual(
            module.worst_records(records, 2),
            [
                (0.25, "/tmp/bad.xls", 10, 2),
                (0.75, "/tmp/medium.xls", 10, 8),
            ],
        )

    def test_xlrd_date_renderer_uses_elapsed_hour_tokens(self) -> None:
        module = load_xls_xlrd_parity_module()

        self.assertEqual(
            module.render_xlrd_cell_value(
                cell_type=3,
                value=10.632060185185185,
                fmt="[hh]:mm:ss",
                datemode=0,
            ),
            "255:10:10",
        )
        self.assertEqual(
            module.render_xlrd_cell_value(
                cell_type=3,
                value=10.632060185185185,
                fmt="h:mm:ss",
                datemode=0,
            ),
            "15:10:10",
        )

    def test_xlrd_date_renderer_normalizes_1900_serial_zero_like_rxls(self) -> None:
        module = load_xls_xlrd_parity_module()

        self.assertEqual(
            module.render_xlrd_cell_value(
                cell_type=3,
                value=0.0,
                fmt="dd/mm/yyyy",
                datemode=0,
            ),
            "00:00:00",
        )
        self.assertEqual(
            module.render_xlrd_cell_value(
                cell_type=3,
                value=0.0,
                fmt="dd/mm/yyyy hh:mm:ss",
                datemode=0,
            ),
            "00:00:00",
        )
        self.assertEqual(
            module.render_xlrd_cell_value(
                cell_type=3,
                value=0.0,
                fmt="dd/mm/yyyy",
                datemode=1,
            ),
            "1904-01-01",
        )

    def test_number_formats_select_sign_zero_and_conditional_sections(self) -> None:
        module = load_xls_xlrd_parity_module()

        cases = [
            (2, 0.5, "0%;0", "50%"),
            (2, -0.5, "0%;0", "-0.5"),
            (2, 0.5, "0;0%", "0.5"),
            (2, -0.5, "0;0%", "-50%"),
            (2, 0.0, "0;0;yyyy-mm-dd", "00:00:00"),
            (2, 42_803.0, "[>=40000]yyyy-mm-dd;0%", "2017-03-09"),
            (2, 0.5, "[>=40000]yyyy-mm-dd;0%", "50%"),
            (3, -1.0, "yyyy-mm-dd;0", "-1"),
        ]
        for cell_type, value, fmt, expected in cases:
            with self.subTest(value=value, fmt=fmt):
                self.assertEqual(
                    module.render_xlrd_cell_value(cell_type, value, fmt, datemode=0),
                    expected,
                )

    def test_cli_skips_corpus_reported_expected_rejections(self) -> None:
        module = load_xls_xlrd_parity_module()

        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            legacy = base / "test-data" / "spreadsheet" / "legacy.xls"
            malformed = base / "test-data" / "spreadsheet" / "malformed.xls"
            perfect = base / "test-data" / "spreadsheet" / "perfect.xls"
            for path in [legacy, malformed, perfect]:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"placeholder")

            corpus_report = base / "corpus-report.txt"
            corpus_report.write_text(
                "failure: .xls test-data/spreadsheet/legacy.xls "
                "kind=legacy_biff decision=unsupported_legacy_biff "
                "evidence=parser_or_support_classification container=unknown "
                "extension_mismatch=true parse: legacy BIFF2-4 unsupported\n"
                "failure: .xls test-data/spreadsheet/malformed.xls "
                "kind=malformed_biff decision=excluded_malformed_workbook "
                "evidence=biff_structure_invalid container=ole2 "
                "extension_mismatch=false parse: malformed BIFF stream\n",
                encoding="utf-8",
            )

            def fake_xlrd_text(path):
                if Path(path).name in {"legacy.xls", "malformed.xls"}:
                    return "unsupported gold"
                return "ok"

            def fake_run(args, **_kwargs):
                self.assertEqual(args[-1], "--typed-values")
                stdout = b"" if Path(args[1]).name != "perfect.xls" else b"ok"
                return SimpleNamespace(stdout=stdout)

            argv = [
                "xls-xlrd-parity.py",
                "--manifest",
                str(base / "manifest.json"),
                "--bin",
                "extract",
                "--corpus-report",
                str(corpus_report),
                "--show-skips",
                "10",
                "--show-worst",
                "10",
                "--min",
                "0.0",
            ]
            files = [str(legacy), str(malformed), str(perfect)]

            with (
                mock.patch.object(sys, "argv", argv),
                mock.patch.object(module, "resolve_binary", return_value="extract"),
                mock.patch.object(module, "manifest_files", return_value=files),
                mock.patch.object(module, "xlrd_text", side_effect=fake_xlrd_text),
                mock.patch.object(module.subprocess, "run", side_effect=fake_run),
            ):
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    with self.assertRaises(SystemExit) as exit_context:
                        module.main()

            self.assertEqual(exit_context.exception.code, 0)
            stdout = output.getvalue()
            self.assertIn("files: 3", stdout)
            self.assertIn("comparable: 1", stdout)
            self.assertIn("by_skip_decision: excluded_malformed_workbook skipped=1", stdout)
            self.assertIn("by_skip_decision: unsupported_legacy_biff skipped=1", stdout)
            self.assertIn("by_skip_evidence: biff_structure_invalid skipped=1", stdout)
            self.assertIn("by_skip_evidence: parser_or_support_classification skipped=1", stdout)
            self.assertIn("by_skip_corpus_kind: legacy_biff skipped=1", stdout)
            self.assertIn(
                "skip: kind=corpus-report-excluded decision=unsupported_legacy_biff "
                "evidence=parser_or_support_classification corpus_kind=legacy_biff",
                stdout,
            )
            self.assertIn(
                "skip: kind=corpus-report-excluded decision=excluded_malformed_workbook "
                "evidence=biff_structure_invalid corpus_kind=malformed_biff",
                stdout,
            )
            self.assertNotIn("low-parity: ratio=0.000", stdout)

    def test_cli_reports_skip_rollups_and_worst_rows(self) -> None:
        module = load_xls_xlrd_parity_module()

        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            encrypted = base / "test-data" / "spreadsheet" / "encrypted.xls"
            oversized = base / "test-data" / "spreadsheet" / "oversized.xls"
            low = base / "test-data" / "spreadsheet" / "low.xls"
            perfect = base / "test-data" / "spreadsheet" / "perfect.xls"
            for path in [encrypted, oversized, low, perfect]:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"placeholder")

            corpus_report = base / "corpus-report.txt"
            corpus_report.write_text(
                "failure: .xls test-data/spreadsheet/encrypted.xls "
                "kind=unsupported_encrypted_workbook decision=unsupported_encrypted "
                "evidence=ole2_encrypted_workbook container=ole2 "
                "extension_mismatch=false parse: unsupported encrypted workbook\n",
                encoding="utf-8",
            )

            def fake_xlrd_text(path):
                name = Path(path).name
                if name == "encrypted.xls":
                    raise RuntimeError("encrypted fixture")
                if name == "oversized.xls":
                    return "G" * 20
                if name == "low.xls":
                    return "abcd"
                return "ok"

            def fake_run(args, **_kwargs):
                self.assertEqual(args[-1], "--typed-values")
                name = Path(args[1]).name
                stdout = {
                    "encrypted.xls": b"rxls text",
                    "oversized.xls": b"R",
                    "low.xls": b"ab",
                    "perfect.xls": b"ok",
                }[name]
                return SimpleNamespace(stdout=stdout)

            argv = [
                "xls-xlrd-parity.py",
                "--manifest",
                str(base / "manifest.json"),
                "--bin",
                "extract",
                "--corpus-report",
                str(corpus_report),
                "--max-compare-chars",
                "10",
                "--show-skips",
                "10",
                "--show-worst",
                "2",
                "--min",
                "0.0",
            ]
            files = [str(encrypted), str(oversized), str(low), str(perfect)]

            with (
                mock.patch.object(sys, "argv", argv),
                mock.patch.object(module, "resolve_binary", return_value="extract"),
                mock.patch.object(module, "manifest_files", return_value=files),
                mock.patch.object(module, "xlrd_text", side_effect=fake_xlrd_text),
                mock.patch.object(module.subprocess, "run", side_effect=fake_run),
            ):
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    with self.assertRaises(SystemExit) as exit_context:
                        module.main()

            self.assertEqual(exit_context.exception.code, 0)
            stdout = output.getvalue()
            self.assertIn("files: 4", stdout)
            self.assertIn("xlrd-unreadable: 0", stdout)
            self.assertIn("oversized-comparisons: 1", stdout)
            self.assertIn("xlrd-unreadable with rxls output: 0/0", stdout)
            self.assertIn("by_skip_decision: needs_bounded_oracle skipped=1", stdout)
            self.assertIn("by_skip_decision: unsupported_encrypted skipped=1", stdout)
            self.assertIn("by_skip_evidence: comparison_budget_exceeded skipped=1", stdout)
            self.assertIn("by_skip_evidence: ole2_encrypted_workbook skipped=1", stdout)
            self.assertIn("by_skip_corpus_kind: unsupported_encrypted_workbook skipped=1", stdout)
            self.assertIn(
                "skip: kind=corpus-report-excluded decision=unsupported_encrypted "
                "evidence=ole2_encrypted_workbook corpus_kind=unsupported_encrypted_workbook",
                stdout,
            )
            self.assertIn("low-parity: ratio=0.667 gold_chars=4 rxls_chars=2", stdout)

    def test_cli_classifies_known_xlrd_exception_buckets(self) -> None:
        module = load_xls_xlrd_parity_module()

        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            formula = base / "formula.xls"
            assertion = base / "assertion.xls"
            biff_header = base / "biff-header.xls"
            compdoc = base / "compdoc.xls"
            truncated = base / "truncated.xls"
            perfect = base / "perfect.xls"
            files = [formula, assertion, biff_header, compdoc, truncated, perfect]
            for path in files:
                path.write_bytes(b"placeholder")

            FormulaError = type("FormulaError", (Exception,), {})
            XLRDError = type("XLRDError", (Exception,), {})
            CompDocError = type("CompDocError", (Exception,), {})

            def fake_xlrd_text(path):
                name = Path(path).name
                if name == "formula.xls":
                    raise FormulaError("ERROR *** Token 0x2d (AreaN) found in NAME formula")
                if name == "assertion.xls":
                    raise AssertionError()
                if name == "biff-header.xls":
                    raise XLRDError("Can't determine file's BIFF version")
                if name == "compdoc.xls":
                    raise CompDocError("Workbook corruption: seen[44] == 3")
                if name == "truncated.xls":
                    import struct

                    raise struct.error("unpack requires a buffer of 2 bytes")
                return "ok"

            def fake_run(args, **_kwargs):
                return SimpleNamespace(stdout=b"ok")

            argv = [
                "xls-xlrd-parity.py",
                "--manifest",
                str(base / "manifest.json"),
                "--bin",
                "extract",
                "--show-skips",
                "10",
                "--min",
                "0.0",
            ]

            with (
                mock.patch.object(sys, "argv", argv),
                mock.patch.object(module, "resolve_binary", return_value="extract"),
                mock.patch.object(module, "manifest_files", return_value=[str(f) for f in files]),
                mock.patch.object(module, "xlrd_text", side_effect=fake_xlrd_text),
                mock.patch.object(module.subprocess, "run", side_effect=fake_run),
            ):
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    with self.assertRaises(SystemExit) as exit_context:
                        module.main()

            self.assertEqual(exit_context.exception.code, 0)
            stdout = output.getvalue()
            self.assertIn("files: 6", stdout)
            self.assertIn("xlrd-unreadable: 5", stdout)
            self.assertIn("comparable: 1", stdout)
            self.assertIn("by_skip_decision: documented_oracle_limitation skipped=3", stdout)
            self.assertIn("by_skip_decision: excluded_malformed_container skipped=1", stdout)
            self.assertIn("by_skip_decision: excluded_malformed_workbook skipped=1", stdout)
            self.assertIn("by_skip_evidence: xlrd_formula_parser_limitation skipped=1", stdout)
            self.assertIn("by_skip_evidence: xlrd_assertion_limitation skipped=1", stdout)
            self.assertIn("by_skip_evidence: xlrd_biff_header_limitation skipped=1", stdout)
            self.assertIn("by_skip_evidence: xlrd_compdoc_error skipped=1", stdout)
            self.assertIn("by_skip_evidence: xlrd_truncated_record skipped=1", stdout)
            self.assertNotIn("by_skip_decision: needs_oracle_triage", stdout)

    def test_cli_admits_oversized_exact_matches_by_hash_without_diffing(self) -> None:
        module = load_xls_xlrd_parity_module()

        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            small = base / "small.xls"
            large = base / "large.xls"
            small.write_bytes(b"placeholder")
            large.write_bytes(b"placeholder")

            def fake_xlrd_text(path):
                if Path(path).name == "large.xls":
                    return "# Large\n" + "x" * 200
                return "# Small\nok"

            def fake_run(args, **_kwargs):
                self.assertEqual(args[-1], "--typed-values")
                if Path(args[1]).name == "large.xls":
                    stdout = b"# Large\n" + (b"x" * 200)
                else:
                    stdout = b"# Small\nok"
                return SimpleNamespace(stdout=stdout)

            argv = [
                "xls-xlrd-parity.py",
                "--manifest",
                str(base / "manifest.json"),
                "--bin",
                "extract",
                "--max-compare-chars",
                "50",
                "--min",
                "1.0",
            ]
            files = [str(small), str(large)]

            with (
                mock.patch.object(sys, "argv", argv),
                mock.patch.object(module, "resolve_binary", return_value="extract"),
                mock.patch.object(module, "manifest_files", return_value=files),
                mock.patch.object(module, "xlrd_text", side_effect=fake_xlrd_text),
                mock.patch.object(module.subprocess, "run", side_effect=fake_run),
            ):
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    with self.assertRaises(SystemExit) as exit_context:
                        module.main()

            self.assertEqual(exit_context.exception.code, 0)
            stdout = output.getvalue()
            self.assertIn("files: 2", stdout)
            self.assertIn("comparable: 2", stdout)
            self.assertIn("oversized-comparisons: 0", stdout)
            self.assertIn("hash-exact-comparisons: 1", stdout)
            self.assertIn("rxls vs xlrd: mean parity 100.000%", stdout)

    def test_cli_reports_oversized_mismatches_and_hash_budget_skips(self) -> None:
        module = load_xls_xlrd_parity_module()

        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            small = base / "small.xls"
            mismatch = base / "mismatch.xls"
            budgeted = base / "budgeted.xls"
            for path in [small, mismatch, budgeted]:
                path.write_bytes(b"placeholder")

            def fake_xlrd_text(path):
                name = Path(path).name
                if name == "small.xls":
                    return "# Small\nok"
                return "# Large\n" + "x" * 200

            def fake_run(args, **_kwargs):
                self.assertEqual(args[-1], "--typed-values")
                name = Path(args[1]).name
                if name == "small.xls":
                    stdout = b"# Small\nok"
                elif name == "mismatch.xls":
                    stdout = b"# Large\n" + (b"y" * 200)
                else:
                    stdout = b"# Large\n" + (b"x" * 200)
                return SimpleNamespace(stdout=stdout)

            argv = [
                "xls-xlrd-parity.py",
                "--manifest",
                str(base / "manifest.json"),
                "--bin",
                "extract",
                "--max-compare-chars",
                "50",
                "--max-hash-chars",
                "50",
                "--show-skips",
                "10",
                "--min",
                "1.0",
            ]
            files = [str(small), str(mismatch), str(budgeted)]

            with (
                mock.patch.object(sys, "argv", argv),
                mock.patch.object(module, "resolve_binary", return_value="extract"),
                mock.patch.object(module, "manifest_files", return_value=files),
                mock.patch.object(module, "xlrd_text", side_effect=fake_xlrd_text),
                mock.patch.object(module.subprocess, "run", side_effect=fake_run),
            ):
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    with self.assertRaises(SystemExit) as exit_context:
                        module.main()

            self.assertEqual(exit_context.exception.code, 0)
            stdout = output.getvalue()
            self.assertIn("files: 3", stdout)
            self.assertIn("comparable: 1", stdout)
            self.assertIn("oversized-comparisons: 2", stdout)
            self.assertIn("hash-exact-comparisons: 0", stdout)
            self.assertIn("by_skip_decision: needs_bounded_oracle skipped=2", stdout)
            self.assertRegex(
                stdout,
                r"skip: kind=oversized-comparison decision=needs_bounded_oracle "
                r"evidence=comparison_budget_exceeded path=.*mismatch\.xls "
                r"reason=combined text length ",
            )
            self.assertRegex(
                stdout,
                r"skip: kind=oversized-comparison decision=needs_bounded_oracle "
                r"evidence=comparison_budget_exceeded path=.*budgeted\.xls "
                r"reason=combined text length .* exceeds 50 hash budget",
            )


if __name__ == "__main__":
    unittest.main()
