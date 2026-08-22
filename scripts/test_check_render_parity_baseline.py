#!/usr/bin/env python3
"""Tests for path-neutral render parity baselines and ratchets."""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


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


def _create_symlink_or_skip_privilege(
    test_case: unittest.TestCase,
    link: Path,
    target: Path,
) -> None:
    try:
        link.symlink_to(target)
    except OSError as error:
        if os.name == "nt" and getattr(error, "winerror", None) == 1314:
            test_case.skipTest("Windows symlink privilege is unavailable")
        raise


def hosted_group_topology() -> list[dict[str, object]]:
    generator_script = ROOT / "scripts" / "generate-render-corpus.py"
    spec = importlib.util.spec_from_file_location(
        "hosted_group_render_corpus_generator",
        generator_script,
    )
    assert spec is not None and spec.loader is not None
    generator = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = generator
    spec.loader.exec_module(generator)
    counts: dict[tuple[str, tuple[str, ...]], int] = {}
    for case in generator.profile_specs("full"):
        key = (case.format, tuple(case.features))
        counts[key] = counts.get(key, 0) + 1
    return [
        {
            "features": list(features),
            "format": format_name,
            "workbooks": count,
        }
        for (format_name, features), count in sorted(counts.items())
    ]


HOSTED_GROUP_TOPOLOGY = hosted_group_topology()


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


def adoption_cohort(workbooks: int) -> dict[str, object]:
    return {
        "comparable_workbooks": workbooks,
        "deltas": {
            metric: delta(3, workbooks)
            for metric in sorted(MODULE.EXPECTED_DELTA_METRICS)
        },
        "scores": {
            metric: score(800_000, workbooks)
            for metric in sorted(MODULE.EXPECTED_SCORE_METRICS)
        },
        "workbooks": workbooks,
    }


def adoption_histogram_cohort(workbooks: int) -> dict[str, object]:
    return {
        "deltas": {
            metric: [[3, workbooks]]
            for metric in sorted(MODULE.EXPECTED_DELTA_METRICS)
        },
        "scores": {
            metric: [[800_000, workbooks]]
            for metric in sorted(MODULE.EXPECTED_SCORE_METRICS)
        },
    }


def adoption_groups() -> list[dict[str, object]]:
    return [
        {
            "comparable_workbooks": row["workbooks"],
            "deltas": {
                metric: [[3, row["workbooks"]]]
                for metric in sorted(MODULE.EXPECTED_DELTA_METRICS)
            },
            "features": row["features"],
            "format": row["format"],
            "scores": {
                metric: [[800_000, row["workbooks"]]]
                for metric in sorted(MODULE.EXPECTED_SCORE_METRICS)
            },
            "workbooks": row["workbooks"],
        }
        for row in HOSTED_GROUP_TOPOLOGY
    ]


def adoption_baseline() -> dict[str, object]:
    campaign = {
        "case_count": 800,
        "feature_counts": dict(MODULE.HOSTED_FULL_FEATURE_COUNTS),
        "format_counts": {
            "ods": 200,
            "xls": 200,
            "xlsb": 200,
            "xlsx": 200,
        },
        "generator": MODULE.HOSTED_FULL_GENERATOR,
        "generator_version": MODULE.HOSTED_FULL_GENERATOR_VERSION,
        "input_set_sha256": MODULE.HOSTED_FULL_INPUT_SET_SHA256,
        "kind": MODULE.HOSTED_FULL_KIND,
        "manifest_sha256": MODULE.HOSTED_FULL_MANIFEST_SHA256,
        "profile": "full",
        "schema": "rxls.render-parity-campaign.v1",
    }
    return {
        "campaign": campaign,
        "classifications": {"within_threshold": 800},
        "cohorts": {
            "all": adoption_cohort(800),
            "by_feature": {
                feature: adoption_cohort(count)
                for feature, count in MODULE.HOSTED_FULL_FEATURE_COUNTS.items()
            },
            "by_format": {
                "ods": adoption_cohort(200),
                "xls": adoption_cohort(200),
                "xlsb": adoption_cohort(200),
                "xlsx": adoption_cohort(200),
            },
        },
        "histograms": {
            "all": adoption_histogram_cohort(800),
            "by_feature": {
                feature: adoption_histogram_cohort(count)
                for feature, count in MODULE.HOSTED_FULL_FEATURE_COUNTS.items()
            },
            "by_format": {
                "ods": adoption_histogram_cohort(200),
                "xls": adoption_histogram_cohort(200),
                "xlsb": adoption_histogram_cohort(200),
                "xlsx": adoption_histogram_cohort(200),
            },
        },
        "groups": adoption_groups(),
        "comparable_files": 800,
        "configuration_sha256": "d" * 64,
        "input_files": 800,
        "input_set_sha256": MODULE.HOSTED_FULL_INPUT_SET_SHA256,
        "schema": MODULE.OBSERVED_CANDIDATE_SCHEMA,
        "statuses": {"compared": 800},
        "warning_counts": {},
    }


def legacy_adoption_baseline() -> dict[str, object]:
    baseline = adoption_baseline()
    del baseline["groups"]
    del baseline["histograms"]
    baseline["schema"] = MODULE.SCOPED_BASELINE_SCHEMA
    return baseline


def adoption_drift_limits(limit: int = 20_000) -> dict[str, int]:
    return {metric: limit for metric in MODULE.ADOPTION_SCORE_METRICS}


def set_histogram_values(
    cohort: dict[str, object],
    histogram_cohort: dict[str, object],
    metric_kind: str,
    metric: str,
    values: list[int],
) -> None:
    histogram = MODULE._histogram(values)
    histogram_cohort[metric_kind][metric] = histogram
    cohort[metric_kind][metric] = MODULE._distribution_from_histogram(
        histogram,
        score=metric_kind == "scores",
    )


def update_partition_values(
    baseline: dict[str, object],
    metric_kind: str,
    metric: str,
    values_by_format: dict[str, list[int]],
) -> None:
    groups = baseline["groups"]
    assert isinstance(groups, list)
    for format_name, values in sorted(values_by_format.items()):
        format_groups = [
            group for group in groups if group["format"] == format_name
        ]
        self_count = sum(group["workbooks"] for group in format_groups)
        if len(values) != self_count:
            raise AssertionError((format_name, len(values), self_count))
        offset = 0
        for group in format_groups:
            count = group["workbooks"]
            group_values = values[offset : offset + count]
            group[metric_kind][metric] = MODULE._histogram(group_values)
            offset += count
    cohorts, histograms = MODULE._certificate_views_from_groups(groups)
    baseline["cohorts"] = cohorts
    baseline["histograms"] = histograms


def constant_format_values(value: int) -> dict[str, list[int]]:
    return {
        format_name: [value] * count
        for format_name, count in MODULE.HOSTED_FULL_FORMAT_COUNTS.items()
    }


def evidence() -> dict[str, object]:
    files = []
    for index, format_name in enumerate(("xlsx", "ods")):
        files.append(
            {
                "classification": "within_threshold",
                "features": ["unicode-text"],
                "format": format_name,
                "metrics": {
                    "max_page_width_delta_pixels": 3,
                    "text_ink_f1_ppm": 800_000,
                },
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
            "measurement_toolchain": {
                "host_tools_identity_sha256": "0" * 64,
                "kind": "poppler",
                "pdffonts_sha256": "1" * 64,
                "pdfinfo_sha256": "2" * 64,
                "pdftoppm_sha256": "3" * 64,
                "pdftotext_sha256": "4" * 64,
            },
            "metric_policy": {"edge_luma_delta": 32},
            "oracle_lock": {"profile": "locked"},
            "renderer_binary": {"bytes": 1_234_567, "sha256": "5" * 64},
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
    def test_json_readers_are_bounded_regular_and_race_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            document = root / "document.json"
            document.write_bytes(b"{}")
            link = root / "document-link.json"
            with self.subTest(file_type="symlink"):
                _create_symlink_or_skip_privilege(self, link, document)
                with self.assertRaisesRegex(
                    MODULE.BaselineError, "evidence_unreadable"
                ):
                    MODULE.read_json(link, "evidence")
                with self.assertRaisesRegex(
                    MODULE.BaselineError, "campaign_manifest_unreadable"
                ):
                    MODULE.campaign_from_manifest(link)
            with self.assertRaisesRegex(
                MODULE.BaselineError, "evidence_unreadable"
            ):
                MODULE.read_json(root, "evidence")
            fifo = root / "document.fifo"
            real_open = MODULE.os.open
            nonblocking = getattr(MODULE.os, "O_NONBLOCK", 0)

            def guarded_open(
                path: object, flags: int, *args: object, **kwargs: object
            ) -> int:
                if nonblocking:
                    self.assertNotEqual(flags & nonblocking, 0)
                return real_open(path, flags, *args, **kwargs)

            if hasattr(MODULE.os, "mkfifo"):
                MODULE.os.mkfifo(fifo)
                with mock.patch.object(
                    MODULE.os,
                    "open",
                    side_effect=guarded_open,
                ), self.assertRaisesRegex(
                    MODULE.BaselineError, "evidence_unreadable"
                ):
                    MODULE.read_json(fifo, "evidence")
            else:
                fifo.write_bytes(b"{}")
                fifo_metadata_values = list(fifo.stat())
                fifo_metadata_values[0] = MODULE.stat.S_IFIFO | 0o600
                fifo_metadata = MODULE.os.stat_result(fifo_metadata_values)
                with mock.patch.object(
                    MODULE.os,
                    "open",
                    side_effect=guarded_open,
                ), mock.patch.object(
                    MODULE.os,
                    "fstat",
                    return_value=fifo_metadata,
                ), self.assertRaisesRegex(
                    MODULE.BaselineError, "evidence_unreadable"
                ):
                    MODULE.read_json(fifo, "evidence")

            document.write_bytes(b"0123456789")
            real_read = MODULE.os.read
            returned = 0

            def observed_read(descriptor: int, count: int) -> bytes:
                nonlocal returned
                chunk = real_read(descriptor, count)
                returned += len(chunk)
                return chunk

            with mock.patch.object(
                MODULE, "MAX_DOCUMENT_BYTES", 4
            ), mock.patch.object(
                MODULE.os, "read", side_effect=observed_read
            ), self.assertRaisesRegex(
                MODULE.BaselineError, "evidence_limit"
            ):
                MODULE.read_json(document, "evidence")
            self.assertEqual(returned, 5)

            for mutation in ("growth", "swap"):
                with self.subTest(mutation=mutation):
                    document.write_bytes(b"{}")
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
                                document.write_bytes(b"{} ")
                            else:
                                replacement.replace(document)
                        return chunk

                    with mock.patch.object(
                        MODULE.os,
                        "read",
                        side_effect=adversarial_read,
                    ), self.assertRaisesRegex(
                        MODULE.BaselineError, "evidence_unreadable"
                    ):
                        MODULE.read_json(document, "evidence")

    def test_baseline_excludes_paths_and_retains_identity_and_warning_counts(self) -> None:
        baseline = MODULE.derive_baseline(evidence())
        rendered = json.dumps(baseline, sort_keys=True)
        self.assertNotIn("private", rendered)
        self.assertEqual(baseline["input_files"], 2)
        self.assertEqual(baseline["warning_counts"], {"pagination_deferred": 2})
        self.assertEqual(baseline["comparable_files"], 2)

    def test_raw_status_and_classification_counts_are_authenticated(self) -> None:
        forged = evidence()
        forged["files"][0]["status"] = "different"
        forged["files"][0]["classification"] = "below_similarity_threshold"
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "evidence_summary_file_counts",
        ):
            MODULE.derive_baseline(forged)
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            evidence_path = root / "evidence.json"
            baseline_path = root / "baseline.json"
            report_path = root / "report.json"
            evidence_path.write_bytes(MODULE.canonical_bytes(forged))
            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--evidence",
                    str(evidence_path),
                    "--baseline",
                    str(baseline_path),
                    "--create",
                    "--report",
                    str(report_path),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            report = json.loads(report_path.read_text())
            payload = evidence_path.read_bytes()
            self.assertEqual(result.returncode, 2)
            self.assertEqual(
                report["source_evidence"],
                {
                    "bytes": len(payload),
                    "sha256": hashlib.sha256(payload).hexdigest(),
                },
            )

        honestly_failed = copy.deepcopy(forged)
        honestly_failed["summary"]["by_status"] = {
            "compared": 1,
            "different": 1,
        }
        honestly_failed["summary"]["by_classification"] = {
            "below_similarity_threshold": 1,
            "within_threshold": 1,
        }
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "evidence_not_full_success",
        ):
            MODULE.derive_baseline(honestly_failed)

        fabricated_skips = evidence()
        for row in fabricated_skips["files"]:
            row["status"] = "skipped"
            row["classification"] = "input_limit"
            del row["metrics"]
        fabricated_skips["summary"]["by_status"] = {"skipped": 2}
        fabricated_skips["summary"]["by_classification"] = {"input_limit": 2}
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "evidence_not_full_success",
        ):
            MODULE.derive_baseline(fabricated_skips)

    def test_artifact_vocabulary_rejects_paths_and_unbounded_features(self) -> None:
        mutations = []
        hostile_home_path = "/" + "Us" + "ers/alice/secret.xlsx"
        hostile_home_prefix = "/" + "Us" + "ers"

        status_path = evidence()
        status_path["files"][0]["status"] = "/private/status"
        status_path["summary"]["by_status"] = {
            "/private/status": 1,
            "compared": 1,
        }
        mutations.append(status_path)

        classification_path = evidence()
        classification_path["files"][0]["classification"] = hostile_home_path
        classification_path["summary"]["by_classification"] = {
            hostile_home_path: 1,
            "within_threshold": 1,
        }
        mutations.append(classification_path)

        feature_path = evidence()
        feature_path["files"][0]["features"] = ["/private/customer"]
        mutations.append(feature_path)

        too_many_features = evidence()
        too_many_features["files"][0]["features"] = [
            f"feature-{index:03d}" for index in range(257)
        ]
        mutations.append(too_many_features)

        metric_path = evidence()
        metric_path["summary"]["metric_cohorts"]["all"]["scores"][
            "/private/metric"
        ] = score(1)
        mutations.append(metric_path)

        for mutation in mutations:
            with self.subTest(mutation=mutation), self.assertRaises(
                MODULE.BaselineError,
            ) as caught:
                MODULE.derive_baseline(mutation)
            self.assertNotIn("/private", str(caught.exception))
            self.assertNotIn(hostile_home_prefix, str(caught.exception))

    def test_warning_aggregation_and_cardinality_are_bounded(self) -> None:
        overflow = evidence()
        for row in overflow["files"]:
            row["scenes"][0]["warnings"][0]["occurrences"] = MODULE.MAX_COUNT
        with self.assertRaisesRegex(MODULE.BaselineError, "evidence_warning"):
            MODULE.derive_baseline(overflow)

        zero = adoption_baseline()
        zero["warning_counts"] = {"pagination_deferred": 0}
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "baseline_warning_counts",
        ):
            MODULE.validate_observed_candidate(zero)

    def test_identical_and_strictly_better_candidates_pass(self) -> None:
        baseline = MODULE.derive_baseline(evidence())
        identical = MODULE.compare(baseline, copy.deepcopy(baseline))
        self.assertTrue(identical["passed"])

        better = copy.deepcopy(baseline)
        score_distribution = better["cohorts"]["all"]["scores"][
            "text_ink_f1_ppm"
        ]
        for key in ("max", "mean", "min", "p10"):
            score_distribution[key] += 1
        delta_distribution = better["cohorts"]["all"]["deltas"][
            "max_page_width_delta_pixels"
        ]
        for key in ("max", "mean", "min", "p50", "p90"):
            delta_distribution[key] -= 1
        better["warning_counts"] = {}
        self.assertTrue(MODULE.compare(baseline, better)["passed"])

    def test_score_delta_warning_classification_and_coverage_regressions_fail(self) -> None:
        baseline = MODULE.derive_baseline(evidence())
        candidate = copy.deepcopy(baseline)
        score_distribution = candidate["cohorts"]["all"]["scores"][
            "text_ink_f1_ppm"
        ]
        score_distribution["min"] -= 1
        score_distribution["p10"] -= 1
        delta_distribution = candidate["cohorts"]["all"]["deltas"][
            "max_page_width_delta_pixels"
        ]
        delta_distribution["p90"] += 1
        delta_distribution["max"] += 1
        delta_distribution["mean"] += 1
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

    def test_measurement_toolchain_and_renderer_are_baseline_identities(self) -> None:
        source = evidence()
        baseline = MODULE.derive_baseline(source)

        for key in (
            "host_tools_identity_sha256",
            "pdffonts_sha256",
            "pdfinfo_sha256",
            "pdftoppm_sha256",
            "pdftotext_sha256",
        ):
            changed = copy.deepcopy(source)
            changed["configuration"]["measurement_toolchain"][key] = "9" * 64
            self.assertNotEqual(
                MODULE.derive_baseline(changed)["configuration_sha256"],
                baseline["configuration_sha256"],
                key,
            )

        changed = copy.deepcopy(source)
        changed["configuration"]["renderer_binary"]["sha256"] = "8" * 64
        self.assertNotEqual(
            MODULE.derive_baseline(changed)["configuration_sha256"],
            baseline["configuration_sha256"],
        )

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

    def test_hosted_full_constants_match_the_canonical_generator(self) -> None:
        generator_script = ROOT / "scripts" / "generate-render-corpus.py"
        spec = importlib.util.spec_from_file_location(
            "canonical_render_corpus_generator",
            generator_script,
        )
        assert spec is not None and spec.loader is not None
        generator = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = generator
        spec.loader.exec_module(generator)
        cases = generator.profile_specs("full")
        format_counts: dict[str, int] = {}
        feature_counts: dict[str, int] = {}
        for case in cases:
            format_counts[case.format] = format_counts.get(case.format, 0) + 1
            for feature in case.features:
                feature_counts[feature] = feature_counts.get(feature, 0) + 1

        self.assertEqual(generator.GENERATOR, MODULE.HOSTED_FULL_GENERATOR)
        self.assertEqual(
            generator.GENERATOR_VERSION,
            MODULE.HOSTED_FULL_GENERATOR_VERSION,
        )
        self.assertEqual(format_counts, MODULE.HOSTED_FULL_FORMAT_COUNTS)
        self.assertEqual(feature_counts, MODULE.HOSTED_FULL_FEATURE_COUNTS)
        lattice = [
            {
                "case_id": case.case_id,
                "features": list(case.features),
                "format": case.format,
                "generator": generator.GENERATOR,
                "generator_version": generator.GENERATOR_VERSION,
                "seed": case.seed,
            }
            for case in sorted(cases, key=lambda case: case.case_id)
        ]
        self.assertEqual(
            MODULE.sha256_json(lattice),
            MODULE.HOSTED_FULL_LATTICE_SHA256,
        )
        self.assertEqual(
            MODULE.group_topology_sha256(adoption_groups()),
            MODULE.HOSTED_FULL_GROUP_TOPOLOGY_SHA256,
        )

    def test_hosted_full_rejects_feature_generator_and_manifest_mutations(
        self,
    ) -> None:
        mutations = []
        one_feature = adoption_baseline()
        one_feature["campaign"]["feature_counts"] = {"latin-text": 800}
        one_feature["cohorts"]["by_feature"] = {
            "latin-text": adoption_cohort(800)
        }
        mutations.append(one_feature)

        wrong_version = adoption_baseline()
        wrong_version["campaign"]["generator_version"] = "1.3.1"
        mutations.append(wrong_version)

        feature_move = adoption_baseline()
        feature_move["campaign"]["manifest_sha256"] = "a" * 64
        mutations.append(feature_move)

        wrong_input_set = adoption_baseline()
        wrong_input_set["campaign"]["input_set_sha256"] = "c" * 64
        wrong_input_set["input_set_sha256"] = "c" * 64
        mutations.append(wrong_input_set)

        for mutated in mutations:
            with self.subTest(
                campaign=mutated["campaign"]
            ), self.assertRaisesRegex(
                MODULE.BaselineError,
                "baseline_campaign_hosted_full_identity",
            ):
                MODULE.validate_observed_candidate(mutated)

    def test_hosted_manifest_lattice_rejects_aggregate_preserving_feature_move(
        self,
    ) -> None:
        generator_script = ROOT / "scripts" / "generate-render-corpus.py"
        spec = importlib.util.spec_from_file_location(
            "feature_move_render_corpus_generator",
            generator_script,
        )
        assert spec is not None and spec.loader is not None
        generator = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = generator
        spec.loader.exec_module(generator)
        cases = generator.profile_specs("full")
        files = [
            {
                "case_id": case.case_id,
                "features": list(case.features),
                "format": case.format,
                "generator": generator.GENERATOR,
                "generator_version": generator.GENERATOR_VERSION,
                "license": "MIT",
                "redistribution": "allowed",
                "render_redistributable": True,
                "rights_tier": "S",
                "seed": case.seed,
                "sha256": f"{index + 1:064x}",
                "source_redistributable": True,
            }
            for index, case in enumerate(cases)
        ]
        manifest = {
            "case_count": 800,
            "feature_counts": dict(MODULE.HOSTED_FULL_FEATURE_COUNTS),
            "files": files,
            "format_counts": dict(MODULE.HOSTED_FULL_FORMAT_COUNTS),
            "generator": generator.GENERATOR,
            "generator_version": generator.GENERATOR_VERSION,
            "license": "MIT",
            "profile": "full",
            "redistribution": "allowed",
            "render_redistributable": True,
            "rights_tier": "S",
            "schema_version": 1,
            "source_redistributable": True,
        }
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "manifest.json"
            path.write_text(json.dumps(manifest))
            with mock.patch.object(
                MODULE,
                "sha256_bytes",
                return_value=MODULE.HOSTED_FULL_MANIFEST_SHA256,
            ), mock.patch.object(
                MODULE,
                "_input_identity",
                return_value=(MODULE.HOSTED_FULL_INPUT_SET_SHA256, 800),
            ):
                pristine = MODULE.campaign_from_manifest(
                    path,
                    require_hosted_full_800=True,
                )
                self.assertEqual(pristine["kind"], MODULE.HOSTED_FULL_KIND)

                moved = copy.deepcopy(manifest)
                left = next(
                    index
                    for index, row in enumerate(moved["files"])
                    if row["format"] == "xlsx"
                    and row["features"] != moved["files"][0]["features"]
                )
                right = next(
                    index
                    for index in range(left + 1, len(moved["files"]))
                    if moved["files"][index]["format"] == "xlsx"
                    and moved["files"][index]["features"]
                    != moved["files"][left]["features"]
                )
                moved["files"][left]["features"], moved["files"][right][
                    "features"
                ] = (
                    moved["files"][right]["features"],
                    moved["files"][left]["features"],
                )
                path.write_text(json.dumps(moved))
                with self.assertRaisesRegex(
                    MODULE.BaselineError,
                    "campaign_not_hosted_full_800",
                ):
                    MODULE.campaign_from_manifest(
                        path,
                        require_hosted_full_800=True,
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
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "evidence_metric_cohorts",
        ):
            MODULE.derive_baseline(missing_feature, campaign)

        missing_metric = copy.deepcopy(source)
        del missing_metric["summary"]["metric_cohorts"]["by_format"]["ods"][
            "scores"
        ]["text_ink_f1_ppm"]
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "evidence_metric_cohorts",
        ):
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
            evidence_payload = evidence_path.read_bytes()

        self.assertEqual(result.returncode, 2)
        self.assertEqual(candidate["schema"], MODULE.SCOPED_BASELINE_SCHEMA)
        self.assertFalse(report["passed"])
        self.assertEqual(report["failures"], ["error:baseline_unreadable"])
        self.assertEqual(report["candidate_sha256"], MODULE.sha256_json(candidate))
        self.assertEqual(
            report["source_evidence"],
            {
                "bytes": len(evidence_payload),
                "sha256": hashlib.sha256(evidence_payload).hexdigest(),
            },
        )

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

        self.assertEqual(result.returncode, 2)
        self.assertIn("input_output_path_alias", result.stderr)
        self.assertFalse(report_path.exists())
        self.assertFalse(baseline_path.exists())

    def test_cli_rejects_all_input_output_aliases_without_overwriting(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            evidence_path = root / "evidence.json"
            baseline_path = root / "baseline.json"
            evidence_path.write_text(json.dumps(evidence()))
            baseline_path.write_bytes(
                MODULE.canonical_bytes(MODULE.derive_baseline(evidence()))
            )
            original_baseline = baseline_path.read_bytes()
            original_evidence = evidence_path.read_bytes()

            direct = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--evidence",
                    str(evidence_path),
                    "--baseline",
                    str(baseline_path),
                    "--report",
                    str(baseline_path),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(direct.returncode, 2)
            self.assertIn("input_output_path_alias", direct.stderr)
            self.assertEqual(baseline_path.read_bytes(), original_baseline)

            report_link = root / "report-link.json"
            with self.subTest(alias_type="symlink"):
                _create_symlink_or_skip_privilege(
                    self,
                    report_link,
                    baseline_path,
                )
                symlinked = subprocess.run(
                    [
                        sys.executable,
                        str(SCRIPT),
                        "--evidence",
                        str(evidence_path),
                        "--baseline",
                        str(baseline_path),
                        "--report",
                        str(report_link),
                    ],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(symlinked.returncode, 2)
                self.assertIn("input_output_path_alias", symlinked.stderr)
                self.assertEqual(baseline_path.read_bytes(), original_baseline)

            evidence_output = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--evidence",
                    str(evidence_path),
                    "--baseline",
                    str(baseline_path),
                    "--candidate-baseline",
                    str(evidence_path),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(evidence_output.returncode, 2)
            self.assertIn("input_output_path_alias", evidence_output.stderr)
            self.assertEqual(evidence_path.read_bytes(), original_evidence)

    def test_atomic_writer_ignores_predictable_temporary_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            output = root / "artifact.json"
            protected = root / "protected.txt"
            protected.write_bytes(b"do-not-touch")
            legacy_temporary = root / ".artifact.json.tmp"
            _create_symlink_or_skip_privilege(
                self,
                legacy_temporary,
                protected,
            )
            MODULE.write_atomic(output, b"new-payload")
            self.assertEqual(output.read_bytes(), b"new-payload")
            self.assertEqual(protected.read_bytes(), b"do-not-touch")
            self.assertTrue(legacy_temporary.is_symlink())

    def test_failure_report_write_error_converges_to_filesystem_error(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            evidence_path = root / "evidence.json"
            evidence_path.write_text(json.dumps(evidence()))
            arguments = MODULE.argparse.Namespace(
                baseline=root / "missing-baseline.json",
                campaign_manifest=None,
                candidate_baseline=None,
                create=False,
                evidence=evidence_path,
                report=root / "report.json",
                require_hosted_full_800=False,
            )
            stderr = io.StringIO()
            with mock.patch.object(
                MODULE,
                "parse_args",
                return_value=arguments,
            ), mock.patch.object(
                MODULE,
                "write_atomic",
                side_effect=OSError("injected write failure"),
            ), mock.patch.object(MODULE.sys, "stderr", stderr):
                status = MODULE.main()

        self.assertEqual(status, 2)
        self.assertEqual(
            stderr.getvalue(),
            "check-render-parity-baseline: filesystem_error\n",
        )

    def test_cli_strict_json_rejects_duplicates_nonfinite_utf8_and_depth(
        self,
    ) -> None:
        malformed_payloads = (
            b'{"schema":"first","schema":"second"}',
            b'{"value":NaN}',
            b'{"value":1.0}',
            b'{"value":1e10000}',
            b'{"value":' + (b"9" * 5_000) + b"}",
            b"\xff",
            ("[" * 2_000 + "]" * 2_000).encode(),
        )
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            baseline_path = root / "baseline.json"
            report_path = root / "report.json"
            for index, payload in enumerate(malformed_payloads):
                evidence_path = root / f"evidence-{index}.json"
                evidence_path.write_bytes(payload)
                result = subprocess.run(
                    [
                        sys.executable,
                        str(SCRIPT),
                        "--evidence",
                        str(evidence_path),
                        "--baseline",
                        str(baseline_path),
                        "--create",
                        "--report",
                        str(report_path),
                    ],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                with self.subTest(index=index):
                    self.assertEqual(result.returncode, 2)
                    self.assertNotIn("Traceback", result.stderr)
                    self.assertEqual(
                        result.stderr.splitlines(),
                        [
                            "check-render-parity-baseline: "
                            "evidence_invalid_json"
                        ],
                    )
                    self.assertNotIn(str(evidence_path), result.stderr)
                    report = json.loads(report_path.read_text())
                    self.assertEqual(
                        report["failures"],
                        ["error:evidence_invalid_json"],
                    )
                    self.assertFalse(baseline_path.exists())

            valid_evidence = root / "valid-evidence.json"
            valid_evidence.write_bytes(MODULE.canonical_bytes(evidence()))
            baseline = MODULE.canonical_bytes(MODULE.derive_baseline(evidence()))
            duplicate_baseline = (
                b'{"schema":"attacker",'
                + baseline.lstrip()[1:]
            )
            baseline_path.write_bytes(duplicate_baseline)
            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--evidence",
                    str(valid_evidence),
                    "--baseline",
                    str(baseline_path),
                    "--report",
                    str(report_path),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 2)
            self.assertNotIn("Traceback", result.stderr)
            self.assertEqual(
                json.loads(report_path.read_text())["failures"],
                ["error:baseline_invalid_json"],
            )

    def test_json_preflight_rejects_depth_complexity_and_number_tokens_before_decode(
        self,
    ) -> None:
        preflight_payloads = (
            b"[" * (MODULE.MAX_JSON_DEPTH + 1)
            + b"]" * (MODULE.MAX_JSON_DEPTH + 1),
            b'{"value":1.25}',
            b'{"value":1e10000}',
            b'{"value":' + (b"7" * 5_000) + b"}",
        )
        for payload in preflight_payloads:
            with self.subTest(payload_size=len(payload)), mock.patch.object(
                MODULE.json,
                "loads",
                side_effect=AssertionError("decoder must not run"),
            ), self.assertRaisesRegex(
                MODULE.BaselineError,
                "evidence_invalid_json",
            ):
                MODULE.parse_json_bytes(payload, "evidence")

        with mock.patch.object(
            MODULE,
            "MAX_JSON_NODES",
            3,
        ), mock.patch.object(
            MODULE.json,
            "loads",
            side_effect=AssertionError("decoder must not run"),
        ), self.assertRaisesRegex(
            MODULE.BaselineError,
            "evidence_invalid_json",
        ):
            MODULE.parse_json_bytes(b"[0,0,0,0]", "evidence")

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
            evidence_payload = evidence_path.read_bytes()

        self.assertEqual(create.returncode, 0, create.stderr)
        self.assertEqual(verify.returncode, 0, verify.stderr)
        self.assertTrue(report["passed"])
        self.assertEqual(
            report["source_evidence"],
            {
                "bytes": len(evidence_payload),
                "sha256": hashlib.sha256(evidence_payload).hexdigest(),
            },
        )
        self.assertNotIn("private", baseline_text)

    def test_distribution_domains_order_counts_and_bools_fail_closed(self) -> None:
        baseline = MODULE.derive_baseline(evidence())
        mutations = []

        negative = copy.deepcopy(baseline)
        negative["cohorts"]["all"]["deltas"][
            "max_page_width_delta_pixels"
        ]["min"] = -1
        mutations.append(negative)

        score_overflow = copy.deepcopy(baseline)
        score_overflow["cohorts"]["all"]["scores"]["text_ink_f1_ppm"][
            "max"
        ] = 1_000_001
        mutations.append(score_overflow)

        inverted = copy.deepcopy(baseline)
        inverted["cohorts"]["all"]["deltas"][
            "max_page_width_delta_pixels"
        ]["p50"] = 4
        mutations.append(inverted)

        wrong_count = copy.deepcopy(baseline)
        wrong_count["cohorts"]["all"]["scores"]["text_ink_f1_ppm"][
            "count"
        ] = 1
        mutations.append(wrong_count)

        boolean = copy.deepcopy(baseline)
        boolean["cohorts"]["all"]["scores"]["text_ink_f1_ppm"][
            "mean"
        ] = True
        mutations.append(boolean)

        for mutated in mutations:
            with self.subTest(mutated=mutated), self.assertRaises(
                MODULE.BaselineError
            ):
                MODULE.validate_baseline(mutated)

    def test_impossible_nearest_rank_score_and_delta_tuples_are_rejected(
        self,
    ) -> None:
        impossible_score = adoption_baseline()
        impossible_score["cohorts"]["all"]["scores"][
            "text_ink_f1_ppm"
        ].update(
            {
                "count": 800,
                "max": 1_000_000,
                "mean": 0,
                "min": 0,
                "p10": 1_000_000,
            }
        )
        impossible_delta = adoption_baseline()
        impossible_delta["cohorts"]["all"]["deltas"][
            "max_page_width_delta_pixels"
        ].update(
            {
                "count": 800,
                "max": 1_000_000,
                "mean": 0,
                "min": 0,
                "p50": 1_000_000,
                "p90": 1_000_000,
            }
        )

        for mutated in (impossible_score, impossible_delta):
            with self.subTest(mutated=mutated), self.assertRaisesRegex(
                MODULE.BaselineError,
                "evidence_distribution_feasibility",
            ):
                MODULE.validate_observed_candidate(mutated)

    def test_small_n_histograms_match_producer_and_enforce_endpoint_aliases(
        self,
    ) -> None:
        for count in range(1, 33):
            values = list(range(count))
            histogram = MODULE._histogram(values)
            for score_kind in (True, False):
                with self.subTest(count=count, score=score_kind):
                    distribution = MODULE._distribution_from_histogram(
                        histogram,
                        score=score_kind,
                    )
                    self.assertEqual(
                        MODULE._validate_distribution(
                            distribution,
                            score=score_kind,
                            expected_count=count,
                        ),
                        distribution,
                    )

            score_distribution = MODULE._distribution_from_histogram(
                histogram,
                score=True,
            )
            if (count + 9) // 10 == 1 and count > 1:
                mutated = dict(score_distribution)
                mutated["p10"] = min(mutated["max"], mutated["min"] + 1)
                if mutated["p10"] != mutated["min"]:
                    with self.assertRaisesRegex(
                        MODULE.BaselineError,
                        "evidence_distribution_feasibility",
                    ):
                        MODULE._validate_distribution(
                            mutated,
                            score=True,
                            expected_count=count,
                        )

            delta_distribution = MODULE._distribution_from_histogram(
                histogram,
                score=False,
            )
            if (count + 1) // 2 == 1 and count > 1:
                mutated = dict(delta_distribution)
                mutated["p50"] = min(mutated["p90"], mutated["min"] + 1)
                if mutated["p50"] != mutated["min"]:
                    with self.assertRaisesRegex(
                        MODULE.BaselineError,
                        "evidence_distribution_feasibility",
                    ):
                        MODULE._validate_distribution(
                            mutated,
                            score=False,
                            expected_count=count,
                        )
            if (9 * count + 9) // 10 == count and count > 1:
                mutated = dict(delta_distribution)
                mutated["p90"] = max(mutated["p50"], mutated["max"] - 1)
                if mutated["p90"] != mutated["max"]:
                    with self.assertRaisesRegex(
                        MODULE.BaselineError,
                        "evidence_distribution_feasibility",
                    ):
                        MODULE._validate_distribution(
                            mutated,
                            score=False,
                            expected_count=count,
                        )

    def test_scoped_format_partition_rejects_incompatible_integer_means(
        self,
    ) -> None:
        baseline = legacy_adoption_baseline()
        metric = "text_ink_f1_ppm"
        baseline["cohorts"]["all"]["scores"][metric].update(
            {
                "max": 1_000_000,
                "mean": 500_000,
                "min": 0,
                "p10": 0,
            }
        )
        for format_name, value in {
            "ods": 0,
            "xls": 1_000_000,
            "xlsb": 1_000_000,
            "xlsx": 1_000_000,
        }.items():
            baseline["cohorts"]["by_format"][format_name]["scores"][
                metric
            ] = score(value, 200)
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "campaign_by_format_partition",
        ):
            MODULE.validate_baseline(baseline)

    def test_certified_candidate_rejects_cross_format_quantile_forgery(
        self,
    ) -> None:
        candidate = adoption_baseline()
        metric = "text_ink_f1_ppm"
        values_by_format = {
            "ods": [0] * 200,
            "xls": [1] * 200,
            "xlsb": [2] * 200,
            "xlsx": [3] * 200,
        }
        update_partition_values(
            candidate,
            "scores",
            metric,
            values_by_format,
        )
        candidate["cohorts"]["all"]["scores"][metric]["p10"] = 1

        legacy_view = MODULE._baseline_view(candidate)
        self.assertEqual(
            MODULE.validate_baseline(legacy_view),
            legacy_view,
        )
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "candidate_group_summary",
        ):
            MODULE.validate_observed_candidate(candidate)

    def test_certified_candidate_requires_exact_format_histogram_sum(
        self,
    ) -> None:
        candidate = adoption_baseline()
        metric = "text_ink_f1_ppm"
        update_partition_values(
            candidate,
            "scores",
            metric,
            {
                "ods": [0] * 200,
                "xls": [1] * 200,
                "xlsb": [2] * 200,
                "xlsx": [3] * 200,
            },
        )
        candidate["histograms"]["all"]["scores"][metric] = [
            [0, 199],
            [1, 201],
            [2, 200],
            [3, 200],
        ]
        candidate["cohorts"]["all"]["scores"][metric] = (
            MODULE._distribution_from_histogram(
                candidate["histograms"]["all"]["scores"][metric],
                score=True,
            )
        )
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "candidate_histogram_format_partition",
        ):
            MODULE.validate_observed_candidate(candidate)

    def test_feature_histogram_must_recompute_its_own_summary(self) -> None:
        candidate = adoption_baseline()
        feature = "chart"
        metric = "text_ink_f1_ppm"
        candidate["histograms"]["by_feature"][feature]["scores"][metric] = [
            [799_999, MODULE.HOSTED_FULL_FEATURE_COUNTS[feature]]
        ]
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "candidate_histogram_summary",
        ):
            MODULE.validate_observed_candidate(candidate)

    def test_correlated_feature_summary_forgery_is_rejected_by_groups(self) -> None:
        candidate = adoption_baseline()
        feature = "chart"
        metric = "text_ink_f1_ppm"
        count = MODULE.HOSTED_FULL_FEATURE_COUNTS[feature]
        forged_histogram = [[790_000, count]]
        candidate["histograms"]["by_feature"][feature]["scores"][metric] = (
            forged_histogram
        )
        candidate["cohorts"]["by_feature"][feature]["scores"][metric] = (
            MODULE._distribution_from_histogram(
                forged_histogram,
                score=True,
            )
        )
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "candidate_group_summary",
        ):
            MODULE.validate_observed_candidate(candidate)

    def test_observed_candidate_requires_exact_full_success_maps(self) -> None:
        candidate = adoption_baseline()
        envelope = MODULE.conservative_adoption_baseline(
            candidate,
            copy.deepcopy(candidate),
            max_score_drift_ppm=adoption_drift_limits(),
        )
        for field in ("statuses", "classifications"):
            mutated = copy.deepcopy(candidate)
            mutated[field] = {}
            with self.subTest(field=field), self.assertRaisesRegex(
                MODULE.BaselineError,
                "candidate_full_coverage",
            ):
                MODULE.validate_observed_candidate(mutated)
            with self.subTest(compare_field=field), self.assertRaisesRegex(
                MODULE.BaselineError,
                "candidate_full_coverage",
            ):
                MODULE.compare(envelope, mutated)

    def test_hosted_candidate_certificates_are_raw_derived_and_path_neutral(
        self,
    ) -> None:
        generator_script = ROOT / "scripts" / "generate-render-corpus.py"
        spec = importlib.util.spec_from_file_location(
            "raw_certificate_render_corpus_generator",
            generator_script,
        )
        assert spec is not None and spec.loader is not None
        generator = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = generator
        spec.loader.exec_module(generator)

        files = []
        metrics = {
            **{
                metric: 800_000
                for metric in MODULE.EXPECTED_SCORE_METRICS
            },
            **{
                metric: 3
                for metric in MODULE.EXPECTED_DELTA_METRICS
            },
            "foreground_libreoffice_pixels": 1,
            "foreground_rxls_pixels": 1,
            "semantic_comparable": 1,
            "semantic_token_libreoffice_items": 1,
            "semantic_token_rxls_items": 1,
        }
        for index, case in enumerate(generator.profile_specs("full")):
            files.append(
                {
                    "classification": "within_threshold",
                    "features": list(case.features),
                    "format": case.format,
                    "metrics": dict(metrics),
                    "path": f"/private/corpus/{case.case_id}",
                    "rights_tier": "S",
                    "scenes": [],
                    "sha256": f"{index + 1:064x}",
                    "status": "compared",
                }
            )
        _, input_files = MODULE._input_identity(files)
        self.assertEqual(input_files, 800)
        campaign = copy.deepcopy(adoption_baseline()["campaign"])
        source = {
            "configuration": {},
            "files": files,
            "mode": "compare",
            "schema": MODULE.EVIDENCE_SCHEMA,
            "summary": {
                "by_classification": {"within_threshold": 800},
                "by_status": {"compared": 800},
                "metric_cohorts": adoption_baseline()["cohorts"],
            },
        }

        with mock.patch.object(
            MODULE,
            "_input_identity",
            return_value=(MODULE.HOSTED_FULL_INPUT_SET_SHA256, 800),
        ):
            candidate = MODULE.derive_baseline(source, campaign)
            self.assertEqual(
                candidate["schema"],
                MODULE.OBSERVED_CANDIDATE_SCHEMA,
            )
            self.assertEqual(
                MODULE.validate_observed_candidate(candidate),
                candidate,
            )
            self.assertNotIn(
                "/private/corpus",
                json.dumps(candidate, sort_keys=True),
            )

            reordered = copy.deepcopy(source)
            reordered["files"].reverse()
            self.assertEqual(
                MODULE.derive_baseline(reordered, campaign),
                candidate,
            )

            tampered = copy.deepcopy(source)
            metric = "text_ink_f1_ppm"
            for cohort_value in (
                tampered["summary"]["metric_cohorts"]["all"],
                *tampered["summary"]["metric_cohorts"]["by_format"].values(),
            ):
                cohort_value["scores"][metric] = score(
                    799_999,
                    cohort_value["comparable_workbooks"],
                )
            with self.assertRaisesRegex(
                MODULE.BaselineError,
                "evidence_metric_cohorts",
            ):
                MODULE.derive_baseline(tampered, campaign)

    def test_compare_rejects_new_metric_and_cohort(self) -> None:
        baseline = MODULE.derive_baseline(evidence())
        candidate = copy.deepcopy(baseline)
        candidate["cohorts"]["all"]["scores"]["future_score_ppm"] = score(
            900_000
        )
        candidate["cohorts"]["by_feature"]["future-feature"] = cohort()
        report = MODULE.compare(baseline, candidate)
        self.assertFalse(report["passed"])
        self.assertIn("all:new_score:future_score_ppm", report["failures"])
        self.assertIn("by_feature:new:future-feature", report["failures"])

    def test_adoption_is_order_independent_and_uses_only_real_gate_bounds(
        self,
    ) -> None:
        first = adoption_baseline()
        update_partition_values(
            first,
            "scores",
            "text_ink_f1_ppm",
            constant_format_values(60),
        )
        second = copy.deepcopy(first)

        def counterexample_values(count: int) -> list[int]:
            rank = (count + 9) // 10
            fixed = [40, *([45] * (rank - 1)), 80]
            remaining_count = count - len(fixed)
            remaining_sum = count * 60 - sum(fixed)
            quotient, remainder = divmod(remaining_sum, remaining_count)
            return sorted(
                [
                    *fixed,
                    *([quotient] * (remaining_count - remainder)),
                    *([quotient + 1] * remainder),
                ]
            )

        update_partition_values(
            second,
            "scores",
            "text_ink_f1_ppm",
            {
                format_name: counterexample_values(count)
                for format_name, count in MODULE.HOSTED_FULL_FORMAT_COUNTS.items()
            },
        )

        forward = MODULE.conservative_adoption_baseline(
            first,
            second,
            max_score_drift_ppm=adoption_drift_limits(),
        )
        reverse = MODULE.conservative_adoption_baseline(
            second,
            first,
            max_score_drift_ppm=adoption_drift_limits(),
        )
        self.assertEqual(forward, reverse)
        self.assertEqual(
            forward["cohorts"]["all"]["scores"]["text_ink_f1_ppm"],
            {
                "count": 800,
                "mean": 60,
                "p10": 45,
            },
        )
        self.assertEqual(forward["schema"], MODULE.RATCHET_ENVELOPE_SCHEMA)
        self.assertNotIn("histograms", forward)
        self.assertNotIn(
            "min",
            forward["cohorts"]["all"]["scores"]["text_ink_f1_ppm"],
        )
        self.assertTrue(MODULE.compare(forward, first)["passed"])
        self.assertTrue(MODULE.compare(forward, second)["passed"])

    def test_adoption_rejects_unbounded_score_delta_and_excessive_drift(
        self,
    ) -> None:
        baseline = adoption_baseline()

        unbounded_score = copy.deepcopy(baseline)
        update_partition_values(
            unbounded_score,
            "scores",
            "semantic_token_f1_ppm",
            {
                format_name: [799_999, *([800_000] * (count - 1))]
                for format_name, count in MODULE.HOSTED_FULL_FORMAT_COUNTS.items()
            },
        )
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "adoption_unbounded_group_drift",
        ):
            MODULE.conservative_adoption_baseline(
                baseline,
                unbounded_score,
                max_score_drift_ppm=adoption_drift_limits(),
            )

    def test_adoption_checks_group_drift_even_when_all_marginals_match(
        self,
    ) -> None:
        first = adoption_baseline()
        second = copy.deepcopy(first)
        metric = "text_ink_f1_ppm"
        feature_sets = (
            {
                "border",
                "cell-fill",
                "chinese-text",
                "column-width",
                "date-format",
                "latin-text",
                "noto-ofl-font",
                "number-cell",
                "right-to-left-layout",
                "row-height",
                "rtl-text",
                "unicode-text",
                "wrapped-text",
            },
            {
                "border",
                "date-format",
                "formula-cached",
                "japanese-text",
                "latin-text",
                "merged-cells",
                "noto-ofl-font",
                "number-cell",
                "percent-format",
                "rtl-text",
                "unicode-text",
            },
            {
                "border",
                "cell-fill",
                "chinese-text",
                "column-width",
                "date-format",
                "latin-text",
                "noto-ofl-font",
                "number-cell",
                "row-height",
                "rtl-text",
                "unicode-text",
            },
            {
                "border",
                "date-format",
                "formula-cached",
                "japanese-text",
                "latin-text",
                "merged-cells",
                "noto-ofl-font",
                "number-cell",
                "percent-format",
                "right-to-left-layout",
                "rtl-text",
                "unicode-text",
                "wrapped-text",
            },
        )

        def selected_groups(candidate: dict[str, object]) -> list[dict[str, object]]:
            return [
                next(
                    group
                    for group in candidate["groups"]
                    if group["format"] == "ods"
                    and set(group["features"]) == features
                )
                for features in feature_sets
            ]

        first_groups = selected_groups(first)
        second_groups = selected_groups(second)
        for index, (left_group, right_group) in enumerate(
            zip(first_groups, second_groups, strict=True)
        ):
            self.assertEqual(left_group["workbooks"], 6)
            left_value = 0 if index < 2 else 1_000_000
            right_value = 1_000_000 - left_value
            left_group["scores"][metric] = [[left_value, 6]]
            right_group["scores"][metric] = [[right_value, 6]]
        for candidate in (first, second):
            candidate["cohorts"], candidate["histograms"] = (
                MODULE._certificate_views_from_groups(candidate["groups"])
            )
            MODULE.validate_observed_candidate(candidate)

        self.assertEqual(first["cohorts"], second["cohorts"])
        self.assertEqual(first["histograms"], second["histograms"])
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "adoption_group_drift_threshold",
        ):
            MODULE.conservative_adoption_baseline(
                first,
                second,
                max_score_drift_ppm=adoption_drift_limits(0),
            )

        baseline = adoption_baseline()
        delta_drift = copy.deepcopy(baseline)
        update_partition_values(
            delta_drift,
            "deltas",
            "max_page_width_delta_pixels",
            constant_format_values(4),
        )
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "adoption_unbounded_group_drift",
        ):
            MODULE.conservative_adoption_baseline(
                baseline,
                delta_drift,
                max_score_drift_ppm=adoption_drift_limits(),
            )

        excessive = copy.deepcopy(baseline)
        update_partition_values(
            excessive,
            "scores",
            "text_ink_f1_ppm",
            constant_format_values(770_000),
        )
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "adoption_group_drift_threshold",
        ):
            MODULE.conservative_adoption_baseline(
                baseline,
                excessive,
                max_score_drift_ppm=adoption_drift_limits(),
            )

    def test_adoption_rejects_identity_and_metric_topology_changes(self) -> None:
        baseline = adoption_baseline()
        changed_identity = copy.deepcopy(baseline)
        changed_identity["configuration_sha256"] = "9" * 64
        with self.assertRaisesRegex(MODULE.BaselineError, "adoption_invariant"):
            MODULE.conservative_adoption_baseline(
                baseline,
                changed_identity,
                max_score_drift_ppm=adoption_drift_limits(),
            )

        changed_topology = copy.deepcopy(baseline)
        for cohort_value in (
            changed_topology["cohorts"]["all"],
            *changed_topology["cohorts"]["by_feature"].values(),
            *changed_topology["cohorts"]["by_format"].values(),
        ):
            cohort_value["scores"]["future_score_ppm"] = score(
                900_000,
                cohort_value["comparable_workbooks"],
            )
        for cohort_value, histogram_value in (
            (
                changed_topology["cohorts"]["all"],
                changed_topology["histograms"]["all"],
            ),
            *[
                (
                    changed_topology["cohorts"]["by_feature"][name],
                    changed_topology["histograms"]["by_feature"][name],
                )
                for name in changed_topology["cohorts"]["by_feature"]
            ],
            *[
                (
                    changed_topology["cohorts"]["by_format"][name],
                    changed_topology["histograms"]["by_format"][name],
                )
                for name in changed_topology["cohorts"]["by_format"]
            ],
        ):
            histogram_value["scores"]["future_score_ppm"] = [
                [900_000, cohort_value["comparable_workbooks"]]
            ]
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "candidate_group_summary",
        ):
            MODULE.conservative_adoption_baseline(
                baseline,
                changed_topology,
                max_score_drift_ppm=adoption_drift_limits(),
            )

    def test_adoption_rejects_sparse_coverage_and_drift_above_observed_maximum(
        self,
    ) -> None:
        baseline = adoption_baseline()
        sparse = copy.deepcopy(baseline)
        sparse["comparable_files"] = 1
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "candidate_full_coverage",
        ):
            MODULE.conservative_adoption_baseline(
                baseline,
                sparse,
                max_score_drift_ppm=adoption_drift_limits(),
            )

        disconnected_all = copy.deepcopy(baseline)
        disconnected_all["cohorts"]["all"]["workbooks"] = 1
        disconnected_all["cohorts"]["all"]["comparable_workbooks"] = 1
        for metric_kind in ("scores", "deltas"):
            for distribution in disconnected_all["cohorts"]["all"][
                metric_kind
            ].values():
                distribution["count"] = 1
        for metric_kind in ("scores", "deltas"):
            for histogram in disconnected_all["histograms"]["all"][
                metric_kind
            ].values():
                histogram[0][1] = 1
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "campaign_all_cohort",
        ):
            MODULE.conservative_adoption_baseline(
                baseline,
                disconnected_all,
                max_score_drift_ppm=adoption_drift_limits(),
            )

        drifted = copy.deepcopy(baseline)
        update_partition_values(
            drifted,
            "scores",
            "text_ink_f1_ppm",
            {
                format_name: [
                    *([799_999] * ((count + 9) // 10)),
                    *([800_000] * (count - (count + 9) // 10)),
                ]
                for format_name, count in MODULE.HOSTED_FULL_FORMAT_COUNTS.items()
            },
        )
        with self.assertRaisesRegex(
            MODULE.BaselineError,
            "adoption_group_drift_threshold",
        ):
            MODULE.conservative_adoption_baseline(
                baseline,
                drifted,
                max_score_drift_ppm=adoption_drift_limits(0),
            )


if __name__ == "__main__":
    unittest.main()
