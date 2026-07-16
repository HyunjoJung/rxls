#!/usr/bin/env python3
"""Tests for the path-private LibreOffice repeatability gate."""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "compare-render-parity-runs.py"


def load_module():
    spec = importlib.util.spec_from_file_location("compare_render_parity_runs", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


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
        "sheet_index": index,
        "similarity_ppm": 900_000 - index,
        "text_ink_f1_ppm": 760_000 - index,
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
            {"sha256": f"{index + 100:064x}", "sheet_index": index, "warnings": []}
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
            "by_classification": {"within_threshold": 2},
            "by_status": {"compared": 2},
            "files": 2,
            "input_bytes_considered": 2_001,
            "metric_cohorts": {},
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
        return MODULE.compare_reports(
            validated(baseline or self.baseline),
            validated(candidate or self.candidate),
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
        second_page["sheet_index"] = 99
        row["pages"].append(second_page)
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
        row["pages"][0]["sheet_index"] = 99
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


if __name__ == "__main__":
    unittest.main()
