from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "verify_libreoffice_xlsm.py"


def _load():
    spec = importlib.util.spec_from_file_location("verify_libreoffice_xlsm", SCRIPT)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _package(path: Path, vba: bytes, *, vba_content_type: str) -> None:
    content_types = f"""<?xml version="1.0"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="bin" ContentType="{vba_content_type}"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.ms-excel.sheet.macroEnabled.main+xml"/>
</Types>"""
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("[Content_Types].xml", content_types)
        archive.writestr("xl/workbook.xml", "<workbook/>")
        archive.writestr("xl/vbaProject.bin", vba)


class LibreOfficeXlsmVerifierTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.module = _load()

    def fixture(self, root: Path) -> tuple[Path, Path, Path, Path, Path]:
        source = root / "source.xlsm"
        edited = root / "edited.xlsm"
        converted = root / "converted.xlsm"
        for path, payload in (
            (source, b"original VBA"),
            (edited, b"original VBA"),
            (converted, b"LibreOffice VBA"),
        ):
            _package(path, payload, vba_content_type=self.module.VBA_CONTENT_TYPE)
        report = root / "report.json"
        report.write_text(
            json.dumps(
                {
                    "format": "xlsm",
                    "features": {"vba_project": True},
                    "warnings": ["MacrosPresentNotExecuted"],
                }
            ),
            encoding="utf-8",
        )
        version = root / "version.txt"
        version.write_text("LibreOffice 26.2.3.2 build-id\n", encoding="utf-8")
        return source, edited, converted, report, version

    def test_accepts_exact_rxls_preservation_and_libreoffice_macro_retention(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            paths = self.fixture(Path(raw))
            evidence = self.module.verify(*paths)

        self.assertTrue(evidence["vba_project"]["package_edit_byte_preserved"])
        self.assertTrue(evidence["vba_project"]["present_after_libreoffice"])
        self.assertEqual(
            evidence["diagnose_warnings"], ["MacrosPresentNotExecuted"]
        )

    def test_evidence_ignores_libreoffice_vba_rewrites(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source, edited, converted, report, version = self.fixture(root)
            first = self.module.verify(source, edited, converted, report, version)
            _package(
                converted,
                b"a different valid LibreOffice VBA rewrite",
                vba_content_type=self.module.VBA_CONTENT_TYPE,
            )
            second = self.module.verify(source, edited, converted, report, version)

        self.assertEqual(first, second)

    def test_rejects_changed_vba_from_the_rxls_package_edit(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source, edited, converted, report, version = self.fixture(root)
            _package(edited, b"changed", vba_content_type=self.module.VBA_CONTENT_TYPE)

            with self.assertRaisesRegex(ValueError, "package edit changed"):
                self.module.verify(source, edited, converted, report, version)

    def test_rejects_libreoffice_output_without_vba(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source, edited, converted, report, version = self.fixture(root)
            with zipfile.ZipFile(converted, "w") as archive:
                archive.writestr("[Content_Types].xml", "<Types/>")

            with self.assertRaisesRegex(ValueError, "exactly one xl/vbaProject.bin"):
                self.module.verify(source, edited, converted, report, version)

    def test_rejects_wrong_content_type_or_warning_set(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source, edited, converted, report, version = self.fixture(root)
            _package(converted, b"VBA", vba_content_type="application/octet-stream")
            with self.assertRaisesRegex(ValueError, "VBA content type"):
                self.module.verify(source, edited, converted, report, version)

            _package(converted, b"VBA", vba_content_type=self.module.VBA_CONTENT_TYPE)
            report.write_text(
                json.dumps(
                    {
                        "format": "xlsm",
                        "features": {"vba_project": True},
                        "warnings": [],
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "expected warnings"):
                self.module.verify(source, edited, converted, report, version)


if __name__ == "__main__":
    unittest.main()
