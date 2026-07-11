#!/usr/bin/env python3
"""Tests for the XLSB public-corpus parity harness."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "xlsb-pyxlsb-parity.py"

sys.path.insert(0, str(SCRIPT.parent))

spec = importlib.util.spec_from_file_location("xlsb_pyxlsb_parity", SCRIPT)
assert spec is not None
xlsb_pyxlsb_parity = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(xlsb_pyxlsb_parity)


class XlsbPyxlsbParityTests(unittest.TestCase):
    def test_skip_classification_maps_rows_to_decisions_and_evidence(self) -> None:
        cases = [
            (
                "pyxlsb-unreadable",
                "BadZipFile: File is not a zip file",
                ("needs_corpus_crosscheck", "pyxlsb_non_zip_container", None),
            ),
            (
                "pyxlsb-unreadable",
                "KeyError: '\\x04'",
                (
                    "documented_oracle_limitation",
                    "pyxlsb_relationship_id_limitation",
                    None,
                ),
            ),
            (
                "pyxlsb-unreadable",
                "ValueError: unknown workbook parse failure",
                ("needs_oracle_triage", "pyxlsb_exception", None),
            ),
        ]

        for kind, reason, expected in cases:
            with self.subTest(kind=kind, reason=reason):
                self.assertEqual(xlsb_pyxlsb_parity.skip_classification(kind, reason), expected)

    def test_token_normalizes_scientific_numeric_notation_only(self) -> None:
        self.assertEqual(
            xlsb_pyxlsb_parity.token("1.0119037707962962e-05"),
            "0.000010119037707962962",
        )
        self.assertEqual(
            xlsb_pyxlsb_parity.token("3.7477917436899863e-22"),
            "0.00000000000000000000037477917436899863",
        )
        self.assertEqual(
            xlsb_pyxlsb_parity.token(1.1243375231069959e-16),
            "0.00000000000000011243375231069959",
        )
        self.assertEqual(xlsb_pyxlsb_parity.token("001"), "001")

    def test_expected_values_match_public_corpus_suffix_and_normalize_tokens(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            expected_path = Path(tmp) / "expected.json"
            expected_path.write_text(
                json.dumps(
                    {
                        "calamine/tests/date.xlsb": [
                            "2021-01-01",
                            15,
                            "2021-01-02",
                            16,
                            "255:10:10",
                            17,
                        ]
                    }
                ),
                encoding="utf-8",
            )

            expected = xlsb_pyxlsb_parity.load_expected_values(expected_path)
            source, values = xlsb_pyxlsb_parity.oracle_for(
                "/repo/local/public-corpus/calamine/tests/date.xlsb",
                expected,
            )

        self.assertEqual(source, "expected")
        self.assertEqual(
            values,
            ["2021-01-01", "15", "2021-01-02", "16", "255:10:10", "17"],
        )

    def test_committed_expected_values_cover_issue_419_shared_string_limitation(self) -> None:
        expected = xlsb_pyxlsb_parity.load_expected_values(
            ROOT / "tests" / "oracles" / "xlsb-visible-values.json"
        )

        values = xlsb_pyxlsb_parity.expected_values_for(
            "/repo/local/public-corpus/calamine/tests/issue_419.xlsb",
            expected,
        )

        self.assertEqual(values, ["hello"])

    def test_show_skips_refines_non_zip_rows_from_corpus_report(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            expected_path = base / "expected.json"
            expected_path.write_text(
                json.dumps({"small.xlsb": ["ok"]}),
                encoding="utf-8",
            )

            small_path = base / "small.xlsb"
            small_path.write_bytes(b"expected override avoids pyxlsb")

            protected_path = base / "apache-poi" / "test-data" / "spreadsheet" / "protected.xlsb"
            protected_path.parent.mkdir(parents=True)
            protected_path.write_bytes(b"not a zip container")

            manifest_path = base / "manifest.json"
            manifest_path.write_text(
                json.dumps(
                    {
                        "files": [
                            {
                                "source": "unit",
                                "path": "small.xlsb",
                                "local_path": str(small_path),
                                "status": "downloaded",
                            },
                            {
                                "source": "unit",
                                "path": "test-data/spreadsheet/protected.xlsb",
                                "local_path": str(protected_path),
                                "status": "downloaded",
                            },
                        ]
                    }
                ),
                encoding="utf-8",
            )

            corpus_report = base / "corpus-report.txt"
            corpus_report.write_text(
                "failure: .xlsb test-data/spreadsheet/protected.xlsb "
                "kind=unsupported_encrypted_ooxml decision=unsupported_encrypted "
                "evidence=encrypted_ooxml_package container=ole2 "
                "extension_mismatch=true parse: unsupported encrypted OOXML package\n",
                encoding="utf-8",
            )

            fake_extract = base / "fake-rxls-extract"
            fake_extract.write_text(
                textwrap.dedent(
                    """\
                    #!/usr/bin/env python3
                    print("# Sheet1")
                    print("ok")
                    """
                ),
                encoding="utf-8",
            )
            os.chmod(fake_extract, 0o755)

            output = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--manifest",
                    str(manifest_path),
                    "--bin",
                    str(fake_extract),
                    "--expected-values",
                    str(expected_path),
                    "--corpus-report",
                    str(corpus_report),
                    "--show-skips",
                    "5",
                    "--min",
                    "1.0",
                ],
                capture_output=True,
                text=True,
            )

            self.assertEqual(output.returncode, 0, output.stdout + output.stderr)
            self.assertIn("files: 2", output.stdout)
            self.assertIn("pyxlsb-unreadable: 1", output.stdout)
            self.assertIn("comparable: 1", output.stdout)
            self.assertIn(
                "by_skip_decision: unsupported_encrypted skipped=1",
                output.stdout,
            )
            self.assertIn(
                "by_skip_evidence: encrypted_ooxml_package skipped=1",
                output.stdout,
            )
            self.assertIn(
                "by_skip_corpus_kind: unsupported_encrypted_ooxml skipped=1",
                output.stdout,
            )
            self.assertRegex(
                output.stdout,
                r"skip: kind=pyxlsb-unreadable decision=unsupported_encrypted evidence=encrypted_ooxml_package corpus_kind=unsupported_encrypted_ooxml path=.*protected\.xlsb reason=BadZipFile: File is not a zip file",
            )

    def test_committed_expected_values_cover_remaining_xlsb_display_residuals(self) -> None:
        expected = xlsb_pyxlsb_parity.load_expected_values(
            ROOT / "tests" / "oracles" / "xlsb-visible-values.json"
        )

        cases = {
            "apache-poi/test-data/spreadsheet/bug66682.xlsb": [
                "id",
                "literal",
                "fornula",
                "1",
                "true",
                "true",
                "2",
                "false",
                "false",
                "3",
                "1",
                "1",
                "4",
                "1.5",
                "1.5",
                "5",
                "abcd",
                "abcd",
                "6",
                "error",
                "#div/0!",
                "7",
                "error",
                "#ref!",
                "8",
                "error",
                "#name?",
                "9",
                "error",
                "#n/a",
            ],
            "apache-poi/test-data/spreadsheet/testVarious.xlsb": [
                "string",
                "this is a string",
                "integer",
                "13",
                "float",
                "13.1211231321",
                "currency",
                "3.03",
                "percent",
                "20%",
                "float 2",
                "13.12131231",
                "long int",
                "123456789012345",
                "longer int",
                "1234567890123450",
                "fraction",
                "0.25",
                "date",
                "2017-03-09",
                "comment",
                "contents",
                "hyperlink",
                "tika_link",
                "formula",
                "4",
                "2",
                "formulaerr",
                "#name?",
                "formulafloat",
                "0.5",
                "march",
                "april",
                "customformat1",
                "46/1963",
                "merchant1",
                "1",
                "3",
                "customformat2",
                "3/128",
                "merchant2",
                "2",
                "4",
                "text test",
                "the",
                "the",
                "quick",
                "comment6",
            ],
            "calamine/tests/issues.xlsb": [
                "1",
                "1.5",
                "ab",
                "false",
                "test",
                "2016-10-20",
                "1",
                "a",
                "2",
                "b",
                "3",
                "c",
                "0",
                "0.5",
                "1",
                "2",
                "ab",
                "false",
                "&",
                "<",
                ">",
                "aaa ' aaa",
                "\"",
                "☺",
                "֍",
                "àâéêèçöïî«»",
            ],
        }

        for suffix, expected_values in cases.items():
            with self.subTest(suffix=suffix):
                self.assertEqual(
                    xlsb_pyxlsb_parity.expected_values_for(
                        f"/repo/local/public-corpus/{suffix}",
                        expected,
                    ),
                    expected_values,
                )


if __name__ == "__main__":
    unittest.main()
