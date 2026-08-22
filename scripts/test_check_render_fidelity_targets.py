#!/usr/bin/env python3
"""Unit tests for the absolute LibreOffice rendering-fidelity gate."""

from __future__ import annotations

from collections import Counter
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
GEOMETRY_AXES = (
    "x_min",
    "x_max",
    "y_min",
    "y_max",
    "center_x",
    "center_y",
    "width",
    "height",
)


def unique_geometry(matched: int) -> dict[str, object]:
    histogram = (
        [{"count": matched, "delta_millipoints": 0}]
        if matched
        else []
    )
    summary = {
        "count": matched,
        "max_delta_millipoints": 0 if matched else None,
        "min_delta_millipoints": 0 if matched else None,
        "negative_overflow_items": 0,
        "positive_overflow_items": 0,
        "sum_delta_millipoints": 0,
    }
    return {
        "delta_histograms_millipoints": {
            axis: copy.deepcopy(histogram) for axis in GEOMETRY_AXES
        },
        "exact_delta_summaries_millipoints": {
            axis: copy.deepcopy(summary) for axis in GEOMETRY_AXES
        },
        "libreoffice_unique_items": matched,
        "matched_items": matched,
        "rxls_unique_items": matched,
    }


def point_geometry(
    *,
    rxls_width: str = "600/1",
    libreoffice_width: str = "600/1",
    rxls_box_width: str | None = None,
    libreoffice_box_width: str | None = None,
    rxls_xhtml_width: str | None = None,
    libreoffice_xhtml_width: str | None = None,
) -> dict[str, object]:
    def side(width: str, box_width: str) -> dict[str, object]:
        page_dimensions = {
            "height_points": "450/1",
            "width_points": width,
        }
        box_dimensions = {
            "height_points": "450/1",
            "width_points": box_width,
        }
        return {
            "crop_box": dict(box_dimensions),
            "media_box": dict(box_dimensions),
            "page_size": dict(page_dimensions),
        }

    rxls_page = MODULE._point(rxls_width, "fixture", positive=True)
    libreoffice_page = MODULE._point(
        libreoffice_width, "fixture", positive=True
    )
    rxls_box = MODULE._point(
        rxls_box_width or rxls_width, "fixture", positive=True
    )
    libreoffice_box = MODULE._point(
        libreoffice_box_width or libreoffice_width,
        "fixture",
        positive=True,
    )
    rxls_xhtml = MODULE._point(
        rxls_xhtml_width or rxls_width, "fixture", positive=True
    )
    libreoffice_xhtml = MODULE._point(
        libreoffice_xhtml_width or libreoffice_width,
        "fixture",
        positive=True,
    )

    def point_text(value: MODULE.Fraction) -> str:
        return f"{value.numerator}/{value.denominator}"

    box_delta = point_text(rxls_box - libreoffice_box)
    return {
        "deltas_points": {
            "crop_box_height": "0/1",
            "crop_box_width": box_delta,
            "libreoffice_xhtml_page_size_height": "0/1",
            "libreoffice_xhtml_page_size_width": point_text(
                libreoffice_xhtml - libreoffice_page
            ),
            "media_box_height": "0/1",
            "media_box_width": box_delta,
            "rxls_xhtml_page_size_height": "0/1",
            "rxls_xhtml_page_size_width": point_text(
                rxls_xhtml - rxls_page
            ),
            "xhtml_height": "0/1",
            "xhtml_width": point_text(rxls_xhtml - libreoffice_xhtml),
        },
        "libreoffice": side(
            libreoffice_width,
            libreoffice_box_width or libreoffice_width,
        ),
        "rxls": side(rxls_width, rxls_box_width or rxls_width),
        "xhtml": {
            "libreoffice": {
                "height_points": "450/1",
                "width_points": point_text(libreoffice_xhtml),
            },
            "rxls": {
                "height_points": "450/1",
                "width_points": point_text(rxls_xhtml),
            },
        },
    }


def page_row(
    *,
    sheet_index: int = 0,
    box_error: int = 100,
    box_count: int = 3,
) -> dict[str, object]:
    return {
        "source_sheet_index": sheet_index,
        "source_pdf_page_index": 0,
        "oracle_output_page_index": 0,
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
        "text_box_rxls_items": box_count,
        "text_box_libreoffice_items": box_count,
        "text_box_matched_items": box_count,
        "text_box_ambiguous_items": 0,
        "text_box_unmatched_items": 0,
        "text_box_rxls_unmatched_items": 0,
        "text_box_libreoffice_unmatched_items": 0,
        "text_box_match_coverage_ppm": 1_000_000,
        "text_box_precision_ppm": 1_000_000,
        "text_box_recall_ppm": 1_000_000,
        "text_box_f1_ppm": 1_000_000,
        "text_box_error_histogram_millipoints": [
            {"error_millipoints": box_error, "count": box_count}
        ],
        "text_box_median_error_millipoints": box_error,
        "text_box_p95_error_millipoints": box_error,
        "text_box_unique_geometry": unique_geometry(box_count),
        "text_line_box_candidate_items": 1,
        "text_line_box_rxls_items": 1,
        "text_line_box_libreoffice_items": 1,
        "text_line_box_matched_items": 1,
        "text_line_box_ambiguous_items": 0,
        "text_line_box_unmatched_items": 0,
        "text_line_box_rxls_unmatched_items": 0,
        "text_line_box_libreoffice_unmatched_items": 0,
        "text_line_box_match_coverage_ppm": 1_000_000,
        "text_line_box_precision_ppm": 1_000_000,
        "text_line_box_recall_ppm": 1_000_000,
        "text_line_box_f1_ppm": 1_000_000,
        "text_line_box_error_histogram_millipoints": [
            {"error_millipoints": box_error, "count": 1}
        ],
        "text_line_box_median_error_millipoints": box_error,
        "text_line_box_p95_error_millipoints": box_error,
        "text_line_box_unique_geometry": unique_geometry(1),
        "pdf_point_geometry": point_geometry(),
    }


def file_row(format_name: str, index: int) -> dict[str, object]:
    page = page_row()
    return {
        "path": f"private/corpus/secret-{index}.{format_name}",
        "sha256": f"{index + 1:064x}",
        "format": format_name,
        "status": "compared",
        "classification": "within_threshold",
        "features": ["basic"],
        "metrics": {
            "max_pdf_point_geometry_delta_millipoints": 0,
            "max_pdf_xhtml_crosscheck_delta_micropoints": 0,
            "pdf_point_geometry_mismatches": 0,
            "similarity_ppm": 1_000_000,
        },
        "pages": [page],
        "scenes": [
            {
                "source_sheet_index": 0,
                "source_pdf_page_index": 0,
                "oracle_output_page_index": 0,
            }
        ],
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
        "native_pdf_attestation": {
            "actual_text_documents": 1,
            "documents": 1,
            "embedded_font_objects": 1,
            "font_objects": 1,
            "identity_set_sha256": "6" * 64,
            "subset_font_objects": 1,
            "type0_cff_font_objects": 0,
            "type0_font_objects": 1,
            "type0_truetype_font_objects": 1,
            "type3_font_objects": 0,
            "unicode_font_objects": 1,
        },
    }


def report_document(count: int = 4) -> dict[str, object]:
    formats = MODULE.ORACLE_FORMATS
    files = [file_row(formats[index % len(formats)], index) for index in range(count)]
    if count >= 40:
        for index, features in enumerate(MODULE.HARD_FEATURE_COHORTS.values()):
            target = min(index, 5)
            files[target]["features"] = sorted(
                {
                    *files[target]["features"],
                    sorted(features)[0],
                }
                - {"basic"}
            )
    report = {
        "schema": MODULE.EVIDENCE_SCHEMA,
        "mode": "compare",
        "configuration": {
            "dpi": 96,
            "lane_filter": {"formats": [], "required_features": []},
            "font_pack": {"pack_sha256": "a" * 64},
            "renderer_binary": {"sha256": "b" * 64},
            "measurement_toolchain": {
                "kind": "poppler",
                "pdffonts_sha256": "1" * 64,
                "pdfinfo_sha256": "e" * 64,
                "pdftoppm_sha256": "f" * 64,
                "pdftotext_sha256": "0" * 64,
            },
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
                "contract_schema": MODULE.METRIC_CONTRACT_SCHEMA,
                "contract_version": 2,
                "mask_match_tolerance_pixels": 1,
                "edge_luma_delta": 32,
                "semantic_content_retained": False,
                "semantic_text_source": (
                    "svg_data-rxls-visible-label_vs_pdftotext_layout"
                ),
                "raster_source": (
                    "rxls_native_print_pdf_vs_libreoffice_calc_pdf"
                ),
                "rasterizer": "same_locked_poppler_pdftoppm_both_sides",
                "text_ink_source": "thresholded_common_poppler_rasters",
                "text_box_content_retained": False,
                "text_box_error_units": "millipoints",
                "text_box_source": (
                    "pdftotext_bbox_layout_word_boxes_both_native_pdfs"
                ),
                "text_line_box_source": (
                    "pdftotext_bbox_layout_line_boxes_both_native_pdfs"
                ),
                "text_box_matching": (
                    "exact_normalized_tokens_nearest_unique_one_to_one_same_bbox_level_symmetric_counts"
                ),
                "text_box_geometry": "nominal_poppler_layout_not_ink_bounds",
                "unique_text_geometry": copy.deepcopy(
                    MODULE.UNIQUE_TEXT_GEOMETRY_POLICY
                ),
                "implementation": {
                    "kind": "numpy_integer_exact_v1",
                    "version": "2.3.1",
                },
            },
        },
        "discovery": {
            "candidate_count": count,
            "pre_shard_selected_count": count,
            "selected_count": count,
            "shard_candidate_count": count,
            "shard_count": 1,
            "shard_index": 0,
            "truncated": False,
        },
        "summary": {
            "files": count,
            "by_status": {"compared": count},
            "by_classification": {"within_threshold": count},
        },
        "files": files,
    }
    report["configuration"]["manifest_binding"] = MODULE._mapping_binding(
        files,
        manifest_sha256="7" * 64,
    )
    return report


def container_report_document(count: int = 4) -> dict[str, object]:
    report = report_document(count)
    font_pack_sha256 = report["configuration"]["font_pack"]["pack_sha256"]
    image_id = "sha256:" + "2" * 64
    manifest_digest = "sha256:" + "6" * 64
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
            "expected_manifest_digest": manifest_digest,
            "identity_status": "pinned_match",
            "manifest_digest": manifest_digest,
        },
        "libreoffice": oracle,
        "lock_file_sha256": "4" * 64,
        "pdf_font_inspector": {
            "host_tools_identity_sha256": "8" * 64,
            "kind": "poppler",
            "pdffonts_sha256": "5" * 64,
            "pdfinfo_sha256": report["configuration"]["measurement_toolchain"][
                "pdfinfo_sha256"
            ],
            "pdftoppm_sha256": report["configuration"]["measurement_toolchain"][
                "pdftoppm_sha256"
            ],
            "pdftotext_sha256": report["configuration"]["measurement_toolchain"][
                "pdftotext_sha256"
            ],
        },
        "runtime": "docker",
        "schema": MODULE.CONTAINER_IDENTITY_SCHEMA,
    }
    report["configuration"]["oracle_lock"] = identity
    report["configuration"]["measurement_toolchain"] = copy.deepcopy(
        identity["pdf_font_inspector"]
    )
    adapter = {
        "font_pack_sha256": font_pack_sha256,
        "image": {
            "architecture": "linux/amd64",
            "expected_id": image_id,
            "expected_manifest_digest": manifest_digest,
            "id": image_id,
            "identity_status": "pinned_match",
            "manifest_digest": manifest_digest,
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
            MIN_HARD_FEATURE_WORKBOOKS=0,
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

    def test_only_compared_within_threshold_rows_enter_passing_cohorts(
        self,
    ) -> None:
        mutations = (
            ("different", "below_similarity_threshold"),
            ("compared", "below_similarity_threshold"),
            ("different", "within_threshold"),
        )
        for status, classification in mutations:
            with self.subTest(
                status=status,
                classification=classification,
            ):
                report = report_document()
                report["files"][0]["status"] = status
                report["files"][0]["classification"] = classification
                statuses = Counter(
                    item["status"] for item in report["files"]
                )
                classifications = Counter(
                    item["classification"] for item in report["files"]
                )
                report["summary"]["by_status"] = dict(sorted(statuses.items()))
                report["summary"]["by_classification"] = dict(
                    sorted(classifications.items())
                )

                result = self.evaluate_small(report)

                self.assertFalse(result["passed"])
                self.assertEqual(
                    result["coverage"]["broad_workbooks"],
                    3,
                )
                self.assertIn(
                    "broad_coverage_incomplete",
                    result["failures"],
                )

    def test_summary_classification_counts_are_exact(self) -> None:
        report = report_document()
        report["summary"]["by_classification"] = {
            "below_similarity_threshold": 1,
            "within_threshold": 3,
        }
        with self.assertRaisesRegex(
            MODULE.GateError,
            "summary_classification_counts",
        ):
            self.evaluate_small(report)

    def test_requires_complete_unsharded_discovery(self) -> None:
        mutations = (
            (
                "shard_count",
                lambda value: value.update({"shard_count": 2}),
                "campaign_incomplete",
            ),
            (
                "truncated",
                lambda value: value.update({"truncated": True}),
                "campaign_incomplete",
            ),
            (
                "selected_count",
                lambda value: value.update({"selected_count": 3}),
                "campaign_coverage",
            ),
            (
                "candidate_count",
                lambda value: value.update({"candidate_count": 3}),
                "campaign_coverage",
            ),
            (
                "bool_shard_count",
                lambda value: value.update({"shard_count": True}),
                "campaign_incomplete",
            ),
        )
        for name, mutate, code in mutations:
            with self.subTest(name=name):
                report = report_document()
                mutate(report["discovery"])
                with self.assertRaisesRegex(MODULE.GateError, code):
                    self.evaluate_small(report)

        report = report_document()
        del report["discovery"]["shard_index"]
        with self.assertRaisesRegex(MODULE.GateError, "discovery_shape"):
            self.evaluate_small(report)

    def test_metric_policy_rejects_bool_integer_alias(self) -> None:
        report = report_document()
        report["configuration"]["metric_policy"][
            "mask_match_tolerance_pixels"
        ] = True
        with self.assertRaisesRegex(MODULE.GateError, "metric_policy"):
            self.evaluate_small(report)

    def test_pinned_container_identity_and_attestations_pass(self) -> None:
        result = self.evaluate_small(container_report_document())
        self.assertTrue(result["passed"])
        self.assertEqual(result["evidence"]["oracle_build_contract_sha256"], "3" * 64)
        self.assertEqual(
            result["evidence"]["oracle_image_config_digest"],
            "sha256:" + "2" * 64,
        )
        self.assertEqual(
            result["evidence"]["oracle_image_manifest_digest"],
            "sha256:" + "6" * 64,
        )
        self.assertEqual(result["evidence"]["pdffonts_sha256"], "5" * 64)
        self.assertEqual(result["evidence"]["host_tools_identity_sha256"], "8" * 64)

    def test_every_host_tool_and_closure_identity_is_cross_bound(self) -> None:
        for key in (
            "host_tools_identity_sha256",
            "pdffonts_sha256",
            "pdfinfo_sha256",
            "pdftoppm_sha256",
            "pdftotext_sha256",
        ):
            with self.subTest(key=key):
                report = container_report_document()
                report["configuration"]["measurement_toolchain"][key] = "9" * 64
                with self.assertRaisesRegex(
                    MODULE.GateError,
                    "configuration_measurement_toolchain",
                ):
                    self.evaluate_small(report)

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
        del report["configuration"]["oracle_lock"]["image"]["manifest_digest"]
        with self.assertRaisesRegex(
            MODULE.GateError, "configuration_container_image"
        ):
            self.evaluate_small(report)

        report = container_report_document()
        report["configuration"]["oracle_lock"]["image"][
            "expected_manifest_digest"
        ] = "sha256:" + "7" * 64
        with self.assertRaisesRegex(
            MODULE.GateError, "configuration_container_image"
        ):
            self.evaluate_small(report)

        report = container_report_document()
        report["files"][0]["oracle_adapter"]["image"]["manifest_digest"] = (
            "sha256:" + "7" * 64
        )
        report["files"][0]["oracle_adapter"]["image"][
            "expected_manifest_digest"
        ] = "sha256:" + "7" * 64
        with self.assertRaisesRegex(MODULE.GateError, "file_oracle_adapter_identity"):
            self.evaluate_small(report)

        report = container_report_document()
        report["files"][0]["oracle_adapter"][
            "schema"
        ] = "rxls.render-oracle-container-execution.v2"
        with self.assertRaisesRegex(MODULE.GateError, "file_oracle_adapter_identity"):
            self.evaluate_small(report)

        report = container_report_document()
        report["configuration"]["oracle_lock"][
            "schema"
        ] = "rxls.render-oracle-container-identity.v1"
        with self.assertRaisesRegex(
            MODULE.GateError, "configuration_container_identity"
        ):
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

    def test_native_pdf_attestation_schema_and_path_counts_are_exact(self) -> None:
        report = report_document()
        report["files"][0]["native_pdf_attestation"][
            "type0_truetype_font_objects"
        ] = 0
        with self.assertRaisesRegex(MODULE.GateError, "native_pdf_attestation"):
            self.evaluate_small(report)

        report = report_document()
        report["files"][0]["native_pdf_attestation"]["unreviewed"] = 0
        with self.assertRaisesRegex(MODULE.GateError, "native_pdf_attestation"):
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
        for field, aliases, failure in (
            ("text_box_ambiguous_items", (), "text_box_mapping_ambiguous"),
            (
                "text_box_rxls_unmatched_items",
                ("text_box_unmatched_items",),
                "text_box_mapping_unmatched",
            ),
        ):
            report = report_document()
            page = report["files"][0]["pages"][0]
            page["text_box_matched_items"] = 2
            page[field] = 1
            for alias in aliases:
                page[alias] = 1
            page["text_box_match_coverage_ppm"] = 666_667
            page["text_box_precision_ppm"] = 666_667
            page["text_box_recall_ppm"] = 666_667
            page["text_box_f1_ppm"] = 666_667
            page["text_box_libreoffice_unmatched_items"] = 1
            page["text_box_error_histogram_millipoints"][0]["count"] = 2
            page["text_box_unique_geometry"] = unique_geometry(2)
            result = self.evaluate_small(report)
            self.assertIn(failure, result["failures"])
            self.assertIn(
                "text_box_match_coverage_below_target", result["failures"]
            )

    def test_extra_reference_text_boxes_fail_symmetric_absolute_gates(self) -> None:
        report = report_document()
        page = report["files"][0]["pages"][0]
        page["text_box_libreoffice_items"] = 4
        page["text_box_libreoffice_unmatched_items"] = 1
        page["text_box_recall_ppm"] = 750_000
        page["text_box_f1_ppm"] = 857_143
        result = self.evaluate_small(report)
        self.assertEqual(result["metrics"]["text_box_precision_ppm"], 1_000_000)
        self.assertIn("text_box_recall_below_target", result["failures"])
        self.assertIn("text_box_f1_below_target", result["failures"])
        self.assertIn("text_box_reference_unmatched", result["failures"])

    def test_extra_reference_line_boxes_fail_symmetric_absolute_gates(self) -> None:
        report = report_document()
        page = report["files"][0]["pages"][0]
        page["text_line_box_libreoffice_items"] = 2
        page["text_line_box_libreoffice_unmatched_items"] = 1
        page["text_line_box_recall_ppm"] = 500_000
        page["text_line_box_f1_ppm"] = 666_667
        result = self.evaluate_small(report)
        self.assertIn("text_line_box_recall_below_target", result["failures"])
        self.assertIn("text_line_box_f1_below_target", result["failures"])
        self.assertIn("text_line_box_reference_unmatched", result["failures"])

    def test_empty_semantic_and_edge_workbook_is_explicitly_rejected(self) -> None:
        report = report_document()
        page = report["files"][0]["pages"][0]
        for key in (
            "semantic_codepoint_rxls_items",
            "semantic_codepoint_libreoffice_items",
            "semantic_codepoint_matched_items",
            "edge_rxls_pixels",
            "edge_libreoffice_pixels",
            "edge_rxls_matched_1px",
            "edge_libreoffice_matched_1px",
        ):
            page[key] = 0
        result = self.evaluate_small(report)
        self.assertIn("semantic_population_empty", result["failures"])
        self.assertIn("edge_population_empty", result["failures"])

    def test_hard_feature_cohort_has_its_own_absolute_gates(self) -> None:
        report = report_document()
        report["files"][0]["features"] = ["chart"]
        report["files"][0]["pages"][0][
            "semantic_codepoint_matched_items"
        ] = 998
        report["configuration"]["manifest_binding"] = MODULE._mapping_binding(
            report["files"],
            manifest_sha256=report["configuration"]["manifest_binding"][
                "manifest_sha256"
            ],
        )
        result = self.evaluate_small(report)
        self.assertIn(
            "hard_feature_semantic_precision_below_target:chart",
            result["failures"],
        )
        self.assertIn(
            "hard_feature_semantic_recall_below_target:chart",
            result["failures"],
        )
        self.assertEqual(result["coverage"]["hard_feature_workbooks"]["chart"], 1)

    def test_self_consistent_feature_substitution_fails_expected_manifest_binding(
        self,
    ) -> None:
        report = report_document()
        expected = copy.deepcopy(report["configuration"]["manifest_binding"])
        report["files"][0]["features"] = ["wrapped-text"]
        report["configuration"]["manifest_binding"] = MODULE._mapping_binding(
            report["files"],
            manifest_sha256=expected["manifest_sha256"],
        )
        with self.assertRaisesRegex(MODULE.GateError, "manifest_binding"):
            with mock.patch.multiple(
                MODULE,
                MIN_BROAD_WORKBOOKS=4,
                MIN_CORE_WORKBOOKS=3,
                MIN_CORE_TEXT_BOXES=4,
                MIN_HARD_FEATURE_WORKBOOKS=0,
            ):
                MODULE.evaluate(
                    report,
                    "c" * 64,
                    1234,
                    expected_manifest_binding=expected,
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
            item["pages"][0]["pdf_point_geometry"] = point_geometry(
                libreoffice_width="1197/2"
            )
            item["metrics"]["pdf_point_geometry_mismatches"] = 1
            item["metrics"]["max_pdf_point_geometry_delta_millipoints"] = 1_500
            item["metrics"][
                "max_pdf_xhtml_crosscheck_delta_micropoints"
            ] = 1_500_000
        result = self.evaluate_small(report)
        self.assertEqual(result["metrics"]["page_box_median_millipoints"], 1_500)
        self.assertIn("page_box_median_error_above_target", result["failures"])
        self.assertNotIn("page_box_p95_error_above_target", result["failures"])

        for item in report["files"]:
            item["pages"][0]["pdf_point_geometry"] = point_geometry(
                libreoffice_width="2379/4"
            )
            item["metrics"]["max_pdf_point_geometry_delta_millipoints"] = 5_250
            item["metrics"][
                "max_pdf_xhtml_crosscheck_delta_micropoints"
            ] = 5_250_000
        result = self.evaluate_small(report)
        self.assertEqual(result["metrics"]["page_box_max_millipoints"], 5_250)
        self.assertIn("page_box_max_error_above_target", result["failures"])

    def test_imported_page_box_quantization_bound_is_inclusive(self) -> None:
        report = report_document()
        item = report["files"][0]
        item["pages"][0]["pdf_point_geometry"] = point_geometry(
            libreoffice_box_width="119997/200"
        )
        item["metrics"]["max_pdf_point_geometry_delta_millipoints"] = 15
        item["metrics"][
            "max_pdf_xhtml_crosscheck_delta_micropoints"
        ] = 0
        result = self.evaluate_small(report)
        self.assertNotIn("pdf_point_geometry_mismatch", result["failures"])
        self.assertNotIn("raster_page_box_mismatch", result["failures"])
        self.assertEqual(
            result["thresholds"][
                "pdf_imported_page_box_quantization_max_micropoints"
            ],
            15_000,
        )

        item["pages"][0]["pdf_point_geometry"] = point_geometry(
            libreoffice_box_width="599984999/1000000"
        )
        item["metrics"]["pdf_point_geometry_mismatches"] = 1
        item["metrics"]["max_pdf_point_geometry_delta_millipoints"] = 16
        result = self.evaluate_small(report)
        self.assertIn("pdf_point_geometry_mismatch", result["failures"])
        self.assertNotIn("raster_page_box_mismatch", result["failures"])

    def test_page_point_geometry_rejects_inconsistent_and_malformed_deltas(
        self,
    ) -> None:
        report = report_document()
        point = report["files"][0]["pages"][0]["pdf_point_geometry"]
        point["deltas_points"]["media_box_width"] = "1/1000000"
        with self.assertRaisesRegex(
            MODULE.GateError, "page_point_geometry_delta"
        ):
            self.evaluate_small(report)

        report = report_document()
        point = report["files"][0]["pages"][0]["pdf_point_geometry"]
        del point["deltas_points"]["crop_box_height"]
        with self.assertRaisesRegex(MODULE.GateError, "page_point_geometry"):
            self.evaluate_small(report)

    def test_poppler_xhtml_precision_crosscheck_is_bounded_not_zeroed(self) -> None:
        report = report_document()
        for item in report["files"]:
            point = item["pages"][0]["pdf_point_geometry"]
            for side in ("rxls", "libreoffice"):
                point["xhtml"][side]["width_points"] = "5999997/10000"
                point["deltas_points"][
                    f"{side}_xhtml_page_size_width"
                ] = "-3/10000"
            item["metrics"][
                "max_pdf_xhtml_crosscheck_delta_micropoints"
            ] = 300
        result = self.evaluate_small(report)
        self.assertNotIn("pdf_point_geometry_mismatch", result["failures"])
        self.assertEqual(
            result["metrics"]["pdf_xhtml_crosscheck_max_micropoints"],
            300,
        )

    def test_cross_document_xhtml_delta_uses_bounded_crosscheck_gate(self) -> None:
        report = report_document()
        for item in report["files"]:
            item["pages"][0]["pdf_point_geometry"] = point_geometry(
                rxls_xhtml_width="600000365/1000000"
            )
            item["metrics"][
                "max_pdf_xhtml_crosscheck_delta_micropoints"
            ] = 365
        result = self.evaluate_small(report)
        self.assertNotIn("pdf_point_geometry_mismatch", result["failures"])
        self.assertNotIn(
            "pdf_xhtml_crosscheck_above_tolerance",
            result["failures"],
        )
        self.assertEqual(
            result["metrics"]["pdf_point_geometry_mismatches"], 0
        )

        for item in report["files"]:
            item["pages"][0]["pdf_point_geometry"] = point_geometry(
                rxls_xhtml_width="600001001/1000000"
            )
            item["metrics"][
                "max_pdf_xhtml_crosscheck_delta_micropoints"
            ] = 1_001
        result = self.evaluate_small(report)
        self.assertNotIn("pdf_point_geometry_mismatch", result["failures"])
        self.assertIn(
            "pdf_xhtml_crosscheck_above_tolerance",
            result["failures"],
        )
        self.assertEqual(
            result["metrics"]["pdf_point_geometry_mismatches"], 0
        )

    def test_sheet_page_mapping_requires_contiguous_exact_indices(self) -> None:
        report = report_document()
        item = report["files"][0]
        item["pages"][0]["oracle_output_page_index"] = 1
        result = self.evaluate_small(report)
        self.assertIn("sheet_page_mapping_not_exact", result["failures"])

    def test_xlsb_is_required_and_not_treated_as_an_exclusion(self) -> None:
        report = report_document()
        report["files"][2]["format"] = "xlsx"
        report["configuration"]["manifest_binding"] = MODULE._mapping_binding(
            report["files"],
            manifest_sha256=report["configuration"]["manifest_binding"][
                "manifest_sha256"
            ],
        )
        result = self.evaluate_small(report)
        self.assertIn("broad_format_missing:xlsb", result["failures"])
        self.assertNotIn("excluded_formats", result["coverage"])

    def test_metric_policy_and_box_histogram_are_strict(self) -> None:
        report = report_document()
        report["configuration"]["metric_policy"]["text_box_matching"] = "ordered"
        with self.assertRaisesRegex(MODULE.GateError, "metric_policy"):
            self.evaluate_small(report)

        report = report_document()
        geometry_policy = report["configuration"]["metric_policy"][
            "unique_text_geometry"
        ]
        self.assertEqual(
            geometry_policy["exact_delta_absolute_limit_millipoints"],
            1_000_000_000,
        )
        self.assertEqual(
            geometry_policy["max_items_per_side_per_page"],
            250_000,
        )
        self.assertEqual(
            geometry_policy["max_geometry_pages_per_report"],
            2_000,
        )
        self.assertEqual(
            geometry_policy["max_histogram_buckets_per_report"],
            50_000,
        )
        self.assertEqual(
            geometry_policy["histogram"],
            {
                "exact_absolute_limit_millipoints": 2,
                "max_buckets_per_axis": 21,
                "middle_absolute_limit_millipoints": 1_000,
                "middle_bucket_width_millipoints": 500,
                "outer_absolute_limit_millipoints": 10_000,
                "outer_bucket_width_millipoints": 2_000,
                "overflow_bucket_absolute_millipoints": 12_000,
                "rounding": (
                    "nearest_width_multiple_half_away_from_zero_"
                    "with_nonzero_sign_preserved"
                ),
            },
        )
        report["configuration"]["metric_policy"]["unique_text_geometry"][
            "histogram"
        ]["rounding"] = "nearest_width_multiple"
        with self.assertRaisesRegex(MODULE.GateError, "metric_policy"):
            self.evaluate_small(report)

        report = report_document()
        report["configuration"]["metric_policy"]["unique_text_geometry"][
            "max_geometry_pages_per_report"
        ] = 2_001
        with self.assertRaisesRegex(MODULE.GateError, "metric_policy"):
            self.evaluate_small(report)

        report = report_document()
        report["configuration"]["measurement_toolchain"]["pdftoppm_sha256"] = (
            "9" * 64
        )
        with self.assertRaisesRegex(
            MODULE.GateError,
            "configuration_measurement_toolchain",
        ):
            self.evaluate_small(report)

        report = report_document()
        page = report["files"][0]["pages"][0]
        page["text_box_p95_error_millipoints"] = 101
        with self.assertRaisesRegex(MODULE.GateError, "quantile_inconsistent"):
            self.evaluate_small(report)

    def test_unique_text_geometry_is_required_and_exact(self) -> None:
        report = report_document()
        del report["files"][0]["pages"][0]["text_box_unique_geometry"]
        with self.assertRaisesRegex(
            MODULE.GateError, "unique_text_geometry_pair"
        ):
            self.evaluate_small(report)

        for kind in ("exact_summary", "cross_axis"):
            with self.subTest(kind=kind):
                report = report_document()
                geometry = report["files"][0]["pages"][0][
                    "text_box_unique_geometry"
                ]
                if kind == "exact_summary":
                    geometry["exact_delta_summaries_millipoints"]["x_min"][
                        "count"
                    ] = 2
                else:
                    geometry["delta_histograms_millipoints"]["center_x"][0][
                        "delta_millipoints"
                    ] = 1
                    summary = geometry[
                        "exact_delta_summaries_millipoints"
                    ]["center_x"]
                    summary["min_delta_millipoints"] = 1
                    summary["max_delta_millipoints"] = 1
                    summary["sum_delta_millipoints"] = 3
                with self.assertRaisesRegex(
                    MODULE.GateError, "unique_text_geometry_page"
                ):
                    self.evaluate_small(report)

    def test_unique_text_geometry_report_caps_are_enforced(self) -> None:
        contract = MODULE.validate_report_geometry.__globals__["CONTRACT"]
        with self.subTest(limit="pages"), mock.patch.object(
            contract,
            "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES",
            3,
        ):
            with self.assertRaisesRegex(
                MODULE.GateError, "unique_text_geometry_report_limit"
            ):
                self.evaluate_small(report_document())

        with self.subTest(limit="histogram_buckets"), mock.patch.object(
            contract,
            "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS",
            63,
        ):
            with self.assertRaisesRegex(
                MODULE.GateError, "unique_text_geometry_report_limit"
            ):
                self.evaluate_small(report_document())

    def test_unique_text_geometry_policy_rejects_bool_integer_alias(self) -> None:
        report = report_document()
        report["configuration"]["metric_policy"]["unique_text_geometry"][
            "diagnostic_only"
        ] = 1
        with self.assertRaisesRegex(MODULE.GateError, "metric_policy"):
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

    def test_report_reader_is_bounded_regular_and_race_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            report = root / "report.json"
            report.write_bytes(b"{}")
            link = root / "report-link.json"
            with self.subTest(case="symlink"):
                try:
                    link.symlink_to(report)
                except OSError as error:
                    if getattr(error, "winerror", None) == 1314:
                        self.skipTest("symlink creation requires Windows privilege")
                    raise
                with self.assertRaisesRegex(
                    MODULE.GateError, "report_unreadable"
                ):
                    MODULE._read_report(link)
            with self.assertRaisesRegex(MODULE.GateError, "report_unreadable"):
                MODULE._read_report(root)
            fifo = root / "report.fifo"
            real_open = MODULE.os.open
            with self.subTest(case="fifo"):
                if hasattr(MODULE.os, "mkfifo"):
                    MODULE.os.mkfifo(fifo)
                    nonblocking = MODULE.os.O_NONBLOCK

                    def guarded_open(
                        path: object, flags: int, *args: object, **kwargs: object
                    ) -> int:
                        self.assertNotEqual(flags & nonblocking, 0)
                        return real_open(path, flags, *args, **kwargs)

                    with mock.patch.object(
                        MODULE.os, "open", side_effect=guarded_open
                    ), self.assertRaisesRegex(
                        MODULE.GateError, "report_unreadable"
                    ):
                        MODULE._read_report(fifo)
                else:
                    fifo.write_bytes(b"{}")
                    fifo_metadata = mock.Mock(st_mode=MODULE.stat.S_IFIFO)
                    with mock.patch.object(
                        MODULE.os, "fstat", return_value=fifo_metadata
                    ), self.assertRaisesRegex(
                        MODULE.GateError, "report_unreadable"
                    ):
                        MODULE._read_report(fifo)

            report.write_bytes(b"0123456789")
            real_read = MODULE.os.read
            returned = 0

            def observed_read(descriptor: int, count: int) -> bytes:
                nonlocal returned
                chunk = real_read(descriptor, count)
                returned += len(chunk)
                return chunk

            with mock.patch.object(
                MODULE, "MAX_REPORT_BYTES", 4
            ), mock.patch.object(
                MODULE.os, "read", side_effect=observed_read
            ), self.assertRaisesRegex(
                MODULE.GateError, "report_size_limit"
            ):
                MODULE._read_report(report)
            self.assertEqual(returned, 5)

            for mutation in ("growth", "swap"):
                with self.subTest(mutation=mutation):
                    report.write_bytes(b"{}")
                    replacement = root / "replacement.json"
                    replacement.write_bytes(b"{}")
                    changed = False

                    def adversarial_read(
                        descriptor: int, count: int
                    ) -> bytes:
                        nonlocal changed
                        chunk = real_read(descriptor, count)
                        if chunk and not changed:
                            changed = True
                            if mutation == "growth":
                                report.write_bytes(b"{} ")
                            else:
                                replacement.replace(report)
                        return chunk

                    with mock.patch.object(
                        MODULE.os,
                        "read",
                        side_effect=adversarial_read,
                    ), self.assertRaisesRegex(
                        MODULE.GateError, "report_unreadable"
                    ):
                        MODULE._read_report(report)

    def test_report_reader_strict_json_is_bounded_before_decode(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "report.json"
            preflight_payloads = (
                (
                    b"[" * (MODULE.MAX_JSON_DEPTH + 1)
                    + b"]" * (MODULE.MAX_JSON_DEPTH + 1)
                ),
                b'{"value":1.25}',
                b'{"value":1e10000}',
                b'{"value":' + b"9" * 5_000 + b"}",
            )
            for payload in preflight_payloads:
                with self.subTest(payload_size=len(payload)):
                    path.write_bytes(payload)
                    with mock.patch.object(
                        MODULE.json,
                        "loads",
                        side_effect=AssertionError("decoder must not run"),
                    ), self.assertRaisesRegex(
                        MODULE.GateError, "report_invalid_json"
                    ):
                        MODULE._read_report(path)

            with mock.patch.object(
                MODULE, "MAX_JSON_NODES", 3
            ), mock.patch.object(
                MODULE.json,
                "loads",
                side_effect=AssertionError("decoder must not run"),
            ), self.assertRaisesRegex(
                MODULE.GateError, "report_invalid_json"
            ):
                path.write_bytes(b"[0,0,0,0]")
                MODULE._read_report(path)

            path.write_bytes(b'{"value":NaN}')
            with self.assertRaisesRegex(
                MODULE.GateError, "report_invalid_json"
            ):
                MODULE._read_report(path)

            path.write_bytes(b"{}")
            with mock.patch.object(
                MODULE.json, "loads", side_effect=RecursionError
            ), self.assertRaisesRegex(
                MODULE.GateError, "report_invalid_json"
            ):
                MODULE._read_report(path)

            hostile = (
                b"[" * (MODULE.MAX_JSON_DEPTH + 1)
                + b"]" * (MODULE.MAX_JSON_DEPTH + 1)
            )
            path.write_bytes(hostile)
            process = subprocess.run(
                [sys.executable, str(SCRIPT), str(path)],
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(process.returncode, 2)
            self.assertIn("report_invalid_json", process.stderr)
            self.assertNotIn("Traceback", process.stderr)


if __name__ == "__main__":
    unittest.main()
