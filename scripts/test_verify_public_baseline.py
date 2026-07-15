#!/usr/bin/env python3
"""Tests for the single-source public corpus release baseline."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "verify_public_baseline.py"
BASELINE = ROOT / "tests" / "oracles" / "public-corpus-baseline.json"


def _load():
    spec = importlib.util.spec_from_file_location("verify_public_baseline", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class PublicBaselineTests(unittest.TestCase):
    def test_readme_block_matches_checked_in_baseline(self) -> None:
        module = _load()
        baseline = module.load_baseline(BASELINE)
        readme = (ROOT / "README.md").read_text(encoding="utf-8")

        self.assertEqual(module.verify_readme(readme, baseline), [])

    def test_verifies_corpus_and_all_parity_formats(self) -> None:
        module = _load()
        baseline = module.load_baseline(BASELINE)
        corpus = """manifest_files: 916
eligible_files: 916
opened: 869
failed: 47
expected_rejections: 47
unexpected_failures: 0
unexpected_accepts: 0
skipped: 0
by_ext: .ods files=16 opened=15 failed=1
by_ext: .xls files=448 opened=422 failed=26
by_ext: .xlsb files=21 opened=19 failed=2
by_ext: .xlsm files=18 opened=18 failed=0
by_ext: .xlsx files=413 opened=395 failed=18
"""
        reports = {
            "xls": "provenance: oracle_reader=xlrd oracle_version=2.0.2\nprovenance: input_manifest_sha256=" + "a" * 64 + "\nfiles: 448   rxls extracted: 422   xlrd-unreadable: 8   comparable: 414\nrxls vs xlrd: mean parity 100.000%   >=99%: 414/414\n",
            "ooxml": "provenance: oracle_reader=openpyxl oracle_version=3.1.5\nprovenance: input_manifest_sha256=" + "a" * 64 + "\nfiles: 431   rxls extracted: 412   openpyxl-unreadable: 38   comparable: 388\nrxls vs openpyxl: mean parity 99.889%   >=99%: 387/388\n",
            "xlsb": "provenance: oracle_reader=pyxlsb oracle_version=1.0.10\nprovenance: input_manifest_sha256=" + "a" * 64 + "\nfiles: 21   rxls extracted: 18   pyxlsb-unreadable: 3   comparable: 18\nrxls vs pyxlsb: mean parity 100.000% over 18 files\n",
            "ods": "provenance: oracle_reader=xml.etree.ElementTree oracle_version=python-3.14.4\nprovenance: input_manifest_sha256=" + "a" * 64 + "\nfiles: 16   rxls extracted: 14   oracle-skipped: 2   comparable: 14\nrxls vs ODS visible oracle: mean recall 100.000% over 14 files\n",
        }

        self.assertEqual(module.verify_corpus(corpus, baseline["corpus"]), [])
        for kind, report in reports.items():
            with self.subTest(kind=kind):
                self.assertEqual(
                    module.verify_parity(report, kind, baseline["parity"][kind]), []
                )

    def test_detects_a_stale_claim(self) -> None:
        module = _load()
        baseline = module.load_baseline(BASELINE)

        errors = module.verify_corpus(
            "manifest_files: 916\neligible_files: 916\nopened: 876\nfailed: 40\n"
            "expected_rejections: 40\nunexpected_failures: 0\nunexpected_accepts: 0\nskipped: 0\n",
            baseline["corpus"],
        )

        self.assertTrue(any("opened: expected 869, found 876" in error for error in errors))

    def test_rejects_parity_without_release_provenance(self) -> None:
        module = _load()
        baseline = module.load_baseline(BASELINE)
        errors = module.verify_parity(
            "files: 448   rxls extracted: 422   comparable: 414\n"
            "rxls vs xlrd: mean parity 100.000%   >=99%: 414/414\n",
            "xls",
            baseline["parity"]["xls"],
        )

        self.assertIn("xls oracle provenance is missing", errors)
        self.assertIn("xls input manifest SHA-256 is missing or invalid", errors)


if __name__ == "__main__":
    unittest.main()
