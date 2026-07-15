#!/usr/bin/env python3
"""Tests for deterministic generated performance workbooks."""

from __future__ import annotations

import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
import zipfile


SCRIPT = Path(__file__).with_name("generate-performance-fixture.py")


def load_module():
    spec = importlib.util.spec_from_file_location("generate_performance_fixture", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class GeneratePerformanceFixtureTests(unittest.TestCase):
    def test_generated_medium_xlsx_is_deterministic_and_bounded(self) -> None:
        module = load_module()
        with tempfile.TemporaryDirectory() as tmp:
            first = Path(tmp) / "first.xlsx"
            second = Path(tmp) / "second.xlsx"

            first_size = module.generate(first, 10)
            second_size = module.generate(second, 10)

            self.assertEqual(first_size, second_size)
            self.assertGreaterEqual(first_size, 10 * module.MIB)
            self.assertLessEqual(first_size, 50 * module.MIB)
            self.assertEqual(sha256(first), sha256(second))
            with zipfile.ZipFile(first) as archive:
                self.assertEqual(
                    archive.namelist(),
                    [
                        "[Content_Types].xml",
                        "_rels/.rels",
                        "xl/workbook.xml",
                        "xl/_rels/workbook.xml.rels",
                        "xl/worksheets/sheet1.xml",
                        "customXml/item1.xml",
                    ],
                )
                self.assertTrue(all(item.date_time == module.ZIP_TIMESTAMP for item in archive.infolist()))
                self.assertGreater(archive.getinfo("customXml/item1.xml").file_size, 10 * module.MIB)

    def test_generator_rejects_sizes_outside_release_class(self) -> None:
        module = load_module()
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "invalid.xlsx"
            for payload_mib in (9, 50):
                with self.subTest(payload_mib=payload_mib):
                    with self.assertRaisesRegex(ValueError, "at least 10 MiB"):
                        module.generate(output, payload_mib)


if __name__ == "__main__":
    unittest.main()
