#!/usr/bin/env python3
"""Tests for the path-private LibreOffice repeatability gate."""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "compare-render-parity-runs.py"
BASELINE_SCRIPT = ROOT / "scripts" / "check-render-parity-baseline.py"


def load_module():
    spec = importlib.util.spec_from_file_location("compare_render_parity_runs", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def load_baseline_module():
    spec = importlib.util.spec_from_file_location(
        "check_render_parity_baseline_contract", BASELINE_SCRIPT
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


BASELINE_MODULE = load_baseline_module()


def renderer_metrics(seed: int) -> dict[str, object]:
    bbox = {
        "bottom": 20 + seed,
        "left": 2,
        "present": 1,
        "right": 40 + seed,
        "top": 3,
    }
    return {
        "edge_rxls_pixels": 100 + seed,
        "foreground_rxls_bbox": bbox,
        "foreground_rxls_centroid_x_millipixels": 20_000 + seed,
        "foreground_rxls_centroid_y_millipixels": 11_000 + seed,
        "foreground_rxls_pixels": 80 + seed,
        "foreground_rxls_x_sum": 1_600 + seed,
        "foreground_rxls_y_sum": 880 + seed,
        "text_ink_rxls_bbox": bbox,
        "text_ink_rxls_centroid_x_millipixels": 21_000 + seed,
        "text_ink_rxls_centroid_y_millipixels": 12_000 + seed,
        "text_ink_rxls_pixels": 60 + seed,
        "text_ink_rxls_x_sum": 1_260 + seed,
        "text_ink_rxls_y_sum": 720 + seed,
    }


def semantic_metrics(seed: int) -> dict[str, int]:
    return {
        "semantic_codepoint_f1_ppm": 990_000 - seed,
        "semantic_codepoint_libreoffice_items": 100 + seed,
        "semantic_codepoint_matched_items": 99 + seed,
        "semantic_codepoint_rxls_items": 100 + seed,
        "semantic_comparable": 1,
        "semantic_exact": 0,
        "semantic_one_sided_empty": 0,
        "semantic_token_f1_ppm": 980_000 - seed,
        "semantic_token_libreoffice_items": 10 + seed,
        "semantic_token_matched_items": 9 + seed,
        "semantic_token_rxls_items": 10 + seed,
    }


def unique_text_geometry(delta_millipoints: int = 0) -> dict[str, object]:
    bucket = MODULE._unique_text_geometry_bucket(delta_millipoints)
    overflow_limit = MODULE.UNIQUE_TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS
    summary = {
        "count": 1,
        "max_delta_millipoints": delta_millipoints,
        "min_delta_millipoints": delta_millipoints,
        "negative_overflow_items": int(delta_millipoints < -overflow_limit),
        "positive_overflow_items": int(delta_millipoints > overflow_limit),
        "sum_delta_millipoints": delta_millipoints,
    }
    return {
        "delta_histograms_millipoints": {
            axis: [{"count": 1, "delta_millipoints": bucket}]
            for axis in MODULE.UNIQUE_TEXT_GEOMETRY_AXES
        },
        "exact_delta_summaries_millipoints": {
            axis: copy.deepcopy(summary)
            for axis in MODULE.UNIQUE_TEXT_GEOMETRY_AXES
        },
        "libreoffice_unique_items": 1,
        "matched_items": 1,
        "rxls_unique_items": 1,
    }


def page_metrics(index: int) -> dict[str, object]:
    return {
        "blurred_luma_similarity_ppm": 920_000 - index,
        "canvas_size": {"height": 100 + index, "width": 200 + index},
        "edge_f1_ppm": 800_000 - index,
        "foreground_f1_ppm": 780_000 - index,
        "libreoffice_size": {"height": 98 + index, "width": 201 + index},
        "metric_work_units": 2_560_000 + index,
        "pixels": 20_000 + index,
        "rxls_size": {"height": 100 + index, "width": 200 + index},
        "source_sheet_index": index,
        "source_pdf_page_index": 0,
        "oracle_output_page_index": 0,
        "similarity_ppm": 900_000 - index,
        "text_box_libreoffice_items": 1,
        "text_box_matched_items": 1,
        "text_box_rxls_items": 1,
        "text_box_unique_geometry": unique_text_geometry(),
        "text_ink_f1_ppm": 760_000 - index,
        "text_line_box_libreoffice_items": 1,
        "text_line_box_matched_items": 1,
        "text_line_box_rxls_items": 1,
        "text_line_box_unique_geometry": unique_text_geometry(),
        **renderer_metrics(index),
        **semantic_metrics(index),
    }


def aggregate_metrics(index: int) -> dict[str, object]:
    return {
        "blurred_luma_similarity_ppm": 925_000 - index,
        "edge_f1_ppm": 805_000 - index,
        "foreground_f1_ppm": 785_000 - index,
        "max_page_height_delta_pixels": 2,
        "max_page_width_delta_pixels": 1,
        "metric_work_units": 2_560_000 + index,
        "page_dimension_mismatches": 1,
        "pages": 1,
        "pixels": 20_000 + index,
        "similarity_ppm": 905_000 - index,
        "stacked_canvas_size": {"height": 100 + index, "width": 200 + index},
        "text_ink_f1_ppm": 765_000 - index,
        **renderer_metrics(index),
        **semantic_metrics(index),
    }


def file_row(index: int, *, private_prefix: str = "/private/baseline") -> dict[str, object]:
    return {
        "artifacts": {"libreoffice_pages": 1, "rxls_pages": 1},
        "bytes": 1_000 + index,
        "classification": "within_threshold",
        "commands": {
            "libreoffice": {"returncode": 0, "status": "ok"},
            "rxls": {"returncode": 0, "status": "ok"},
        },
        "features": ["unicode-text"],
        "format": "xlsx" if index == 0 else "ods",
        "metrics": aggregate_metrics(index),
        "pages": [page_metrics(index)],
        "path": f"{private_prefix}/workbook-{index}.xlsx",
        "raster_commands": [{"returncode": 0, "status": "ok"}],
        "renderer": {
            "fixed_units_per_pixel": 1024,
            "font_pack_sha256": "f" * 64,
            "name": "rxls-render",
            "version": "0.1.0",
        },
        "rights_tier": "S",
        "scenes": [
            {
                "oracle_output_page_index": 0,
                "sha256": f"{index + 100:064x}",
                "source_pdf_page_index": 0,
                "source_sheet_index": index,
                "warnings": [],
            }
        ],
        "semantic_command": {"returncode": 0, "status": "ok"},
        "sha256": f"{index + 1:064x}",
        "status": "compared",
    }


def report(*, private_prefix: str = "/private/baseline") -> dict[str, object]:
    identity = {"bytes": 4_273_408, "sha256": "a" * 64}
    rows = [file_row(0, private_prefix=private_prefix), file_row(1, private_prefix=private_prefix)]
    return {
        "configuration": {
            "dpi": 96,
            "metric_policy": {
                "unique_text_geometry": copy.deepcopy(
                    MODULE.UNIQUE_TEXT_GEOMETRY_POLICY
                )
            },
            "print_mode": "single-page-sheets",
            "renderer_binary": identity,
            "secret_configuration_path": "/never/publish/configuration",
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
        "files": rows,
        "mode": "compare",
        "preflight": {
            "oracle_lock": {"configured": True},
            "rxls_command": {
                "binary_identity": identity,
                "tokens": ["/never/publish/rxls-render"],
            },
        },
        "schema": MODULE.INPUT_SCHEMA,
        "summary": {
            "authored_print": None,
            "by_classification": {"within_threshold": 2},
            "by_status": {"compared": 2},
            "files": 2,
            "input_bytes_considered": 2_001,
            "metric_cohorts": MODULE._recompute_metric_cohorts(rows),
        },
    }


def validated(document: dict[str, object]):
    payload = MODULE.canonical_bytes(document)
    loaded = MODULE.LoadedReport(
        document=document,
        bytes=len(payload),
        sha256=hashlib.sha256(payload).hexdigest(),
    )
    return MODULE.validate_report(loaded)


class CompareRenderParityRunsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.baseline = report()
        self.candidate = report(private_prefix="/different/host/candidate")
        self.candidate["files"].reverse()

    def compare(self, baseline=None, candidate=None, **thresholds):
        baseline = copy.deepcopy(baseline or self.baseline)
        candidate = copy.deepcopy(candidate or self.candidate)
        for document in (baseline, candidate):
            document["summary"]["metric_cohorts"] = (
                MODULE._recompute_metric_cohorts(document["files"])
            )
        return MODULE.compare_reports(
            validated(baseline),
            validated(candidate),
            **thresholds,
        )

    def test_clean_profile_calibrated_drift_passes_with_raw_distributions(self) -> None:
        candidate = copy.deepcopy(self.candidate)
        by_sha = {row["sha256"]: row for row in candidate["files"]}
        changed = by_sha[f"{1:064x}"]
        for metrics in (changed["metrics"], changed["pages"][0]):
            metrics["similarity_ppm"] -= 11_447
            metrics["blurred_luma_similarity_ppm"] -= 11_368
            metrics["foreground_f1_ppm"] -= 16_828
        result = self.compare(candidate=candidate)
        self.assertEqual(result["status"], "pass")
        self.assertEqual(result["failures"], [])
        self.assertEqual(result["thresholds_ppm"]["mask_f1_max_absolute_drift"], 20_000)
        self.assertEqual(result["drift"]["similarity"]["max_absolute_delta_ppm"], 11_447)
        self.assertEqual(
            result["drift"]["mask_f1"]["max_absolute_delta_ppm"], 16_828
        )
        self.assertEqual(result["coverage"], {
            "pages": 2,
            "visual_observations_per_metric": 4,
            "workbooks": 2,
        })
        self.assertEqual(
            result["drift"]["similarity"]["absolute_deltas_ppm"],
            [0, 0, 11_447, 11_447],
        )

    def test_configuration_preflight_and_renderer_binary_identity_are_exact(self) -> None:
        candidate = copy.deepcopy(self.candidate)
        candidate["configuration"]["dpi"] = 144
        result = self.compare(candidate=candidate)
        self.assertIn("configuration_mismatch", result["failures"])

        candidate = copy.deepcopy(self.candidate)
        replacement = {"bytes": 4_273_409, "sha256": "b" * 64}
        candidate["configuration"]["renderer_binary"] = replacement
        candidate["preflight"]["rxls_command"]["binary_identity"] = replacement
        result = self.compare(candidate=candidate)
        self.assertIn("configuration_mismatch", result["failures"])
        self.assertIn("preflight_mismatch", result["failures"])
        self.assertIn("renderer_binary_mismatch", result["failures"])

    def test_type_aliases_cannot_claim_canonical_identity_equality(self) -> None:
        baseline = copy.deepcopy(self.baseline)
        candidate = copy.deepcopy(self.candidate)
        baseline["configuration"]["identity_alias_probe"] = False
        candidate["configuration"]["identity_alias_probe"] = 0
        result = self.compare(baseline=baseline, candidate=candidate)
        configuration = result["identity"]["configuration"]
        self.assertNotEqual(
            configuration["baseline_sha256"],
            configuration["candidate_sha256"],
        )
        self.assertFalse(configuration["equal"])
        self.assertEqual(result["status"], "fail")
        self.assertIn("configuration_mismatch", result["failures"])

        candidate = copy.deepcopy(self.candidate)
        candidate["preflight"]["oracle_lock"]["configured"] = 1
        result = self.compare(candidate=candidate)
        preflight = result["identity"]["preflight"]
        self.assertNotEqual(
            preflight["baseline_sha256"],
            preflight["candidate_sha256"],
        )
        self.assertFalse(preflight["equal"])
        self.assertEqual(result["status"], "fail")
        self.assertIn("preflight_mismatch", result["failures"])

        baseline = copy.deepcopy(self.baseline)
        candidate = copy.deepcopy(self.candidate)
        baseline["files"][0]["renderer"]["identity_alias_probe"] = False
        candidate_by_sha = {
            row["sha256"]: row for row in candidate["files"]
        }
        candidate_by_sha[f"{1:064x}"]["renderer"][
            "identity_alias_probe"
        ] = 0
        result = self.compare(baseline=baseline, candidate=candidate)
        self.assertEqual(result["status"], "fail")
        self.assertIn("renderer_evidence_mismatch", result["failures"])

    def test_summary_evidence_is_bound_without_collapsing_visual_thresholds(
        self,
    ) -> None:
        candidate = copy.deepcopy(self.candidate)
        candidate["summary"]["input_bytes_considered"] += 1
        result = self.compare(candidate=candidate)
        self.assertEqual(result["status"], "fail")
        self.assertIn(
            "summary_input_bytes_mismatch",
            result["failures"],
        )

        tampered = copy.deepcopy(self.baseline)
        tampered["summary"]["metric_cohorts"] = {"tampered": True}
        with self.assertRaisesRegex(
            MODULE.MalformedReport,
            "summary_metric_cohorts",
        ):
            validated(tampered)

        candidate = copy.deepcopy(self.candidate)
        by_sha = {row["sha256"]: row for row in candidate["files"]}
        changed = by_sha[f"{1:064x}"]
        for metrics in (changed["metrics"], changed["pages"][0]):
            metrics["similarity_ppm"] -= 1
        result = self.compare(candidate=candidate)
        self.assertEqual(result["status"], "pass")
        self.assertEqual(result["failures"], [])

    def test_unique_text_geometry_policy_is_exact_and_fail_closed(self) -> None:
        missing = copy.deepcopy(self.baseline)
        del missing["configuration"]["metric_policy"]["unique_text_geometry"]
        with self.assertRaisesRegex(
            MODULE.MalformedReport, "metric_policy_unique_text_geometry"
        ):
            validated(missing)

        drifted = copy.deepcopy(self.baseline)
        drifted["configuration"]["metric_policy"]["unique_text_geometry"][
            "histogram"
        ]["exact_absolute_limit_millipoints"] = 3
        with self.assertRaisesRegex(
            MODULE.MalformedReport, "metric_policy_unique_text_geometry"
        ):
            validated(drifted)

        report_limit_drift = copy.deepcopy(self.baseline)
        report_limit_drift["configuration"]["metric_policy"][
            "unique_text_geometry"
        ]["max_histogram_buckets_per_report"] += 1
        with self.assertRaisesRegex(
            MODULE.MalformedReport, "metric_policy_unique_text_geometry"
        ):
            validated(report_limit_drift)
        self.assertEqual(
            MODULE.UNIQUE_TEXT_GEOMETRY_POLICY["shard_budget"],
            "equal_floor_partition_by_declared_shard_count",
        )

    def test_unique_text_geometry_report_budget_is_aggregate(self) -> None:
        with (
            mock.patch.object(
                MODULE,
                "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES",
                1,
            ),
            self.assertRaisesRegex(
                MODULE.MalformedReport,
                "unique_text_geometry_report_limit",
            ),
        ):
            validated(copy.deepcopy(self.baseline))

        with (
            mock.patch.object(
                MODULE,
                "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS",
                31,
            ),
            self.assertRaisesRegex(
                MODULE.MalformedReport,
                "unique_text_geometry_report_limit",
            ),
        ):
            validated(copy.deepcopy(self.baseline))

    def test_every_compared_page_requires_paired_unique_text_geometry(self) -> None:
        for missing_keys in (
            ("text_box_unique_geometry",),
            ("text_line_box_unique_geometry",),
            (
                "text_box_unique_geometry",
                "text_line_box_unique_geometry",
            ),
        ):
            document = copy.deepcopy(self.baseline)
            page = document["files"][0]["pages"][0]
            for key in missing_keys:
                del page[key]
            with self.subTest(missing_keys=missing_keys), self.assertRaisesRegex(
                MODULE.MalformedReport, "page_unique_text_geometry_pair"
            ):
                validated(document)

    def test_unique_text_geometry_validates_exact_bounded_page_contract(self) -> None:
        document = copy.deepcopy(self.baseline)
        geometry = document["files"][0]["pages"][0][
            "text_box_unique_geometry"
        ]
        allowed = sorted(MODULE.UNIQUE_TEXT_GEOMETRY_ALLOWED_BUCKETS)
        self.assertEqual(len(allowed), MODULE.MAX_UNIQUE_TEXT_GEOMETRY_BUCKETS)
        matched = len(allowed)
        geometry["rxls_unique_items"] = matched
        geometry["libreoffice_unique_items"] = matched
        geometry["matched_items"] = matched
        page = document["files"][0]["pages"][0]
        page["text_box_rxls_items"] = matched
        page["text_box_libreoffice_items"] = matched
        page["text_box_matched_items"] = matched
        for axis in MODULE.UNIQUE_TEXT_GEOMETRY_AXES:
            geometry["delta_histograms_millipoints"][axis] = [
                {"count": 1, "delta_millipoints": delta}
                for delta in allowed
            ]
            geometry["exact_delta_summaries_millipoints"][axis] = {
                "count": matched,
                "max_delta_millipoints": 10_001,
                "min_delta_millipoints": -10_001,
                "negative_overflow_items": 1,
                "positive_overflow_items": 1,
                "sum_delta_millipoints": 0,
            }
        validated(document)

        empty = copy.deepcopy(self.baseline)
        empty_geometry = empty["files"][0]["pages"][0][
            "text_box_unique_geometry"
        ]
        empty_geometry["rxls_unique_items"] = 0
        empty_geometry["libreoffice_unique_items"] = 0
        empty_geometry["matched_items"] = 0
        for axis in MODULE.UNIQUE_TEXT_GEOMETRY_AXES:
            empty_geometry["delta_histograms_millipoints"][axis] = []
            empty_geometry["exact_delta_summaries_millipoints"][axis] = {
                "count": 0,
                "max_delta_millipoints": None,
                "min_delta_millipoints": None,
                "negative_overflow_items": 0,
                "positive_overflow_items": 0,
                "sum_delta_millipoints": 0,
            }
        validated(empty)

        malformed_mutations = {
            "content_field": lambda value: value.__setitem__(
                "normalized_text", "private"
            ),
            "axis_set": lambda value: value[
                "delta_histograms_millipoints"
            ].pop("height"),
            "bucket_universe": lambda value: value[
                "delta_histograms_millipoints"
            ]["x_min"][10].__setitem__("delta_millipoints", 3),
            "bucket_order": lambda value: value[
                "delta_histograms_millipoints"
            ]["x_min"].reverse(),
            "bucket_population": lambda value: value[
                "delta_histograms_millipoints"
            ]["x_min"].pop(),
            "exact_sum_bound": lambda value: value[
                "exact_delta_summaries_millipoints"
            ]["x_min"].__setitem__(
                "sum_delta_millipoints",
                matched
                * MODULE.MAX_UNIQUE_TEXT_GEOMETRY_DELTA_MILLIPOINTS
                + 1,
            ),
            "overflow_count": lambda value: value[
                "exact_delta_summaries_millipoints"
            ]["x_min"].__setitem__("positive_overflow_items", 0),
            "extrema_bucket": lambda value: value[
                "exact_delta_summaries_millipoints"
            ]["x_min"].__setitem__("min_delta_millipoints", -9_999),
        }
        for name, mutate in malformed_mutations.items():
            malformed = copy.deepcopy(document)
            malformed_geometry = malformed["files"][0]["pages"][0][
                "text_box_unique_geometry"
            ]
            mutate(malformed_geometry)
            with self.subTest(name=name), self.assertRaisesRegex(
                MODULE.MalformedReport, "page_unique_text_geometry"
            ):
                validated(malformed)

    def test_unique_text_geometry_rejects_impossible_sum_and_item_counts(
        self,
    ) -> None:
        impossible = copy.deepcopy(self.baseline)
        page = impossible["files"][0]["pages"][0]
        geometry = page["text_box_unique_geometry"]
        geometry["rxls_unique_items"] = 2
        geometry["libreoffice_unique_items"] = 2
        geometry["matched_items"] = 2
        page["text_box_rxls_items"] = 2
        page["text_box_libreoffice_items"] = 2
        page["text_box_matched_items"] = 2
        for axis in MODULE.UNIQUE_TEXT_GEOMETRY_AXES:
            geometry["delta_histograms_millipoints"][axis] = [
                {"count": 1, "delta_millipoints": -500},
                {"count": 1, "delta_millipoints": 500},
            ]
            geometry["exact_delta_summaries_millipoints"][axis] = {
                "count": 2,
                "max_delta_millipoints": 749,
                "min_delta_millipoints": -3,
                "negative_overflow_items": 0,
                "positive_overflow_items": 0,
                "sum_delta_millipoints": (
                    1_498 if axis == "x_min" else 746
                ),
            }
        with self.assertRaisesRegex(
            MODULE.MalformedReport, "page_unique_text_geometry"
        ):
            validated(impossible)

        impossible_axis = copy.deepcopy(self.baseline)
        axis_geometry = impossible_axis["files"][0]["pages"][0][
            "text_line_box_unique_geometry"
        ]
        axis_geometry["delta_histograms_millipoints"]["center_x"] = [
            {"count": 1, "delta_millipoints": 1}
        ]
        axis_geometry["exact_delta_summaries_millipoints"]["center_x"] = {
            "count": 1,
            "max_delta_millipoints": 1,
            "min_delta_millipoints": 1,
            "negative_overflow_items": 0,
            "positive_overflow_items": 0,
            "sum_delta_millipoints": 1,
        }
        with self.assertRaisesRegex(
            MODULE.MalformedReport, "page_unique_text_geometry"
        ):
            validated(impossible_axis)

        for field in (
            "text_box_rxls_items",
            "text_box_libreoffice_items",
            "text_box_matched_items",
        ):
            drifted = copy.deepcopy(self.baseline)
            drifted["files"][0]["pages"][0][field] = 0
            with self.subTest(field=field), self.assertRaisesRegex(
                MODULE.MalformedReport, "page_unique_text_geometry"
            ):
                validated(drifted)

    def test_unique_text_geometry_is_exact_same_sha_evidence_not_a_score(self) -> None:
        candidate = copy.deepcopy(self.candidate)
        candidate["files"][0]["pages"][0][
            "text_box_unique_geometry"
        ] = unique_text_geometry(1)
        result = self.compare(candidate=candidate)
        self.assertEqual(result["status"], "fail")
        self.assertIn(
            "unique_text_geometry_evidence_mismatch", result["failures"]
        )
        self.assertNotIn("non_oracle_metric_evidence_mismatch", result["failures"])
        self.assertNotIn("similarity_drift_threshold", result["failures"])
        self.assertEqual(
            result["metric_policy"]["unique_text_geometry"],
            "schema_validated_exact_same_sha_diagnostic_non_scoring",
        )

    def test_baseline_contract_hashes_match_baseline_derivation_domains(self) -> None:
        result = self.compare()
        contract = result["identity"]["baseline_contract"]
        self.assertEqual(
            contract["configuration"]["baseline_sha256"],
            BASELINE_MODULE.configuration_identity_sha256(
                self.baseline["configuration"]
            ),
        )
        self.assertEqual(
            contract["configuration"]["candidate_sha256"],
            BASELINE_MODULE.configuration_identity_sha256(
                self.candidate["configuration"]
            ),
        )
        self.assertEqual(
            contract["input_set"]["baseline_sha256"],
            BASELINE_MODULE._input_identity(self.baseline["files"])[0],
        )
        self.assertEqual(
            contract["input_set"]["candidate_sha256"],
            BASELINE_MODULE._input_identity(self.candidate["files"])[0],
        )

    def test_authored_print_summary_contract_is_explicitly_separate(self) -> None:
        missing = copy.deepcopy(self.baseline)
        del missing["summary"]["authored_print"]
        with self.assertRaisesRegex(MODULE.MalformedReport, "summary_shape"):
            validated(missing)

        structured = copy.deepcopy(self.baseline)
        structured["configuration"]["print_mode"] = "authored"
        structured["summary"]["authored_print"] = {"attested_workbooks": 2}
        with self.assertRaisesRegex(
            MODULE.MalformedReport, "summary_authored_print"
        ):
            validated(structured)

        non_null = copy.deepcopy(self.baseline)
        non_null["summary"]["authored_print"] = {}
        with self.assertRaisesRegex(
            MODULE.MalformedReport, "summary_authored_print"
        ):
            validated(non_null)

    def test_missing_partial_overlap_and_duplicate_inputs_fail_closed(self) -> None:
        missing = copy.deepcopy(self.candidate)
        missing["files"].pop()
        for key in ("pre_shard_selected_count", "selected_count", "shard_candidate_count"):
            missing["discovery"][key] = 1
        missing["summary"]["files"] = 1
        missing["summary"]["by_classification"] = {"within_threshold": 1}
        missing["summary"]["by_status"] = {"compared": 1}
        result = self.compare(candidate=missing)
        self.assertEqual(result["status"], "fail")
        self.assertIn("input_set_mismatch", result["failures"])
        self.assertEqual(result["coverage"]["visual_observations_per_metric"], 0)

        partial = copy.deepcopy(self.candidate)
        partial["files"][0]["sha256"] = "9" * 64
        result = self.compare(candidate=partial)
        self.assertIn("input_set_mismatch", result["failures"])

        duplicate = copy.deepcopy(self.candidate)
        duplicate["files"][1]["sha256"] = duplicate["files"][0]["sha256"]
        with self.assertRaisesRegex(MODULE.MalformedReport, "overlapping_input"):
            validated(duplicate)

    def test_renderer_scene_and_artifact_evidence_drift_fails(self) -> None:
        candidate = copy.deepcopy(self.candidate)
        row = candidate["files"][0]
        row["renderer"]["version"] = "unexpected"
        row["scenes"][0]["sha256"] = "c" * 64
        result = self.compare(candidate=candidate)
        self.assertIn("renderer_evidence_mismatch", result["failures"])
        self.assertIn("scene_evidence_mismatch", result["failures"])

        candidate = copy.deepcopy(self.candidate)
        row = candidate["files"][0]
        second_page = copy.deepcopy(row["pages"][0])
        second_page["source_pdf_page_index"] = 1
        second_page["oracle_output_page_index"] = 1
        row["pages"].append(second_page)
        second_scene = copy.deepcopy(row["scenes"][0])
        second_scene["source_pdf_page_index"] = 1
        second_scene["oracle_output_page_index"] = 1
        row["scenes"].append(second_scene)
        row["metrics"]["pages"] = 2
        row["artifacts"] = {"libreoffice_pages": 2, "rxls_pages": 2}
        result = self.compare(candidate=candidate)
        self.assertIn("artifact_evidence_mismatch", result["failures"])
        self.assertIn("page_mapping_mismatch", result["failures"])

    def test_semantic_and_page_dimension_drift_is_not_tolerated(self) -> None:
        candidate = copy.deepcopy(self.candidate)
        row = candidate["files"][0]
        row["metrics"]["semantic_token_libreoffice_items"] += 1
        row["pages"][0]["libreoffice_size"]["width"] += 1
        row["pages"][0]["source_sheet_index"] = 99
        row["scenes"][0]["source_sheet_index"] = 99
        result = self.compare(candidate=candidate)
        self.assertIn("semantic_counts_mismatch", result["failures"])
        self.assertIn("page_dimensions_mismatch", result["failures"])
        self.assertIn("page_mapping_mismatch", result["failures"])

    def test_status_classification_and_unknown_non_oracle_metrics_are_exact(self) -> None:
        candidate = copy.deepcopy(self.candidate)
        row = candidate["files"][0]
        row["status"] = "different"
        row["classification"] = "below_similarity_threshold"
        candidate["summary"]["by_status"] = {"compared": 1, "different": 1}
        candidate["summary"]["by_classification"] = {
            "below_similarity_threshold": 1,
            "within_threshold": 1,
        }
        row["metrics"]["future_renderer_counter"] = 2
        self.baseline["files"][1]["metrics"]["future_renderer_counter"] = 1
        result = self.compare(candidate=candidate)
        self.assertIn("status_or_classification_mismatch", result["failures"])
        self.assertIn("non_oracle_metric_evidence_mismatch", result["failures"])

    def test_explicit_visual_blur_and_mask_thresholds_fail(self) -> None:
        candidate = copy.deepcopy(self.candidate)
        row = candidate["files"][0]
        for metrics in (row["metrics"], row["pages"][0]):
            metrics["similarity_ppm"] -= 21_000
            metrics["blurred_luma_similarity_ppm"] -= 22_000
            metrics["foreground_f1_ppm"] -= 23_000
        result = self.compare(candidate=candidate)
        self.assertEqual(result["status"], "fail")
        self.assertIn("similarity_drift_threshold", result["failures"])
        self.assertIn("blur_drift_threshold", result["failures"])
        self.assertIn("mask_drift_threshold", result["failures"])

        result = self.compare(
            candidate=candidate,
            max_similarity_drift_ppm=21_000,
            max_blur_drift_ppm=22_000,
            max_mask_drift_ppm=23_000,
        )
        self.assertEqual(result["status"], "pass")

    def test_output_contains_hashes_and_distributions_but_no_paths_or_content(self) -> None:
        baseline = copy.deepcopy(self.baseline)
        candidate = copy.deepcopy(self.candidate)
        for document in (baseline, candidate):
            document["files"][0]["opaque_content"] = "TOP-SECRET-CELL-CONTENT"
        result = self.compare(baseline=baseline, candidate=candidate)
        encoded = MODULE.canonical_bytes(result).decode("utf-8")
        self.assertEqual(result["schema"], MODULE.OUTPUT_SCHEMA)
        self.assertRegex(result["reports"]["baseline"]["sha256"], r"^[0-9a-f]{64}$")
        self.assertNotIn('"path"', encoded)
        self.assertNotIn("opaque_content", encoded)
        for forbidden in (
            "/private/",
            "/different/",
            "/never/publish/",
            "workbook-0.xlsx",
            "TOP-SECRET-CELL-CONTENT",
        ):
            self.assertNotIn(forbidden, encoded)

    def test_cli_is_atomic_canonical_deterministic_and_preserves_output_on_malformed_input(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            baseline = root / "baseline.json"
            candidate = root / "candidate.json"
            output = root / "repeatability.json"
            baseline.write_bytes(MODULE.canonical_bytes(self.baseline))
            candidate.write_bytes(MODULE.canonical_bytes(self.candidate))
            command = [
                sys.executable,
                str(SCRIPT),
                str(baseline),
                str(candidate),
                "--output",
                str(output),
            ]
            first = subprocess.run(command, check=False, capture_output=True, text=True)
            first_payload = output.read_bytes()
            second = subprocess.run(command, check=False, capture_output=True, text=True)
            second_payload = output.read_bytes()
            document = json.loads(second_payload)
            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertEqual(second.returncode, 0, second.stderr)
            self.assertEqual(first_payload, second_payload)
            self.assertEqual(second_payload, MODULE.canonical_bytes(document))
            self.assertFalse(list(root.glob(f".{output.name}.*.tmp")))

            drifted = copy.deepcopy(self.candidate)
            drifted["files"][0]["metrics"]["similarity_ppm"] -= 1
            drifted["summary"]["metric_cohorts"] = (
                MODULE._recompute_metric_cohorts(drifted["files"])
            )
            candidate.write_bytes(MODULE.canonical_bytes(drifted))
            failed = subprocess.run(
                [*command, "--max-similarity-drift-ppm", "0"],
                check=False,
                capture_output=True,
                text=True,
            )
            failure_document = json.loads(output.read_bytes())
            self.assertEqual(failed.returncode, 1, failed.stderr)
            self.assertEqual(failure_document["status"], "fail")
            self.assertEqual(output.read_bytes(), MODULE.canonical_bytes(failure_document))
            self.assertFalse(list(root.glob(f".{output.name}.*.tmp")))

            output.write_bytes(b"sentinel\n")
            candidate.write_text("{}", encoding="utf-8")
            malformed = subprocess.run(command, check=False, capture_output=True, text=True)
            self.assertEqual(malformed.returncode, 2)
            self.assertEqual(output.read_bytes(), b"sentinel\n")

    def test_report_byte_bound_is_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "report.json"
            path.write_bytes(MODULE.canonical_bytes(self.baseline))
            with self.assertRaisesRegex(MODULE.MalformedReport, "report_bytes_limit"):
                MODULE.read_report(path, 1)

            path.write_text('{"schema": 1, "schema": 2}', encoding="utf-8")
            with self.assertRaisesRegex(MODULE.MalformedReport, "duplicate_json_key"):
                MODULE.read_report(path, 1_000)

            path.write_text('{"value": NaN}', encoding="utf-8")
            with self.assertRaisesRegex(MODULE.MalformedReport, "nonfinite_number"):
                MODULE.read_report(path, 1_000)

            target = Path(raw) / "target.json"
            target.write_text("{}\n", encoding="utf-8")
            link = Path(raw) / "link.json"
            link.symlink_to(target)
            with self.assertRaisesRegex(
                MODULE.MalformedReport,
                "report_bytes_limit",
            ):
                MODULE.read_report(link, 1_000)

            fifo = Path(raw) / "report.fifo"
            os.mkfifo(fifo)
            with self.assertRaisesRegex(
                MODULE.MalformedReport,
                "report_bytes_limit",
            ):
                MODULE.read_report(fifo, 1_000)

    def test_cli_rejects_hostile_json_numbers_and_depth_with_stable_errors(
        self,
    ) -> None:
        malformed_payloads = (
            (
                b'{"value":' + (b"8" * 5_000) + b"}",
                "report_integer_limit",
            ),
            (b'{"value":1.5}', "report_nonintegral_number"),
            (b'{"value":1e10000}', "report_nonintegral_number"),
            (
                b"[" * (MODULE.MAX_JSON_DEPTH + 1)
                + b"]" * (MODULE.MAX_JSON_DEPTH + 1),
                "report_json_depth",
            ),
        )
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            baseline = root / "baseline.json"
            candidate = root / "candidate.json"
            output = root / "repeatability.json"
            baseline.write_bytes(MODULE.canonical_bytes(self.baseline))
            command = [
                sys.executable,
                str(SCRIPT),
                str(baseline),
                str(candidate),
                "--output",
                str(output),
            ]
            for payload, error_code in malformed_payloads:
                candidate.write_bytes(payload)
                output.write_bytes(b"sentinel\n")
                result = subprocess.run(
                    command,
                    check=False,
                    capture_output=True,
                    text=True,
                )
                with self.subTest(error_code=error_code):
                    self.assertEqual(result.returncode, 2)
                    self.assertEqual(
                        result.stderr.splitlines(),
                        [f"compare-render-parity-runs: {error_code}"],
                    )
                    self.assertNotIn("Traceback", result.stderr)
                    self.assertNotIn(str(candidate), result.stderr)
                    self.assertEqual(output.read_bytes(), b"sentinel\n")

    def test_json_preflight_rejects_structural_complexity_before_decode(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "report.json"
            path.write_bytes(b"[0,0,0,0]")
            with mock.patch.object(
                MODULE,
                "MAX_JSON_NODES",
                3,
            ), mock.patch.object(
                MODULE.json,
                "loads",
                side_effect=AssertionError("decoder must not run"),
            ), self.assertRaisesRegex(
                MODULE.MalformedReport,
                "report_json_complexity",
            ):
                MODULE.read_report(path, 1_000)


if __name__ == "__main__":
    unittest.main()
