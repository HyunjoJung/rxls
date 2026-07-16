#!/usr/bin/env python3
"""Tests for complete, exact-identity render-parity shard merging."""

from __future__ import annotations

import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "merge-render-parity-reports.py"


def load_module():
    spec = importlib.util.spec_from_file_location("merge_render_parity_reports", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def metric(value: int) -> dict[str, int]:
    return {
        "edge_f1_ppm": value,
        "max_page_width_delta_pixels": 1,
        "similarity_ppm": value,
    }


def file_row(index: int) -> dict[str, object]:
    return {
        "classification": "within_threshold",
        "features": ["unicode-text"],
        "format": "xlsx" if index % 2 == 0 else "ods",
        "metrics": metric(900_000 + index),
        "path": f"private/input-{index}.xlsx",
        "rights_tier": "S",
        "scenes": [],
        "sha256": f"{index + 1:064x}",
        "status": "compared",
    }


def report(shard_index: int, rows: list[dict[str, object]]) -> dict[str, object]:
    return {
        "configuration": {
            "dpi": 96,
            "font_pack": {"pack_sha256": "f" * 64},
            "renderer_binary": {"sha256": "a" * 64},
        },
        "discovery": {
            "candidate_count": 4,
            "pre_shard_selected_count": 4,
            "selected_count": len(rows),
            "shard_candidate_count": len(rows),
            "shard_count": 2,
            "shard_index": shard_index,
            "truncated": False,
        },
        "files": rows,
        "mode": "compare",
        "preflight": {"oracle_lock": {"configured": True}},
        "schema": MODULE.EVIDENCE_SCHEMA,
        "summary": {
            "by_classification": {"within_threshold": len(rows)},
            "by_status": {"compared": len(rows)},
            "files": len(rows),
            "input_bytes_considered": len(rows) * 100,
            "metric_cohorts": {},
        },
    }


class MergeRenderParityReportsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.first = report(0, [file_row(0), file_row(2)])
        self.second = report(1, [file_row(1), file_row(3)])

    def test_complete_set_merges_in_content_identity_order_and_recomputes_metrics(self) -> None:
        merged = MODULE.merge_reports([self.second, self.first])
        self.assertEqual(merged["discovery"]["shard_count"], 1)
        self.assertEqual(merged["summary"]["files"], 4)
        self.assertEqual(merged["summary"]["input_bytes_considered"], 400)
        self.assertEqual(
            [row["sha256"] for row in merged["files"]],
            [f"{index:064x}" for index in range(1, 5)],
        )
        cohort = merged["summary"]["metric_cohorts"]["all"]
        self.assertEqual(cohort["comparable_workbooks"], 4)
        self.assertEqual(cohort["scores"]["edge_f1_ppm"]["count"], 4)

    def test_identity_mismatch_duplicate_index_overlap_and_truncation_fail(self) -> None:
        changed = copy.deepcopy(self.second)
        changed["configuration"]["dpi"] = 144
        with self.assertRaisesRegex(MODULE.MergeError, "configuration_mismatch"):
            MODULE.merge_reports([self.first, changed])

        duplicate = copy.deepcopy(self.second)
        duplicate["discovery"]["shard_index"] = 0
        with self.assertRaisesRegex(MODULE.MergeError, "duplicate_shard_index"):
            MODULE.merge_reports([self.first, duplicate])

        overlap = copy.deepcopy(self.second)
        overlap["files"][0]["sha256"] = self.first["files"][0]["sha256"]
        with self.assertRaisesRegex(MODULE.MergeError, "overlapping_input"):
            MODULE.merge_reports([self.first, overlap])

        truncated = copy.deepcopy(self.second)
        truncated["discovery"]["truncated"] = True
        with self.assertRaisesRegex(MODULE.MergeError, "shard_truncated"):
            MODULE.merge_reports([self.first, truncated])

    def test_missing_shard_and_incomplete_combined_coverage_fail(self) -> None:
        with self.assertRaisesRegex(MODULE.MergeError, "report_count"):
            MODULE.merge_reports([self.first])
        incomplete = copy.deepcopy(self.second)
        incomplete["files"].pop()
        incomplete["discovery"]["selected_count"] = 1
        incomplete["discovery"]["shard_candidate_count"] = 1
        incomplete["summary"]["files"] = 1
        with self.assertRaisesRegex(MODULE.MergeError, "combined_coverage"):
            MODULE.merge_reports([self.first, incomplete])

    def test_cli_is_atomic_and_emits_one_valid_evidence_report(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            first = root / "first.json"
            second = root / "second.json"
            output = root / "merged.json"
            first.write_text(json.dumps(self.first), encoding="utf-8")
            second.write_text(json.dumps(self.second), encoding="utf-8")
            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    str(first),
                    str(second),
                    "--output",
                    str(output),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            document = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(document["schema"], MODULE.EVIDENCE_SCHEMA)
        self.assertEqual(document["mode"], "compare")
        self.assertEqual(document["summary"]["files"], 4)

    def test_complete_campaigns_combine_without_a_local_super_manifest(self) -> None:
        first = MODULE.merge_reports([self.first, self.second])
        second = copy.deepcopy(first)
        for index, row in enumerate(second["files"], start=10):
            row["sha256"] = f"{index + 1:064x}"
            row["path"] = f"generated/input-{index}.xlsx"
        second["discovery"]["candidate_count"] = 4
        combined = MODULE.combine_campaigns([second, first])
        self.assertEqual(combined["summary"]["files"], 8)
        self.assertEqual(combined["summary"]["input_bytes_considered"], 800)
        self.assertEqual(combined["discovery"]["candidate_count"], 8)
        self.assertEqual(combined["discovery"]["selected_count"], 8)
        self.assertFalse(combined["discovery"]["truncated"])

        incomplete = copy.deepcopy(second)
        incomplete["discovery"]["shard_count"] = 2
        with self.assertRaisesRegex(MODULE.MergeError, "campaign_incomplete"):
            MODULE.combine_campaigns([first, incomplete])


if __name__ == "__main__":
    unittest.main()
