#!/usr/bin/env python3
"""Tests for the aggregate authored-print parity gate."""

from __future__ import annotations

import copy
import importlib.util
from pathlib import Path
import sys
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "check-authored-print-parity.py"


def load_module():
    spec = importlib.util.spec_from_file_location("check_authored_print_parity", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def report_document() -> dict[str, object]:
    image_id = "sha256:" + "1" * 64
    identity = {
        "build_contract_sha256": "2" * 64,
        "font_pack_sha256": "3" * 64,
        "image": {
            "architecture": "linux/amd64",
            "config_digest": image_id,
            "expected_config_digest": image_id,
            "identity_status": "pinned_match",
        },
        "libreoffice": {
            "artifact_sha256": MODULE.CONTAINER_LIBREOFFICE_ARTIFACT_SHA256,
            "name": "LibreOffice",
            "version": "26.2.3.2",
        },
        "lock_file_sha256": "5" * 64,
        "pdf_font_inspector": {"kind": "poppler", "pdffonts_sha256": "6" * 64},
        "runtime": "docker",
        "schema": MODULE.CONTAINER_IDENTITY_SCHEMA,
    }
    adapter = {
        "font_pack_sha256": "3" * 64,
        "image": {
            "architecture": "linux/amd64",
            "expected_id": image_id,
            "id": image_id,
            "identity_status": "pinned_match",
        },
        "lock_file_sha256": "5" * 64,
        "lock_sha256": "2" * 64,
        "oracle": identity["libreoffice"],
        "runtime": "docker",
        "schema": MODULE.CONTAINER_EXECUTION_SCHEMA,
    }
    files = []
    for workbook_index, scale_mode in enumerate(("scale", "fit")):
        pages = [
            {
                "sheet_index": index,
                "rxls_size": {"width": 816, "height": 1056},
                "libreoffice_size": {"width": 816, "height": 1056},
                "pixels": 816 * 1056,
                "absolute_error_sum": 0,
                "edge_rxls_pixels": 100,
                "edge_libreoffice_pixels": 100,
                "edge_rxls_matched_1px": 100,
                "edge_libreoffice_matched_1px": 100,
                "semantic_codepoint_rxls_items": 10,
                "semantic_codepoint_libreoffice_items": 10,
                "semantic_codepoint_matched_items": 10,
                "text_box_candidate_items": 2,
                "text_box_matched_items": 2,
                "text_box_ambiguous_items": 0,
                "text_box_unmatched_items": 0,
                "text_box_error_histogram_millipoints": [
                    {"error_millipoints": 0, "count": 2}
                ],
                "text_box_match_coverage_ppm": 1_000_000,
                "text_box_median_error_millipoints": 0,
                "text_box_p95_error_millipoints": 0,
            }
            for index in range(4)
        ]
        files.append(
            {
                "sha256": str(workbook_index + 7) * 64,
                "format": "xlsx",
                "features": ["print-settings"],
                "status": "compared",
                "classification": "within_threshold",
                "authored_print": {
                    "expected_page_height_pixels": 1056,
                    "expected_page_width_pixels": 816,
                    "header_footer": True,
                    "manual_col_breaks": 1,
                    "manual_row_breaks": 1,
                    "margins": True,
                    "paper_code": 1,
                    "print_area": True,
                    "repeated_cols": True,
                    "repeated_rows": True,
                    "scale_mode": scale_mode,
                },
                "artifacts": {"rxls_pages": 4, "libreoffice_pages": 4},
                "metrics": {"pages": 4, "similarity_ppm": 1_000_000},
                "pages": pages,
                "scenes": [{"sheet_index": index} for index in range(4)],
                "font_attestation": {
                    "font_objects": 2,
                    "embedded_font_objects": 2,
                    "matched_font_objects": 2,
                    "subset_font_objects": 2,
                    "unicode_font_objects": 2,
                    "normalized_identities_sha256": "8" * 64,
                },
                "oracle_adapter": copy.deepcopy(adapter),
            }
        )
    return {
        "schema": MODULE.EVIDENCE_SCHEMA,
        "mode": "compare",
        "configuration": {
            "dpi": 96,
            "print_mode": "authored",
            "lane_filter": {
                "formats": ["xlsx"],
                "required_features": ["print-settings"],
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
                    "exact_svg_data-rxls-visible-label_nearest_unique_"
                    "pdftotext_bbox_layout"
                ),
                "implementation": {
                    "kind": "numpy_integer_exact_v1",
                    "version": "2.4.2",
                },
            },
            "renderer_binary": {"sha256": "9" * 64},
            "font_pack": {"pack_sha256": "3" * 64},
            "oracle_lock": identity,
        },
        "summary": {
            "files": 2,
            "by_status": {"compared": 2},
            "by_classification": {"within_threshold": 2},
        },
        "files": files,
    }


class AuthoredPrintGateTests(unittest.TestCase):
    def test_exact_page_count_boxes_and_both_scale_modes_pass(self) -> None:
        result = MODULE.evaluate(
            report_document(),
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertTrue(result["passed"])
        self.assertEqual(result["coverage"]["by_scale_mode"], {"fit": 1, "scale": 1})
        self.assertEqual(result["coverage"]["page_count_histogram"], {"4": 2})
        self.assertEqual(result["metrics"]["similarity_mean_ppm"], 1_000_000)
        self.assertEqual(result["metrics"]["text_box_match_coverage_ppm"], 1_000_000)
        self.assertEqual(
            result["thresholds"],
            {
                "edge_f1_min_ppm": 970_000,
                "page_box_max_millipoints": 5_000,
                "page_box_median_max_millipoints": 1_000,
                "page_box_p95_max_millipoints": 2_500,
                "semantic_codepoint_precision_min_ppm": 999_000,
                "semantic_codepoint_recall_min_ppm": 999_000,
                "similarity_mean_min_ppm": 950_000,
                "text_box_match_min_ppm": 999_000,
                "text_box_median_max_millipoints": 1_000,
                "text_box_p95_max_millipoints": 2_500,
            },
        )
        self.assertNotIn("path", result["evidence"])

    def test_page_count_and_calibrated_page_box_thresholds_fail(self) -> None:
        report = report_document()
        report["files"][0]["artifacts"]["libreoffice_pages"] = 3
        report["files"][0]["pages"][0]["libreoffice_size"]["width"] = 826
        result = MODULE.evaluate(
            report,
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertFalse(result["passed"])
        self.assertIn("page_count_mismatch", result["failures"])
        self.assertIn("page_box_max_above_target", result["failures"])

    def test_correct_page_count_cannot_hide_bad_visual_or_text_placement(self) -> None:
        report = report_document()
        page = report["files"][0]["pages"][0]
        page["absolute_error_sum"] = page["pixels"] * 3 * 255
        page["edge_rxls_matched_1px"] = 0
        page["edge_libreoffice_matched_1px"] = 0
        page["semantic_codepoint_matched_items"] = 0
        page["text_box_matched_items"] = 0
        page["text_box_unmatched_items"] = 2
        page["text_box_error_histogram_millipoints"] = []
        page["text_box_match_coverage_ppm"] = 0
        page["text_box_median_error_millipoints"] = None
        page["text_box_p95_error_millipoints"] = None
        pixels = sum(item["pixels"] for item in report["files"][0]["pages"])
        absolute = sum(
            item["absolute_error_sum"] for item in report["files"][0]["pages"]
        )
        report["files"][0]["metrics"]["similarity_ppm"] = (
            1_000_000
            - MODULE._ratio_ppm(absolute, pixels * 3 * 255)
        )
        result = MODULE.evaluate(
            report,
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertFalse(result["passed"])
        self.assertIn("similarity_mean_below_target", result["failures"])
        self.assertIn("edge_f1_below_target", result["failures"])
        self.assertIn(
            "semantic_codepoint_precision_below_target", result["failures"]
        )
        self.assertIn(
            "semantic_codepoint_recall_below_target", result["failures"]
        )
        self.assertIn("text_box_match_coverage_below_target", result["failures"])
        self.assertIn("text_box_mapping_unmatched", result["failures"])

    def test_unpinned_container_and_incomplete_source_attestation_are_rejected(self) -> None:
        report = report_document()
        report["configuration"]["oracle_lock"]["image"]["expected_config_digest"] = None
        with self.assertRaisesRegex(MODULE.GateError, "oracle_image"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

        report = report_document()
        report["files"][0]["authored_print"]["header_footer"] = False
        with self.assertRaisesRegex(MODULE.GateError, "source_attestation"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )


if __name__ == "__main__":
    unittest.main()
