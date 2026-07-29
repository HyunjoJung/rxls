#!/usr/bin/env python3
"""Tests for the aggregate authored-print parity gate."""

from __future__ import annotations

import copy
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


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
    rxls_width: str = "612/1",
    libreoffice_width: str = "612/1",
) -> dict[str, object]:
    def side(width: str) -> dict[str, object]:
        dimensions = {"height_points": "792/1", "width_points": width}
        return {
            "crop_box": dict(dimensions),
            "media_box": dict(dimensions),
            "page_size": dict(dimensions),
        }

    delta = MODULE._point(
        rxls_width, "fixture", positive=True
    ) - MODULE._point(libreoffice_width, "fixture", positive=True)
    return {
        "deltas_points": {
            "crop_box_height": "0/1",
            "crop_box_width": f"{delta.numerator}/{delta.denominator}",
            "libreoffice_xhtml_page_size_height": "0/1",
            "libreoffice_xhtml_page_size_width": "0/1",
            "media_box_height": "0/1",
            "media_box_width": f"{delta.numerator}/{delta.denominator}",
            "rxls_xhtml_page_size_height": "0/1",
            "rxls_xhtml_page_size_width": "0/1",
            "xhtml_height": "0/1",
            "xhtml_width": f"{delta.numerator}/{delta.denominator}",
        },
        "libreoffice": side(libreoffice_width),
        "rxls": side(rxls_width),
        "xhtml": {
            "libreoffice": {
                "height_points": "792/1",
                "width_points": libreoffice_width,
            },
            "rxls": {
                "height_points": "792/1",
                "width_points": rxls_width,
            },
        },
    }


def report_document() -> dict[str, object]:
    image_id = "sha256:" + "1" * 64
    manifest_digest = "sha256:" + "4" * 64
    identity = {
        "build_contract_sha256": "2" * 64,
        "font_pack_sha256": "3" * 64,
        "image": {
            "architecture": "linux/amd64",
            "config_digest": image_id,
            "expected_config_digest": image_id,
            "expected_manifest_digest": manifest_digest,
            "identity_status": "pinned_match",
            "manifest_digest": manifest_digest,
        },
        "libreoffice": {
            "artifact_sha256": MODULE.CONTAINER_LIBREOFFICE_ARTIFACT_SHA256,
            "name": "LibreOffice",
            "version": "26.2.3.2",
        },
        "lock_file_sha256": "5" * 64,
        "pdf_font_inspector": {
            "host_tools_identity_sha256": "d" * 64,
            "kind": "poppler",
            "pdffonts_sha256": "6" * 64,
            "pdfinfo_sha256": "a" * 64,
            "pdftoppm_sha256": "b" * 64,
            "pdftotext_sha256": "c" * 64,
        },
        "runtime": "docker",
        "schema": MODULE.CONTAINER_IDENTITY_SCHEMA,
    }
    adapter = {
        "font_pack_sha256": "3" * 64,
        "image": {
            "architecture": "linux/amd64",
            "expected_id": image_id,
            "expected_manifest_digest": manifest_digest,
            "id": image_id,
            "identity_status": "pinned_match",
            "manifest_digest": manifest_digest,
        },
        "lock_file_sha256": "5" * 64,
        "lock_sha256": "2" * 64,
        "oracle": identity["libreoffice"],
        "runtime": "docker",
        "schema": MODULE.CONTAINER_EXECUTION_SCHEMA,
    }
    files = []
    for workbook_index, scale_mode in enumerate(("scale", "fit")):
        page_count = MODULE.EXPECTED_PAGES_BY_SCALE_MODE[scale_mode]
        pages = [
            {
                "source_sheet_index": 0,
                "source_pdf_page_index": index,
                "oracle_output_page_index": index,
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
                "text_box_rxls_items": 2,
                "text_box_libreoffice_items": 2,
                "text_box_matched_items": 2,
                "text_box_ambiguous_items": 0,
                "text_box_unmatched_items": 0,
                "text_box_rxls_unmatched_items": 0,
                "text_box_libreoffice_unmatched_items": 0,
                "text_box_error_histogram_millipoints": [
                    {"error_millipoints": 0, "count": 2}
                ],
                "text_box_match_coverage_ppm": 1_000_000,
                "text_box_precision_ppm": 1_000_000,
                "text_box_recall_ppm": 1_000_000,
                "text_box_f1_ppm": 1_000_000,
                "text_box_median_error_millipoints": 0,
                "text_box_p95_error_millipoints": 0,
                "text_box_unique_geometry": unique_geometry(2),
                "text_line_box_candidate_items": 1,
                "text_line_box_rxls_items": 1,
                "text_line_box_libreoffice_items": 1,
                "text_line_box_matched_items": 1,
                "text_line_box_ambiguous_items": 0,
                "text_line_box_unmatched_items": 0,
                "text_line_box_rxls_unmatched_items": 0,
                "text_line_box_libreoffice_unmatched_items": 0,
                "text_line_box_error_histogram_millipoints": [
                    {"error_millipoints": 0, "count": 1}
                ],
                "text_line_box_match_coverage_ppm": 1_000_000,
                "text_line_box_precision_ppm": 1_000_000,
                "text_line_box_recall_ppm": 1_000_000,
                "text_line_box_f1_ppm": 1_000_000,
                "text_line_box_median_error_millipoints": 0,
                "text_line_box_p95_error_millipoints": 0,
                "text_line_box_unique_geometry": unique_geometry(1),
                "pdf_point_geometry": point_geometry(),
            }
            for index in range(page_count)
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
                "artifacts": {
                    "rxls_pages": page_count,
                    "libreoffice_pages": page_count,
                },
                "metrics": {
                    "max_pdf_point_geometry_delta_millipoints": 0,
                    "max_pdf_xhtml_crosscheck_delta_micropoints": 0,
                    "pages": page_count,
                    "pdf_point_geometry_mismatches": 0,
                    "similarity_ppm": 1_000_000,
                },
                "pages": pages,
                "scenes": [
                    {
                        "source_sheet_index": 0,
                        "source_pdf_page_index": index,
                        "oracle_output_page_index": index,
                    }
                    for index in range(page_count)
                ],
                "font_attestation": {
                    "font_objects": 2,
                    "embedded_font_objects": 2,
                    "matched_font_objects": 2,
                    "subset_font_objects": 2,
                    "unicode_font_objects": 2,
                    "normalized_identities_sha256": "8" * 64,
                    "unique_font_identities": 1,
                },
                "native_pdf_attestation": {
                    "actual_text_documents": 1,
                    "charprocs_documents": 1,
                    "documents": 1,
                    "embedded_font_objects": 1,
                    "font_objects": 1,
                    "identity_set_sha256": "f" * 64,
                    "subset_font_objects": 1,
                    "type3_documents": 1,
                    "type3_font_objects": 1,
                    "unicode_font_objects": 1,
                },
                "oracle_adapter": copy.deepcopy(adapter),
            }
        )
    report = {
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
                    "version": "2.4.2",
                },
            },
            "measurement_toolchain": {
                "host_tools_identity_sha256": "d" * 64,
                "kind": "poppler",
                "pdffonts_sha256": "6" * 64,
                "pdfinfo_sha256": "a" * 64,
                "pdftoppm_sha256": "b" * 64,
                "pdftotext_sha256": "c" * 64,
            },
            "renderer_binary": {"sha256": "9" * 64},
            "font_pack": {"pack_sha256": "3" * 64},
            "oracle_lock": identity,
        },
        "discovery": {
            "candidate_count": 2,
            "pre_shard_selected_count": 2,
            "selected_count": 2,
            "shard_candidate_count": 2,
            "shard_count": 1,
            "shard_index": 0,
            "truncated": False,
        },
        "summary": {
            "files": 2,
            "by_status": {"compared": 2},
            "by_classification": {"within_threshold": 2},
        },
        "files": files,
    }
    report["configuration"]["manifest_binding"] = MODULE._mapping_binding(
        files,
        manifest_sha256="e" * 64,
    )
    return report


class AuthoredPrintGateTests(unittest.TestCase):
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
                lambda value: value.update({"selected_count": 1}),
                "campaign_coverage",
            ),
            (
                "candidate_count",
                lambda value: value.update({"candidate_count": 1}),
                "campaign_coverage",
            ),
            (
                "bool_shard_index",
                lambda value: value.update({"shard_index": False}),
                "campaign_incomplete",
            ),
        )
        for name, mutate, code in mutations:
            with self.subTest(name=name):
                report = report_document()
                mutate(report["discovery"])
                with self.assertRaisesRegex(MODULE.GateError, code):
                    MODULE.evaluate(
                        report,
                        report_sha256="a" * 64,
                        report_bytes=1234,
                        expected_workbooks=2,
                    )

        report = report_document()
        del report["discovery"]["pre_shard_selected_count"]
        with self.assertRaisesRegex(MODULE.GateError, "discovery_shape"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

    def test_exact_integer_contracts_reject_bool_aliases(self) -> None:
        report = report_document()
        report["configuration"]["metric_policy"][
            "mask_match_tolerance_pixels"
        ] = True
        with self.assertRaisesRegex(MODULE.GateError, "metric_policy"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

        report = report_document()
        report["files"][0]["authored_print"]["paper_code"] = True
        with self.assertRaisesRegex(MODULE.GateError, "source_attestation"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

    def test_report_reader_is_bounded_regular_and_race_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            report = root / "report.json"
            report.write_bytes(b"{}")
            link = root / "report-link.json"
            link.symlink_to(report)
            with self.assertRaisesRegex(MODULE.GateError, "report_unreadable"):
                MODULE._read(link)
            with self.assertRaisesRegex(MODULE.GateError, "report_unreadable"):
                MODULE._read(root)
            fifo = root / "report.fifo"
            MODULE.os.mkfifo(fifo)
            real_open = MODULE.os.open
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
                MODULE._read(fifo)

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
                MODULE.GateError, "report_size"
            ):
                MODULE._read(report)
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
                        MODULE._read(report)

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
                        MODULE.GateError, "report_json"
                    ):
                        MODULE._read(path)

            with mock.patch.object(
                MODULE, "MAX_JSON_NODES", 3
            ), mock.patch.object(
                MODULE.json,
                "loads",
                side_effect=AssertionError("decoder must not run"),
            ), self.assertRaisesRegex(
                MODULE.GateError, "report_json"
            ):
                path.write_bytes(b"[0,0,0,0]")
                MODULE._read(path)

            for payload, code in (
                (b'{"value":NaN}', "report_json"),
                (b'{"value":1,"value":2}', "duplicate_json_key"),
            ):
                with self.subTest(code=code):
                    path.write_bytes(payload)
                    with self.assertRaisesRegex(MODULE.GateError, code):
                        MODULE._read(path)

            path.write_bytes(b"{}")
            with mock.patch.object(
                MODULE.json, "loads", side_effect=RecursionError
            ), self.assertRaisesRegex(MODULE.GateError, "report_json"):
                MODULE._read(path)

            hostile = (
                b"[" * (MODULE.MAX_JSON_DEPTH + 1)
                + b"]" * (MODULE.MAX_JSON_DEPTH + 1)
            )
            path.write_bytes(hostile)
            process = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    str(path),
                    "--expected-workbooks",
                    "2",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(process.returncode, 2)
            self.assertIn("report_json", process.stderr)
            self.assertNotIn("Traceback", process.stderr)

    def test_unique_text_geometry_policy_drift_is_rejected(self) -> None:
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
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

        report = report_document()
        report["configuration"]["metric_policy"]["unique_text_geometry"][
            "max_histogram_buckets_per_report"
        ] = 50_001
        with self.assertRaisesRegex(MODULE.GateError, "metric_policy"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

    def test_exact_page_count_boxes_and_both_scale_modes_pass(self) -> None:
        result = MODULE.evaluate(
            report_document(),
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertTrue(result["passed"])
        self.assertEqual(result["schema"], "rxls.authored-print-parity.v2")
        self.assertEqual(result["coverage"]["by_scale_mode"], {"fit": 1, "scale": 1})
        self.assertEqual(
            result["coverage"]["page_count_histogram"],
            {"1": 1, "4": 1},
        )
        self.assertEqual(result["coverage"]["pages"], 5)
        self.assertEqual(
            result["expected"]["pages_per_workbook_by_scale_mode"],
            {"fit": 1, "scale": 4},
        )
        self.assertEqual(result["metrics"]["similarity_mean_ppm"], 1_000_000)
        self.assertEqual(result["metrics"]["text_box_match_coverage_ppm"], 1_000_000)
        self.assertEqual(
            result["evidence"]["oracle_image_manifest_digest"],
            "sha256:" + "4" * 64,
        )
        self.assertEqual(
            result["thresholds"],
            {
                "edge_f1_min_ppm": 970_000,
                "page_box_max_millipoints": 5_000,
                "page_box_median_max_millipoints": 1_000,
                "page_box_p95_max_millipoints": 2_500,
                "pdf_point_geometry_exact": True,
                "pdf_xhtml_crosscheck_max_micropoints": 1_000,
                "semantic_codepoint_precision_min_ppm": 999_000,
                "semantic_codepoint_recall_min_ppm": 999_000,
                "similarity_mean_min_ppm": 950_000,
                "text_box_match_min_ppm": 999_000,
                "text_box_median_max_millipoints": 1_000,
                "text_box_p95_max_millipoints": 2_500,
            },
        )
        self.assertNotIn("path", result["evidence"])

    def test_page_counts_are_bound_to_each_scale_mode(self) -> None:
        report = report_document()
        report["files"][0]["authored_print"]["scale_mode"] = "fit"
        report["files"][1]["authored_print"]["scale_mode"] = "scale"
        result = MODULE.evaluate(
            report,
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertFalse(result["passed"])
        self.assertIn("page_count_mismatch", result["failures"])
        self.assertNotIn("scale_fit_coverage_incomplete", result["failures"])

    def test_page_count_and_calibrated_page_box_thresholds_fail(self) -> None:
        report = report_document()
        report["files"][0]["artifacts"]["libreoffice_pages"] = 3
        report["files"][0]["pages"][0]["pdf_point_geometry"] = point_geometry(
            libreoffice_width="606/1"
        )
        report["files"][0]["metrics"][
            "pdf_point_geometry_mismatches"
        ] = 1
        report["files"][0]["metrics"][
            "max_pdf_point_geometry_delta_millipoints"
        ] = 6_000
        result = MODULE.evaluate(
            report,
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertFalse(result["passed"])
        self.assertIn("page_count_mismatch", result["failures"])
        self.assertIn("page_box_max_above_target", result["failures"])

    def test_hidden_source_sheet_gap_with_authored_multipage_output_passes(self) -> None:
        report = report_document()
        for item in report["files"]:
            for page, scene in zip(item["pages"], item["scenes"], strict=True):
                page["source_sheet_index"] = 1
                scene["source_sheet_index"] = 1
        result = MODULE.evaluate(
            report,
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertTrue(result["passed"])

    def test_sub_raster_pdf_point_delta_fails_exact_geometry_gate(self) -> None:
        report = report_document()
        page = report["files"][0]["pages"][0]
        page["pdf_point_geometry"] = point_geometry(
            libreoffice_width="6119/10"
        )
        report["files"][0]["metrics"]["pdf_point_geometry_mismatches"] = 1
        report["files"][0]["metrics"][
            "max_pdf_point_geometry_delta_millipoints"
        ] = 100
        result = MODULE.evaluate(
            report,
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertFalse(result["passed"])
        self.assertIn("pdf_point_geometry_mismatch", result["failures"])
        self.assertNotIn("raster_page_box_mismatch", result["failures"])

    def test_correct_page_count_cannot_hide_bad_visual_or_text_placement(self) -> None:
        report = report_document()
        page = report["files"][0]["pages"][0]
        page["absolute_error_sum"] = page["pixels"] * 3 * 255
        page["edge_rxls_matched_1px"] = 0
        page["edge_libreoffice_matched_1px"] = 0
        page["semantic_codepoint_matched_items"] = 0
        page["text_box_matched_items"] = 0
        page["text_box_unmatched_items"] = 2
        page["text_box_rxls_unmatched_items"] = 2
        page["text_box_libreoffice_unmatched_items"] = 2
        page["text_box_error_histogram_millipoints"] = []
        page["text_box_match_coverage_ppm"] = 0
        page["text_box_precision_ppm"] = 0
        page["text_box_recall_ppm"] = 0
        page["text_box_f1_ppm"] = 0
        page["text_box_median_error_millipoints"] = None
        page["text_box_p95_error_millipoints"] = None
        page["text_box_unique_geometry"] = unique_geometry(0)
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

    def test_extra_reference_text_boxes_cannot_be_ignored(self) -> None:
        report = report_document()
        page = report["files"][0]["pages"][0]
        page["text_box_libreoffice_items"] = 3
        page["text_box_libreoffice_unmatched_items"] = 1
        page["text_box_recall_ppm"] = 666_667
        page["text_box_f1_ppm"] = 800_000
        result = MODULE.evaluate(
            report,
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertFalse(result["passed"])
        self.assertEqual(result["metrics"]["text_box_precision_ppm"], 1_000_000)
        self.assertIn("text_box_recall_below_target", result["failures"])
        self.assertIn("text_box_f1_below_target", result["failures"])
        self.assertIn("text_box_reference_unmatched", result["failures"])

    def test_extra_reference_line_boxes_cannot_be_ignored(self) -> None:
        report = report_document()
        page = report["files"][0]["pages"][0]
        page["text_line_box_libreoffice_items"] = 2
        page["text_line_box_libreoffice_unmatched_items"] = 1
        page["text_line_box_recall_ppm"] = 500_000
        page["text_line_box_f1_ppm"] = 666_667
        result = MODULE.evaluate(
            report,
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertIn("text_line_box_recall_below_target", result["failures"])
        self.assertIn("text_line_box_f1_below_target", result["failures"])
        self.assertIn("text_line_box_reference_unmatched", result["failures"])

    def test_empty_semantic_and_edge_populations_are_not_perfect_evidence(self) -> None:
        report = report_document()
        for page in report["files"][0]["pages"]:
            page["semantic_codepoint_rxls_items"] = 0
            page["semantic_codepoint_libreoffice_items"] = 0
            page["semantic_codepoint_matched_items"] = 0
            page["edge_rxls_pixels"] = 0
            page["edge_libreoffice_pixels"] = 0
            page["edge_rxls_matched_1px"] = 0
            page["edge_libreoffice_matched_1px"] = 0
        result = MODULE.evaluate(
            report,
            report_sha256="a" * 64,
            report_bytes=1234,
            expected_workbooks=2,
        )
        self.assertEqual(result["metrics"]["edge_f1_ppm"], 1_000_000)
        self.assertEqual(
            result["metrics"]["semantic_codepoint_precision_ppm"],
            1_000_000,
        )
        self.assertIn("semantic_population_empty", result["failures"])
        self.assertIn("edge_population_empty", result["failures"])

    def test_unique_text_geometry_is_required_and_exact(self) -> None:
        report = report_document()
        del report["files"][0]["pages"][0]["text_box_unique_geometry"]
        with self.assertRaisesRegex(
            MODULE.GateError, "unique_text_geometry_pair"
        ):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

        for kind in ("exact_summary", "cross_axis"):
            with self.subTest(kind=kind):
                report = report_document()
                geometry = report["files"][0]["pages"][0][
                    "text_box_unique_geometry"
                ]
                if kind == "exact_summary":
                    geometry["exact_delta_summaries_millipoints"]["x_min"][
                        "count"
                    ] = 1
                else:
                    geometry["delta_histograms_millipoints"]["center_x"][0][
                        "delta_millipoints"
                    ] = 1
                    summary = geometry[
                        "exact_delta_summaries_millipoints"
                    ]["center_x"]
                    summary["min_delta_millipoints"] = 1
                    summary["max_delta_millipoints"] = 1
                    summary["sum_delta_millipoints"] = 2
                with self.assertRaisesRegex(
                    MODULE.GateError, "unique_text_geometry_page"
                ):
                    MODULE.evaluate(
                        report,
                        report_sha256="a" * 64,
                        report_bytes=1234,
                        expected_workbooks=2,
                    )

    def test_unique_text_geometry_report_caps_are_enforced(self) -> None:
        contract = MODULE.validate_report_geometry.__globals__["CONTRACT"]
        with self.subTest(limit="pages"), mock.patch.object(
            contract,
            "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES",
            4,
        ):
            with self.assertRaisesRegex(
                MODULE.GateError, "unique_text_geometry_report_limit"
            ):
                MODULE.evaluate(
                    report_document(),
                    report_sha256="a" * 64,
                    report_bytes=1234,
                    expected_workbooks=2,
                )

        with self.subTest(limit="histogram_buckets"), mock.patch.object(
            contract,
            "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS",
            79,
        ):
            with self.assertRaisesRegex(
                MODULE.GateError, "unique_text_geometry_report_limit"
            ):
                MODULE.evaluate(
                    report_document(),
                    report_sha256="a" * 64,
                    report_bytes=1234,
                    expected_workbooks=2,
                )

    def test_unique_text_geometry_policy_rejects_bool_integer_alias(self) -> None:
        report = report_document()
        report["configuration"]["metric_policy"]["unique_text_geometry"][
            "diagnostic_only"
        ] = 1
        with self.assertRaisesRegex(MODULE.GateError, "metric_policy"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

    def test_self_consistent_input_substitution_is_rejected_by_manifest_binding(
        self,
    ) -> None:
        report = report_document()
        expected = copy.deepcopy(report["configuration"]["manifest_binding"])
        report["files"][0]["sha256"] = "f" * 64
        report["configuration"]["manifest_binding"] = MODULE._mapping_binding(
            report["files"],
            manifest_sha256=expected["manifest_sha256"],
        )
        with self.assertRaisesRegex(MODULE.GateError, "manifest_binding"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
                expected_manifest_binding=expected,
            )

    def test_unpinned_container_and_incomplete_source_attestation_are_rejected(self) -> None:
        for key in (
            "host_tools_identity_sha256",
            "pdffonts_sha256",
            "pdfinfo_sha256",
            "pdftoppm_sha256",
            "pdftotext_sha256",
        ):
            with self.subTest(key=key):
                report = report_document()
                report["configuration"]["measurement_toolchain"][key] = "0" * 64
                with self.assertRaisesRegex(
                    MODULE.GateError,
                    "measurement_toolchain",
                ):
                    MODULE.evaluate(
                        report,
                        report_sha256="a" * 64,
                        report_bytes=1234,
                        expected_workbooks=2,
                    )

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
        del report["configuration"]["oracle_lock"]["image"]["manifest_digest"]
        with self.assertRaisesRegex(MODULE.GateError, "oracle_image"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

        report = report_document()
        report["configuration"]["oracle_lock"]["image"][
            "expected_manifest_digest"
        ] = "sha256:" + "0" * 64
        with self.assertRaisesRegex(MODULE.GateError, "oracle_image"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

        report = report_document()
        report["files"][0]["oracle_adapter"]["image"]["manifest_digest"] = (
            "sha256:" + "0" * 64
        )
        report["files"][0]["oracle_adapter"]["image"][
            "expected_manifest_digest"
        ] = "sha256:" + "0" * 64
        with self.assertRaisesRegex(MODULE.GateError, "oracle_adapter"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

        report = report_document()
        report["files"][0]["oracle_adapter"][
            "schema"
        ] = "rxls.render-oracle-container-execution.v2"
        with self.assertRaisesRegex(MODULE.GateError, "oracle_adapter"):
            MODULE.evaluate(
                report,
                report_sha256="a" * 64,
                report_bytes=1234,
                expected_workbooks=2,
            )

        report = report_document()
        report["configuration"]["oracle_lock"][
            "schema"
        ] = "rxls.render-oracle-container-identity.v1"
        with self.assertRaisesRegex(MODULE.GateError, "oracle_identity"):
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
