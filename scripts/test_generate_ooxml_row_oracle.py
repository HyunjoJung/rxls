#!/usr/bin/env python3
"""Tests for the deterministic OOXML implicit-row diagnostic generator."""

from __future__ import annotations

from hashlib import sha256
import importlib.util
import io
import json
from pathlib import Path
import shutil
import tempfile
import unittest
from xml.etree import ElementTree
from zipfile import ZIP_STORED, ZipFile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "generate-ooxml-row-oracle.py"
RELEASE_GENERATOR = ROOT / "scripts" / "generate-render-corpus.py"
EXPECTED_FULL_MANIFEST_SHA256 = (
    "d33e6b7f27e351dac45feab8a780ed77cc04241aafe5a280a3b009c47dd85f49"
)
EXPECTED_DIAGNOSTIC_MANIFEST_SHA256 = (
    "c94f37252d4f78e5352299b831d2620be39178c676b145cda7d076f7d3d09e8a"
)
EXPECTED_PAYLOAD_SHA256 = {
    "row-missing-carlito-11": (
        "b86dd37fae68af9bcd5442d5bd105206491e413e4abaeab4533e723c3504d0fa"
    ),
    "row-missing-carlito-12": (
        "f6c82316ef7629ec7556b820ed8e1756eaf4ac08a879c10128af61f8607cfa9e"
    ),
    "row-missing-noto-11": (
        "02ccfbcf6842cde88a4fceb562658007c0a46d44b0c0ff7ce66ebdd8467e7abb"
    ),
    "row-missing-noto-11-explicit-row-height": (
        "e2dc94f0c65aa6cbfff32718acc278d9b5376c7e37defcc1e6666c30ff7b8d43"
    ),
    "row-missing-noto-11-hidden-row": (
        "99c24a9aa4e6fb3fc1dd0bc9d12f9ca09727a1e5b748b0ff0770ad087e8db5eb"
    ),
    "row-missing-noto-11-image-drawing": (
        "90a68c31e7cd05d218c22e36ce34e1b1c5758db21ecb5cffab0292db81ba16ff"
    ),
    "row-missing-noto-11-right-to-left-layout": (
        "e44f26036a49a55e7c59bb3abcafd52c180b4f40235c36edaf72bd3a40ec5f0c"
    ),
    "row-missing-noto-12": (
        "d16b58bed41c94bb14dcc2180709f2cab6870a4a5acb6b63556991af63946454"
    ),
    "row-present-carlito-11": (
        "c92c6297abe26ea6c33f791c9cb34c8c2dc764abfff8cff9584fd7f20595f491"
    ),
    "row-present-carlito-12": (
        "3efebc0f2cdc95886ae75aab128dbca6bd160fbe1c5cccf8c28349a5159bde03"
    ),
    "row-present-noto-11": (
        "69054a1a4760a4203612aa0ed626eb8bc571be50edc2c6ad4710f4658a0c7c2c"
    ),
    "row-present-noto-12": (
        "97e81addc6230f2fc2977b103c07647ba986fb6fa97445022172e3c52dec2951"
    ),
}


def load_script(path: Path, name: str):
    specification = importlib.util.spec_from_file_location(name, path)
    if specification is None or specification.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(specification)
    import sys

    sys.modules[name] = module
    specification.loader.exec_module(module)
    return module


MODULE = load_script(SCRIPT, "rxls_generate_ooxml_row_oracle")
RELEASE = load_script(RELEASE_GENERATOR, "rxls_generate_render_corpus_regression")


class OoxmlRowOracleGeneratorTests(unittest.TestCase):
    def test_exact_matrix_and_feature_counts(self) -> None:
        manifest, cases = MODULE.materialize()
        self.assertEqual(len(cases), 12)
        self.assertEqual(manifest["case_count"], 12)
        self.assertEqual(manifest["format_counts"], {"xlsx": 12})
        self.assertEqual(
            manifest["feature_counts"],
            {
                "explicit-row-height": 1,
                "hidden-row": 1,
                "image-drawing": 1,
                "normal-font-carlito": 4,
                "normal-font-noto": 8,
                "normal-size-11": 8,
                "normal-size-12": 4,
                "ooxml-implicit-row": 12,
                "right-to-left-layout": 1,
                "sheet-format-missing": 8,
                "sheet-format-present": 4,
            },
        )
        combinations = {
            (
                spec.sheet_format_present,
                spec.font_family,
                spec.font_size,
                spec.toggle,
            )
            for spec, _ in cases
        }
        expected_core = {
            (present, family, size, None)
            for present in (False, True)
            for family in ("Noto Sans CJK KR", "Carlito")
            for size in (11, 12)
        }
        expected_stress = {
            (False, "Noto Sans CJK KR", 11, toggle)
            for toggle in (
                "explicit-row-height",
                "hidden-row",
                "right-to-left-layout",
                "image-drawing",
            )
        }
        self.assertEqual(combinations, expected_core | expected_stress)
        for spec, _ in cases:
            self.assertEqual(spec.features, tuple(sorted(set(spec.features))))

    def test_manifest_rights_rows_and_golden_hashes(self) -> None:
        manifest, cases = MODULE.materialize()
        payload = MODULE._json_bytes(manifest)
        self.assertEqual(
            sha256(payload).hexdigest(), EXPECTED_DIAGNOSTIC_MANIFEST_SHA256
        )
        self.assertEqual(manifest["schema_version"], 1)
        self.assertEqual(manifest["profile"], "ooxml-row-diagnostic")
        self.assertEqual(manifest["generator"], "rxls-ooxml-row-diagnostic")
        self.assertEqual(manifest["generator_version"], "1.0.0")
        self.assertEqual(manifest["rights_tier"], "S")
        self.assertEqual(manifest["license"], "MIT")
        self.assertEqual(manifest["redistribution"], "allowed")
        self.assertIs(manifest["source_redistributable"], True)
        self.assertIs(manifest["render_redistributable"], True)
        self.assertEqual(
            {spec.case_id: sha256(case).hexdigest() for spec, case in cases},
            EXPECTED_PAYLOAD_SHA256,
        )
        for row, (spec, case) in zip(manifest["files"], cases, strict=True):
            self.assertEqual(
                set(row),
                {
                    "byte_length",
                    "case_id",
                    "features",
                    "format",
                    "generator",
                    "generator_version",
                    "license",
                    "path",
                    "redistribution",
                    "render_redistributable",
                    "rights_tier",
                    "seed",
                    "sha256",
                    "source_redistributable",
                },
            )
            self.assertEqual(row["case_id"], spec.case_id)
            self.assertEqual(row["path"], spec.relative_path)
            self.assertEqual(row["format"], "xlsx")
            self.assertEqual(row["features"], list(spec.features))
            self.assertEqual(row["byte_length"], len(case))
            self.assertEqual(row["sha256"], sha256(case).hexdigest())

    def test_release_full_manifest_bytes_remain_exact(self) -> None:
        manifest, _ = RELEASE.materialize("full")
        self.assertEqual(
            sha256(RELEASE._json_bytes(manifest)).hexdigest(),
            EXPECTED_FULL_MANIFEST_SHA256,
        )
        self.assertEqual(manifest["case_count"], 800)
        self.assertEqual(
            manifest["format_counts"],
            {"ods": 200, "xls": 200, "xlsb": 200, "xlsx": 200},
        )

    def test_packages_are_canonical_and_structurally_isolated(self) -> None:
        namespace = {"s": MODULE.SHEET_NS}
        for spec in MODULE.CASES:
            first = MODULE.build_case(spec)
            second = MODULE.build_case(spec)
            self.assertEqual(first, second)
            with ZipFile(io.BytesIO(first)) as archive:
                names = archive.namelist()
                with self.subTest(case=spec.case_id):
                    self.assertIsNone(archive.testzip())
                    self.assertEqual(len(names), len(set(names)))
                    self.assertLessEqual(len(names), MODULE.MAX_ZIP_PARTS)
                    self.assertTrue(
                        all(item.date_time == MODULE.DOS_EPOCH for item in archive.infolist())
                    )
                    self.assertTrue(
                        all(item.compress_type == ZIP_STORED for item in archive.infolist())
                    )
                    self.assertTrue(
                        all(item.flag_bits & 0x1 == 0 for item in archive.infolist())
                    )
                    for name in names:
                        if name.endswith((".xml", ".rels")):
                            ElementTree.fromstring(archive.read(name))
                    sheet = ElementTree.fromstring(
                        archive.read("xl/worksheets/sheet1.xml")
                    )
                    styles = ElementTree.fromstring(archive.read("xl/styles.xml"))
                    self.assertEqual(
                        sheet.find("s:dimension", namespace).attrib,
                        {"ref": "A1:B8"},
                    )
                    rows = sheet.findall("s:sheetData/s:row", namespace)
                    self.assertEqual(rows[0].attrib, {"r": "1"})
                    self.assertEqual(rows[-1].attrib, {"r": "8"})
                    font = styles.find("s:fonts/s:font", namespace)
                    self.assertEqual(
                        font.find("s:name", namespace).attrib["val"],
                        spec.font_family,
                    )
                    self.assertEqual(
                        font.find("s:sz", namespace).attrib["val"],
                        str(spec.font_size),
                    )
                    external_relationships = []
                    for name in names:
                        if name.endswith(".rels"):
                            root = ElementTree.fromstring(archive.read(name))
                            external_relationships.extend(
                                row
                                for row in root
                                if row.attrib.get("TargetMode") == "External"
                            )
                    self.assertEqual(external_relationships, [])

    def test_each_feature_changes_only_its_reviewed_package_surface(self) -> None:
        baseline = next(
            spec
            for spec in MODULE.CASES
            if spec.case_id == "row-missing-noto-11"
        )
        baseline_payload = MODULE.build_case(baseline)
        with ZipFile(io.BytesIO(baseline_payload)) as archive:
            baseline_parts = {
                name: archive.read(name) for name in archive.namelist()
            }
        for spec in MODULE.CASES:
            with ZipFile(io.BytesIO(MODULE.build_case(spec))) as archive:
                parts = {name: archive.read(name) for name in archive.namelist()}
            with self.subTest(case=spec.case_id):
                if spec.toggle == "image-drawing":
                    self.assertIn("xl/media/image1.png", parts)
                    self.assertTrue(
                        parts["xl/media/image1.png"].startswith(b"\x89PNG\r\n\x1a\n")
                    )
                else:
                    self.assertNotIn("xl/media/image1.png", parts)
                if spec.font_family != baseline.font_family or spec.font_size != baseline.font_size:
                    self.assertNotEqual(
                        parts["xl/styles.xml"], baseline_parts["xl/styles.xml"]
                    )
                elif spec.toggle != "image-drawing":
                    self.assertEqual(
                        parts["xl/styles.xml"], baseline_parts["xl/styles.xml"]
                    )
                sheet = parts["xl/worksheets/sheet1.xml"].decode("utf-8")
                self.assertEqual(
                    '<sheetFormatPr defaultRowHeight="15" customHeight="1"/>'
                    in sheet,
                    spec.sheet_format_present,
                )
                self.assertEqual(' ht="21" customHeight="1"' in sheet, spec.toggle == "explicit-row-height")
                self.assertEqual('<row r="4" hidden="1"/>' in sheet, spec.toggle == "hidden-row")
                self.assertEqual(' rightToLeft="1"' in sheet, spec.toggle == "right-to-left-layout")
                self.assertEqual('<drawing r:id="rIdDrawing"/>' in sheet, spec.toggle == "image-drawing")

    def test_generate_verify_replace_and_tamper_detection(self) -> None:
        MODULE.OUTPUT_BASE.mkdir(parents=True, exist_ok=True)
        temporary = Path(
            tempfile.mkdtemp(prefix="row-oracle-test-", dir=MODULE.OUTPUT_BASE)
        )
        output = temporary / "matrix"
        try:
            first = MODULE.generate(output)
            self.assertEqual(MODULE.verify(output), first)
            stale = output / "stale.txt"
            stale.write_text("stale", encoding="utf-8")
            second = MODULE.generate(output)
            self.assertEqual(first, second)
            self.assertFalse(stale.exists())
            payload = output / second["files"][0]["path"]
            payload.write_bytes(payload.read_bytes() + b"tamper")
            with self.assertRaisesRegex(MODULE.OracleCorpusError, "payload mismatch"):
                MODULE.verify(output)
        finally:
            shutil.rmtree(temporary, ignore_errors=True)

    def test_output_and_manifest_path_guards(self) -> None:
        for candidate in (ROOT / "tests", MODULE.OUTPUT_BASE):
            with self.subTest(candidate=candidate):
                with self.assertRaises(MODULE.OracleCorpusError):
                    MODULE.resolve_output(str(candidate))
        with self.assertRaises(MODULE.OracleCorpusError):
            MODULE._safe_manifest_path(MODULE.DEFAULT_OUTPUT, "../escape.xlsx")
        with self.assertRaises(MODULE.OracleCorpusError):
            MODULE._safe_manifest_path(MODULE.DEFAULT_OUTPUT, "/tmp/escape.xlsx")

    def test_list_output_is_exact_json(self) -> None:
        manifest, _ = MODULE.materialize()
        payload = MODULE._json_bytes(manifest)
        self.assertEqual(json.loads(payload), manifest)
        self.assertLessEqual(len(payload), MODULE.MAX_MANIFEST_BYTES)


if __name__ == "__main__":
    unittest.main()
