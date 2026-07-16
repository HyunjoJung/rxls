#!/usr/bin/env python3
"""Tests for path-neutral render parity baselines and ratchets."""

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
SCRIPT = ROOT / "scripts" / "check-render-parity-baseline.py"


def load_module():
    spec = importlib.util.spec_from_file_location("check_render_parity_baseline", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def score(value: int, count: int = 2) -> dict[str, int]:
    return {"count": count, "max": value, "mean": value, "min": value, "p10": value}


def delta(value: int, count: int = 2) -> dict[str, int]:
    return {
        "count": count,
        "max": value,
        "mean": value,
        "min": value,
        "p50": value,
        "p90": value,
    }


def cohort(workbooks: int = 2, comparable: int = 2) -> dict[str, object]:
    return {
        "comparable_workbooks": comparable,
        "deltas": {"max_page_width_delta_pixels": delta(3, comparable)},
        "scores": {"text_ink_f1_ppm": score(800_000, comparable)},
        "workbooks": workbooks,
    }


def evidence() -> dict[str, object]:
    files = []
    for index, format_name in enumerate(("xlsx", "ods")):
        files.append(
            {
                "classification": "within_threshold",
                "features": ["unicode-text"],
                "format": format_name,
                "path": f"private/source-{index}.{format_name}",
                "rights_tier": "S",
                "scenes": [
                    {
                        "sha256": str(index + 1) * 64,
                        "sheet_index": 0,
                        "warnings": [
                            {"code": "pagination_deferred", "occurrences": 1}
                        ],
                    }
                ],
                "sha256": chr(ord("a") + index) * 64,
                "status": "compared",
            }
        )
    return {
        "configuration": {
            "dpi": 96,
            "font_pack": {"pack_sha256": "f" * 64},
            "locale": "C.UTF-8",
            "metric_policy": {"edge_luma_delta": 32},
            "oracle_lock": {"profile": "locked"},
        },
        "files": files,
        "mode": "compare",
        "schema": MODULE.EVIDENCE_SCHEMA,
        "summary": {
            "by_classification": {"within_threshold": 2},
            "by_status": {"compared": 2},
            "metric_cohorts": {
                "all": cohort(),
                "by_feature": {"unicode-text": cohort()},
                "by_format": {"ods": cohort(1, 1), "xlsx": cohort(1, 1)},
            },
        },
    }


def campaign_manifest(source: dict[str, object]) -> dict[str, object]:
    files = source["files"]
    assert isinstance(files, list)
    format_counts: dict[str, int] = {}
    feature_counts: dict[str, int] = {}
    manifest_files = []
    for row in files:
        assert isinstance(row, dict)
        format_name = row["format"]
        assert isinstance(format_name, str)
        format_counts[format_name] = format_counts.get(format_name, 0) + 1
        for feature in row["features"]:
            feature_counts[feature] = feature_counts.get(feature, 0) + 1
        manifest_files.append(
            {
                "features": row["features"],
                "format": format_name,
                "rights_tier": row["rights_tier"],
                "sha256": row["sha256"],
            }
        )
    return {
        "case_count": len(manifest_files),
        "feature_counts": feature_counts,
        "files": manifest_files,
        "format_counts": format_counts,
        "generator": "rxls-synthetic-render-corpus",
        "generator_version": "test",
        "license": "MIT",
        "profile": "full",
        "redistribution": "allowed",
        "render_redistributable": True,
        "rights_tier": "S",
        "schema_version": 1,
        "source_redistributable": True,
    }


class RenderParityBaselineTests(unittest.TestCase):
    def test_baseline_excludes_paths_and_retains_identity_and_warning_counts(self) -> None:
        baseline = MODULE.derive_baseline(evidence())
        rendered = json.dumps(baseline, sort_keys=True)
        self.assertNotIn("private", rendered)
        self.assertEqual(baseline["input_files"], 2)
        self.assertEqual(baseline["warning_counts"], {"pagination_deferred": 2})
        self.assertEqual(baseline["comparable_files"], 2)

    def test_identical_and_strictly_better_candidates_pass(self) -> None:
        baseline = MODULE.derive_baseline(evidence())
        identical = MODULE.compare(baseline, copy.deepcopy(baseline))
        self.assertTrue(identical["passed"])

        better = copy.deepcopy(baseline)
        better["cohorts"]["all"]["scores"]["text_ink_f1_ppm"]["p10"] += 1
        better["cohorts"]["all"]["deltas"]["max_page_width_delta_pixels"]["max"] -= 1
        better["warning_counts"] = {}
        self.assertTrue(MODULE.compare(baseline, better)["passed"])

    def test_score_delta_warning_classification_and_coverage_regressions_fail(self) -> None:
        baseline = MODULE.derive_baseline(evidence())
        candidate = copy.deepcopy(baseline)
        candidate["cohorts"]["all"]["scores"]["text_ink_f1_ppm"]["mean"] -= 1
        candidate["cohorts"]["all"]["deltas"]["max_page_width_delta_pixels"]["p90"] += 1
        candidate["warning_counts"]["new_warning"] = 1
        candidate["classifications"]["new_skip"] = 1
        candidate["comparable_files"] = 1
        report = MODULE.compare(baseline, candidate)

        self.assertFalse(report["passed"])
        joined = "\n".join(report["failures"])
        self.assertIn("score_regression", joined)
        self.assertIn("delta_regression", joined)
        self.assertIn("warning:new:new_warning", joined)
        self.assertIn("classification:new:new_skip", joined)
        self.assertIn("coverage", joined)
        self.assertIn("warning:unclassified:new_warning:1", report["failures"])
        self.assertEqual(
            report["warning_policy"]["unclassified_codes"], ["new_warning"]
        )

    def test_changed_inputs_or_configuration_fail_identity(self) -> None:
        baseline = MODULE.derive_baseline(evidence())
        candidate = copy.deepcopy(baseline)
        candidate["input_set_sha256"] = "0" * 64
        candidate["configuration_sha256"] = "1" * 64
        report = MODULE.compare(baseline, candidate)
        self.assertFalse(report["passed"])
        self.assertIn("identity_mismatch:input_set_sha256", report["failures"])
        self.assertIn("identity_mismatch:configuration_sha256", report["failures"])

    def test_scoped_campaign_binds_generated_manifest_and_rejects_legacy_baseline(
        self,
    ) -> None:
        source = evidence()
        with tempfile.TemporaryDirectory() as raw:
            manifest_path = Path(raw) / "manifest.json"
            manifest_path.write_text(json.dumps(campaign_manifest(source)))
            campaign = MODULE.campaign_from_manifest(manifest_path)

        scoped = MODULE.derive_baseline(source, campaign)
        self.assertEqual(scoped["schema"], MODULE.SCOPED_BASELINE_SCHEMA)
        self.assertEqual(scoped["campaign"]["case_count"], 2)
        self.assertEqual(
            scoped["campaign"]["kind"], "project_generated_manifest"
        )
        report = MODULE.compare(MODULE.derive_baseline(source), scoped)
        self.assertFalse(report["passed"])
        self.assertIn("identity_mismatch:schema", report["failures"])
        self.assertIn("identity_mismatch:campaign", report["failures"])

    def test_hosted_full_contract_rejects_small_or_acquired_campaigns(self) -> None:
        source = evidence()
        with tempfile.TemporaryDirectory() as raw:
            manifest_path = Path(raw) / "manifest.json"
            manifest_path.write_text(json.dumps(campaign_manifest(source)))
            with self.assertRaisesRegex(
                MODULE.BaselineError, "campaign_not_hosted_full_800"
            ):
                MODULE.campaign_from_manifest(
                    manifest_path, require_hosted_full_800=True
                )

    def test_scoped_baseline_requires_every_manifest_cohort_and_metric(self) -> None:
        source = evidence()
        with tempfile.TemporaryDirectory() as raw:
            manifest_path = Path(raw) / "manifest.json"
            manifest_path.write_text(json.dumps(campaign_manifest(source)))
            campaign = MODULE.campaign_from_manifest(manifest_path)

        missing_feature = copy.deepcopy(source)
        del missing_feature["summary"]["metric_cohorts"]["by_feature"][
            "unicode-text"
        ]
        with self.assertRaisesRegex(MODULE.BaselineError, "by_feature_coverage"):
            MODULE.derive_baseline(missing_feature, campaign)

        missing_metric = copy.deepcopy(source)
        del missing_metric["summary"]["metric_cohorts"]["by_format"]["ods"][
            "scores"
        ]["text_ink_f1_ppm"]
        with self.assertRaisesRegex(MODULE.BaselineError, "by_format_cohort"):
            MODULE.derive_baseline(missing_metric, campaign)

            acquired = campaign_manifest(source)
            acquired["generator"] = "rxls-public-render-corpus"
            manifest_path.write_text(json.dumps(acquired))
            self.assertEqual(
                MODULE.campaign_from_manifest(manifest_path)["kind"],
                "acquired_corpus_manifest",
            )
            acquired["case_count"] = 800
            acquired["format_counts"] = {
                "ods": 200,
                "xls": 200,
                "xlsb": 200,
                "xlsx": 200,
            }
            manifest_path.write_text(json.dumps(acquired))
            with self.assertRaises(MODULE.BaselineError):
                MODULE.campaign_from_manifest(
                    manifest_path, require_hosted_full_800=True
                )

    def test_missing_reviewed_baseline_still_writes_aggregate_candidate_and_failure(
        self,
    ) -> None:
        source = evidence()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            evidence_path = root / "evidence.json"
            manifest_path = root / "manifest.json"
            missing_baseline = root / "reviewed.json"
            candidate_path = root / "candidate.json"
            report_path = root / "report.json"
            evidence_path.write_text(json.dumps(source))
            manifest_path.write_text(json.dumps(campaign_manifest(source)))
            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--evidence",
                    str(evidence_path),
                    "--baseline",
                    str(missing_baseline),
                    "--campaign-manifest",
                    str(manifest_path),
                    "--candidate-baseline",
                    str(candidate_path),
                    "--report",
                    str(report_path),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            candidate = json.loads(candidate_path.read_text())
            report = json.loads(report_path.read_text())

        self.assertEqual(result.returncode, 2)
        self.assertEqual(candidate["schema"], MODULE.SCOPED_BASELINE_SCHEMA)
        self.assertFalse(report["passed"])
        self.assertEqual(report["failures"], ["error:baseline_unreadable"])
        self.assertEqual(report["candidate_sha256"], MODULE.sha256_json(candidate))

    def test_candidate_output_cannot_overwrite_reviewed_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            evidence_path = root / "evidence.json"
            baseline_path = root / "baseline.json"
            report_path = root / "report.json"
            evidence_path.write_text(json.dumps(evidence()))
            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--evidence",
                    str(evidence_path),
                    "--baseline",
                    str(baseline_path),
                    "--candidate-baseline",
                    str(baseline_path),
                    "--report",
                    str(report_path),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            report = json.loads(report_path.read_text())

        self.assertEqual(result.returncode, 2)
        self.assertEqual(
            report["failures"],
            ["error:candidate_baseline_overwrites_reviewed_baseline"],
        )

    def test_cli_create_then_verify_is_atomic_and_path_neutral(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            evidence_path = root / "evidence.json"
            baseline_path = root / "baseline.json"
            report_path = root / "report.json"
            evidence_path.write_text(json.dumps(evidence()))
            create = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--evidence",
                    str(evidence_path),
                    "--baseline",
                    str(baseline_path),
                    "--create",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            verify = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--evidence",
                    str(evidence_path),
                    "--baseline",
                    str(baseline_path),
                    "--report",
                    str(report_path),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            report = json.loads(report_path.read_text())
            baseline_text = baseline_path.read_text()

        self.assertEqual(create.returncode, 0, create.stderr)
        self.assertEqual(verify.returncode, 0, verify.stderr)
        self.assertTrue(report["passed"])
        self.assertNotIn("private", baseline_text)


if __name__ == "__main__":
    unittest.main()
