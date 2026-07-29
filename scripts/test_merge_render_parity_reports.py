#!/usr/bin/env python3
"""Tests for complete, exact-identity render-parity shard merging."""

from __future__ import annotations

from collections import Counter
import copy
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
SCRIPT = ROOT / "scripts" / "merge-render-parity-reports.py"


def load_module():
    spec = importlib.util.spec_from_file_location("merge_render_parity_reports", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def load_repeatability_fixture():
    path = ROOT / "scripts" / "test_compare_render_parity_runs.py"
    spec = importlib.util.spec_from_file_location(
        "merge_repeatability_fixture",
        path,
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


REPEATABILITY_FIXTURE = load_repeatability_fixture()


def file_row(index: int) -> dict[str, object]:
    return copy.deepcopy(REPEATABILITY_FIXTURE.file_row(index))


def report(shard_index: int, rows: list[dict[str, object]]) -> dict[str, object]:
    renderer_identity = {
        "bytes": 4_273_408,
        "sha256": "a" * 64,
    }
    return {
        "configuration": {
            "dpi": 96,
            "font_pack": {"pack_sha256": "f" * 64},
            "metric_policy": {
                "unique_text_geometry": copy.deepcopy(
                    MODULE.HARNESS.UNIQUE_TEXT_GEOMETRY_POLICY
                )
            },
            "print_mode": MODULE.HARNESS.PRINT_MODE_SINGLE_PAGE,
            "renderer_binary": renderer_identity,
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
        "preflight": {
            "oracle_lock": {"configured": True},
            "rxls_command": {
                "binary_identity": copy.deepcopy(renderer_identity),
            },
        },
        "schema": MODULE.EVIDENCE_SCHEMA,
        "summary": {
            "authored_print": None,
            "by_classification": {"within_threshold": len(rows)},
            "by_status": {"compared": len(rows)},
            "files": len(rows),
            "input_bytes_considered": sum(
                int(row["bytes"]) for row in rows
            ),
            "metric_cohorts": MODULE.HARNESS.metric_cohorts(rows),
        },
    }


def refresh_summary(document: dict[str, object]) -> None:
    rows = document["files"]
    statuses = Counter(str(row["status"]) for row in rows)
    classifications = Counter(
        str(row["classification"]) for row in rows
    )
    document["summary"].update(
        {
            "authored_print": MODULE.HARNESS.authored_print_summary(
                rows,
                document["configuration"]["print_mode"],
            ),
            "by_classification": dict(sorted(classifications.items())),
            "by_status": dict(sorted(statuses.items())),
            "files": len(rows),
            "input_bytes_considered": sum(
                int(row["bytes"]) for row in rows
            ),
            "metric_cohorts": MODULE.HARNESS.metric_cohorts(rows),
        }
    )


class MergeRenderParityReportsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.first = report(0, [file_row(0), file_row(2)])
        self.second = report(1, [file_row(1), file_row(3)])

    def test_complete_set_merges_in_content_identity_order_and_recomputes_metrics(self) -> None:
        merged = MODULE.merge_reports([self.second, self.first])
        self.assertEqual(merged["discovery"]["shard_count"], 1)
        self.assertEqual(merged["summary"]["files"], 4)
        self.assertEqual(
            merged["summary"]["input_bytes_considered"],
            4_006,
        )
        self.assertEqual(
            [row["sha256"] for row in merged["files"]],
            [f"{index:064x}" for index in range(1, 5)],
        )
        cohort = merged["summary"]["metric_cohorts"]["all"]
        self.assertEqual(cohort["comparable_workbooks"], 4)
        self.assertEqual(cohort["scores"]["edge_f1_ppm"]["count"], 4)

    def test_identity_mismatch_duplicate_index_overlap_and_truncation_fail(self) -> None:
        aliased = copy.deepcopy(self.first)
        aliased["configuration"]["renderer_binary"]["bytes"] = True
        aliased["preflight"]["rxls_command"]["binary_identity"][
            "bytes"
        ] = True
        with self.assertRaisesRegex(MODULE.MergeError, "report_identity"):
            MODULE.merge_reports([aliased, self.second])

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
        refresh_summary(incomplete)
        with self.assertRaisesRegex(MODULE.MergeError, "combined_coverage"):
            MODULE.merge_reports([self.first, incomplete])

        impossible = copy.deepcopy(self.first)
        impossible["discovery"]["candidate_count"] = 0
        with self.assertRaisesRegex(MODULE.MergeError, "shard_coverage"):
            MODULE.merge_reports([impossible, self.second])

    def test_geometry_policy_and_merged_report_budget_are_fail_closed(self) -> None:
        drifted = copy.deepcopy(self.first)
        drifted["configuration"]["metric_policy"][
            "unique_text_geometry"
        ]["max_geometry_pages_per_report"] += 1
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "metric_policy_unique_text_geometry",
        ):
            MODULE.merge_reports([drifted, self.second])

        aliased = copy.deepcopy(self.first)
        aliased["configuration"]["metric_policy"][
            "unique_text_geometry"
        ]["diagnostic_only"] = 1
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "metric_policy_unique_text_geometry",
        ):
            MODULE.merge_reports([aliased, self.second])

        first = copy.deepcopy(self.first)
        second = copy.deepcopy(self.second)
        with (
            mock.patch.object(
                MODULE.HARNESS,
                "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES",
                3,
            ),
            self.assertRaisesRegex(
                MODULE.MergeError,
                "unique_text_geometry_report_limit",
            ),
        ):
            MODULE.merge_reports([first, second])
        with (
            mock.patch.object(
                MODULE.HARNESS,
                "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS",
                48,
            ),
            self.assertRaisesRegex(
                MODULE.MergeError,
                "unique_text_geometry_report_limit",
            ),
        ):
            MODULE.merge_reports([first, second])

    def test_metric_status_requires_geometry_and_incomparable_rows_reject_it(self) -> None:
        unknown = copy.deepcopy(self.first)
        unknown["files"][0]["status"] = "compard"
        unknown["files"][0].pop("pages")
        unknown["files"][0].pop("metrics")
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "file_status_or_classification",
        ):
            MODULE.merge_reports([unknown, self.second])

        missing = copy.deepcopy(self.first)
        missing["files"][0].pop("pages")
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "unique_text_geometry_report_shape",
        ):
            MODULE.merge_reports([missing, self.second])

        incomparable = copy.deepcopy(self.first)
        incomparable["files"][0]["status"] = "error"
        incomparable["files"][0]["classification"] = "renderer_failed"
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "incomparable_row_metrics",
        ):
            MODULE.merge_reports([incomparable, self.second])

        incomparable["files"][0].pop("pages")
        incomparable["files"][0].pop("metrics")
        refresh_summary(incomparable)
        merged = MODULE.merge_reports([incomparable, self.second])
        self.assertEqual(merged["summary"]["by_status"]["error"], 1)

        malformed_metrics = copy.deepcopy(self.first)
        malformed_metrics["files"][0]["metrics"].update(
            {
                "semantic_token_rxls_items": [],
                "semantic_token_libreoffice_items": 0,
                "foreground_rxls_pixels": 0,
                "foreground_libreoffice_pixels": 0,
            }
        )
        with self.assertRaisesRegex(MODULE.MergeError, "report_row_contract"):
            MODULE.merge_reports([malformed_metrics, self.second])

        aliased_metric = copy.deepcopy(self.first)
        aliased_metric["files"][0]["metrics"]["edge_f1_ppm"] = True
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "report_row_contract",
        ):
            MODULE.merge_reports([aliased_metric, self.second])

        for key, value in (
            ("features", [True]),
            ("rights_tier", "private"),
        ):
            malformed_identity = copy.deepcopy(self.first)
            malformed_identity["files"][0][key] = value
            with (
                self.subTest(key=key),
                self.assertRaisesRegex(
                    MODULE.MergeError,
                    "report_row_contract",
                ),
            ):
                MODULE.merge_reports(
                    [malformed_identity, self.second]
                )

        invalid_geometry = copy.deepcopy(self.first)
        invalid_geometry["files"][0]["pages"][0][
            "text_box_unique_geometry"
        ]["delta_histograms_millipoints"]["x_min"][0][
            "delta_millipoints"
        ] = 3
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "unique_text_geometry_report_shape",
        ):
            MODULE.merge_reports([invalid_geometry, self.second])

    def test_authored_print_contract_is_validated_before_reduction(self) -> None:
        def authored_value(scale_mode: str) -> dict[str, object]:
            return {
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
            }

        first = copy.deepcopy(self.first)
        second = copy.deepcopy(self.second)
        for document in (first, second):
            document["configuration"]["print_mode"] = (
                MODULE.HARNESS.PRINT_MODE_AUTHORED
            )
            for index, row in enumerate(document["files"]):
                row["authored_print"] = authored_value(
                    "fit" if index % 2 else "scale"
                )
            document["summary"]["authored_print"] = (
                MODULE.HARNESS.authored_print_summary(
                    document["files"],
                    MODULE.HARNESS.PRINT_MODE_AUTHORED,
                )
            )
        merged = MODULE.merge_reports([first, second])
        self.assertEqual(
            merged["summary"]["authored_print"]["attested_workbooks"],
            4,
        )

        aliased_summary = copy.deepcopy(first)
        aliased_summary["summary"]["authored_print"][
            "attested_workbooks"
        ] = True
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "authored_print_summary",
        ):
            MODULE.merge_reports([aliased_summary, second])

        malformed = copy.deepcopy(first)
        malformed["files"][0]["authored_print"][
            "manual_row_breaks"
        ] = []
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "authored_print_contract",
        ):
            MODULE.merge_reports([malformed, second])

        missing = copy.deepcopy(first)
        missing["files"][0].pop("authored_print")
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "authored_print_contract",
        ):
            MODULE.merge_reports([missing, second])

        extra = copy.deepcopy(self.first)
        extra["files"][0]["authored_print"] = authored_value("scale")
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "authored_print_contract",
        ):
            MODULE.merge_reports([extra, self.second])

        for key in (
            "manual_col_breaks",
            "manual_row_breaks",
            "paper_code",
        ):
            aliased = copy.deepcopy(first)
            aliased["files"][0]["authored_print"][key] = True
            with (
                self.subTest(key=key),
                self.assertRaisesRegex(
                    MODULE.MergeError,
                    "authored_print_contract",
                ),
            ):
                MODULE.merge_reports([aliased, second])

    def test_shard_geometry_budget_uses_equal_floor_partition(self) -> None:
        uneven_first = report(
            0,
            [file_row(0), file_row(2), file_row(4)],
        )
        uneven_second = report(1, [file_row(1)])

        with mock.patch.object(
            MODULE.HARNESS,
            "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES",
            4,
        ):
            self.assertEqual(
                MODULE.merge_reports(
                    [copy.deepcopy(self.first), copy.deepcopy(self.second)]
                )["summary"]["files"],
                4,
            )
            with self.assertRaisesRegex(
                MODULE.MergeError,
                "unique_text_geometry_report_limit",
            ):
                MODULE.merge_reports([uneven_first, uneven_second])

        with (
            mock.patch.object(
                MODULE.HARNESS,
                "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES",
                8,
            ),
            mock.patch.object(
                MODULE.HARNESS,
                "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS",
                64,
            ),
        ):
            self.assertEqual(
                MODULE.merge_reports(
                    [copy.deepcopy(self.first), copy.deepcopy(self.second)]
                )["summary"]["files"],
                4,
            )
            with self.assertRaisesRegex(
                MODULE.MergeError,
                "unique_text_geometry_report_limit",
            ):
                MODULE.merge_reports([uneven_first, uneven_second])

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

    def test_atomic_writer_does_not_follow_predictable_temp_symlink(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            output = root / "merged.json"
            victim = root / "victim.json"
            victim.write_bytes(b"private")
            predictable = root / ".merged.json.tmp"
            predictable.symlink_to(victim)
            MODULE.write_atomic(output, b'{"complete":true}\n')
            self.assertEqual(victim.read_bytes(), b"private")
            self.assertEqual(
                output.read_bytes(),
                b'{"complete":true}\n',
            )

    def test_strict_input_preflight_rejects_duplicates_and_complexity(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "report.json"
            path.write_text('{"schema":1,"schema":2}\n', encoding="utf-8")
            with self.assertRaisesRegex(
                MODULE.MergeError,
                "report_duplicate_json_key",
            ):
                MODULE.read_report(path, MODULE.MAX_REPORT_BYTES)

            target = Path(raw) / "target.json"
            target.write_text("{}\n", encoding="utf-8")
            link = Path(raw) / "link.json"
            link.symlink_to(target)
            with self.assertRaisesRegex(
                MODULE.MergeError,
                "report_bytes_limit",
            ):
                MODULE.read_report(link, MODULE.MAX_REPORT_BYTES)

            fifo = Path(raw) / "report.fifo"
            os.mkfifo(fifo)
            with self.assertRaisesRegex(
                MODULE.MergeError,
                "report_bytes_limit",
            ):
                MODULE.read_report(fifo, MODULE.MAX_REPORT_BYTES)

            oversized = Path(raw) / "oversized.json"
            oversized.write_bytes(b"12345")
            with (
                mock.patch.object(MODULE, "MAX_REPORT_BYTES", 4),
                self.assertRaisesRegex(
                    MODULE.MergeError,
                    "report_bytes_limit",
                ),
            ):
                MODULE.read_report(oversized, 4)

            path.write_text('{"a":[],"b":[]}\n', encoding="utf-8")
            with (
                mock.patch.object(MODULE, "MAX_JSON_NODES", 3),
                self.assertRaisesRegex(
                    MODULE.MergeError,
                    "report_json_complexity",
                ),
            ):
                MODULE.read_report(path, MODULE.MAX_REPORT_BYTES)

    def test_final_report_obeys_strict_reader_size_and_node_limits(self) -> None:
        with (
            mock.patch.object(
                MODULE.HARNESS,
                "MAX_EVIDENCE_REPORT_BYTES",
                1,
            ),
            self.assertRaisesRegex(MODULE.MergeError, "report_bytes_limit"),
        ):
            MODULE.merge_reports([self.first, self.second])

        with (
            mock.patch.object(
                MODULE.HARNESS,
                "MAX_EVIDENCE_REPORT_JSON_NODES",
                1,
            ),
            self.assertRaisesRegex(
                MODULE.MergeError,
                "report_json_complexity",
            ),
        ):
            MODULE.merge_reports([self.first, self.second])

    def test_complete_campaigns_combine_without_a_local_super_manifest(self) -> None:
        first = MODULE.merge_reports([self.first, self.second])
        second = copy.deepcopy(first)
        for index, row in enumerate(second["files"], start=10):
            row["sha256"] = f"{index + 1:064x}"
            row["path"] = f"generated/input-{index}.xlsx"
        second["discovery"]["candidate_count"] = 4
        combined = MODULE.combine_campaigns([second, first])
        self.assertEqual(combined["summary"]["files"], 8)
        self.assertEqual(
            combined["summary"]["input_bytes_considered"],
            8_012,
        )
        self.assertEqual(combined["discovery"]["candidate_count"], 8)
        self.assertEqual(combined["discovery"]["selected_count"], 8)
        self.assertFalse(combined["discovery"]["truncated"])

        incomplete = copy.deepcopy(second)
        incomplete["discovery"]["shard_count"] = 2
        with self.assertRaisesRegex(MODULE.MergeError, "campaign_incomplete"):
            MODULE.combine_campaigns([first, incomplete])

        for key, alias in (
            ("shard_count", True),
            ("shard_index", False),
        ):
            aliased = copy.deepcopy(second)
            aliased["discovery"][key] = alias
            with (
                self.subTest(key=key),
                self.assertRaisesRegex(
                    MODULE.MergeError,
                    "campaign_incomplete",
                ),
            ):
                MODULE.combine_campaigns([first, aliased])

        overflow_first = copy.deepcopy(first)
        overflow_second = copy.deepcopy(second)
        overflow_first["discovery"]["candidate_count"] = (
            MODULE.MAX_JSON_INTEGER
        )
        overflow_second["discovery"]["candidate_count"] = (
            MODULE.MAX_JSON_INTEGER
        )
        with self.assertRaisesRegex(
            MODULE.MergeError,
            "candidate_count",
        ):
            MODULE.combine_campaigns(
                [overflow_first, overflow_second]
            )

        with (
            mock.patch.object(
                MODULE.HARNESS,
                "MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES",
                6,
            ),
            self.assertRaisesRegex(
                MODULE.MergeError,
                "unique_text_geometry_report_limit",
            ),
        ):
            MODULE.combine_campaigns([first, second])

    def test_merged_single_page_report_satisfies_repeatability_contract(self) -> None:
        complete = REPEATABILITY_FIXTURE.report()
        shards = []
        for index, row in enumerate(complete["files"]):
            shard = copy.deepcopy(complete)
            shard["files"] = [copy.deepcopy(row)]
            shard["discovery"].update(
                {
                    "candidate_count": 2,
                    "pre_shard_selected_count": 2,
                    "selected_count": 1,
                    "shard_candidate_count": 1,
                    "shard_count": 2,
                    "shard_index": index,
                    "truncated": False,
                }
            )
            refresh_summary(shard)
            shards.append(shard)

        merged = MODULE.merge_reports(shards)
        validated = REPEATABILITY_FIXTURE.validated(merged)
        self.assertEqual(validated.page_count, 2)
        self.assertIsNone(merged["summary"]["authored_print"])


if __name__ == "__main__":
    unittest.main()
