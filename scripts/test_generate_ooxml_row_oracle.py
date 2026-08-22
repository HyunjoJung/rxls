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
from unittest import mock
from xml.etree import ElementTree
from zipfile import ZIP_STORED, ZipFile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "generate-ooxml-row-oracle.py"
RELEASE_GENERATOR = ROOT / "scripts" / "generate-render-corpus.py"
EXPECTED_FULL_MANIFEST_SHA256 = (
    "5c6466a53e4328bb50f04cd3c63d102bf53da1a6b3478380f3724574c31b248d"
)
EXPECTED_DIAGNOSTIC_MANIFEST_SHA256 = (
    "088db320a0d35494fa8e0a8c33ba95e12a824cfe1b7163c2071cf70528c5d0a2"
)
EXPECTED_PAYLOAD_SHA256 = {
    "row-missing-noto-11-auto-bold-font": (
        "3cb0d407edf4198c9ba73101e8e364c27006e27be35bf70d57a407837b573f12"
    ),
    "row-missing-noto-11-auto-bold-font-wrapped": (
        "fad3d67238db8b7c68425435d6ae9681e0913642c5811ccff1d7c1dba2f2fbfb"
    ),
    "row-missing-noto-11-auto-heading-western-asian": (
        "70dfc8e7fbfd553f49c9e5012fe4a88cff049b4ad0905ecee60eff6a85778ed6"
    ),
    "row-missing-noto-11-auto-heading-western-complex": (
        "bbd0464c01a92fb3b4f711d0d432cf9b855c9cc687fc65d3e3ddfebb1169af21"
    ),
    "row-missing-noto-11-auto-large-font": (
        "2c50da9539d0728e438e01b6369539e54977130127b2622afaa52ba8fadbdcbc"
    ),
    "row-missing-noto-11-auto-long-unwrapped": (
        "bffa9c454630fe6e754f53ff7245e8b2e18c4d3ad1ef4146747acbfc987c6141"
    ),
    "row-missing-noto-11-auto-numeric-color-conditional": (
        "ce93551100e206c1176b19e5a0808287b5c6370551c5b0551591f67e9bd58961"
    ),
    "row-missing-noto-11-auto-numeric-no-conditional": (
        "0d348978c79364d35a63e25574b3c4af208f1e8a34deade3523b65c29b8f9b02"
    ),
    "row-missing-noto-11-auto-wrapped-color-conditional": (
        "fcfff374387758af77f66fadd344eb544b522b1f8659496fb2afb78c269d50fe"
    ),
    "row-missing-noto-11-auto-wrapped-explicit": (
        "d7c3d6da0ec43505797c2881a0c3930db8089e10ff43e4875c788da919a17785"
    ),
    "row-missing-noto-11-auto-wrapped-hidden": (
        "cf8fac84379109a0a12f08177285cb81b385b579955c831d897ed45b8034d753"
    ),
    "row-missing-noto-11-auto-wrapped-image": (
        "21a99a7d283e4abfdb9b293c391b12c61547e006759947177618836bca21cbfd"
    ),
    "row-missing-noto-11-auto-wrapped-long": (
        "108c49917d62c4af604998f275c8c24b7bb64f25118725e53c37fa91a5b9e88f"
    ),
    "row-missing-noto-11-auto-wrapped-long-anchor": (
        "cedd3e1799c19dc443a7f55985c42c5b8cdec3baccbb098d00639eaebb982c0b"
    ),
    "row-missing-noto-11-auto-wrapped-merged": (
        "f2c9ac3ba3a3616843dbfe649faba32a166fe9037b37ec86394a90dac0b5fc8a"
    ),
    "row-missing-noto-11-auto-wrapped-no-conditional": (
        "a259cb85fb569df1cf023548271004dcea58dbd807b0746a587265a4d49f179e"
    ),
    "row-missing-noto-11-auto-wrapped-rtl": (
        "f9e8b3ffc14c6f5c2efc96e50cd22b09353a4815805480e1a01ce0ee42047f2b"
    ),
    "row-missing-noto-11-auto-wrapped-wide": (
        "bc52b4dd8cea61b6d342f4e889eae1c6d8a0e66e239971917597ccd33ad687dd"
    ),
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
    "row-missing-noto-11-hidden-heading-western-asian": (
        "7beb0f91f32ff79cb0221cab4d93dea22b5b443afa32850f97e2bd15dd47aaa8"
    ),
    "row-missing-noto-11-hidden-heading-western-complex": (
        "d2590903f61860cf648f9c006defd2b5839d247edbbc7698aced999570e0e2e7"
    ),
    "row-missing-noto-11-image-drawing": (
        "90a68c31e7cd05d218c22e36ce34e1b1c5758db21ecb5cffab0292db81ba16ff"
    ),
    "row-missing-noto-11-right-to-left-layout": (
        "e44f26036a49a55e7c59bb3abcafd52c180b4f40235c36edaf72bd3a40ec5f0c"
    ),
    "row-missing-noto-11-manual-heading-western-asian": (
        "786edcbb4cdaba5659a90ebb164ebcdf42cbff9833b1266cb8c54741e5021384"
    ),
    "row-missing-noto-11-manual-heading-western-complex": (
        "86cc7ab76a7c7dcc0382c245a27f90ecf7973c98a5ee093b5822eddb022f99db"
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
        self.assertEqual(len(cases), 34)
        self.assertEqual(manifest["case_count"], 34)
        self.assertEqual(manifest["format_counts"], {"xlsx": 34})
        self.assertEqual(
            manifest["feature_counts"],
            {
                "auto-bold-font": 1,
                "auto-bold-font-wrapped": 1,
                "auto-heading-western-asian": 1,
                "auto-heading-western-complex": 1,
                "auto-large-font": 1,
                "auto-long-unwrapped": 1,
                "auto-numeric-color-conditional": 1,
                "auto-numeric-no-conditional": 1,
                "auto-wrapped-color-conditional": 1,
                "auto-wrapped-explicit": 1,
                "auto-wrapped-hidden": 1,
                "auto-wrapped-image": 1,
                "auto-wrapped-long": 1,
                "auto-wrapped-long-anchor": 1,
                "auto-wrapped-merged": 1,
                "auto-wrapped-no-conditional": 1,
                "auto-wrapped-rtl": 1,
                "auto-wrapped-wide": 1,
                "explicit-row-height": 1,
                "hidden-heading-western-asian": 1,
                "hidden-heading-western-complex": 1,
                "hidden-row": 1,
                "image-drawing": 1,
                "manual-heading-western-asian": 1,
                "manual-heading-western-complex": 1,
                "normal-font-carlito": 4,
                "normal-font-noto": 30,
                "normal-size-11": 30,
                "normal-size-12": 4,
                "ooxml-implicit-row": 34,
                "right-to-left-layout": 1,
                "sheet-format-missing": 30,
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
                MODULE.BASELINE_TOGGLES
                + MODULE.AUTOHEIGHT_TOGGLES
                + MODULE.MULTICELL_TOGGLES
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
        self.assertEqual(manifest["generator_version"], "1.2.0")
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
                        {
                            "ref": (
                                "A1:D8"
                                if spec.toggle in MODULE.MULTICELL_TOGGLES
                                else "A1:B8"
                            )
                        },
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
                    a1 = sheet.find(
                        "s:sheetData/s:row[@r='1']/s:c[@r='A1']",
                        namespace,
                    )
                    self.assertIsNotNone(a1)
                    self.assertNotIn("s", a1.attrib)
                    style_xfs = styles.findall("s:cellStyleXfs/s:xf", namespace)
                    cell_xfs = styles.findall("s:cellXfs/s:xf", namespace)
                    for xf in (*style_xfs, cell_xfs[0]):
                        self.assertNotIn("applyAlignment", xf.attrib)
                        self.assertIsNone(xf.find("s:alignment", namespace))
                    if spec.toggle in MODULE.WRAPPED_TOGGLES:
                        self.assertEqual(len(cell_xfs), 2)
                        self.assertEqual(
                            cell_xfs[1].find("s:alignment", namespace).attrib,
                            {"vertical": "top", "wrapText": "1"},
                        )
                    else:
                        self.assertTrue(
                            all(
                                xf.find("s:alignment", namespace) is None
                                for xf in cell_xfs
                            )
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
                if spec.toggle in MODULE.DRAWING_TOGGLES:
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
                elif spec.toggle not in (
                    MODULE.WRAPPED_TOGGLES
                    | MODULE.BOLD_FONT_TOGGLES
                    | MODULE.LARGE_FONT_TOGGLES
                    | MODULE.CONDITIONAL_MATRIX_TOGGLES
                ):
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
                self.assertEqual(
                    ' rightToLeft="1"' in sheet,
                    spec.toggle
                    in {"right-to-left-layout", "auto-wrapped-rtl"},
                )
                self.assertEqual(
                    '<drawing r:id="rIdDrawing"/>' in sheet,
                    spec.toggle in MODULE.DRAWING_TOGGLES,
                )

    def test_wrapping_controls_preserve_identical_probe_content(self) -> None:
        by_toggle = {spec.toggle: spec for spec in MODULE.CASES}

        def parts(toggle: str) -> dict[str, bytes]:
            with ZipFile(
                io.BytesIO(MODULE.build_case(by_toggle[toggle]))
            ) as archive:
                return {
                    name: archive.read(name) for name in archive.namelist()
                }

        unwrapped = parts("auto-long-unwrapped")
        wrapped = parts("auto-wrapped-long")
        self.assertEqual(
            {
                name
                for name in unwrapped
                if unwrapped[name] != wrapped[name]
            },
            {"xl/styles.xml", "xl/worksheets/sheet1.xml"},
        )
        namespace = {"s": MODULE.SHEET_NS}
        unwrapped_sheet = ElementTree.fromstring(
            unwrapped["xl/worksheets/sheet1.xml"]
        )
        wrapped_sheet = ElementTree.fromstring(
            wrapped["xl/worksheets/sheet1.xml"]
        )
        unwrapped_cell = unwrapped_sheet.find(
            "s:sheetData/s:row[@r='4']/s:c[@r='A4']",
            namespace,
        )
        wrapped_cell = wrapped_sheet.find(
            "s:sheetData/s:row[@r='4']/s:c[@r='A4']",
            namespace,
        )
        self.assertNotIn("s", unwrapped_cell.attrib)
        self.assertEqual(wrapped_cell.attrib["s"], "1")
        self.assertEqual(
            unwrapped_cell.find("s:is/s:t", namespace).text,
            wrapped_cell.find("s:is/s:t", namespace).text,
        )

        bold = parts("auto-bold-font")
        bold_wrapped = parts("auto-bold-font-wrapped")
        self.assertEqual(
            {
                name for name in bold if bold[name] != bold_wrapped[name]
            },
            {"xl/styles.xml"},
        )

    def test_multicell_heading_controls_preserve_exact_cells(self) -> None:
        namespace = {"s": MODULE.SHEET_NS}
        by_toggle = {spec.toggle: spec for spec in MODULE.CASES}
        for script, expected_texts in (
            ("western-asian", MODULE.WESTERN_ASIAN_HEADING),
            ("western-complex", MODULE.WESTERN_COMPLEX_HEADING),
        ):
            observed_cells = []
            for mode, expected_row in (
                ("auto", {"r": "4"}),
                (
                    "manual",
                    {"customHeight": "1", "ht": "30", "r": "4"},
                ),
                ("hidden", {"hidden": "1", "r": "4"}),
            ):
                toggle = f"{mode}-heading-{script}"
                with ZipFile(
                    io.BytesIO(MODULE.build_case(by_toggle[toggle]))
                ) as archive:
                    sheet = ElementTree.fromstring(
                        archive.read("xl/worksheets/sheet1.xml")
                    )
                row = sheet.find("s:sheetData/s:row[@r='4']", namespace)
                self.assertEqual(row.attrib, expected_row)
                cells = row.findall("s:c", namespace)
                self.assertEqual(
                    [cell.attrib for cell in cells],
                    [
                        {"r": f"{column}4", "s": "1", "t": "inlineStr"}
                        for column in "ABCD"
                    ],
                )
                self.assertEqual(
                    tuple(
                        cell.find("s:is/s:t", namespace).text
                        for cell in cells
                    ),
                    expected_texts,
                )
                observed_cells.append(
                    tuple(ElementTree.tostring(cell) for cell in cells)
                )
            self.assertEqual(observed_cells[0], observed_cells[1])
            self.assertEqual(observed_cells[0], observed_cells[2])

    def test_color_only_conditional_pairs_are_structurally_equivalent(self) -> None:
        namespace = {"s": MODULE.SHEET_NS}
        by_toggle = {spec.toggle: spec for spec in MODULE.CASES}

        def parts(toggle: str) -> dict[str, bytes]:
            with ZipFile(
                io.BytesIO(MODULE.build_case(by_toggle[toggle]))
            ) as archive:
                return {
                    name: archive.read(name) for name in archive.namelist()
                }

        for prefix in ("auto-numeric", "auto-wrapped"):
            conditional = parts(f"{prefix}-color-conditional")
            control = parts(f"{prefix}-no-conditional")
            self.assertEqual(
                {
                    name
                    for name in conditional
                    if conditional[name] != control[name]
                },
                {"xl/worksheets/sheet1.xml"},
            )
            conditional_sheet = ElementTree.fromstring(
                conditional["xl/worksheets/sheet1.xml"]
            )
            control_sheet = ElementTree.fromstring(
                control["xl/worksheets/sheet1.xml"]
            )
            rule = conditional_sheet.find(
                "s:conditionalFormatting/s:cfRule",
                namespace,
            )
            self.assertEqual(
                rule.attrib,
                {
                    "dxfId": "0",
                    "operator": "greaterThan",
                    "priority": "1",
                    "type": "cellIs",
                },
            )
            self.assertEqual(
                rule.find("s:formula", namespace).text,
                "0",
            )
            self.assertIsNone(
                control_sheet.find("s:conditionalFormatting", namespace)
            )
            styles = ElementTree.fromstring(
                conditional["xl/styles.xml"]
            )
            differential = styles.find("s:dxfs/s:dxf", namespace)
            self.assertEqual(
                [child.tag.rsplit("}", 1)[-1] for child in differential],
                ["font"],
            )
            font = differential.find("s:font", namespace)
            self.assertEqual(
                [child.tag.rsplit("}", 1)[-1] for child in font],
                ["color"],
            )
            self.assertEqual(
                font.find("s:color", namespace).attrib,
                {"rgb": "FF9C0006"},
            )

    def test_package_validation_rejects_explicit_vertical_alignment(self) -> None:
        spec = MODULE.CASES[0]
        original = MODULE._styles

        def explicit_alignment(value):
            return original(value).replace(
                '<cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs>',
                '<cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0" applyAlignment="1"><alignment vertical="bottom"/></xf></cellXfs>',
            )

        with mock.patch.object(MODULE, "_styles", side_effect=explicit_alignment):
            with self.assertRaisesRegex(
                MODULE.OracleCorpusError,
                "implicit vertical alignment contract",
            ):
                MODULE.build_case(spec)

    def test_package_validation_rejects_false_rtl_attribute(self) -> None:
        spec = next(
            value
            for value in MODULE.CASES
            if value.toggle == "right-to-left-layout"
        )
        original = MODULE._worksheet

        def false_rtl(value):
            return original(value).replace(
                ' rightToLeft="1"',
                ' rightToLeft="0"',
            )

        with mock.patch.object(MODULE, "_worksheet", side_effect=false_rtl):
            with self.assertRaisesRegex(
                MODULE.OracleCorpusError,
                "RTL feature mismatch",
            ):
                MODULE.build_case(spec)

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
