#!/usr/bin/env python3
"""Tests for the project-owned deterministic render corpus generator."""

from __future__ import annotations

from hashlib import sha256
import importlib.util
import io
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from xml.etree import ElementTree
from zipfile import ZipFile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "generate-render-corpus.py"
FIDELITY_GATE = ROOT / "scripts" / "check-render-fidelity-targets.py"


def load_module():
    spec = importlib.util.spec_from_file_location("generate_render_corpus", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def load_fidelity_gate():
    spec = importlib.util.spec_from_file_location(
        "check_render_fidelity_targets_for_corpus_test", FIDELITY_GATE
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def first_case(cases, fmt: str, feature: str, enabled: bool = True):
    return next(
        case
        for case in cases
        if case.format == fmt and ((feature in case.features) is enabled)
    )


class GenerateRenderCorpusTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.module = load_module()

    def test_profiles_are_balanced_stable_and_full_reaches_800_cases(self) -> None:
        pilot = self.module.profile_specs("pilot")
        full = self.module.profile_specs("full")

        self.assertEqual(len(pilot), 40)
        self.assertEqual(len(full), 800)
        for fmt in self.module.FORMATS:
            self.assertEqual(sum(case.format == fmt for case in pilot), 10)
            self.assertEqual(sum(case.format == fmt for case in full), 200)
            pilot_format = [case for case in pilot if case.format == fmt]
            full_format = [case for case in full if case.format == fmt]
            self.assertEqual(
                [case.index for case in pilot_format],
                list(self.module.PILOT_INDICES[fmt]),
            )
            full_by_index = {case.index: case for case in full_format}
            self.assertEqual(
                pilot_format,
                [full_by_index[index] for index in self.module.PILOT_INDICES[fmt]],
            )

        manifest, cases = self.module.materialize("full")
        self.assertEqual(manifest["case_count"], 800)
        self.assertEqual(
            manifest["format_counts"],
            {"xls": 200, "xlsx": 200, "xlsb": 200, "ods": 200},
        )
        self.assertEqual(len(cases), 800)
        self.assertLess(manifest["total_bytes"], self.module.MAX_TOTAL_BYTES)

    def test_pilot_retains_every_feature_and_absolute_gate_core_cohort(self) -> None:
        gate = load_fidelity_gate()
        expected_excluded = frozenset(
            {
                "chart",
                "conditional-format",
                "image-drawing",
                "print-settings",
                "right-to-left-layout",
                "rtl-text",
                "sparkline",
                "wrapped-text",
            }
        )
        self.assertEqual(gate.CORE_EXCLUDED_FEATURES, expected_excluded)
        self.assertEqual(gate.MIN_CORE_WORKBOOKS, 10)
        self.assertEqual(
            self.module.PILOT_INDICES,
            {
                fmt: (0, 1, 2, 3, 4, 5, 6, 7, 8, 24)
                for fmt in self.module.FORMATS
            },
        )

        pilot = self.module.profile_specs("pilot")
        core = [
            case
            for case in pilot
            if not expected_excluded.intersection(case.features)
        ]
        self.assertGreaterEqual(len(core), gate.MIN_CORE_WORKBOOKS)
        for fmt in self.module.FORMATS:
            observed = set().union(
                *(set(case.features) for case in pilot if case.format == fmt)
            )
            self.assertEqual(observed, set(self.module.FORMAT_FEATURES[fmt]))

    def test_pairwise_lattice_covers_every_pair_and_feature_bucket(self) -> None:
        full = self.module.profile_specs("full")
        for fmt in self.module.FORMATS:
            cases = [case for case in full if case.format == fmt]
            counts = self.module._feature_counts(cases)
            for feature in self.module.FORMAT_FEATURES[fmt]:
                with self.subTest(format=fmt, feature=feature):
                    self.assertGreaterEqual(counts[feature], 25)

            lattice = self.module.FORMAT_LATTICE_FEATURES[fmt]
            for left_index, left in enumerate(lattice):
                for right in lattice[left_index + 1 :]:
                    combinations = {
                        (left in case.features, right in case.features)
                        for case in cases[: self.module.PAIRWISE_PERIOD]
                    }
                    with self.subTest(format=fmt, left=left, right=right):
                        self.assertEqual(
                            combinations,
                            {(False, False), (False, True), (True, False), (True, True)},
                        )

        expected_global = {
            "border": 200,
            "cell-fill": 200,
            "chart": 100,
            "chinese-text": 400,
            "column-width": 400,
            "conditional-format": 100,
            "date-format": 400,
            "formula-cached": 400,
            "hidden-column": 400,
            "hidden-row": 400,
            "image-drawing": 100,
            "japanese-text": 400,
            "korean-text": 416,
            "latin-text": 800,
            "merged-cells": 400,
            "noto-ofl-font": 600,
            "number-cell": 800,
            "percent-format": 400,
            "print-settings": 400,
            "right-to-left-layout": 200,
            "row-height": 400,
            "rtl-text": 400,
            "sparkline": 100,
            "unicode-text": 752,
            "wrapped-text": 200,
        }
        self.assertEqual(self.module._feature_counts(full), expected_global)

    def test_feature_tags_are_sorted_supported_and_derived_truthfully(self) -> None:
        for spec in self.module.profile_specs("full"):
            with self.subTest(case=spec.case_id):
                self.assertEqual(spec.features, tuple(sorted(spec.features)))
                self.assertEqual(
                    spec.features,
                    self.module.case_features(spec.format, spec.index),
                )
                self.assertTrue(set(spec.features) <= set(self.module.FORMAT_FEATURES[spec.format]))
                self.assertIn("latin-text", spec.features)
                self.assertIn("number-cell", spec.features)
                has_non_latin = bool(
                    set(spec.features)
                    & {"korean-text", "japanese-text", "chinese-text", "rtl-text"}
                )
                self.assertEqual("unicode-text" in spec.features, has_non_latin)

        for feature in ("chart", "conditional-format", "image-drawing", "sparkline"):
            self.assertIn(feature, self.module.FORMAT_FEATURES["xlsx"])
            self.assertNotIn(feature, self.module.FORMAT_FEATURES["xls"])
            self.assertNotIn(feature, self.module.FORMAT_FEATURES["xlsb"])
            self.assertNotIn(feature, self.module.FORMAT_FEATURES["ods"])
        self.assertNotIn("noto-ofl-font", self.module.FORMAT_FEATURES["xlsb"])

    def test_exact_bytes_are_reproducible_with_golden_hashes(self) -> None:
        expected = {
            "xls-0000": "1a3c7407c94dc7429db7fd12c2bce2f9cc49087034f0ef2e82f7f66a981fc062",
            "xlsx-0000": "ab2560d3354591599b995e6571fde822109833bba4fc560c7fdb477f03d86131",
            "xlsb-0000": "637c4c276da0dd387dc375ebe85c777c1a017f41c0560dc47a8ce1585b4d1708",
            "ods-0000": "bc590b68230ad1acd484b7925b53bfa7aeeb16e8423fc039003b7ccc70ef770e",
        }
        for spec in (case for case in self.module.profile_specs("pilot") if case.index == 0):
            first = self.module.build_case(spec)
            second = self.module.build_case(spec)
            with self.subTest(format=spec.format):
                self.assertEqual(first, second)
                self.assertEqual(sha256(first).hexdigest(), expected[spec.case_id])
                self.assertLess(len(first), self.module.MAX_CASE_BYTES)

    def test_multilingual_tags_match_authored_visible_cell_text(self) -> None:
        pilot = self.module.profile_specs("pilot")
        phrases = {
            "korean-text": "한국어 렌더링 사례",
            "japanese-text": "日本語レンダリング事例",
            "chinese-text": "中文渲染案例",
            "rtl-text": "مرحبا بالعالم",
        }
        for fmt in self.module.FORMATS:
            encoding = "utf-16le" if fmt in {"xls", "xlsb"} else "utf-8"
            for feature, phrase in phrases.items():
                enabled = first_case(pilot, fmt, feature)
                disabled = first_case(pilot, fmt, feature, False)
                with self.subTest(format=fmt, feature=feature):
                    self.assertIn(phrase.encode(encoding), self.module.build_case(enabled))
                    self.assertNotIn(phrase.encode(encoding), self.module.build_case(disabled))

    def test_custom_date_format_is_explicit_and_applied_to_typed_cells(self) -> None:
        pilot = self.module.profile_specs("pilot")
        format_id = self.module.CUSTOM_DATE_FORMAT_ID
        format_code = self.module.CUSTOM_DATE_FORMAT_CODE

        enabled_xls = first_case(pilot, "xls", "date-format")
        disabled_xls = first_case(pilot, "xls", "date-format", False)
        xls = self.module.build_case(enabled_xls)
        self.assertIn(self.module._biff_format(format_id, format_code), xls)
        self.assertIn(
            self.module._biff_number(
                1, 1, 45_366 + enabled_xls.index, style=1
            ),
            xls,
        )
        self.assertIn(
            self.module._biff_number(
                1, 1, 45_366 + disabled_xls.index, style=0
            ),
            self.module.build_case(disabled_xls),
        )

        enabled_xlsx = first_case(pilot, "xlsx", "date-format")
        disabled_xlsx = first_case(pilot, "xlsx", "date-format", False)
        with ZipFile(io.BytesIO(self.module.build_case(enabled_xlsx))) as archive:
            styles = archive.read("xl/styles.xml").decode("utf-8")
            sheet = archive.read("xl/worksheets/sheet1.xml").decode("utf-8")
        self.assertIn(
            f'<numFmt numFmtId="{format_id}" formatCode="{format_code}"/>',
            styles,
        )
        self.assertIn(
            f'<c r="B2" s="2"><v>{45_366 + enabled_xlsx.index}</v></c>',
            sheet,
        )
        with ZipFile(io.BytesIO(self.module.build_case(disabled_xlsx))) as archive:
            disabled_sheet = archive.read("xl/worksheets/sheet1.xml").decode("utf-8")
        self.assertIn(
            f'<c r="B2" s="0"><v>{45_366 + disabled_xlsx.index}</v></c>',
            disabled_sheet,
        )

        enabled_xlsb = first_case(pilot, "xlsb", "date-format")
        disabled_xlsb = first_case(pilot, "xlsb", "date-format", False)
        with ZipFile(io.BytesIO(self.module.build_case(enabled_xlsb))) as archive:
            styles_bin = archive.read("xl/styles.bin")
            sheet_bin = archive.read("xl/worksheets/sheet1.bin")
        self.assertIn(self.module._xlsb_fmt(format_id, format_code), styles_bin)
        self.assertIn(
            self.module._xlsb_cell_real(
                1, float(45_366 + enabled_xlsb.index), style=1
            ),
            sheet_bin,
        )
        with ZipFile(io.BytesIO(self.module.build_case(disabled_xlsb))) as archive:
            disabled_sheet_bin = archive.read("xl/worksheets/sheet1.bin")
        self.assertIn(
            self.module._xlsb_cell_real(
                1, float(45_366 + disabled_xlsb.index), style=0
            ),
            disabled_sheet_bin,
        )

        enabled_ods = first_case(pilot, "ods", "date-format")
        disabled_ods = first_case(pilot, "ods", "date-format", False)
        with ZipFile(io.BytesIO(self.module.build_case(enabled_ods))) as archive:
            content = archive.read("content.xml").decode("utf-8")
        self.assertIn(
            '<number:date-style style:name="Ndate" number:automatic-order="false">'
            '<number:year number:style="long"/><number:text>-</number:text>'
            '<number:month number:style="long"/><number:text>-</number:text>'
            '<number:day number:style="long"/></number:date-style>',
            content,
        )
        self.assertIn(
            'table:style-name="ce-date" office:value-type="date" '
            'office:date-value=',
            content,
        )
        with ZipFile(io.BytesIO(self.module.build_case(disabled_ods))) as archive:
            disabled_content = archive.read("content.xml").decode("utf-8")
        self.assertNotIn('office:value-type="date"', disabled_content)

    def test_ods_defaults_fix_typography_and_row_geometry_without_losing_wrap(self) -> None:
        pilot = self.module.profile_specs("pilot")
        wrapped = first_case(pilot, "ods", "wrapped-text")
        with ZipFile(io.BytesIO(self.module.build_case(wrapped))) as archive:
            content = archive.read("content.xml").decode("utf-8")
            styles = archive.read("styles.xml").decode("utf-8")
        self.assertIn('fo:font-size="11pt"', styles)
        self.assertIn('style:font-size-asian="11pt"', styles)
        self.assertIn('style:font-size-complex="11pt"', styles)
        self.assertIn(
            '<style:default-style style:family="table-row">'
            f'<style:table-row-properties style:row-height="{self.module.ODS_DEFAULT_ROW_HEIGHT}"/>'
            '</style:default-style>',
            styles,
        )
        self.assertIn(
            f'<style:style style:name="ro-default" style:family="table-row"><style:table-row-properties style:row-height="{self.module.ODS_DEFAULT_ROW_HEIGHT}" style:use-optimal-row-height="false"/></style:style>',
            content,
        )
        self.assertIn(
            f'<style:style style:name="ro-wrap" style:family="table-row"><style:table-row-properties style:row-height="{self.module.ODS_WRAPPED_ROW_HEIGHT}" style:use-optimal-row-height="false"/></style:style>',
            content,
        )
        self.assertIn('table:table-row table:style-name="ro-default"', content)
        self.assertIn('table:table-row table:style-name="ro-wrap"', content)
        self.assertIn('table:style-name="ce-wrap"', content)
        self.assertIn(
            "Wrapped project-authored text for",
            content,
        )

    def test_declared_core_visual_capabilities_are_authored(self) -> None:
        pilot = self.module.profile_specs("pilot")

        xls_spec = first_case(pilot, "xls", "print-settings")
        xls = self.module.build_case(xls_spec)
        self.assertIn(self.module._biff_font("Noto Sans CJK KR"), xls)
        self.assertIn(self.module._biff_row(0, 600), xls)
        self.assertIn(self.module._biff_row(3, 255, hidden=True), xls)
        self.assertIn(self.module._biff_col(5, 5, 8 * 256, hidden=True), xls)
        self.assertIn(self.module._biff_merge(2, 0, 2, 2), xls)
        self.assertIn(self.module._biff_page_setup(), xls)
        no_print_xls = self.module.build_case(first_case(pilot, "xls", "print-settings", False))
        self.assertNotIn(self.module._biff_page_setup(), no_print_xls)

        xlsb_spec = first_case(pilot, "xlsb", "print-settings")
        xlsb = self.module.build_case(xlsb_spec)
        with ZipFile(io.BytesIO(xlsb)) as archive:
            sheet_bin = archive.read("xl/worksheets/sheet1.bin")
        self.assertIn(
            self.module._xlsb_row(
                0,
                600,
                first_col=0,
                last_col=4,
                custom_height=True,
            ),
            sheet_bin,
        )
        self.assertIn(
            self.module._xlsb_row(
                3,
                255,
                first_col=0,
                last_col=0,
                hidden=True,
            ),
            sheet_bin,
        )
        self.assertIn(self.module._xlsb_col(5, 5, 8 * 256, hidden=True), sheet_bin)
        self.assertIn(self.module._xlsb_merge(2, 0, 2, 2), sheet_bin)
        self.assertIn(self.module._xlsb_print_settings(), sheet_bin)

        xlsx_spec = first_case(pilot, "xlsx", "print-settings")
        with ZipFile(io.BytesIO(self.module.build_case(xlsx_spec))) as archive:
            sheet = archive.read("xl/worksheets/sheet1.xml").decode("utf-8")
            styles = archive.read("xl/styles.xml").decode("utf-8")
            workbook = archive.read("xl/workbook.xml").decode("utf-8")
        self.assertIn('rightToLeft="1"', sheet)
        self.assertIn('hidden="1"', sheet)
        self.assertIn('<mergeCell ref="A3:C3"/>', sheet)
        self.assertIn('<pageSetup orientation="portrait" paperSize="1" scale="85"', sheet)
        self.assertIn('<rowBreaks count="1" manualBreakCount="1">', sheet)
        self.assertIn('<colBreaks count="1" manualBreakCount="1">', sheet)
        self.assertIn('<oddHeader>&amp;LAuthored&amp;CPage &amp;P of &amp;N</oddHeader>', sheet)
        self.assertIn('name="_xlnm.Print_Titles"', workbook)
        self.assertIn('r="A5" s="6"', sheet)
        self.assertIn('r="A3" s="5"', sheet)
        self.assertIn('r="A1" s="4"', sheet)
        self.assertIn('<f>', sheet)
        self.assertIn('name val="Noto Sans CJK KR"', styles)

        fit_spec = next(
            case
            for case in pilot
            if case.format == "xlsx"
            and "print-settings" in case.features
            and case.index % 2 == 1
        )
        with ZipFile(io.BytesIO(self.module.build_case(fit_spec))) as archive:
            fit_sheet = archive.read("xl/worksheets/sheet1.xml").decode("utf-8")
        self.assertIn('<pageSetUpPr fitToPage="1"/>', fit_sheet)
        self.assertIn('fitToWidth="2" fitToHeight="2"', fit_sheet)
        self.assertNotIn(' scale="85"', fit_sheet)

        ods_spec = first_case(pilot, "ods", "print-settings")
        with ZipFile(io.BytesIO(self.module.build_case(ods_spec))) as archive:
            content = archive.read("content.xml").decode("utf-8")
            styles = archive.read("styles.xml").decode("utf-8")
        self.assertIn('table:style-name="ta-rtl"', content)
        self.assertEqual(content.count('table:visibility="collapse"'), 2)
        self.assertIn('table:number-columns-spanned="3"', content)
        self.assertIn('table:style-name="ce-wrap"', content)
        self.assertIn('table:style-name="ce-fill"', content)
        self.assertIn('table:style-name="ce-border"', content)
        self.assertIn('style:print-orientation="landscape"', styles)
        self.assertIn('style:name="Noto Sans CJK KR"', styles)

    def test_xlsb_record_streams_are_complete_counted_and_well_nested(self) -> None:
        spec = next(
            case
            for case in self.module.profile_specs("pilot")
            if case.format == "xlsb" and case.index == 0
        )
        with ZipFile(io.BytesIO(self.module.build_case(spec))) as archive:
            workbook = self.module._biff12_records(archive.read("xl/workbook.bin"))
            shared = self.module._biff12_records(
                archive.read("xl/sharedStrings.bin")
            )
            styles = self.module._biff12_records(archive.read("xl/styles.bin"))
            sheet = self.module._biff12_records(
                archive.read("xl/worksheets/sheet1.bin")
            )

        self.assertEqual(
            [record_type for record_type, _ in workbook],
            [0x0083, 0x0080, 0x008F, 0x009C, 0x0090, 0x0084],
        )
        bundle = workbook[3][1]
        self.assertEqual(int.from_bytes(bundle[0:4], "little"), 0)
        self.assertEqual(int.from_bytes(bundle[4:8], "little"), 1)

        self.assertEqual(shared[0][0], 0x009F)
        self.assertEqual(shared[-1], (0x00A0, b""))
        self.assertEqual(int.from_bytes(shared[0][1][0:4], "little"), 7)
        self.assertEqual(int.from_bytes(shared[0][1][4:8], "little"), 7)
        self.assertEqual([kind for kind, _ in shared].count(0x0013), 7)

        expected_style_types = [
            0x0116,
            0x0267,
            0x002C,
            0x0268,
            0x0263,
            0x002B,
            0x0264,
            0x025B,
            0x002D,
            0x025C,
            0x0265,
            0x002E,
            0x0266,
            0x0272,
            0x002F,
            0x0273,
            0x0269,
            0x002F,
            0x002F,
            0x002F,
            0x026A,
            0x026B,
            0x0030,
            0x026C,
            0x0117,
        ]
        self.assertEqual([kind for kind, _ in styles], expected_style_types)
        self.assertEqual(int.from_bytes(styles[1][1], "little"), 1)
        self.assertEqual(
            styles[2],
            (
                0x002C,
                self.module._u16(self.module.CUSTOM_DATE_FORMAT_ID)
                + self.module._xlsb_wstr(self.module.CUSTOM_DATE_FORMAT_CODE),
            ),
        )
        self.assertEqual(int.from_bytes(styles[4][1], "little"), 1)
        self.assertEqual(int.from_bytes(styles[7][1], "little"), 1)
        self.assertEqual(int.from_bytes(styles[10][1], "little"), 1)
        self.assertEqual(int.from_bytes(styles[13][1], "little"), 1)
        self.assertEqual(int.from_bytes(styles[16][1], "little"), 3)
        self.assertEqual(int.from_bytes(styles[21][1], "little"), 1)
        self.assertEqual(len(styles[5][1]), 39)
        self.assertEqual(len(styles[8][1]), 68)
        self.assertEqual(len(styles[11][1]), 51)
        self.assertTrue(
            all(len(payload) == 16 for kind, payload in styles if kind == 0x002F)
        )
        cell_xf_formats = [
            int.from_bytes(styles[index][1][2:4], "little")
            for index in (17, 18, 19)
        ]
        self.assertEqual(
            cell_xf_formats, [0, self.module.CUSTOM_DATE_FORMAT_ID, 10]
        )

        sheet_types = [kind for kind, _ in sheet]
        self.assertEqual(sheet_types[0:2], [0x0081, 0x0094])
        self.assertEqual(sheet_types[-1], 0x0082)
        self.assertLess(sheet_types.index(0x0186), sheet_types.index(0x0187))
        begin_data = sheet_types.index(0x0091)
        end_data = sheet_types.index(0x0092)
        self.assertLess(begin_data, end_data)
        self.assertEqual(sheet[1][1], bytes.fromhex("00000000030000000000000004000000"))

        data_records = sheet[begin_data + 1 : end_data]
        rows = [payload for kind, payload in data_records if kind == 0]
        self.assertEqual(len(rows), 4)
        self.assertTrue(all(len(payload) == 25 for payload in rows))
        self.assertEqual(
            [int.from_bytes(payload[13:17], "little") for payload in rows],
            [1, 1, 1, 1],
        )
        self.assertEqual(
            [
                (
                    int.from_bytes(payload[17:21], "little"),
                    int.from_bytes(payload[21:25], "little"),
                )
                for payload in rows
            ],
            [(0, 4), (0, 3), (0, 0), (0, 0)],
        )
        self.assertTrue(int.from_bytes(rows[0][10:12], "little") & (1 << 13))
        self.assertTrue(int.from_bytes(rows[3][10:12], "little") & (1 << 12))

        begin_merges = sheet_types.index(177)
        self.assertEqual(int.from_bytes(sheet[begin_merges][1], "little"), 1)
        self.assertEqual(sheet_types[begin_merges : begin_merges + 3], [177, 176, 178])
        self.assertEqual(
            [(kind, len(payload)) for kind, payload in sheet if kind in {476, 477, 478}],
            [(477, 2), (476, 48), (478, 38)],
        )

    def test_biff12_parser_rejects_adversarial_record_headers_and_truncation(self) -> None:
        malformed = (
            (b"\x80", "truncated BIFF12 record type"),
            (b"\x80\x80\x00\x00", "BIFF12 record type exceeds 2 bytes"),
            (b"\x80\x00\x00", "non-canonical BIFF12 record type"),
            (b"\x01\x80", "truncated BIFF12 record size"),
            (b"\x01\x05ab", "truncated BIFF12 record payload"),
        )
        for payload, message in malformed:
            with self.subTest(payload=payload.hex()):
                with self.assertRaisesRegex(self.module.CorpusError, message):
                    self.module._biff12_records(payload)

        with self.assertRaisesRegex(self.module.CorpusError, "record type is out of range"):
            self.module._biff12_record(0x4000, b"")
        encoded = self.module._biff12_record(0x009C, b"payload")
        self.assertEqual(self.module._biff12_records(encoded), ((0x009C, b"payload"),))

    def test_generated_xlsb_has_visible_cells_in_libreoffice_pdf(self) -> None:
        libreoffice = shutil.which("soffice")
        mac_binary = Path(
            "/Applications/LibreOffice.app/Contents/MacOS/soffice"
        )
        if libreoffice is None and mac_binary.is_file():
            libreoffice = str(mac_binary)
        pdftotext = shutil.which("pdftotext")
        if libreoffice is None or pdftotext is None:
            self.skipTest("LibreOffice and Poppler pdftotext are required")

        spec = next(
            case
            for case in self.module.profile_specs("pilot")
            if case.format == "xlsb" and case.index == 0
        )
        with tempfile.TemporaryDirectory(prefix="rxls-xlsb-libreoffice-") as tmp:
            base = Path(tmp)
            source = base / f"{spec.case_id}.xlsb"
            output = base / "pdf"
            profile = base / "lo-profile"
            output.mkdir()
            profile.mkdir()
            source.write_bytes(self.module.build_case(spec))

            converted = subprocess.run(
                (
                    libreoffice,
                    f"-env:UserInstallation={profile.as_uri()}",
                    "--headless",
                    "--convert-to",
                    "pdf",
                    "--outdir",
                    str(output),
                    str(source),
                ),
                check=False,
                capture_output=True,
                text=True,
                timeout=60,
            )
            self.assertEqual(
                converted.returncode,
                0,
                msg=f"LibreOffice failed:\n{converted.stdout}\n{converted.stderr}",
            )
            pdf = output / f"{spec.case_id}.pdf"
            self.assertTrue(pdf.is_file(), msg=f"missing PDF; stdout={converted.stdout}")
            extracted = subprocess.run(
                (pdftotext, "-layout", str(pdf), "-"),
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertEqual(
                extracted.returncode,
                0,
                msg=f"pdftotext failed:\n{extracted.stdout}\n{extracted.stderr}",
            )
            text = " ".join(extracted.stdout.split())
            self.assertIn("Latin render case", text)
            self.assertIn("Merged xlsb-0000", text)
            self.assertIn("0.25", text)
            self.assertNotIn("Hidden xlsb-0000", text)
            self.assertNotIn("45366", text)

    def test_custom_dates_render_as_iso_text_in_libreoffice_pdf(self) -> None:
        libreoffice = shutil.which("soffice")
        mac_binary = Path(
            "/Applications/LibreOffice.app/Contents/MacOS/soffice"
        )
        if libreoffice is None and mac_binary.is_file():
            libreoffice = str(mac_binary)
        pdftotext = shutil.which("pdftotext")
        if libreoffice is None or pdftotext is None:
            self.skipTest("LibreOffice and Poppler pdftotext are required")

        specs = [
            case
            for case in self.module.profile_specs("pilot")
            if case.index == 0
        ]
        self.assertEqual([case.format for case in specs], list(self.module.FORMATS))
        self.assertTrue(all("date-format" in case.features for case in specs))
        with tempfile.TemporaryDirectory(prefix="rxls-date-libreoffice-") as tmp:
            base = Path(tmp)
            output = base / "pdf"
            profile = base / "lo-profile"
            output.mkdir()
            profile.mkdir()
            sources = []
            for spec in specs:
                source = base / f"{spec.case_id}.{spec.format}"
                source.write_bytes(self.module.build_case(spec))
                sources.append(source)

            converted = subprocess.run(
                (
                    libreoffice,
                    f"-env:UserInstallation={profile.as_uri()}",
                    "--headless",
                    "--convert-to",
                    "pdf",
                    "--outdir",
                    str(output),
                    *(str(source) for source in sources),
                ),
                check=False,
                capture_output=True,
                text=True,
                timeout=120,
            )
            self.assertEqual(
                converted.returncode,
                0,
                msg=f"LibreOffice failed:\n{converted.stdout}\n{converted.stderr}",
            )
            for spec in specs:
                pdf = output / f"{spec.case_id}.pdf"
                with self.subTest(format=spec.format):
                    self.assertTrue(pdf.is_file())
                    extracted = subprocess.run(
                        (pdftotext, "-layout", str(pdf), "-"),
                        check=False,
                        capture_output=True,
                        text=True,
                        timeout=30,
                    )
                    self.assertEqual(extracted.returncode, 0)
                    self.assertIn("2024-03-15", extracted.stdout)

    def test_ods_defaults_survive_libreoffice_import(self) -> None:
        libreoffice = shutil.which("soffice")
        mac_binary = Path(
            "/Applications/LibreOffice.app/Contents/MacOS/soffice"
        )
        if libreoffice is None and mac_binary.is_file():
            libreoffice = str(mac_binary)
        if libreoffice is None:
            self.skipTest("LibreOffice is required")

        spec = first_case(self.module.profile_specs("pilot"), "ods", "wrapped-text")
        with tempfile.TemporaryDirectory(prefix="rxls-ods-defaults-libreoffice-") as tmp:
            base = Path(tmp)
            source = base / f"{spec.case_id}.ods"
            output = base / "xlsx"
            profile = base / "lo-profile"
            output.mkdir()
            profile.mkdir()
            source.write_bytes(self.module.build_case(spec))
            converted = subprocess.run(
                (
                    libreoffice,
                    f"-env:UserInstallation={profile.as_uri()}",
                    "--headless",
                    "--convert-to",
                    "xlsx",
                    "--outdir",
                    str(output),
                    str(source),
                ),
                check=False,
                capture_output=True,
                text=True,
                timeout=60,
            )
            self.assertEqual(
                converted.returncode,
                0,
                msg=f"LibreOffice failed:\n{converted.stdout}\n{converted.stderr}",
            )
            xlsx = output / f"{spec.case_id}.xlsx"
            self.assertTrue(xlsx.is_file())
            with ZipFile(xlsx) as archive:
                sheet = ElementTree.fromstring(
                    archive.read("xl/worksheets/sheet1.xml")
                )
                styles = ElementTree.fromstring(archive.read("xl/styles.xml"))

        namespace = {
            "s": "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
        }
        rows = {
            int(row.attrib["r"]): row
            for row in sheet.findall(".//s:sheetData/s:row", namespace)
        }
        for row_number in (2, 3, 4):
            self.assertEqual(rows[row_number].attrib.get("ht"), "15")
            self.assertEqual(rows[row_number].attrib.get("customHeight"), "true")
        self.assertEqual(rows[5].attrib.get("ht"), "45")
        self.assertEqual(rows[5].attrib.get("customHeight"), "true")
        fonts = styles.findall(".//s:fonts/s:font", namespace)
        self.assertTrue(
            any(
                font.find("s:sz", namespace) is not None
                and font.find("s:sz", namespace).attrib.get("val") == "11"
                and font.find("s:name", namespace) is not None
                and font.find("s:name", namespace).attrib.get("val")
                == "Noto Sans CJK KR"
                for font in fonts
            )
        )

    def test_xlsx_advanced_parts_relationships_and_assets_are_real_and_bounded(self) -> None:
        pilot = self.module.profile_specs("pilot")
        spec = next(
            case
            for case in pilot
            if case.format == "xlsx"
            and {"chart", "conditional-format", "image-drawing", "sparkline"}
            <= set(case.features)
        )
        payload = self.module.build_case(spec)
        with ZipFile(io.BytesIO(payload)) as archive:
            names = set(archive.namelist())
            self.assertLessEqual(len(names), self.module.MAX_ZIP_PARTS)
            expected_parts = {
                "xl/charts/chart1.xml",
                "xl/drawings/drawing1.xml",
                "xl/drawings/_rels/drawing1.xml.rels",
                "xl/media/image1.png",
                "xl/worksheets/_rels/sheet1.xml.rels",
            }
            self.assertTrue(expected_parts <= names)
            png = archive.read("xl/media/image1.png")
            self.assertTrue(png.startswith(b"\x89PNG\r\n\x1a\n"))
            self.assertLessEqual(len(png), self.module.MAX_IMAGE_BYTES)
            drawing = archive.read("xl/drawings/drawing1.xml").decode("utf-8")
            drawing_rels = archive.read(
                "xl/drawings/_rels/drawing1.xml.rels"
            ).decode("utf-8")
            sheet = archive.read("xl/worksheets/sheet1.xml").decode("utf-8")
            content_types = archive.read("[Content_Types].xml").decode("utf-8")
            self.assertIn('r:embed="rIdImage"', drawing)
            self.assertIn('r:id="rIdChart"', drawing)
            self.assertIn('Target="../media/image1.png"', drawing_rels)
            self.assertIn('Target="../charts/chart1.xml"', drawing_rels)
            self.assertIn("<conditionalFormatting", sheet)
            self.assertIn("<x14:sparklineGroups", sheet)
            self.assertIn("drawing1.xml", content_types)
            self.assertIn("chart1.xml", content_types)
            relationship_count = sum(
                archive.read(name).count(b"<Relationship ")
                for name in archive.namelist()
                if name.endswith(".rels")
            )
            self.assertLessEqual(
                relationship_count, self.module.MAX_PACKAGE_RELATIONSHIPS
            )
            self.assertNotIn(b'TargetMode="External"', payload)
            self.assertLessEqual(
                drawing.count("<xdr:twoCellAnchor"),
                self.module.MAX_DRAWING_OBJECTS,
            )
            chart = archive.read("xl/charts/chart1.xml")
            self.assertLessEqual(
                chart.count(b"<c:pt "), self.module.MAX_CHART_POINTS
            )
            for name in archive.namelist():
                if name.endswith((".xml", ".rels")):
                    ElementTree.fromstring(archive.read(name))

        for feature, part in (
            ("image-drawing", "xl/media/image1.png"),
            ("chart", "xl/charts/chart1.xml"),
        ):
            disabled = first_case(pilot, "xlsx", feature, False)
            with ZipFile(io.BytesIO(self.module.build_case(disabled))) as archive:
                self.assertNotIn(part, archive.namelist())
        no_conditional = first_case(pilot, "xlsx", "conditional-format", False)
        no_sparkline = first_case(pilot, "xlsx", "sparkline", False)
        with ZipFile(io.BytesIO(self.module.build_case(no_conditional))) as archive:
            self.assertNotIn(
                b"<conditionalFormatting", archive.read("xl/worksheets/sheet1.xml")
            )
        with ZipFile(io.BytesIO(self.module.build_case(no_sparkline))) as archive:
            self.assertNotIn(
                b"<x14:sparklineGroups", archive.read("xl/worksheets/sheet1.xml")
            )

    def test_every_conditional_format_case_satisfies_its_visible_rule(self) -> None:
        cases = [
            case
            for case in self.module.profile_specs("full")
            if case.format == "xlsx" and "conditional-format" in case.features
        ]
        self.assertEqual(len(cases), 100)
        for spec in cases:
            with ZipFile(io.BytesIO(self.module.build_case(spec))) as archive:
                sheet = ElementTree.fromstring(
                    archive.read("xl/worksheets/sheet1.xml")
                )
            namespace = {
                "s": "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
            }
            cell = sheet.find(".//s:c[@r='A2']/s:v", namespace)
            rule = sheet.find(
                ".//s:conditionalFormatting[@sqref='A2']/s:cfRule", namespace
            )
            formula = (
                rule.find("s:formula", namespace) if rule is not None else None
            )
            with self.subTest(case=spec.case_id):
                self.assertIsNotNone(cell)
                self.assertIsNotNone(rule)
                self.assertEqual(rule.attrib.get("operator"), "greaterThan")
                self.assertIsNotNone(formula)
                self.assertGreater(float(cell.text), float(formula.text))

    def test_sparkline_destination_stays_visible_when_column_f_is_hidden(self) -> None:
        spec = next(
            case
            for case in self.module.profile_specs("pilot")
            if case.format == "xlsx"
            and {"hidden-column", "sparkline"} <= set(case.features)
        )
        with ZipFile(io.BytesIO(self.module.build_case(spec))) as archive:
            sheet = archive.read("xl/worksheets/sheet1.xml").decode("utf-8")
        self.assertIn('<col min="6" max="6" hidden="1"/>', sheet)
        self.assertIn('<c r="A6"><v>0</v></c>', sheet)
        self.assertIn("<xm:f>Render!B7:E7</xm:f><xm:sqref>A6</xm:sqref>", sheet)
        self.assertNotIn("<xm:sqref>F7</xm:sqref>", sheet)

    def test_zip_parts_use_fixed_order_timestamp_storage_and_valid_xml(self) -> None:
        for spec in (
            case
            for case in self.module.profile_specs("pilot")
            if case.index == 0 and case.format != "xls"
        ):
            with ZipFile(io.BytesIO(self.module.build_case(spec))) as archive:
                with self.subTest(format=spec.format):
                    self.assertIsNone(archive.testzip())
                    self.assertLessEqual(len(archive.infolist()), self.module.MAX_ZIP_PARTS)
                    self.assertEqual(len(archive.namelist()), len(set(archive.namelist())))
                    self.assertTrue(
                        all(item.date_time == self.module.DOS_EPOCH for item in archive.infolist())
                    )
                    self.assertTrue(all(item.compress_type == 0 for item in archive.infolist()))
                    for name in archive.namelist():
                        if name.endswith((".xml", ".rels")):
                            ElementTree.fromstring(archive.read(name))
        xls = next(
            spec
            for spec in self.module.profile_specs("pilot")
            if spec.format == "xls" and spec.index == 0
        )
        self.assertTrue(
            self.module.build_case(xls).startswith(bytes.fromhex("d0cf11e0a1b11ae1"))
        )

    def test_manifest_rows_include_rights_features_hashes_and_caps(self) -> None:
        manifest, cases = self.module.materialize("pilot")
        self.assertEqual(manifest["schema_version"], 1)
        self.assertEqual(manifest["generator"], self.module.GENERATOR)
        self.assertEqual(manifest["generator_version"], "1.3.0")
        self.assertEqual(manifest["license"], "MIT")
        self.assertEqual(manifest["redistribution"], "allowed")
        self.assertEqual(manifest["rights_tier"], "S")
        self.assertIs(manifest["source_redistributable"], True)
        self.assertIs(manifest["render_redistributable"], True)
        self.assertEqual(len(manifest["files"]), len(cases))
        self.assertEqual(
            manifest["feature_counts"],
            self.module._feature_counts(spec for spec, _ in cases),
        )
        self.assertEqual(
            manifest["format_feature_counts"],
            self.module._format_feature_counts(spec for spec, _ in cases),
        )
        for row, (spec, payload) in zip(manifest["files"], cases, strict=True):
            with self.subTest(case=spec.case_id):
                self.assertEqual(row["case_id"], spec.case_id)
                self.assertEqual(row["seed"], spec.seed)
                self.assertEqual(row["format"], spec.format)
                self.assertEqual(row["path"], spec.relative_path)
                self.assertEqual(row["features"], list(spec.features))
                self.assertEqual(row["byte_length"], len(payload))
                self.assertEqual(row["sha256"], sha256(payload).hexdigest())
                self.assertEqual(row["generator"], self.module.GENERATOR)
                self.assertEqual(row["generator_version"], self.module.GENERATOR_VERSION)
                self.assertEqual(row["license"], "MIT")
                self.assertEqual(row["redistribution"], "allowed")
                self.assertEqual(row["rights_tier"], "S")
                self.assertIs(row["source_redistributable"], True)
                self.assertIs(row["render_redistributable"], True)

    def test_generate_verify_replace_and_tamper_detection(self) -> None:
        self.module.OUTPUT_BASE.mkdir(parents=True, exist_ok=True)
        temporary = Path(
            tempfile.mkdtemp(prefix="generator-test-", dir=self.module.OUTPUT_BASE)
        )
        output = temporary / "pilot"
        try:
            first = self.module.generate("pilot", output)
            self.assertEqual(self.module.verify("pilot", output), first)

            sentinel = output / "stale.txt"
            sentinel.write_text("must disappear", encoding="utf-8")
            second = self.module.generate("pilot", output)
            self.assertEqual(first, second)
            self.assertFalse(sentinel.exists())
            self.assertEqual(self.module.verify("pilot", output), second)

            payload = output / second["files"][0]["path"]
            payload.write_bytes(payload.read_bytes() + b"tamper")
            with self.assertRaisesRegex(
                self.module.CorpusError, "not exactly reproducible"
            ):
                self.module.verify("pilot", output)
        finally:
            shutil.rmtree(temporary, ignore_errors=True)

    def test_output_guard_rejects_repository_and_base_paths(self) -> None:
        for candidate in (ROOT / "tests" / "fixtures", self.module.OUTPUT_BASE):
            with self.subTest(path=candidate):
                with self.assertRaises(self.module.CorpusError):
                    self.module.resolve_output("pilot", str(candidate))


if __name__ == "__main__":
    unittest.main()
