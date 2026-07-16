#!/usr/bin/env python3
"""Unit tests for the absolute LibreOffice rendering-fidelity gate."""

from __future__ import annotations

import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "check-render-fidelity-targets.py"


def load_module():
    spec = importlib.util.spec_from_file_location(
        "check_render_fidelity_targets", SCRIPT
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def page_row(
    *,
    sheet_index: int = 0,
    box_error: int = 100,
    box_count: int = 3,
) -> dict[str, object]:
    return {
        "sheet_index": sheet_index,
        "pixels": 10_000,
        "absolute_error_sum": 0,
        "similarity_ppm": 1_000_000,
        "edge_rxls_pixels": 1_000,
        "edge_libreoffice_pixels": 1_000,
        "edge_rxls_matched_1px": 1_000,
        "edge_libreoffice_matched_1px": 1_000,
        "semantic_codepoint_rxls_items": 1_000,
        "semantic_codepoint_libreoffice_items": 1_000,
        "semantic_codepoint_matched_items": 1_000,
        "rxls_size": {"width": 800, "height": 600},
        "libreoffice_size": {"width": 800, "height": 600},
        "text_box_candidate_items": box_count,
        "text_box_matched_items": box_count,
        "text_box_ambiguous_items": 0,
        "text_box_unmatched_items": 0,
        "text_box_match_coverage_ppm": 1_000_000,
        "text_box_error_histogram_millipoints": [
            {"error_millipoints": box_error, "count": box_count}
        ],
        "text_box_median_error_millipoints": box_error,
        "text_box_p95_error_millipoints": box_error,
    }


def file_row(format_name: str, index: int) -> dict[str, object]:
    page = page_row()
    return {
        "path": f"private/corpus/secret-{index}.{format_name}",
        "format": format_name,
        "status": "compared",
        "classification": "within_threshold",
        "features": ["basic"],
        "metrics": {"similarity_ppm": 1_000_000},
        "pages": [page],
        "scenes": [{"sheet_index": 0}],
        "artifacts": {"rxls_pages": 1, "libreoffice_pages": 1},
        "font_attestation": {
            "embedded_font_objects": 2,
            "font_objects": 2,
            "matched_font_objects": 2,
            "normalized_identities_sha256": "9" * 64,
            "subset_font_objects": 2,
            "unicode_font_objects": 2,
            "unique_font_identities": 1,
        },
    }


def report_document(count: int = 4) -> dict[str, object]:
    formats = MODULE.ORACLE_FORMATS
    files = [file_row(formats[index % len(formats)], index) for index in range(count)]
    return {
        "schema": MODULE.EVIDENCE_SCHEMA,
        "mode": "compare",
        "configuration": {
            "dpi": 96,
            "font_pack": {"pack_sha256": "a" * 64},
            "renderer_binary": {"sha256": "b" * 64},
            "oracle_lock": {
                "profile": "locked-linux-x86_64",
                "configuration": {"dpi": 96, "profile_sha256": "c" * 64},
                "font_pack_sha256": "a" * 64,
                "libreoffice": {"executable_sha256": "d" * 64},
                "python": {"numpy_version": "2.3.1"},
                "pdf_rasterizer": {
                    "kind": "poppler",
                    "pdffonts_sha256": "1" * 64,
                    "pdfinfo_sha256": "e" * 64,
                    "pdftoppm_sha256": "f" * 64,
                    "pdftotext_sha256": "0" * 64,
                },
            },
            "metric_policy": {
                "mask_match_tolerance_pixels": 1,
                "edge_luma_delta": 32,
                "semantic_content_retained": False,
                "semantic_text_source": (
                    "svg_data-rxls-visible-label_vs_pdftotext_layout"
                ),
                "text_box_content_retained": False,
                "text_box_error_units": "millipoints",
                "text_box_source": (
                    "svg_clipped_glyph_bounds_vs_pdftotext_bbox_layout"
                ),
                "text_box_matching": (
                    "exact_svg_data-rxls-visible-label_nearest_unique_pdftotext_bbox_layout"
                ),
                "implementation": {
                    "kind": "numpy_integer_exact_v1",
                    "version": "2.3.1",
                },
            },
        },
        "summary": {"files": count, "by_status": {"compared": count}},
        "files": files,
    }


def container_report_document(count: int = 4) -> dict[str, object]:
    report = report_document(count)
    font_pack_sha256 = report["configuration"]["font_pack"]["pack_sha256"]
    image_id = "sha256:" + "2" * 64
    oracle = {
        "artifact_sha256": MODULE.CONTAINER_LIBREOFFICE_ARTIFACT_SHA256,
        "name": "LibreOffice",
        "version": "26.2.3.2",
    }
    identity = {
        "build_contract_sha256": "3" * 64,
        "font_pack_sha256": font_pack_sha256,
        "image": {
            "architecture": "linux/amd64",
            "config_digest": image_id,
            "expected_config_digest": image_id,
            "identity_status": "pinned_match",
        },
        "libreoffice": oracle,
        "lock_file_sha256": "4" * 64,
        "pdf_font_inspector": {
            "kind": "poppler",
            "pdffonts_sha256": "5" * 64,
        },
        "runtime": "docker",
        "schema": MODULE.CONTAINER_IDENTITY_SCHEMA,
    }
    report["configuration"]["oracle_lock"] = identity
    adapter = {
        "font_pack_sha256": font_pack_sha256,
        "image": {
            "architecture": "linux/amd64",
            "expected_id": image_id,
            "id": image_id,
            "identity_status": "pinned_match",
        },
        "lock_file_sha256": "4" * 64,
        "lock_sha256": "3" * 64,
        "oracle": oracle,
        "runtime": "docker",
        "schema": MODULE.CONTAINER_EXECUTION_SCHEMA,
    }
    for item in report["files"]:
        item["oracle_adapter"] = copy.deepcopy(adapter)
    return report


def synchronize_similarity(item: dict[str, object]) -> None:
    pages = item["pages"]
    assert isinstance(pages, list)
    similarity = MODULE._file_similarity(pages)
    metrics = item["metrics"]
    assert isinstance(metrics, dict)
    metrics["similarity_ppm"] = similarity


class CheckRenderFidelityTargetsTests(unittest.TestCase):
    def evaluate_small(self, report: dict[str, object]) -> dict[str, object]:
        with mock.patch.multiple(
            MODULE,
            MIN_BROAD_WORKBOOKS=4,
            MIN_CORE_WORKBOOKS=4,
            MIN_CORE_TEXT_BOXES=4,
        ):
            return MODULE.evaluate(report, "c" * 64, 1234)

    def test_complete_required_format_cohort_passes(self) -> None:
        result = self.evaluate_small(report_document())
        self.assertTrue(result["passed"])
        self.assertEqual(
            result["coverage"]["format_workbooks"],
            {"ods": 1, "xls": 1, "xlsb": 1, "xlsx": 1},
        )
        self.assertEqual(result["metrics"]["text_box_match_coverage_ppm"], 1_000_000)
        self.assertEqual(result["coverage"]["libreoffice_pdf_font_objects"], 8)

    def test_pinned_container_identity_and_attestations_pass(self) -> None:
        result = self.evaluate_small(container_report_document())
        self.assertTrue(result["passed"])
        self.assertEqual(result["evidence"]["oracle_build_contract_sha256"], "3" * 64)
        self.assertEqual(
            result["evidence"]["oracle_image_config_digest"],
            "sha256:" + "2" * 64,
        )
        self.assertEqual(result["evidence"]["pdffonts_sha256"], "5" * 64)

    def test_container_identity_is_fail_closed_for_missing_mixed_and_unpinned_rows(self) -> None:
        report = container_report_document()
        del report["files"][0]["oracle_adapter"]
        with self.assertRaisesRegex(MODULE.GateError, "file_oracle_adapter"):
            self.evaluate_small(report)

        report = container_report_document()
        report["files"][0]["oracle_adapter"]["lock_sha256"] = "6" * 64
        with self.assertRaisesRegex(MODULE.GateError, "file_oracle_adapter_identity"):
            self.evaluate_small(report)

        report = container_report_document()
        report["configuration"]["oracle_lock"]["image"][
            "expected_config_digest"
        ] = None
        report["configuration"]["oracle_lock"]["image"][
            "identity_status"
        ] = "runtime_verified"
        with self.assertRaisesRegex(MODULE.GateError, "configuration_container_image"):
            self.evaluate_small(report)

        report = container_report_document()
        report["configuration"]["oracle_lock"]["host_path"] = "/private/oracle"
        with self.assertRaisesRegex(MODULE.GateError, "configuration_container_identity"):
            self.evaluate_small(report)

    def test_font_attestation_and_pdffonts_lock_are_exact(self) -> None:
        report = report_document()
        del report["configuration"]["oracle_lock"]["pdf_rasterizer"][
            "pdffonts_sha256"
        ]
        with self.assertRaisesRegex(MODULE.GateError, "configuration_identity"):
            self.evaluate_small(report)

        report = report_document()
        del report["files"][0]["font_attestation"]
        with self.assertRaisesRegex(MODULE.GateError, "font_attestation"):
            self.evaluate_small(report)

        report = report_document()
        report["files"][0]["font_attestation"]["matched_font_objects"] = 1
        with self.assertRaisesRegex(MODULE.GateError, "font_attestation_incomplete"):
            self.evaluate_small(report)

        report = report_document()
        report["files"][0]["font_attestation"][
            "normalized_identities_sha256"
        ] = "/private/font-name"
        with self.assertRaisesRegex(MODULE.GateError, "font_attestation"):
            self.evaluate_small(report)

    def test_cli_output_is_path_and_content_neutral(self) -> None:
        report = report_document(40)
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "private-report.json"
            path.write_text(json.dumps(report), encoding="utf-8")
            process = subprocess.run(
                [sys.executable, str(SCRIPT), str(path)],
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(process.returncode, 0, process.stderr)
        output = json.loads(process.stdout)
        self.assertTrue(output["passed"])
        self.assertNotIn("secret", process.stdout)
        self.assertNotIn("private/corpus", process.stdout)
        self.assertNotIn(str(path), process.stdout)

    def test_semantic_edge_and_similarity_thresholds_use_raw_page_counts(self) -> None:
        report = report_document()
        for item in report["files"]:
            page = item["pages"][0]
            page["semantic_codepoint_matched_items"] = 998
            page["edge_rxls_matched_1px"] = 960
            page["edge_libreoffice_matched_1px"] = 960
            page["absolute_error_sum"] = 500_000
            synchronize_similarity(item)
        result = self.evaluate_small(report)
        self.assertFalse(result["passed"])
        self.assertIn("semantic_codepoint_precision_below_target", result["failures"])
        self.assertIn("semantic_codepoint_recall_below_target", result["failures"])
        self.assertIn("edge_f1_below_target", result["failures"])
        self.assertIn("core_similarity_below_target", result["failures"])
        self.assertIn("broad_similarity_below_target", result["failures"])

    def test_reported_similarity_cannot_override_raw_evidence(self) -> None:
        report = report_document()
        report["files"][0]["pages"][0]["absolute_error_sum"] = 10
        with self.assertRaisesRegex(MODULE.GateError, "similarity_metric_inconsistent"):
            self.evaluate_small(report)

    def test_text_box_mapping_is_exact_and_fail_closed(self) -> None:
        for field, failure in (
            ("text_box_ambiguous_items", "text_box_mapping_ambiguous"),
            ("text_box_unmatched_items", "text_box_mapping_unmatched"),
        ):
            report = report_document()
            page = report["files"][0]["pages"][0]
            page["text_box_matched_items"] = 2
            page[field] = 1
            page["text_box_match_coverage_ppm"] = 666_667
            page["text_box_error_histogram_millipoints"][0]["count"] = 2
            result = self.evaluate_small(report)
            self.assertIn(failure, result["failures"])
            self.assertIn(
                "text_box_match_coverage_below_target", result["failures"]
            )

    def test_text_box_geometry_thresholds_are_absolute(self) -> None:
        report = report_document()
        for item in report["files"]:
            page = item["pages"][0]
            page["text_box_error_histogram_millipoints"][0][
                "error_millipoints"
            ] = 1_001
            page["text_box_median_error_millipoints"] = 1_001
            page["text_box_p95_error_millipoints"] = 1_001
        result = self.evaluate_small(report)
        self.assertIn("text_box_median_error_above_target", result["failures"])
        self.assertNotIn("text_box_p95_error_above_target", result["failures"])

        for item in report["files"]:
            page = item["pages"][0]
            page["text_box_error_histogram_millipoints"][0][
                "error_millipoints"
            ] = 2_501
            page["text_box_median_error_millipoints"] = 2_501
            page["text_box_p95_error_millipoints"] = 2_501
        result = self.evaluate_small(report)
        self.assertIn("text_box_p95_error_above_target", result["failures"])

    def test_page_geometry_thresholds_are_calibrated_in_points(self) -> None:
        report = report_document()
        for item in report["files"]:
            item["pages"][0]["libreoffice_size"]["width"] = 798
        result = self.evaluate_small(report)
        self.assertEqual(result["metrics"]["page_box_median_millipoints"], 1_500)
        self.assertIn("page_box_median_error_above_target", result["failures"])
        self.assertNotIn("page_box_p95_error_above_target", result["failures"])

        for item in report["files"]:
            item["pages"][0]["libreoffice_size"]["width"] = 793
        result = self.evaluate_small(report)
        self.assertEqual(result["metrics"]["page_box_max_millipoints"], 5_250)
        self.assertIn("page_box_max_error_above_target", result["failures"])

    def test_sheet_page_mapping_requires_contiguous_exact_indices(self) -> None:
        report = report_document()
        item = report["files"][0]
        item["pages"][0]["sheet_index"] = 1
        item["scenes"][0]["sheet_index"] = 1
        result = self.evaluate_small(report)
        self.assertIn("sheet_page_mapping_not_exact", result["failures"])

    def test_xlsb_is_required_and_not_treated_as_an_exclusion(self) -> None:
        report = report_document()
        report["files"][2]["format"] = "xlsx"
        result = self.evaluate_small(report)
        self.assertIn("broad_format_missing:xlsb", result["failures"])
        self.assertNotIn("excluded_formats", result["coverage"])

    def test_metric_policy_and_box_histogram_are_strict(self) -> None:
        report = report_document()
        report["configuration"]["metric_policy"]["text_box_matching"] = "ordered"
        with self.assertRaisesRegex(MODULE.GateError, "metric_policy"):
            self.evaluate_small(report)

        report = report_document()
        page = report["files"][0]["pages"][0]
        page["text_box_p95_error_millipoints"] = 101
        with self.assertRaisesRegex(MODULE.GateError, "quantile_inconsistent"):
            self.evaluate_small(report)

    def test_summary_counts_duplicate_json_and_size_caps_fail_closed(self) -> None:
        report = report_document()
        report["summary"]["by_status"] = {"compared": 3, "error": 1}
        with self.assertRaisesRegex(MODULE.GateError, "summary_status_counts"):
            self.evaluate_small(report)

        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "duplicate.json"
            path.write_text('{"schema":1,"schema":2}', encoding="utf-8")
            with self.assertRaisesRegex(MODULE.GateError, "duplicate_json_key"):
                MODULE._read_report(path)
            with mock.patch.object(MODULE, "MAX_REPORT_BYTES", 4), self.assertRaisesRegex(
                MODULE.GateError, "report_size_limit"
            ):
                MODULE._read_report(path)


if __name__ == "__main__":
    unittest.main()
