#!/usr/bin/env python3
"""Tests for npm tag Render Oracle prerequisite evidence."""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import warnings
import zipfile


ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "scripts" / "check_render_oracle_release_evidence.py"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "render-package-release.yml"
CORPUS_GENERATOR = ROOT / "scripts" / "generate-render-corpus.py"
FAILURE_SUMMARY_TEST_SUPPORT = (
    ROOT / "scripts" / "test_summarize_render_oracle_failure.py"
)


def _load():
    spec = importlib.util.spec_from_file_location(
        "check_render_oracle_release_evidence", CHECKER
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _load_corpus_generator():
    spec = importlib.util.spec_from_file_location(
        "rxls_release_evidence_corpus_generator",
        CORPUS_GENERATOR,
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _load_failure_summary_test_support():
    spec = importlib.util.spec_from_file_location(
        "rxls_release_evidence_failure_summary_test_support",
        FAILURE_SUMMARY_TEST_SUPPORT,
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class _FakeResponse:
    def __init__(
        self,
        payload: bytes,
        *,
        status: int,
        headers: dict[str, str],
        url: str,
    ) -> None:
        self._stream = io.BytesIO(payload)
        self.status = status
        self.headers = headers
        self._url = url

    def read(self, size: int = -1) -> bytes:
        return self._stream.read(size)

    def close(self) -> None:
        self._stream.close()

    def geturl(self) -> str:
        return self._url


class _FakeOpener:
    def __init__(self, response: _FakeResponse) -> None:
        self.response = response
        self.requests = []

    def open(self, request, timeout: int) -> _FakeResponse:
        self.requests.append((request, timeout))
        return self.response


class RenderOracleReleaseEvidenceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.checker = _load()
        cls.corpus_generator = _load_corpus_generator()
        cls.head_sha = "a" * 40

    def _write(self, path: Path, value: object) -> bytes:
        payload = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()
        path.write_bytes(payload)
        return payload

    def test_failure_summary_validator_is_bound_private_and_fail_closed(
        self,
    ) -> None:
        summarizer = self.checker._load_failure_summarizer()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            path = root / self.checker.FAILURE_SUMMARY_NAME
            summary = summarizer.rejected_summary(
                profile="pilot",
                baseline_mode="verify",
                head_sha=self.head_sha,
            )
            path.write_bytes(summarizer._json(summary))

            validated = self.checker.validate_failure_summary(
                path,
                head_sha=self.head_sha,
                profile="pilot",
                baseline_mode="verify",
            )
            self.assertEqual(
                validated["schema"],
                self.checker.FAILURE_SUMMARY_SCHEMA,
            )
            self.assertEqual(
                validated["ingestion"]["status"], "rejected"
            )

            mutations = []
            value = copy.deepcopy(summary)
            value["source_url"] = "https://private.invalid/corpus.xlsx"
            mutations.append(value)
            value = copy.deepcopy(summary)
            value["schema"] = "rxls.render-oracle-failure-summary.v9"
            mutations.append(value)
            value = copy.deepcopy(summary)
            value["reports"][0]["case_diagnostics"]["cases"] = [
                {
                    "case_id": "a" * 64,
                    "cell_contents": "private workbook text",
                }
            ]
            mutations.append(value)
            for index, value in enumerate(mutations):
                candidate = (
                    root
                    / f"candidate-{index}"
                    / self.checker.FAILURE_SUMMARY_NAME
                )
                candidate.parent.mkdir()
                candidate.write_bytes(summarizer._json(value))
                with self.assertRaises(self.checker.EvidenceError):
                    self.checker.validate_failure_summary(
                        candidate,
                        head_sha=self.head_sha,
                        profile="pilot",
                        baseline_mode="verify",
                    )

            noncanonical = (
                root
                / "noncanonical"
                / self.checker.FAILURE_SUMMARY_NAME
            )
            noncanonical.parent.mkdir()
            noncanonical.write_text(
                json.dumps(summary, sort_keys=False),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "failure_summary_canonical",
            ):
                self.checker.validate_failure_summary(
                    noncanonical,
                    head_sha=self.head_sha,
                    profile="pilot",
                    baseline_mode="verify",
                )

            oversized = (
                root
                / "oversized"
                / self.checker.FAILURE_SUMMARY_NAME
            )
            oversized.parent.mkdir()
            with oversized.open("wb") as output:
                output.seek(
                    self.checker.MAX_FAILURE_SUMMARY_BYTES
                )
                output.write(b"\n")
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "failure_summary_size",
            ):
                self.checker.validate_failure_summary(
                    oversized,
                    head_sha=self.head_sha,
                    profile="pilot",
                    baseline_mode="verify",
                )

    def test_failure_summary_consumer_recomputes_case_bindings(
        self,
    ) -> None:
        support = _load_failure_summary_test_support()
        summarizer = self.checker._load_failure_summarizer()
        summary = support._summarize_pilot(support._pilot_rows())
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            accepted = root / "accepted" / self.checker.FAILURE_SUMMARY_NAME
            accepted.parent.mkdir()
            accepted.write_bytes(summarizer._json(summary))
            self.checker.validate_failure_summary(
                accepted,
                head_sha=self.head_sha,
                profile="pilot",
                baseline_mode="verify",
            )

            mutations = {}
            value = copy.deepcopy(summary)
            value["case_id_policy"]["algorithm"] = "sha256"
            mutations["case-id-policy"] = value

            value = copy.deepcopy(summary)
            diagnostics = value["reports"][1]["case_diagnostics"]
            format_name = next(
                iter(diagnostics["available_cases_by_format"])
            )
            diagnostics["available_cases_by_format"][format_name] += 1
            mutations["available-format-count"] = value

            value = copy.deepcopy(summary)
            case = value["reports"][1]["case_diagnostics"]["cases"][0]
            case["format"] = (
                "ods" if case["format"] != "ods" else "xlsx"
            )
            mutations["case-format"] = value

            value = copy.deepcopy(summary)
            value["reports"][1]["case_diagnostics"]["cases"][0][
                "raster"
            ]["similarity_ppm"] = 0
            mutations["case-raster"] = value

            value = copy.deepcopy(summary)
            axis = value["reports"][1]["case_diagnostics"]["cases"][
                0
            ]["page_box"]["by_axis"]["width"]
            axis.update(
                {
                    "max_delta_micropoints": 1,
                    "min_delta_micropoints": 1,
                    "nonzero_pages": 1,
                    "sum_delta_micropoints": 1,
                }
            )
            mutations["case-page-box"] = value

            for name, candidate_value in mutations.items():
                with self.subTest(name=name):
                    candidate = (
                        root
                        / name
                        / self.checker.FAILURE_SUMMARY_NAME
                    )
                    candidate.parent.mkdir()
                    candidate.write_bytes(
                        summarizer._json(candidate_value)
                    )
                    with self.assertRaisesRegex(
                        self.checker.EvidenceError,
                        "failure_summary_schema",
                    ):
                        self.checker.validate_failure_summary(
                            candidate,
                            head_sha=self.head_sha,
                            profile="pilot",
                            baseline_mode="verify",
                        )

    def test_rejected_cli_evidence_remains_valid_for_immediate_upload(
        self,
    ) -> None:
        summarizer = self.checker._load_failure_summarizer()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            hosted = root / "hosted"
            hosted.mkdir()
            (hosted / "parity-report-a.json").write_text(
                '{"schema":"unreviewed"}\n',
                encoding="utf-8",
            )
            output = (
                root
                / "failure"
                / self.checker.FAILURE_SUMMARY_NAME
            )
            stderr = io.StringIO()
            with mock.patch.object(
                summarizer.sys, "stderr", stderr
            ):
                result = summarizer.main(
                    [
                        "--input-root",
                        str(hosted),
                        "--profile",
                        "pilot",
                        "--baseline-mode",
                        "verify",
                        "--head-sha",
                        self.head_sha,
                        "--output",
                        str(output),
                    ]
                )
            self.assertEqual(result, 0)
            self.assertEqual(
                stderr.getvalue(),
                "render-oracle-failure-summary: "
                "unsafe_or_incomplete_reports_rejected\n",
            )
            validated = self.checker.validate_failure_summary(
                output,
                head_sha=self.head_sha,
                profile="pilot",
                baseline_mode="verify",
            )
            self.assertEqual(
                validated["ingestion"]["status"],
                "rejected",
            )

    def test_hosted_full_generator_derives_each_authoritative_identity(self) -> None:
        manifest, cases = self.corpus_generator.materialize("full")
        manifest_payload = self.corpus_generator._json_bytes(manifest)
        rows = manifest["files"]
        self.assertIsInstance(rows, list)

        campaign_identities = sorted(
            (
                {
                    "features": row["features"],
                    "format": row["format"],
                    "rights_tier": row["rights_tier"],
                    "sha256": row["sha256"],
                }
                for row in rows
            ),
            key=lambda row: (
                row["sha256"],
                row["format"],
                row["rights_tier"],
                row["features"],
            ),
        )
        binding_inputs = sorted(row["sha256"] for row in rows)
        lattice = [
            {
                "case_id": spec.case_id,
                "features": list(spec.features),
                "format": spec.format,
                "generator": self.corpus_generator.GENERATOR,
                "generator_version": self.corpus_generator.GENERATOR_VERSION,
                "seed": spec.seed,
            }
            for spec, _ in sorted(cases, key=lambda item: item[0].case_id)
        ]
        group_counts: dict[tuple[str, tuple[str, ...]], int] = {}
        for row in rows:
            key = (row["format"], tuple(row["features"]))
            group_counts[key] = group_counts.get(key, 0) + 1
        topology = [
            {
                "features": list(features),
                "format": format_name,
                "workbooks": count,
            }
            for (format_name, features), count in sorted(group_counts.items())
        ]

        def canonical(value: object) -> bytes:
            return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()

        baseline_checker = self.checker._load_baseline_checker()
        self.assertEqual(
            hashlib.sha256(manifest_payload).hexdigest(),
            self.checker.EXPECTED_HOSTED_FULL_MANIFEST_SHA256,
        )
        self.assertEqual(
            hashlib.sha256(canonical(campaign_identities)).hexdigest(),
            self.checker.EXPECTED_HOSTED_FULL_INPUT_SET_SHA256,
        )
        self.assertEqual(
            hashlib.sha256(canonical(binding_inputs)).hexdigest(),
            self.checker.EXPECTED_HOSTED_FULL_BINDING_INPUT_SET_SHA256,
        )
        self.assertNotEqual(
            self.checker.EXPECTED_HOSTED_FULL_INPUT_SET_SHA256,
            self.checker.EXPECTED_HOSTED_FULL_BINDING_INPUT_SET_SHA256,
        )
        self.assertEqual(
            hashlib.sha256(canonical(lattice)).hexdigest(),
            baseline_checker.HOSTED_FULL_LATTICE_SHA256,
        )
        self.assertEqual(
            hashlib.sha256(canonical(topology)).hexdigest(),
            self.checker.EXPECTED_HOSTED_FULL_GROUP_TOPOLOGY_SHA256,
        )

    def _archive(
        self,
        artifact: Path,
        archive_path: Path,
        *,
        renamed_member: tuple[str, str] | None = None,
        duplicate_member: str | None = None,
        symlink_member: str | None = None,
        compression: int = zipfile.ZIP_DEFLATED,
    ) -> tuple[int, str]:
        with zipfile.ZipFile(archive_path, "w") as archive:
            for path in sorted(artifact.iterdir()):
                name = path.name
                if renamed_member is not None and name == renamed_member[0]:
                    name = renamed_member[1]
                info = zipfile.ZipInfo(name, date_time=(2026, 7, 13, 0, 0, 0))
                info.create_system = 3
                info.compress_type = compression
                if path.name == symlink_member:
                    info.external_attr = (stat.S_IFLNK | 0o777) << 16
                else:
                    info.external_attr = (stat.S_IFREG | 0o600) << 16
                archive.writestr(info, path.read_bytes())
            if duplicate_member is not None:
                duplicate = zipfile.ZipInfo(
                    duplicate_member,
                    date_time=(2026, 7, 13, 0, 0, 0),
                )
                duplicate.create_system = 3
                duplicate.compress_type = compression
                duplicate.external_attr = (stat.S_IFREG | 0o600) << 16
                with warnings.catch_warnings():
                    warnings.simplefilter("ignore", UserWarning)
                    archive.writestr(
                        duplicate,
                        (artifact / duplicate_member).read_bytes(),
                    )
        payload = archive_path.read_bytes()
        return len(payload), "sha256:" + hashlib.sha256(payload).hexdigest()

    def _fixture(
        self,
        root: Path,
        *,
        baseline_mode: str = "verify",
    ) -> tuple[Path, Path, Path, Path]:
        self.assertIn(baseline_mode, {"candidate", "verify"})
        artifact = root / "artifact"
        artifact.mkdir()
        baseline = root / "reviewed-baseline.json"
        wrapper = root / "run-render-oracle-container.py"
        wrapper_payload = b"#!/usr/bin/env python3\n# authenticated test wrapper\n"
        wrapper.write_bytes(wrapper_payload)
        wrapper_sha256 = self.checker._sha256(wrapper_payload)
        config_digest = "sha256:" + "2" * 64
        manifest_digest = "sha256:" + "6" * 64
        lock = root / "lock.json"
        bootstrap_source_commit = "b" * 40
        bootstrap_run_id = 101
        bootstrap_run_attempt = 2
        lock_document = {
            "schema": "rxls.render-oracle-container-lock.v3",
            "built_image": {
                "bootstrap_receipt": {
                    "artifact": {
                        "digest": "sha256:" + "a" * 64,
                        "id": 202,
                        "name": (
                            f"render-oracle-image-{bootstrap_source_commit}-"
                            f"{bootstrap_run_id}-{bootstrap_run_attempt}"
                        ),
                        "size_in_bytes": 4096,
                    },
                    "evidence": {
                        "bytes": 2048,
                        "member": "render-oracle-image-build.json",
                        "sha256": "b" * 64,
                    },
                    "job": {
                        "conclusion": "failure",
                        "id": 303,
                        "name": "locked LibreOffice oracle image",
                        "run_attempt": bootstrap_run_attempt,
                        "run_id": bootstrap_run_id,
                    },
                    "repository": {
                        "full_name": "HyunjoJung/rxls",
                        "id": 1_297_467_060,
                    },
                    "run": {
                        "conclusion": "failure",
                        "event": "pull_request",
                        "head_sha": bootstrap_source_commit,
                        "id": bootstrap_run_id,
                        "run_attempt": bootstrap_run_attempt,
                        "workflow": ".github/workflows/render-hardening.yml",
                    },
                    "schema": "rxls.render-oracle-bootstrap-receipt.v1",
                },
                "expected_id": config_digest,
                "expected_manifest_digest": manifest_digest,
                "identity_kind": (
                    "docker_schema2_manifest_digest_plus_oci_image_config_digest"
                ),
                "source_date_epoch": 1_783_900_800,
                "unpinned_verification": (
                    "bootstrap_only_two_isolated_no_cache_builds_plus_exact_config_"
                    "manifest_descriptor_rootfs_contract_and_labels"
                ),
            },
            "wrapper": {
                "bytes": len(wrapper_payload),
                "path": "scripts/run-render-oracle-container.py",
                "sha256": wrapper_sha256,
            },
        }
        self._write(lock, lock_document)
        contract = self.checker._release_contract(lock, wrapper)
        campaign = {
            "schema": "rxls.render-parity-campaign.v1",
            "kind": "project_generated_hosted_full",
            "profile": "full",
            "generator": "rxls-synthetic-render-corpus",
            "generator_version": "1.5.0",
            "case_count": 800,
            "format_counts": copy.deepcopy(
                self.checker.EXPECTED_FORMAT_COUNTS
            ),
            "feature_counts": copy.deepcopy(
                self.checker.EXPECTED_FEATURE_COUNTS
            ),
            "manifest_sha256": (
                self.checker.EXPECTED_HOSTED_FULL_MANIFEST_SHA256
            ),
            "input_set_sha256": (
                self.checker.EXPECTED_HOSTED_FULL_INPUT_SET_SHA256
            ),
        }
        def score_distribution(count: int, value: int = 900_000) -> dict[str, int]:
            return {
                "count": count,
                "max": value,
                "mean": value,
                "min": value,
                "p10": value,
            }

        def delta_distribution(count: int, value: int = 0) -> dict[str, int]:
            return {
                "count": count,
                "max": value,
                "mean": value,
                "min": value,
                "p50": value,
                "p90": value,
            }

        baseline_checker = self.checker._load_baseline_checker()

        def cohort(count: int) -> dict[str, object]:
            return {
                "comparable_workbooks": count,
                "deltas": {
                    metric: delta_distribution(count)
                    for metric in sorted(
                        baseline_checker.EXPECTED_DELTA_METRICS
                    )
                },
                "scores": {
                    metric: score_distribution(count)
                    for metric in sorted(
                        baseline_checker.EXPECTED_SCORE_METRICS
                    )
                },
                "workbooks": count,
            }

        def histogram_cohort(count: int) -> dict[str, object]:
            return {
                "deltas": {
                    metric: [[0, count]]
                    for metric in sorted(
                        baseline_checker.EXPECTED_DELTA_METRICS
                    )
                },
                "scores": {
                    metric: [[900_000, count]]
                    for metric in sorted(
                        baseline_checker.EXPECTED_SCORE_METRICS
                    )
                },
            }

        group_counts: dict[tuple[str, tuple[str, ...]], int] = {}
        for case in self.corpus_generator.profile_specs("full"):
            key = (case.format, tuple(case.features))
            group_counts[key] = group_counts.get(key, 0) + 1
        groups = [
            {
                "comparable_workbooks": count,
                "deltas": histogram_cohort(count)["deltas"],
                "features": list(features),
                "format": format_name,
                "scores": histogram_cohort(count)["scores"],
                "workbooks": count,
            }
            for (format_name, features), count in sorted(
                group_counts.items()
            )
        ]
        self.assertEqual(len(groups), 96)
        self.assertEqual(
            baseline_checker.group_topology_sha256(groups),
            baseline_checker.HOSTED_FULL_GROUP_TOPOLOGY_SHA256,
        )

        candidate_template = {
            "campaign": campaign,
            "classifications": {"within_threshold": 800},
            "cohorts": {
                "all": cohort(800),
                "by_feature": {
                    feature: cohort(count)
                    for feature, count in (
                        self.checker.EXPECTED_FEATURE_COUNTS.items()
                    )
                },
                "by_format": {
                    "ods": cohort(200),
                    "xls": cohort(200),
                    "xlsb": cohort(200),
                    "xlsx": cohort(200),
                },
            },
            "comparable_files": 800,
            "configuration_sha256": "d" * 64,
            "input_files": 800,
            "input_set_sha256": (
                self.checker.EXPECTED_HOSTED_FULL_INPUT_SET_SHA256
            ),
            "groups": groups,
            "histograms": {
                "all": histogram_cohort(800),
                "by_feature": {
                    feature: histogram_cohort(count)
                    for feature, count in (
                        self.checker.EXPECTED_FEATURE_COUNTS.items()
                    )
                },
                "by_format": {
                    name: histogram_cohort(count)
                    for name, count in (
                        self.checker.EXPECTED_FORMAT_COUNTS.items()
                    )
                },
            },
            "schema": "rxls.render-parity-observed-candidate.v1",
            "statuses": {"compared": 800},
            "warning_counts": {},
        }
        reviewed = baseline_checker.conservative_adoption_baseline(
            candidate_template,
            candidate_template,
            max_score_drift_ppm={
                metric: 0
                for metric in baseline_checker.ADOPTION_SCORE_METRICS
            },
        )
        self._write(baseline, reviewed)
        reviewed_sha = self.checker._canonical_sha256(reviewed)
        warning_policy = {
            "candidate_code_count": 0,
            "candidate_counts_sha256": "d" * 64,
            "reviewed_code_count": 0,
            "reviewed_counts_sha256": "e" * 64,
            "reviewed_codes_sha256": "f" * 64,
            "unclassified_codes": [],
        }
        report_identities = [
            {"bytes": 1234, "sha256": "5" * 64},
            {"bytes": 1234, "sha256": "6" * 64},
        ]
        candidates = []
        gates = []
        for label in ("a", "b"):
            index = "ab".index(label)
            candidate = copy.deepcopy(candidate_template)
            candidate_payload = self._write(
                artifact / f"baseline-candidate-{label}.json", candidate
            )
            if baseline_mode == "verify":
                gate = baseline_checker.compare(reviewed, candidate)
                warning_policy = gate["warning_policy"]
            else:
                gate = {
                    "schema": "rxls.render-parity-baseline-check.v1",
                    "baseline_sha256": self.checker._canonical_sha256(candidate),
                    "created": True,
                    "passed": True,
                }
            gate["source_evidence"] = report_identities[index]
            gate_payload = self._write(
                artifact / f"baseline-gate-{label}.json", gate
            )
            candidates.append((candidate, candidate_payload))
            gates.append((gate, gate_payload))

        fidelities = []
        font_pack_sha256 = "f" * 64
        host_tools_identity = {"platform": {"machine": "x86_64"}}
        host_tools_identity_sha256 = self.checker._canonical_sha256(
            host_tools_identity
        )
        poppler_sha256 = {
            "pdffonts": "1" * 64,
            "pdfinfo": "2" * 64,
            "pdftoppm": "3" * 64,
            "pdftotext": "4" * 64,
        }

        def text_metrics() -> dict[str, int]:
            return {
                "ambiguous": 0,
                "f1_ppm": 999_000,
                "libreoffice_items": 1000,
                "libreoffice_unmatched": 1,
                "matched": 999,
                "median_error_millipoints": 0,
                "p95_error_millipoints": 1,
                "precision_ppm": 999_000,
                "recall_ppm": 999_000,
                "rxls_items": 1000,
                "rxls_unmatched": 1,
            }

        def hard_feature_metrics(count: int) -> dict[str, object]:
            return {
                "edge_f1_ppm": 990_000,
                "edge_libreoffice_pixels": 1000,
                "edge_rxls_pixels": 1000,
                "semantic_codepoint_libreoffice_items": 1000,
                "semantic_codepoint_precision_ppm": 999_000,
                "semantic_codepoint_recall_ppm": 999_000,
                "semantic_codepoint_rxls_items": 1000,
                "similarity_mean_ppm": 990_000,
                "text_box": text_metrics(),
                "text_line_box": text_metrics(),
                "workbooks": count,
            }

        for label in ("a", "b"):
            index = "ab".index(label)
            fidelity = {
                "schema": "rxls.render-fidelity-targets.v1",
                "passed": True,
                "failures": [],
                "coverage": {
                    "broad_workbooks": 800,
                    "core_text_box_ambiguous": 0,
                    "core_text_box_candidates": 1000,
                    "core_text_box_libreoffice_items": 1000,
                    "core_text_box_libreoffice_unmatched": 1,
                    "core_text_box_matches": 999,
                    "core_text_box_unmatched": 1,
                    "core_text_line_box_ambiguous": 0,
                    "core_text_line_box_candidates": 1000,
                    "core_text_line_box_libreoffice_items": 1000,
                    "core_text_line_box_libreoffice_unmatched": 1,
                    "core_text_line_box_matches": 999,
                    "core_text_line_box_unmatched": 1,
                    "core_workbooks": 118,
                    "format_workbooks": copy.deepcopy(
                        self.checker.EXPECTED_FORMAT_COUNTS
                    ),
                    "hard_feature_workbooks": copy.deepcopy(
                        self.checker.EXPECTED_HARD_FEATURE_COUNTS
                    ),
                    "libreoffice_pdf_font_objects": 800,
                    "native_pdf_documents": 800,
                    "native_pdf_font_objects": 800,
                    "native_pdf_type0_cff_font_objects": 0,
                    "native_pdf_type0_font_objects": 800,
                    "native_pdf_type0_truetype_font_objects": 800,
                    "native_pdf_type3_font_objects": 0,
                    "pages": 800,
                    "report_workbooks": 800,
                    "status_counts": {"compared": 800},
                },
                "evidence": {
                    "bytes": report_identities[index]["bytes"],
                    "feature_map_sha256": "e" * 64,
                    "font_pack_sha256": font_pack_sha256,
                    "host_tools_identity_sha256": (
                        host_tools_identity_sha256
                    ),
                    "input_set_sha256": (
                        self.checker.EXPECTED_HOSTED_FULL_BINDING_INPUT_SET_SHA256
                    ),
                    "manifest_sha256": (
                        self.checker.EXPECTED_HOSTED_FULL_MANIFEST_SHA256
                    ),
                    "oracle_build_contract_sha256": contract[
                        "build_contract_sha256"
                    ],
                    "oracle_image_config_digest": config_digest,
                    "oracle_image_manifest_digest": manifest_digest,
                    "oracle_libreoffice_artifact_sha256": (
                        self.checker.LIBREOFFICE_ARTIFACT_SHA256
                    ),
                    "oracle_lock_file_sha256": contract["lock_file_sha256"],
                    **{
                        f"{name}_sha256": digest
                        for name, digest in poppler_sha256.items()
                    },
                    "renderer_sha256": "4" * 64,
                    "sha256": report_identities[index]["sha256"],
                },
                "metrics": {
                    "broad_similarity_mean_ppm": 990_000,
                    "core_edge_f1_ppm": 990_000,
                    "core_semantic_codepoint_precision_ppm": 999_000,
                    "core_semantic_codepoint_recall_ppm": 999_000,
                    "core_similarity_mean_ppm": 990_000,
                    "hard_feature_cohorts": {
                        name: hard_feature_metrics(count)
                        for name, count in (
                            self.checker.EXPECTED_HARD_FEATURE_COUNTS.items()
                        )
                    },
                    "page_box_max_millipoints": 2,
                    "page_box_median_millipoints": 0,
                    "page_box_p95_millipoints": 1,
                    "pdf_point_geometry_mismatches": 0,
                    "pdf_xhtml_crosscheck_max_micropoints": 0,
                    "text_box_f1_ppm": 999_000,
                    "text_box_match_coverage_ppm": 999_000,
                    "text_box_median_error_millipoints": 0,
                    "text_box_p95_error_millipoints": 1,
                    "text_box_precision_ppm": 999_000,
                    "text_box_recall_ppm": 999_000,
                    "text_line_box_f1_ppm": 999_000,
                    "text_line_box_median_error_millipoints": 0,
                    "text_line_box_p95_error_millipoints": 1,
                    "text_line_box_precision_ppm": 999_000,
                    "text_line_box_recall_ppm": 999_000,
                },
                "policy": copy.deepcopy(
                    self.checker.EXPECTED_FIDELITY_POLICY
                ),
                "thresholds": copy.deepcopy(
                    self.checker.EXPECTED_FIDELITY_THRESHOLDS
                ),
            }
            payload = self._write(artifact / f"fidelity-{label}.json", fidelity)
            fidelities.append((fidelity, payload))
        authored = {
            "schema": "rxls.authored-print-parity.v2",
            "passed": True,
            "failures": [],
            "coverage": {
                "by_scale_mode": {"fit": 50, "scale": 50},
                "edge_libreoffice_pixels": 1000,
                "edge_rxls_pixels": 1000,
                "libreoffice_pdf_font_objects": 100,
                "native_pdf_documents": 100,
                "native_pdf_font_objects": 100,
                "native_pdf_type0_cff_font_objects": 0,
                "native_pdf_type0_font_objects": 100,
                "native_pdf_type0_truetype_font_objects": 100,
                "native_pdf_type3_font_objects": 0,
                "page_count_histogram": {"1": 50, "4": 50},
                "pages": 250,
                "semantic_codepoint_libreoffice_items": 1000,
                "semantic_codepoint_rxls_items": 1000,
                "text_box_candidates": 1000,
                "text_box_libreoffice_items": 1000,
                "text_box_matches": 999,
                "text_line_box_candidates": 1000,
                "text_line_box_libreoffice_items": 1000,
                "text_line_box_matches": 999,
                "workbooks": 100,
            },
            "evidence": {
                "feature_map_sha256": "a" * 64,
                "font_pack_sha256": font_pack_sha256,
                "host_tools_identity_sha256": host_tools_identity_sha256,
                "input_set_sha256": "9" * 64,
                "manifest_sha256": (
                    self.checker.EXPECTED_HOSTED_FULL_MANIFEST_SHA256
                ),
                "oracle_build_contract_sha256": contract[
                    "build_contract_sha256"
                ],
                "oracle_image_config_digest": config_digest,
                "oracle_image_manifest_digest": manifest_digest,
                "oracle_libreoffice_artifact_sha256": (
                    self.checker.LIBREOFFICE_ARTIFACT_SHA256
                ),
                "oracle_lock_file_sha256": contract["lock_file_sha256"],
                **{
                    f"{name}_sha256": digest
                    for name, digest in poppler_sha256.items()
                },
                "renderer_sha256": "4" * 64,
                "report_bytes": 4321,
                "report_sha256": "1" * 64,
            },
            "expected": {
                "page_box_pixels": {"height": 1056, "width": 816},
                "page_box_points": {"height": "792/1", "width": "612/1"},
                "pages_per_workbook_by_scale_mode": {"fit": 1, "scale": 4},
                "workbooks_by_scale_mode": {"fit": 50, "scale": 50},
            },
            "metrics": {
                "edge_f1_ppm": 990_000,
                "page_box_max_millipoints": 2,
                "page_box_median_millipoints": 0,
                "page_box_p95_millipoints": 1,
                "pdf_point_geometry_mismatches": 0,
                "pdf_xhtml_crosscheck_max_micropoints": 0,
                "semantic_codepoint_precision_ppm": 999_000,
                "semantic_codepoint_recall_ppm": 999_000,
                "similarity_mean_ppm": 990_000,
                "text_box_ambiguous": 0,
                "text_box_f1_ppm": 999_000,
                "text_box_libreoffice_unmatched": 1,
                "text_box_match_coverage_ppm": 999_000,
                "text_box_median_error_millipoints": 0,
                "text_box_p95_error_millipoints": 1,
                "text_box_precision_ppm": 999_000,
                "text_box_recall_ppm": 999_000,
                "text_box_unmatched": 1,
                "text_line_box_ambiguous": 0,
                "text_line_box_f1_ppm": 999_000,
                "text_line_box_libreoffice_unmatched": 1,
                "text_line_box_median_error_millipoints": 0,
                "text_line_box_p95_error_millipoints": 1,
                "text_line_box_precision_ppm": 999_000,
                "text_line_box_recall_ppm": 999_000,
                "text_line_box_unmatched": 1,
            },
            "thresholds": copy.deepcopy(
                self.checker.EXPECTED_AUTHORED_THRESHOLDS
            ),
        }
        authored_payload = self._write(artifact / "authored-print-gate.json", authored)
        repeated_deltas = {
            "absolute_deltas_ppm": [0] * 801,
            "count": 801,
            "max_absolute_delta_ppm": 0,
        }
        repeatability = {
            "schema": "rxls.libreoffice-render-repeatability.v2",
            "status": "pass",
            "failures": [],
            "coverage": {
                "pages": 1,
                "visual_observations_per_metric": 801,
                "workbooks": 800,
            },
            "drift": {
                "blurred_luma_similarity": repeated_deltas,
                "mask_f1": {
                    "edge": repeated_deltas,
                    "foreground": repeated_deltas,
                    "max_absolute_delta_ppm": 0,
                    "text_ink": repeated_deltas,
                },
                "similarity": repeated_deltas,
            },
            "identity": {
                "baseline_contract": {
                    "configuration": {
                        "baseline_sha256": "d" * 64,
                        "candidate_sha256": "d" * 64,
                        "equal": True,
                    },
                    "input_set": {
                        "baseline_count": 800,
                        "baseline_sha256": (
                            self.checker.EXPECTED_HOSTED_FULL_INPUT_SET_SHA256
                        ),
                        "candidate_count": 800,
                        "candidate_sha256": (
                            self.checker.EXPECTED_HOSTED_FULL_INPUT_SET_SHA256
                        ),
                        "equal": True,
                    },
                },
                "configuration": {
                    "baseline_sha256": "7" * 64,
                    "candidate_sha256": "7" * 64,
                    "equal": True,
                },
                "input_set": {
                    "baseline_count": 800,
                    "baseline_sha256": (
                        self.checker.EXPECTED_HOSTED_FULL_BINDING_INPUT_SET_SHA256
                    ),
                    "candidate_count": 800,
                    "candidate_sha256": (
                        self.checker.EXPECTED_HOSTED_FULL_BINDING_INPUT_SET_SHA256
                    ),
                    "equal": True,
                },
                "preflight": {
                    "baseline_sha256": "9" * 64,
                    "candidate_sha256": "9" * 64,
                    "equal": True,
                },
                "renderer_binary": {
                    "baseline": {"bytes": 123, "sha256": "4" * 64},
                    "candidate": {"bytes": 123, "sha256": "4" * 64},
                    "equal": True,
                },
            },
            "metric_policy": {
                "distribution": (
                    "sorted_absolute_paired_integer_ppm_deltas"
                ),
                "input_pairing": "sha256",
                "observations": "workbook_aggregate_and_page",
                "paths_or_content_retained": False,
                "unique_text_geometry": (
                    "schema_validated_exact_same_sha_"
                    "diagnostic_non_scoring"
                ),
            },
            "reports": {
                "baseline": {"bytes": 1234, "sha256": "5" * 64},
                "candidate": {"bytes": 1234, "sha256": "6" * 64},
            },
            "thresholds_ppm": {
                "blurred_luma_similarity_max_absolute_drift": 20_000,
                "mask_f1_max_absolute_drift": 20_000,
                "similarity_max_absolute_drift": 20_000,
            },
        }
        repeatability_payload = self._write(
            artifact / "repeatability.json", repeatability
        )
        rootfs_diff_ids = ["sha256:" + "7" * 64, "sha256:" + "8" * 64]
        descriptor = {
            "annotations": {
                "org.opencontainers.image.created": "2026-07-13T00:00:00Z"
            },
            "digest": manifest_digest,
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "platform": {"architecture": "amd64", "os": "linux"},
            "size": 12345,
        }
        identity = {
            "config_id": config_digest,
            "created": "2026-07-13T00:00:00Z",
            "descriptor": descriptor,
            "labels": {
                "org.opencontainers.image.version": "26.2.3.2",
                "org.rxls.render-oracle.architecture": "linux/amd64",
                "org.rxls.render-oracle.libreoffice-artifact-sha256": (
                    "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5"
                ),
                "org.rxls.render-oracle.lock-sha256": contract[
                    "build_contract_sha256"
                ],
            },
            "manifest_digest": manifest_digest,
            "platform": "linux/amd64",
            "rootfs_diff_ids": rootfs_diff_ids,
        }
        identity["identity_sha256"] = self.checker._canonical_sha256(identity)
        identity["rootfs_diff_ids_sha256"] = self.checker._canonical_sha256(
            rootfs_diff_ids
        )
        reproducibility = {
            "build_count": 2,
            "buildkit_compatibility": {
                "explicit": False,
                "source": "pinned-buildkit-default",
                "version": 30,
            },
            "buildkit_commit": "e42e1bfd389af7203238cce77b1f7dad447285e9",
            "buildkit_image": (
                "docker.io/moby/buildkit:v0.31.2@sha256:"
                "2f5adac4ecd194d9f8c10b7b5d7bceb5186853db1b26e5abd3a657af0b7e26ec"
            ),
            "buildkit_version": "v0.31.2",
            "buildx_commit": "a319e5b15052cf6557ceb666eb8ff6e32380b782",
            "buildx_version": "v0.35.0",
            "config_ids": [config_digest, config_digest],
            "descriptor_digests": [manifest_digest, manifest_digest],
            "descriptor_media_types": [
                "application/vnd.docker.distribution.manifest.v2+json",
                "application/vnd.docker.distribution.manifest.v2+json",
            ],
            "descriptor_sizes": [12345, 12345],
            "driver": "docker-container",
            "export_archive_max_bytes": 4 * 1024 * 1024 * 1024,
            "export_destination": "stdout",
            "export_media_type": (
                "application/vnd.docker.distribution.manifest.v2+json"
            ),
            "export_tar": True,
            "identities": [identity, identity],
            "identity_sha256": [
                identity["identity_sha256"],
                identity["identity_sha256"],
            ],
            "manifest_digests": [manifest_digest, manifest_digest],
            "no_cache": True,
            "provenance": False,
            "rewrite_timestamp": True,
            "rootfs_diff_ids_sha256": [
                identity["rootfs_diff_ids_sha256"],
                identity["rootfs_diff_ids_sha256"],
            ],
            "sbom": False,
            "snapshotter": "native",
            "source_date_epoch": 1_783_900_800,
            "status": "matched",
        }
        build = {
            "schema": "rxls.render-oracle-container-build.v3",
            "status": "ok",
            "platform": "linux/amd64",
            "image_identity_status": "pinned_match",
            "expected_image_id": config_digest,
            "built_image_id": config_digest,
            "expected_manifest_digest": manifest_digest,
            "built_manifest_digest": manifest_digest,
            "build_contract_sha256": contract["build_contract_sha256"],
            "lock_file_sha256": contract["lock_file_sha256"],
            "source_commit": self.head_sha,
            "wrapper_sha256": wrapper_sha256,
            "reproducibility": reproducibility,
        }
        self._write(artifact / "build.json", build)
        host_tools = {
            "schema": "rxls.render-oracle-host-tools-evidence.v1",
            "identity_status": "pinned_match",
            "scope": "all",
            "identity": host_tools_identity,
            "captured_identity_sha256": host_tools_identity_sha256,
            "expected_identity_sha256": host_tools_identity_sha256,
            "lock_file_sha256": "b" * 64,
        }
        self._write(artifact / "host-tools.json", host_tools)
        renderer = {"bytes": 123, "sha256": "4" * 64}
        self._write(artifact / "renderer.json", renderer)

        baseline_candidates = []
        baseline_gates = []
        evidence_runs = []
        fidelity_summaries = []
        for index, label in enumerate(("a", "b")):
            candidate, candidate_payload = candidates[index]
            gate, gate_payload = gates[index]
            fidelity, fidelity_payload = fidelities[index]
            baseline_candidates.append(
                {
                    "bytes": len(candidate_payload),
                    "campaign_sha256": self.checker._canonical_sha256(campaign),
                    "sha256": self.checker._sha256(candidate_payload),
                    "warning_counts": {},
                }
            )
            baseline_gates.append(
                {
                    "baseline_sha256": (
                        reviewed_sha if baseline_mode == "verify" else None
                    ),
                    "bytes": len(gate_payload),
                    "candidate_sha256": self.checker._canonical_sha256(candidate),
                    "failures": [],
                    "passed": True,
                    "sha256": self.checker._sha256(gate_payload),
                    "warning_policy": (
                        warning_policy if baseline_mode == "verify" else None
                    ),
                }
            )
            evidence_runs.append(
                {
                    "baseline_candidate_bytes": len(candidate_payload),
                    "baseline_candidate_sha256": self.checker._sha256(
                        candidate_payload
                    ),
                    "baseline_gate_bytes": len(gate_payload),
                    "baseline_gate_sha256": self.checker._sha256(
                        gate_payload
                    ),
                    "campaign_sha256": self.checker._canonical_sha256(
                        campaign
                    ),
                    "fidelity_gate_bytes": len(fidelity_payload),
                    "fidelity_gate_sha256": self.checker._sha256(fidelity_payload),
                    "report_bytes": report_identities[index]["bytes"],
                    "report_sha256": report_identities[index]["sha256"],
                }
            )
            fidelity_summaries.append(
                {
                    key: fidelity[key]
                    for key in ("coverage", "metrics", "passed", "thresholds")
                }
            )
        authored_summary = {
            key: authored[key]
            for key in ("coverage", "evidence", "expected", "metrics", "passed", "thresholds")
        }
        authored_summary["sha256"] = self.checker._sha256(authored_payload)
        repeatability_summary = {
            key: repeatability[key] for key in ("coverage", "status", "thresholds_ppm")
        }
        repeatability_summary["sha256"] = self.checker._sha256(repeatability_payload)
        summary = {
            "schema": "rxls.render-oracle-hosted-campaign.v7",
            "head_sha": self.head_sha,
            "baseline_mode": baseline_mode,
            "campaign": {
                "mode": "full",
                "case_count": 800,
                "repetitions": 2,
                "shard_count": 4,
                "parallel_shards": 2,
                "shard_case_counts": [200, 200, 200, 200],
                "shard_format_counts": [
                    {"ods": 50, "xls": 50, "xlsb": 50, "xlsx": 50}
                    for _ in range(4)
                ],
                "sha256": self.checker._canonical_sha256(campaign),
            },
            "summary": {
                "by_classification": {"within_threshold": 800},
                "by_status": {"compared": 800},
                "files": 800,
                "input_bytes_considered": 800,
                "warning_counts": {},
            },
            "corpus": {
                "acquired_corpus_included": False,
                "profile": "full",
                "case_count": 800,
                "feature_counts": copy.deepcopy(
                    self.checker.EXPECTED_FEATURE_COUNTS
                ),
                "format_counts": copy.deepcopy(
                    self.checker.EXPECTED_FORMAT_COUNTS
                ),
                "generator": "rxls-synthetic-render-corpus",
                "generator_version": "1.5.0",
                "group_topology_sha256": (
                    self.checker.EXPECTED_HOSTED_FULL_GROUP_TOPOLOGY_SHA256
                ),
                "input_set_sha256": (
                    self.checker.EXPECTED_HOSTED_FULL_INPUT_SET_SHA256
                ),
                "license": "MIT",
                "manifest_sha256": (
                    self.checker.EXPECTED_HOSTED_FULL_MANIFEST_SHA256
                ),
                "render_redistributable": True,
                "rights_tier": "S",
                "redistribution": "allowed",
                "schema_version": 1,
                "scope": "project_generated_hosted_acceptance",
                "source_redistributable": True,
            },
            "renderer": renderer,
            "font_pack": {
                "alias_count": 10,
                "attestation_required": True,
                "configured": True,
                "font_count": 26,
                "fonts_conf_sha256": "7" * 64,
                "license": "SIL-OFL-1.1",
                "pack_sha256": font_pack_sha256,
                "pdf_identities_sha256": "8" * 64,
                "pdf_identity_count": 59,
            },
            "host_tools": host_tools,
            "metrics": copy.deepcopy(candidates[1][0]["cohorts"]),
            "container": {
                "build_contract_sha256": build["build_contract_sha256"],
                "identity_status": "pinned_match",
                "image_id": build["built_image_id"],
                "expected_image_id": build["built_image_id"],
                "manifest_digest": build["built_manifest_digest"],
                "expected_manifest_digest": build["built_manifest_digest"],
                "lock_file_sha256": build["lock_file_sha256"],
                "oracle_artifact_sha256": (
                    self.checker.LIBREOFFICE_ARTIFACT_SHA256
                ),
                "oracle_version": "26.2.3.2",
                "source_commit": build["source_commit"],
                "wrapper_sha256": build["wrapper_sha256"],
            },
            "baseline_ratcheting": {
                "applies": baseline_mode == "verify",
                "passed": True,
                "reviewed_baseline_available": baseline_mode == "verify",
                "candidate_baselines": baseline_candidates,
                "gates": baseline_gates,
                "mode": baseline_mode,
                "reviewed_warning_policy": (
                    warning_policy if baseline_mode == "verify" else None
                ),
            },
            "evidence_runs": evidence_runs,
            "fidelity": fidelity_summaries,
            "authored_print": authored_summary,
            "repeatability": repeatability_summary,
        }
        self._write(artifact / "hosted-summary.json", summary)
        return artifact, baseline, lock, wrapper

    def _rebind_candidate(self, artifact: Path, label: str) -> None:
        index = "ab".index(label)
        candidate_path = artifact / f"baseline-candidate-{label}.json"
        candidate = json.loads(candidate_path.read_text(encoding="utf-8"))
        candidate_payload = self._write(candidate_path, candidate)
        gate_path = artifact / f"baseline-gate-{label}.json"
        gate = json.loads(gate_path.read_text(encoding="utf-8"))
        if "created" in gate:
            gate["baseline_sha256"] = self.checker._canonical_sha256(candidate)
        else:
            gate["candidate_sha256"] = self.checker._canonical_sha256(candidate)
            gate["campaign"] = {
                "case_count": candidate["campaign"]["case_count"],
                "kind": candidate["campaign"]["kind"],
                "manifest_sha256": candidate["campaign"]["manifest_sha256"],
                "sha256": self.checker._canonical_sha256(candidate["campaign"]),
            }
        gate_payload = self._write(gate_path, gate)
        summary_path = artifact / "hosted-summary.json"
        summary = json.loads(summary_path.read_text(encoding="utf-8"))
        summary_candidate = summary["baseline_ratcheting"]["candidate_baselines"][
            index
        ]
        summary_candidate.update(
            {
                "bytes": len(candidate_payload),
                "campaign_sha256": self.checker._canonical_sha256(
                    candidate["campaign"]
                ),
                "sha256": self.checker._sha256(candidate_payload),
                "warning_counts": candidate["warning_counts"],
            }
        )
        summary_gate = summary["baseline_ratcheting"]["gates"][index]
        summary_gate.update(
            {
                "bytes": len(gate_payload),
                "candidate_sha256": self.checker._canonical_sha256(candidate),
                "sha256": self.checker._sha256(gate_payload),
            }
        )
        summary["evidence_runs"][index].update(
            {
                "baseline_candidate_bytes": len(candidate_payload),
                "baseline_candidate_sha256": self.checker._sha256(
                    candidate_payload
                ),
                "baseline_gate_bytes": len(gate_payload),
                "baseline_gate_sha256": self.checker._sha256(gate_payload),
                "campaign_sha256": self.checker._canonical_sha256(
                    candidate["campaign"]
                ),
            }
        )
        if index == 1:
            summary["metrics"] = candidate["cohorts"]
            summary["summary"]["by_classification"] = candidate[
                "classifications"
            ]
            summary["summary"]["by_status"] = candidate["statuses"]
            summary["summary"]["warning_counts"] = candidate["warning_counts"]
        self._write(summary_path, summary)

    def _rebind_repeatability(self, artifact: Path) -> None:
        repeatability_path = artifact / "repeatability.json"
        payload = repeatability_path.read_bytes()
        summary_path = artifact / "hosted-summary.json"
        summary = json.loads(summary_path.read_text(encoding="utf-8"))
        summary["repeatability"]["sha256"] = hashlib.sha256(payload).hexdigest()
        self._write(summary_path, summary)

    def test_accepts_exact_full_ratchet_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, baseline, lock, wrapper = self._fixture(Path(temporary))

            report = self.checker.validate(
                artifact,
                self.head_sha,
                baseline,
                oracle_lock=lock,
                oracle_wrapper=wrapper,
            )

        self.assertTrue(report["passed"])
        self.assertEqual(report["bootstrap_source_commit"], "b" * 40)
        self.assertEqual(report["full_cases"], 800)
        self.assertEqual(report["oracle_config_digest"], "sha256:" + "2" * 64)
        self.assertEqual(report["oracle_manifest_digest"], "sha256:" + "6" * 64)
        self.assertEqual(report["ratchets"], 2)
        self.assertEqual(report["baseline_mode"], "verify")
        self.assertEqual(report["campaign"], "full")

    def test_accepts_full_candidate_without_a_reviewed_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, _, lock, wrapper = self._fixture(
                Path(temporary),
                baseline_mode="candidate",
            )

            report = self.checker.validate(
                artifact,
                self.head_sha,
                None,
                baseline_mode="candidate",
                campaign="full",
                workflow_run_id=101,
                workflow_run_attempt=2,
                artifact_id=303,
                artifact_name=(
                    f"render-oracle-{self.head_sha}-101-2-full-candidate"
                ),
                artifact_size_bytes=4096,
                artifact_digest="sha256:" + "a" * 64,
                artifact_repository="HyunjoJung/rxls",
                oracle_lock=lock,
                oracle_wrapper=wrapper,
            )

        self.assertTrue(report["passed"])
        self.assertEqual(report["baseline_mode"], "candidate")
        self.assertEqual(report["campaign"], "full")
        self.assertIsNone(report["reviewed_baseline_sha256"])

    def test_live_authenticates_successful_exact_sha_candidate_artifact(
        self,
    ) -> None:
        run_id = 101
        attempt = 2
        artifact_id = 303
        artifact_name = (
            f"render-oracle-{self.head_sha}-{run_id}-{attempt}-full-candidate"
        )
        digest = "sha256:" + "d" * 64
        run_url = (
            "https://api.github.com/repos/HyunjoJung/rxls/"
            f"actions/runs/{run_id}"
        )
        artifacts_url = run_url + "/artifacts?per_page=100"
        run = {
            "conclusion": "success",
            "event": "workflow_dispatch",
            "head_sha": self.head_sha,
            "id": run_id,
            "path": ".github/workflows/fuzz.yml",
            "repository": {
                "full_name": "HyunjoJung/rxls",
                "id": 1_297_467_060,
            },
            "run_attempt": attempt,
            "status": "completed",
        }
        artifact = {
            "digest": digest,
            "expired": False,
            "id": artifact_id,
            "name": artifact_name,
            "size_in_bytes": 4096,
            "workflow_run": {"head_sha": self.head_sha, "id": run_id},
        }
        run_payload = json.dumps(run).encode()
        artifacts_payload = json.dumps(
            {"artifacts": [artifact], "total_count": 1}
        ).encode()
        authenticated = self.checker.authenticate_candidate_run_artifact(
            repository="HyunjoJung/rxls",
            head_sha=self.head_sha,
            workflow_run_id=run_id,
            workflow_run_attempt=attempt,
            artifact_id=artifact_id,
            artifact_name=artifact_name,
            artifact_size_bytes=4096,
            artifact_digest=digest,
            token="github-test-token",
            run_opener=_FakeOpener(
                _FakeResponse(
                    run_payload,
                    status=200,
                    headers={"Content-Length": str(len(run_payload))},
                    url=run_url,
                )
            ),
            artifacts_opener=_FakeOpener(
                _FakeResponse(
                    artifacts_payload,
                    status=200,
                    headers={
                        "Content-Length": str(len(artifacts_payload))
                    },
                    url=artifacts_url,
                )
            ),
        )
        self.assertEqual(authenticated["head_sha"], self.head_sha)
        self.assertEqual(authenticated["artifact_digest"], digest)

    def test_live_authentication_rejects_run_and_artifact_tampering(self) -> None:
        run_id = 101
        attempt = 2
        artifact_id = 303
        artifact_name = (
            f"render-oracle-{self.head_sha}-{run_id}-{attempt}-full-candidate"
        )
        digest = "sha256:" + "d" * 64
        run_url = (
            "https://api.github.com/repos/HyunjoJung/rxls/"
            f"actions/runs/{run_id}"
        )
        artifacts_url = run_url + "/artifacts?per_page=100"
        base_run = {
            "conclusion": "success",
            "event": "workflow_dispatch",
            "head_sha": self.head_sha,
            "id": run_id,
            "path": ".github/workflows/render-oracle.yml",
            "repository": {
                "full_name": "HyunjoJung/rxls",
                "id": 1_297_467_060,
            },
            "run_attempt": attempt,
            "status": "completed",
        }
        base_artifact = {
            "digest": digest,
            "expired": False,
            "id": artifact_id,
            "name": artifact_name,
            "size_in_bytes": 4096,
            "workflow_run": {"head_sha": self.head_sha, "id": run_id},
        }
        cases = (
            ("head_sha", "b" * 40, None, None),
            ("conclusion", "failure", None, None),
            ("event", "pull_request", None, None),
            ("path", ".github/workflows/ci.yml", None, None),
            (None, None, "expired", True),
            (None, None, "digest", "sha256:" + "e" * 64),
            (None, None, "id", artifact_id + 1),
        )
        for run_key, run_value, artifact_key, artifact_value in cases:
            run = copy.deepcopy(base_run)
            artifact = copy.deepcopy(base_artifact)
            if run_key is not None:
                run[run_key] = run_value
            if artifact_key is not None:
                artifact[artifact_key] = artifact_value
            run_payload = json.dumps(run).encode()
            artifacts_payload = json.dumps(
                {"artifacts": [artifact], "total_count": 1}
            ).encode()
            with self.subTest(
                run_key=run_key,
                artifact_key=artifact_key,
            ), self.assertRaises(self.checker.EvidenceError):
                self.checker.authenticate_candidate_run_artifact(
                    repository="HyunjoJung/rxls",
                    head_sha=self.head_sha,
                    workflow_run_id=run_id,
                    workflow_run_attempt=attempt,
                    artifact_id=artifact_id,
                    artifact_name=artifact_name,
                    artifact_size_bytes=4096,
                    artifact_digest=digest,
                    token="github-test-token",
                    run_opener=_FakeOpener(
                        _FakeResponse(
                            run_payload,
                            status=200,
                            headers={
                                "Content-Length": str(len(run_payload))
                            },
                            url=run_url,
                        )
                    ),
                    artifacts_opener=_FakeOpener(
                        _FakeResponse(
                            artifacts_payload,
                            status=200,
                            headers={
                                "Content-Length": str(
                                    len(artifacts_payload)
                                )
                            },
                            url=artifacts_url,
                        )
                    ),
                )

    def test_candidate_and_verify_modes_fail_closed_on_cross_mode_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate_artifact, baseline, lock, wrapper = self._fixture(
                root,
                baseline_mode="candidate",
            )
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "candidate_reviewed_baseline_forbidden",
            ):
                self.checker.validate(
                    candidate_artifact,
                    self.head_sha,
                    baseline,
                    baseline_mode="candidate",
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )
            with self.assertRaises(self.checker.EvidenceError):
                self.checker.validate(
                    candidate_artifact,
                    self.head_sha,
                    None,
                    baseline_mode="verify",
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )

        with tempfile.TemporaryDirectory() as temporary:
            artifact, _, lock, wrapper = self._fixture(Path(temporary))
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "reviewed_baseline_required",
            ):
                self.checker.validate(
                    artifact,
                    self.head_sha,
                    None,
                    baseline_mode="verify",
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )

    def test_authenticates_extracts_and_reports_exact_artifact_binding(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifact, baseline, lock, wrapper = self._fixture(root)
            archive = root / "artifact.zip"
            size, digest = self._archive(artifact, archive)
            extracted = root / "extracted"

            self.checker.extract_authenticated_artifact(
                archive,
                extracted,
                size,
                digest,
            )
            report = self.checker.validate(
                extracted,
                self.head_sha,
                baseline,
                workflow_run_id=101,
                workflow_run_attempt=2,
                artifact_id=303,
                artifact_name=(
                    f"render-oracle-{self.head_sha}-101-2-full-verify"
                ),
                artifact_size_bytes=size,
                artifact_digest=digest,
                artifact_repository="HyunjoJung/rxls",
                oracle_lock=lock,
                oracle_wrapper=wrapper,
            )

        self.assertTrue(report["passed"])
        self.assertEqual(report["workflow_run_id"], 101)
        self.assertEqual(report["workflow_run_attempt"], 2)
        self.assertEqual(report["artifact_id"], 303)
        self.assertEqual(report["artifact_size_bytes"], size)
        self.assertEqual(report["artifact_digest"], digest)

    def test_rejects_archive_digest_size_type_and_unsafe_members(self) -> None:
        cases = (
            "digest",
            "size",
            "symlink_archive",
            "traversal",
            "duplicate",
            "symlink_member",
            "compression",
        )
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                artifact, _, _, _ = self._fixture(root)
                archive = root / "artifact.zip"
                if case == "traversal":
                    size, digest = self._archive(
                        artifact,
                        archive,
                        renamed_member=("build.json", "../build.json"),
                    )
                elif case == "duplicate":
                    size, digest = self._archive(
                        artifact,
                        archive,
                        duplicate_member="build.json",
                    )
                elif case == "symlink_member":
                    size, digest = self._archive(
                        artifact,
                        archive,
                        symlink_member="build.json",
                    )
                elif case == "compression":
                    size, digest = self._archive(
                        artifact,
                        archive,
                        compression=zipfile.ZIP_BZIP2,
                    )
                else:
                    size, digest = self._archive(artifact, archive)
                candidate = root / "candidate"
                if case == "digest":
                    digest = "sha256:" + "0" * 64
                elif case == "size":
                    size += 1
                elif case == "symlink_archive":
                    original = root / "original.zip"
                    archive.rename(original)
                    archive.symlink_to(original)

                with self.assertRaises(self.checker.EvidenceError):
                    self.checker.extract_authenticated_artifact(
                        archive,
                        candidate,
                        size,
                        digest,
                    )
                self.assertFalse(candidate.exists())

    def test_bounded_direct_download_does_not_forward_github_token(self) -> None:
        payload = b"authenticated immutable artifact archive"
        expected_digest = "sha256:" + hashlib.sha256(payload).hexdigest()
        signed_url = "https://artifacts.example.invalid/signed/archive.zip?token=x"
        api_response = _FakeResponse(
            b"",
            status=302,
            headers={"Location": signed_url},
            url="https://api.github.com/",
        )
        archive_response = _FakeResponse(
            payload,
            status=200,
            headers={
                "Content-Encoding": "identity",
                "Content-Length": str(len(payload)),
            },
            url=signed_url,
        )
        api_opener = _FakeOpener(api_response)
        archive_opener = _FakeOpener(archive_response)
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "artifact.zip"
            self.checker.download_artifact_archive(
                "HyunjoJung/rxls",
                303,
                destination,
                len(payload),
                expected_digest,
                token="github-test-token",
                api_opener=api_opener,
                archive_opener=archive_opener,
            )
            self.assertEqual(destination.read_bytes(), payload)

        api_request, api_timeout = api_opener.requests[0]
        archive_request, archive_timeout = archive_opener.requests[0]
        self.assertEqual(
            api_request.full_url,
            "https://api.github.com/repos/HyunjoJung/rxls/actions/"
            "artifacts/303/zip",
        )
        self.assertEqual(
            api_request.get_header("Authorization"),
            "Bearer github-test-token",
        )
        self.assertIsNone(archive_request.get_header("Authorization"))
        self.assertEqual(archive_request.full_url, signed_url)
        self.assertEqual(
            api_timeout,
            self.checker.DOWNLOAD_TIMEOUT_SECONDS,
        )
        self.assertEqual(
            archive_timeout,
            self.checker.DOWNLOAD_TIMEOUT_SECONDS,
        )

    def test_direct_download_fails_closed_on_transport_drift(self) -> None:
        payload = b"expected archive"
        expected_digest = "sha256:" + hashlib.sha256(payload).hexdigest()
        cases = (
            "insecure_redirect",
            "oversize",
            "undersize",
            "digest",
            "content_length",
            "content_encoding",
        )
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                signed_url = (
                    "http://artifacts.example.invalid/archive.zip"
                    if case == "insecure_redirect"
                    else "https://artifacts.example.invalid/archive.zip"
                )
                api_opener = _FakeOpener(
                    _FakeResponse(
                        b"",
                        status=302,
                        headers={"Location": signed_url},
                        url="https://api.github.com/",
                    )
                )
                body = payload
                digest = expected_digest
                content_length = len(payload)
                content_encoding = "identity"
                if case == "oversize":
                    body += b"x"
                elif case == "undersize":
                    body = body[:-1]
                elif case == "digest":
                    digest = "sha256:" + "0" * 64
                elif case == "content_length":
                    content_length += 1
                elif case == "content_encoding":
                    content_encoding = "gzip"
                archive_opener = _FakeOpener(
                    _FakeResponse(
                        body,
                        status=200,
                        headers={
                            "Content-Encoding": content_encoding,
                            "Content-Length": str(content_length),
                        },
                        url=signed_url,
                    )
                )
                destination = Path(temporary) / "artifact.zip"

                with self.assertRaises(self.checker.EvidenceError):
                    self.checker.download_artifact_archive(
                        "HyunjoJung/rxls",
                        303,
                        destination,
                        len(payload),
                        digest,
                        token="github-test-token",
                        api_opener=api_opener,
                        archive_opener=archive_opener,
                    )
                self.assertFalse(destination.exists())

    def test_rejects_partial_or_cross_run_artifact_binding(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, baseline, lock, wrapper = self._fixture(Path(temporary))
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "artifact_binding_incomplete",
            ):
                self.checker.validate(
                    artifact,
                    self.head_sha,
                    baseline,
                    workflow_run_id=101,
                    artifact_digest="sha256:" + "a" * 64,
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "artifact_name",
            ):
                self.checker.validate(
                    artifact,
                    self.head_sha,
                    baseline,
                    workflow_run_id=101,
                    workflow_run_attempt=2,
                    artifact_id=303,
                    artifact_name=(
                        f"render-oracle-{self.head_sha}-102-2-full-verify"
                    ),
                    artifact_size_bytes=4096,
                    artifact_digest="sha256:" + "a" * 64,
                    artifact_repository="HyunjoJung/rxls",
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )

    def test_release_workflow_uses_authenticated_artifact_id_transport(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertNotIn("gh run download", workflow)
        for required in (
            '--download-repository "$GITHUB_REPOSITORY"',
            '--github-artifact-id "$artifact_id"',
            '--artifact-name "$artifact_name"',
            '--artifact-size-bytes "$size_bytes"',
            "--baseline-mode verify",
            "--campaign full",
            '--workflow-run-id "$run_id"',
            '--workflow-run-attempt "$run_attempt"',
            '--artifact-digest "$digest"',
        ):
            with self.subTest(required=required):
                self.assertIn(required, workflow)

    def test_rejects_failed_mismatched_missing_and_path_bearing_evidence(self) -> None:
        mutations = ("failed", "head", "missing", "path", "baseline")
        for mutation in mutations:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                artifact, baseline, lock, wrapper = self._fixture(Path(temporary))
                if mutation == "failed":
                    gate_path = artifact / "baseline-gate-a.json"
                    gate = json.loads(gate_path.read_text(encoding="utf-8"))
                    gate["passed"] = False
                    gate["failures"] = ["regression"]
                    self._write(gate_path, gate)
                elif mutation == "head":
                    summary_path = artifact / "hosted-summary.json"
                    summary = json.loads(summary_path.read_text(encoding="utf-8"))
                    summary["head_sha"] = "b" * 40
                    self._write(summary_path, summary)
                elif mutation == "missing":
                    (artifact / "repeatability.json").unlink()
                elif mutation == "path":
                    build_path = artifact / "build.json"
                    build = json.loads(build_path.read_text(encoding="utf-8"))
                    build["path"] = "/" + "home/runner/private"
                    self._write(build_path, build)
                else:
                    self._write(
                        baseline,
                        {
                            "schema": "rxls.render-parity-baseline.v2",
                            "fixture": "changed",
                        },
                    )

                with self.assertRaises(self.checker.EvidenceError):
                    self.checker.validate(
                        artifact,
                        self.head_sha,
                        baseline,
                        oracle_lock=lock,
                        oracle_wrapper=wrapper,
                    )

    def test_path_neutral_rejects_key_variants_traversal_and_artifact_names(
        self,
    ) -> None:
        mac_home_key = "/" + "/".join(
            ("Users", "alice", "Secret", "client.xlsx")
        )
        rejected = (
            {"source_path": "redacted"},
            {"host-path": "redacted"},
            "../secret.xlsx",
            "nested/../secret.xlsx",
            r"private\secret.xlsx",
            "secret.xlsx",
            "secret.xls",
            "secret.xlsb",
            "secret.xlsm",
            "secret.ods",
            "secret.fods",
            "secret.pdf",
            "secret.png",
            "secret.svg",
            {mac_home_key: 2},
            {"token_ghp_SUPERSECRET": 2},
        )
        for value in rejected:
            with self.subTest(value=value), self.assertRaises(
                self.checker.EvidenceError
            ):
                self.checker._path_neutral(value)

        self.checker._path_neutral(
            {
                "schema": "rxls.render-oracle-hosted-campaign.v7",
                "sha256": "a" * 64,
                "media_type": "application/pdf",
                "paths_or_content_retained": False,
            }
        )
        for key in (
            "Paths_or_content_retained",
            "path_s_or_content_retained",
            "paths-or-content-retained",
            "paths_or_content_retained_",
            "not_paths_or_content_retained",
            "source_paths_retained",
        ):
            with self.subTest(key=key), self.assertRaises(
                self.checker.EvidenceError
            ):
                self.checker._path_neutral({key: False})
        with self.assertRaisesRegex(
            self.checker.EvidenceError,
            "path_retention_attestation",
        ):
            self.checker._path_neutral(
                {"paths_or_content_retained": True}
            )

    def test_strict_json_ingestion_rejects_hostile_numbers_depth_and_duplicates(
        self,
    ) -> None:
        malformed_payloads = (
            (b'{"value":1,"value":2}', "duplicate_json_key"),
            (b'{"value":NaN}', "evidence_invalid_json"),
            (b'{"value":1.5}', "evidence_invalid_json"),
            (b'{"value":1e10000}', "evidence_invalid_json"),
            (
                b'{"value":' + (b"6" * 5_000) + b"}",
                "evidence_invalid_json",
            ),
            (
                b"[" * (self.checker.MAX_JSON_DEPTH + 1)
                + b"]" * (self.checker.MAX_JSON_DEPTH + 1),
                "evidence_invalid_json",
            ),
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for index, (payload, error_code) in enumerate(malformed_payloads):
                path = root / f"evidence-{index}.json"
                path.write_bytes(payload)
                with self.subTest(
                    error_code=error_code,
                ), self.assertRaisesRegex(
                    self.checker.EvidenceError,
                    f"^{error_code}$",
                ) as raised:
                    self.checker._read_json(path)
                self.assertNotIn(str(path), str(raised.exception))

    def test_json_ingestion_is_bounded_to_verified_regular_files(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target.json"
            target.write_text("{}\n", encoding="utf-8")

            link = root / "link.json"
            link.symlink_to(target)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                r"\Aevidence_file_type\Z",
            ):
                self.checker._read_json(link)

            fifo = root / "report.fifo"
            os.mkfifo(fifo)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                r"\Aevidence_file_type\Z",
            ):
                self.checker._read_json(fifo)

            oversized = root / "oversized.json"
            oversized.write_bytes(b"12345")
            with (
                mock.patch.object(
                    self.checker,
                    "MAX_FILE_BYTES",
                    4,
                ),
                self.assertRaisesRegex(
                    self.checker.EvidenceError,
                    r"\Aevidence_file_size\Z",
                ),
            ):
                self.checker._read_json(oversized)

    def test_json_preflight_rejects_complexity_and_depth_before_decode(
        self,
    ) -> None:
        preflight_payloads = (
            b"[" * (self.checker.MAX_JSON_DEPTH + 1)
            + b"]" * (self.checker.MAX_JSON_DEPTH + 1),
            b'{"value":1.25}',
            b'{"value":1e10000}',
            b'{"value":' + (b"5" * 5_000) + b"}",
        )
        for payload in preflight_payloads:
            with self.subTest(payload_size=len(payload)), mock.patch.object(
                self.checker.json,
                "loads",
                side_effect=AssertionError("decoder must not run"),
            ), self.assertRaisesRegex(
                self.checker.EvidenceError,
                "^evidence_invalid_json$",
            ):
                self.checker._strict_json_loads(
                    payload,
                    "evidence_invalid_json",
                )

        with mock.patch.object(
            self.checker,
            "MAX_JSON_NODES",
            3,
        ), mock.patch.object(
            self.checker.json,
            "loads",
            side_effect=AssertionError("decoder must not run"),
        ), self.assertRaisesRegex(
            self.checker.EvidenceError,
            "^evidence_invalid_json$",
        ):
            self.checker._strict_json_loads(
                b"[0,0,0,0]",
                "evidence_invalid_json",
            )

    def test_path_neutral_traversal_is_iterative_for_deep_values(self) -> None:
        value: object = "safe"
        for _ in range(5_000):
            value = [value]
        self.checker._path_neutral(value)

        rejected: object = "/private/workbook"
        for _ in range(5_000):
            rejected = [rejected]
        with self.assertRaisesRegex(
            self.checker.EvidenceError,
            "^absolute_path$",
        ):
            self.checker._path_neutral(rejected)

    def test_cli_reports_json_failures_as_one_path_neutral_line(self) -> None:
        head_sha = "a" * 40
        arguments = [
            str(CHECKER),
            "--download-repository",
            "HyunjoJung/rxls",
            "--github-artifact-id",
            "3",
            "--artifact-name",
            f"render-oracle-{head_sha}-1-1-full-candidate",
            "--artifact-size-bytes",
            "1",
            "--baseline-mode",
            "candidate",
            "--campaign",
            "full",
            "--head-sha",
            head_sha,
            "--workflow-run-id",
            "1",
            "--workflow-run-attempt",
            "1",
            "--artifact-digest",
            "sha256:" + ("b" * 64),
        ]
        malformed_payloads = (
            b'{"value":1e10000}',
            b'{"value":' + (b"4" * 5_000) + b"}",
            b"[" * (self.checker.MAX_JSON_DEPTH + 1)
            + b"]" * (self.checker.MAX_JSON_DEPTH + 1),
        )
        for payload in malformed_payloads:
            stderr = io.StringIO()

            def reject_evidence(*_args, **_kwargs):
                return self.checker._strict_json_loads(
                    payload,
                    "evidence_invalid_json",
                )

            with self.subTest(payload_size=len(payload)), mock.patch.object(
                sys,
                "argv",
                arguments,
            ), mock.patch.object(
                self.checker,
                "download_artifact_archive",
            ), mock.patch.object(
                self.checker,
                "extract_authenticated_artifact",
            ), mock.patch.object(
                self.checker,
                "validate",
                side_effect=reject_evidence,
            ), mock.patch.object(
                sys,
                "stderr",
                stderr,
            ):
                self.assertEqual(self.checker.main(), 1)
            self.assertEqual(
                stderr.getvalue().splitlines(),
                ["render release prerequisites: evidence_invalid_json"],
            )
            self.assertNotIn("Traceback", stderr.getvalue())

    def test_repeatability_evidence_is_strict_and_cross_bound(self) -> None:
        direct_mutations = (
            (
                "threshold",
                lambda document: document["thresholds_ppm"].update(
                    {"similarity_max_absolute_drift": 20_001}
                ),
                "repeatability_thresholds",
            ),
            (
                "unsorted",
                lambda document: document["drift"]["similarity"].update(
                    {
                        "absolute_deltas_ppm": [1, 0] + [0] * 799,
                        "max_absolute_delta_ppm": 1,
                    }
                ),
                "repeatability_distribution",
            ),
            (
                "oversized",
                lambda document: document["drift"]["similarity"].update(
                    {
                        "absolute_deltas_ppm": [0] * 800 + [20_001],
                        "max_absolute_delta_ppm": 20_001,
                    }
                ),
                "repeatability_distribution",
            ),
            (
                "identity",
                lambda document: document["identity"]["input_set"].update(
                    {"equal": False}
                ),
                "repeatability_identity",
            ),
            (
                "coverage",
                lambda document: document["coverage"].update({"pages": 2}),
                "repeatability_coverage",
            ),
            (
                "policy-bool-int-alias",
                lambda document: document["metric_policy"].update(
                    {"paths_or_content_retained": 0}
                ),
                "repeatability_policy",
            ),
        )
        for name, mutate, error in direct_mutations:
            with self.subTest(
                name=name
            ), tempfile.TemporaryDirectory() as temporary:
                artifact, _, _, _ = self._fixture(Path(temporary))
                repeatability = json.loads(
                    (artifact / "repeatability.json").read_text(
                        encoding="utf-8"
                    )
                )
                mutate(repeatability)
                with self.assertRaisesRegex(
                    self.checker.EvidenceError,
                    error,
                ):
                    self.checker._validate_repeatability(repeatability)

        with tempfile.TemporaryDirectory() as temporary:
            artifact, baseline, lock, wrapper = self._fixture(Path(temporary))
            repeatability_path = artifact / "repeatability.json"
            repeatability = json.loads(
                repeatability_path.read_text(encoding="utf-8")
            )
            repeatability["reports"]["baseline"]["sha256"] = "f" * 64
            self._write(repeatability_path, repeatability)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "repeatability_source_report_binding",
            ):
                self.checker.validate(
                    artifact,
                    self.head_sha,
                    baseline,
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )

    def test_native_pdf_coverage_path_counts_are_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, _, _, _ = self._fixture(Path(temporary))
            fidelity = json.loads(
                (artifact / "fidelity-a.json").read_text(encoding="utf-8")
            )
            fidelity["coverage"]["native_pdf_type0_truetype_font_objects"] = 799
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "fidelity_native_pdf_coverage",
            ):
                self.checker._validate_fidelity_gate(fidelity)

            authored = json.loads(
                (artifact / "authored-print-gate.json").read_text(
                    encoding="utf-8"
                )
            )
            authored["coverage"]["native_pdf_type3_font_objects"] = 1
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "authored_native_pdf_coverage",
            ):
                self.checker._validate_authored_gate(authored)

    def test_repeatability_baseline_contract_and_observed_drift_are_bound(
        self,
    ) -> None:
        observed = {
            "blurred_luma_similarity_ppm": 101,
            "edge_f1_ppm": 303,
            "foreground_f1_ppm": 404,
            "similarity_ppm": 202,
            "text_ink_f1_ppm": 505,
        }
        with tempfile.TemporaryDirectory() as temporary:
            artifact, _, lock, wrapper = self._fixture(
                Path(temporary),
                baseline_mode="candidate",
            )
            repeatability_path = artifact / "repeatability.json"
            repeatability = json.loads(
                repeatability_path.read_text(encoding="utf-8")
            )

            def set_observed(distribution: dict[str, object], value: int) -> None:
                distribution["absolute_deltas_ppm"] = [0] * 800 + [value]
                distribution["max_absolute_delta_ppm"] = value

            set_observed(
                repeatability["drift"]["blurred_luma_similarity"],
                observed["blurred_luma_similarity_ppm"],
            )
            set_observed(
                repeatability["drift"]["similarity"],
                observed["similarity_ppm"],
            )
            masks = repeatability["drift"]["mask_f1"]
            set_observed(masks["edge"], observed["edge_f1_ppm"])
            set_observed(
                masks["foreground"],
                observed["foreground_f1_ppm"],
            )
            set_observed(masks["text_ink"], observed["text_ink_f1_ppm"])
            masks["max_absolute_delta_ppm"] = max(
                observed["edge_f1_ppm"],
                observed["foreground_f1_ppm"],
                observed["text_ink_f1_ppm"],
            )
            self._write(repeatability_path, repeatability)
            self._rebind_repeatability(artifact)

            self.checker._validate_repeatability(repeatability)
            self.assertEqual(
                self.checker._repeatability_score_drift_limits(
                    repeatability
                ),
                observed,
            )
            candidates = [
                json.loads(
                    (artifact / f"baseline-candidate-{label}.json").read_text(
                        encoding="utf-8"
                    )
                )
                for label in ("a", "b")
            ]
            renderer = json.loads(
                (artifact / "renderer.json").read_text(encoding="utf-8")
            )
            fidelity_results = [
                self.checker._validate_fidelity_gate(
                    json.loads(
                        (
                            artifact / f"fidelity-{label}.json"
                        ).read_text(encoding="utf-8")
                    )
                )
                for label in ("a", "b")
            ]
            self.checker._validate_repeatability_bindings(
                repeatability,
                candidates,
                renderer,
                fidelity_results,
            )

            changed_configuration = copy.deepcopy(repeatability)
            changed_configuration["identity"]["baseline_contract"][
                "configuration"
            ].update(
                {
                    "baseline_sha256": "e" * 64,
                    "candidate_sha256": "e" * 64,
                }
            )
            self.checker._validate_repeatability(changed_configuration)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "repeatability_configuration_binding",
            ):
                self.checker._validate_repeatability_bindings(
                    changed_configuration,
                    candidates,
                    renderer,
                    fidelity_results,
                )

            changed_input = copy.deepcopy(repeatability)
            changed_input["identity"]["baseline_contract"]["input_set"].update(
                {
                    "baseline_sha256": "e" * 64,
                    "candidate_sha256": "e" * 64,
                }
            )
            self.checker._validate_repeatability(changed_input)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "repeatability_input_binding",
            ):
                self.checker._validate_repeatability_bindings(
                    changed_input,
                    candidates,
                    renderer,
                    fidelity_results,
                )

            baseline_checker = self.checker._load_baseline_checker()
            adoption = baseline_checker.conservative_adoption_baseline
            with mock.patch.object(
                baseline_checker,
                "conservative_adoption_baseline",
                wraps=adoption,
            ) as adopt:
                _, receipt = (
                    self.checker.build_adoption_baseline_and_receipt(
                        artifact,
                        head_sha=self.head_sha,
                        workflow_run_id=101,
                        workflow_run_attempt=2,
                        artifact_id=303,
                        artifact_name=(
                            f"render-oracle-{self.head_sha}-101-2-full-candidate"
                        ),
                        artifact_size_bytes=4096,
                        artifact_digest="sha256:" + "a" * 64,
                        artifact_repository="HyunjoJung/rxls",
                        oracle_lock=lock,
                        oracle_wrapper=wrapper,
                    )
                )
            self.assertEqual(
                adopt.call_args.kwargs["max_score_drift_ppm"],
                observed,
            )
            self.assertEqual(
                receipt["policy"]["observed_score_drift_maximum_ppm"],
                observed,
            )

    def test_candidate_corpus_bindings_are_authoritative_and_nonzero(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, _, lock, wrapper = self._fixture(
                Path(temporary),
                baseline_mode="candidate",
            )
            candidate_path = artifact / "baseline-candidate-a.json"
            candidate = json.loads(candidate_path.read_text(encoding="utf-8"))
            candidate["campaign"]["manifest_sha256"] = "0" * 64
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "candidate_invalid:baseline_campaign_hosted_full_identity",
            ):
                self.checker._validate_candidate(candidate)

        with tempfile.TemporaryDirectory() as temporary:
            artifact, _, _, _ = self._fixture(
                Path(temporary),
                baseline_mode="candidate",
            )
            candidate = json.loads(
                (artifact / "baseline-candidate-a.json").read_text(
                    encoding="utf-8"
                )
            )
            candidate["cohorts"]["all"]["workbooks"] = 1
            candidate["cohorts"]["all"]["comparable_workbooks"] = 1
            for metric_kind in ("scores", "deltas"):
                for distribution in candidate["cohorts"]["all"][
                    metric_kind
                ].values():
                    distribution["count"] = 1
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "candidate_invalid:campaign_all_cohort",
            ):
                self.checker._validate_candidate(candidate)

        mutations = (
            "summary_manifest",
            "summary_feature_counts",
            "fidelity_manifest",
            "fidelity_input",
            "fidelity_feature_map",
        )
        for mutation in mutations:
            with self.subTest(
                mutation=mutation
            ), tempfile.TemporaryDirectory() as temporary:
                artifact, _, lock, wrapper = self._fixture(
                    Path(temporary),
                    baseline_mode="candidate",
                )
                if mutation.startswith("summary_"):
                    path = artifact / "hosted-summary.json"
                    document = json.loads(path.read_text(encoding="utf-8"))
                    if mutation == "summary_manifest":
                        document["corpus"]["manifest_sha256"] = "f" * 64
                    else:
                        document["corpus"]["feature_counts"] = {
                            "unicode-text": 799
                        }
                    self._write(path, document)
                else:
                    path = artifact / "fidelity-a.json"
                    document = json.loads(path.read_text(encoding="utf-8"))
                    key = mutation.removeprefix("fidelity_") + "_sha256"
                    document["evidence"][key] = "f" * 64
                    self._write(path, document)

                with self.assertRaises(self.checker.EvidenceError):
                    self.checker.validate(
                        artifact,
                        self.head_sha,
                        None,
                        baseline_mode="candidate",
                        oracle_lock=lock,
                        oracle_wrapper=wrapper,
                    )

    def test_builds_order_neutral_path_neutral_adoption_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, _, lock, wrapper = self._fixture(
                Path(temporary),
                baseline_mode="candidate",
            )
            candidate_b_path = artifact / "baseline-candidate-b.json"
            candidate_b = json.loads(
                candidate_b_path.read_text(encoding="utf-8")
            )
            baseline_checker = self.checker._load_baseline_checker()
            for group in candidate_b["groups"]:
                group["scores"]["similarity_ppm"] = [
                    [899_600, group["workbooks"]]
                ]
            (
                candidate_b["cohorts"],
                candidate_b["histograms"],
            ) = baseline_checker._certificate_views_from_groups(
                candidate_b["groups"]
            )
            self._write(candidate_b_path, candidate_b)
            self._rebind_candidate(artifact, "b")
            repeatability_path = artifact / "repeatability.json"
            repeatability = json.loads(
                repeatability_path.read_text(encoding="utf-8")
            )
            similarity_drift = repeatability["drift"]["similarity"]
            similarity_drift["absolute_deltas_ppm"] = [400] * 801
            similarity_drift["max_absolute_delta_ppm"] = 400
            self._write(repeatability_path, repeatability)
            self._rebind_repeatability(artifact)
            self.checker.validate(
                artifact,
                self.head_sha,
                None,
                baseline_mode="candidate",
                oracle_lock=lock,
                oracle_wrapper=wrapper,
            )
            adopted_payload, receipt = (
                self.checker.build_adoption_baseline_and_receipt(
                    artifact,
                    head_sha=self.head_sha,
                    workflow_run_id=101,
                    workflow_run_attempt=2,
                    artifact_id=303,
                    artifact_name=(
                        f"render-oracle-{self.head_sha}-101-2-full-candidate"
                    ),
                    artifact_size_bytes=4096,
                    artifact_digest="sha256:" + "a" * 64,
                    artifact_repository="HyunjoJung/rxls",
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )
            )

        adopted = json.loads(adopted_payload)
        self.assertEqual(
            adopted["cohorts"]["all"]["scores"]["similarity_ppm"]["p10"],
            899_600,
        )
        self.assertEqual(
            receipt["adopted_baseline_sha256"],
            hashlib.sha256(adopted_payload).hexdigest(),
        )
        self.assertEqual(receipt["previous_baseline_sha256"], None)
        self.assertEqual(receipt["head_sha"], self.head_sha)
        self.assertEqual(receipt["workflow"], {"run_attempt": 2, "run_id": 101})
        self.assertEqual(len(receipt["candidate_sha256"]), 2)
        self.assertEqual(
            receipt["candidate_sha256"],
            sorted(receipt["candidate_sha256"]),
        )
        self.assertEqual(
            receipt["policy"]["id"],
            "rxls.repeatability-bounded-ratchet-envelope.v1",
        )
        self.assertEqual(
            receipt["policy"]["observed_score_drift_maximum_ppm"][
                "similarity_ppm"
            ],
            400,
        )
        self.checker._path_neutral(receipt)

    def test_adoption_rejects_unbounded_candidate_drift(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, _, lock, wrapper = self._fixture(
                Path(temporary),
                baseline_mode="candidate",
            )
            candidate_b_path = artifact / "baseline-candidate-b.json"
            candidate_b = json.loads(
                candidate_b_path.read_text(encoding="utf-8")
            )
            baseline_checker = self.checker._load_baseline_checker()
            metric = "max_page_width_delta_pixels"
            for group in candidate_b["groups"]:
                group["deltas"][metric] = [[1, group["workbooks"]]]
            (
                candidate_b["cohorts"],
                candidate_b["histograms"],
            ) = baseline_checker._certificate_views_from_groups(
                candidate_b["groups"]
            )
            self._write(candidate_b_path, candidate_b)
            self._rebind_candidate(artifact, "b")
            self.checker.validate(
                artifact,
                self.head_sha,
                None,
                baseline_mode="candidate",
                oracle_lock=lock,
                oracle_wrapper=wrapper,
            )
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "adoption_unbounded_group_drift",
            ):
                self.checker.build_adoption_baseline_and_receipt(
                    artifact,
                    head_sha=self.head_sha,
                    workflow_run_id=101,
                    workflow_run_attempt=2,
                    artifact_id=303,
                    artifact_name=(
                        f"render-oracle-{self.head_sha}-101-2-full-candidate"
                    ),
                    artifact_size_bytes=4096,
                    artifact_digest="sha256:" + "a" * 64,
                    artifact_repository="HyunjoJung/rxls",
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )

    def test_adoption_rejects_score_drift_above_observed_maximum(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, _, lock, wrapper = self._fixture(
                Path(temporary),
                baseline_mode="candidate",
            )
            candidate_b_path = artifact / "baseline-candidate-b.json"
            candidate_b = json.loads(
                candidate_b_path.read_text(encoding="utf-8")
            )
            baseline_checker = self.checker._load_baseline_checker()
            metric = "similarity_ppm"
            for group in candidate_b["groups"]:
                group["scores"][metric] = [
                    [899_999, group["workbooks"]]
                ]
            (
                candidate_b["cohorts"],
                candidate_b["histograms"],
            ) = baseline_checker._certificate_views_from_groups(
                candidate_b["groups"]
            )
            self._write(candidate_b_path, candidate_b)
            self._rebind_candidate(artifact, "b")

            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "adoption_group_drift_threshold",
            ):
                self.checker.build_adoption_baseline_and_receipt(
                    artifact,
                    head_sha=self.head_sha,
                    workflow_run_id=101,
                    workflow_run_attempt=2,
                    artifact_id=303,
                    artifact_name=(
                        f"render-oracle-{self.head_sha}-101-2-full-candidate"
                    ),
                    artifact_size_bytes=4096,
                    artifact_digest="sha256:" + "a" * 64,
                    artifact_repository="HyunjoJung/rxls",
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )

    def test_exact_clean_checkout_and_atomic_no_clobber_are_required(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scripts = root / "scripts"
            scripts.mkdir()
            seed = root / "seed.txt"
            seed.write_text("seed\n", encoding="utf-8")
            for command in (
                ["git", "init", "--quiet"],
                ["git", "config", "user.name", "test"],
                ["git", "config", "user.email", "test@example.invalid"],
                ["git", "add", "seed.txt"],
                ["git", "commit", "--quiet", "-m", "seed"],
            ):
                subprocess.run(command, cwd=root, check=True)
            head_sha = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=root,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            destination = scripts / "render-parity-baseline-full.json"
            self.assertEqual(
                self.checker.validate_adoption_checkout(
                    destination,
                    head_sha,
                    repository_root=root,
                ).resolve(strict=False),
                destination.resolve(strict=False),
            )

            dirty = root / "dirty.txt"
            dirty.write_text("dirty\n", encoding="utf-8")
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "adoption_checkout_dirty",
            ):
                self.checker.validate_adoption_checkout(
                    destination,
                    head_sha,
                    repository_root=root,
                )
            dirty.unlink()
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "adoption_checkout_head",
            ):
                self.checker.validate_adoption_checkout(
                    destination,
                    "f" * 40,
                    repository_root=root,
                )

            payload = b'{\n  "schema": "test"\n}\n'
            self.checker.write_new_atomic(destination, payload)
            self.assertEqual(destination.read_bytes(), payload)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "adoption_destination_exists",
            ):
                self.checker.write_new_atomic(destination, b"replacement")
            self.assertEqual(destination.read_bytes(), payload)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scripts = root / "scripts"
            scripts.mkdir()
            seed = root / "seed.txt"
            seed.write_text("seed\n", encoding="utf-8")
            destination = scripts / "render-parity-baseline-full.json"
            destination.symlink_to(seed)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "adoption_destination_exists",
            ):
                self.checker.validate_adoption_checkout(
                    destination,
                    "a" * 40,
                    repository_root=root,
                )

    def test_adoption_pair_is_no_clobber_and_rolls_back_partial_install(
        self,
    ) -> None:
        baseline_payload = b'{\n  "schema": "baseline"\n}\n'
        receipt_payload = b'{\n  "schema": "receipt"\n}\n'
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline.json"
            receipt = root / "receipt.json"

            self.checker.write_adoption_pair_atomic(
                baseline,
                baseline_payload,
                receipt,
                receipt_payload,
            )
            self.assertEqual(baseline.read_bytes(), baseline_payload)
            self.assertEqual(receipt.read_bytes(), receipt_payload)

            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "adoption_destination_exists",
            ):
                self.checker.write_adoption_pair_atomic(
                    baseline,
                    b"replacement baseline",
                    receipt,
                    b"replacement receipt",
                )
            self.assertEqual(baseline.read_bytes(), baseline_payload)
            self.assertEqual(receipt.read_bytes(), receipt_payload)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline.json"
            receipt = root / "receipt.json"
            real_write = self.checker.write_new_atomic
            writes = 0

            def fail_second_write(path: Path, payload: bytes) -> None:
                nonlocal writes
                writes += 1
                if writes == 2:
                    raise self.checker.EvidenceError("injected")
                real_write(path, payload)

            with mock.patch.object(
                self.checker,
                "write_new_atomic",
                side_effect=fail_second_write,
            ), self.assertRaisesRegex(
                self.checker.EvidenceError,
                "injected",
            ):
                self.checker.write_adoption_pair_atomic(
                    baseline,
                    baseline_payload,
                    receipt,
                    receipt_payload,
                )
            self.assertFalse(baseline.exists())
            self.assertFalse(receipt.exists())

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline.json"
            receipt = root / "receipt.json"
            existing = b"existing baseline"
            baseline.write_bytes(existing)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "adoption_destination_exists",
            ):
                self.checker.write_adoption_pair_atomic(
                    baseline,
                    baseline_payload,
                    receipt,
                    receipt_payload,
                )
            self.assertEqual(baseline.read_bytes(), existing)
            self.assertFalse(receipt.exists())

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline.json"
            receipt = root / "receipt.json"
            with mock.patch.object(
                self.checker,
                "_fsync_directory",
                side_effect=[
                    None,
                    OSError("injected fsync failure"),
                    None,
                    None,
                ],
            ) as fsync_directory, self.assertRaisesRegex(
                self.checker.EvidenceError,
                "adoption_write",
            ):
                self.checker.write_adoption_pair_atomic(
                    baseline,
                    baseline_payload,
                    receipt,
                    receipt_payload,
                )
            self.assertEqual(fsync_directory.call_count, 4)
            self.assertFalse(baseline.exists())
            self.assertFalse(receipt.exists())

    def test_verify_gate_is_recomputed_even_when_receipts_are_rebound(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, baseline, lock, wrapper = self._fixture(Path(temporary))
            gate_path = artifact / "baseline-gate-a.json"
            gate = json.loads(gate_path.read_text(encoding="utf-8"))
            gate["campaign"]["sha256"] = "f" * 64
            gate_payload = self._write(gate_path, gate)

            summary_path = artifact / "hosted-summary.json"
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
            summary["baseline_ratcheting"]["gates"][0].update(
                {
                    "bytes": len(gate_payload),
                    "sha256": hashlib.sha256(gate_payload).hexdigest(),
                }
            )
            summary["evidence_runs"][0].update(
                {
                    "baseline_gate_bytes": len(gate_payload),
                    "baseline_gate_sha256": hashlib.sha256(
                        gate_payload
                    ).hexdigest(),
                }
            )
            self._write(summary_path, summary)

            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "baseline_gate_recomputed",
            ):
                self.checker.validate(
                    artifact,
                    self.head_sha,
                    baseline,
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )

    def test_deep_gate_and_summary_contracts_reject_adversarial_mutations(
        self,
    ) -> None:
        mutations = {
            "fidelity_policy": (
                "fidelity-a.json",
                lambda value: value["policy"].update(
                    {"minimum_core_workbooks": 11}
                ),
                "fidelity_policy",
            ),
            "fidelity_policy_bool_int_alias": (
                "fidelity-a.json",
                lambda value: value["policy"].update(
                    {"minimum_hard_feature_workbooks": True}
                ),
                "fidelity_policy",
            ),
            "fidelity_threshold": (
                "fidelity-a.json",
                lambda value: value["thresholds"].update(
                    {"core_similarity_min_ppm": 979_999}
                ),
                "fidelity_thresholds",
            ),
            "fidelity_threshold_bool_int_alias": (
                "fidelity-a.json",
                lambda value: value["thresholds"].update(
                    {
                        "pdf_imported_page_box_quantization_max_micropoints": (
                            True
                        )
                    }
                ),
                "fidelity_thresholds",
            ),
            "fidelity_metric": (
                "fidelity-a.json",
                lambda value: value["metrics"].update(
                    {"core_similarity_mean_ppm": 979_999}
                ),
                "fidelity_metric_threshold",
            ),
            "fidelity_text_ratio": (
                "fidelity-a.json",
                lambda value: value["metrics"].update(
                    {"text_box_precision_ppm": 1_000_000}
                ),
                "fidelity_text_threshold",
            ),
            "fidelity_source_bytes": (
                "fidelity-a.json",
                lambda value: value["evidence"].update({"bytes": 1235}),
                "baseline_gate_recomputed",
            ),
            "authored_threshold": (
                "authored-print-gate.json",
                lambda value: value["thresholds"].update(
                    {"similarity_mean_min_ppm": 949_999}
                ),
                "authored_thresholds",
            ),
            "authored_threshold_bool_int_alias": (
                "authored-print-gate.json",
                lambda value: value["thresholds"].update(
                    {"pdf_point_geometry_exact": 1}
                ),
                "authored_thresholds",
            ),
            "authored_geometry_order": (
                "authored-print-gate.json",
                lambda value: value["metrics"].update(
                    {
                        "page_box_median_millipoints": 2,
                        "page_box_p95_millipoints": 1,
                    }
                ),
                "authored_geometry",
            ),
            "authored_report_bytes": (
                "authored-print-gate.json",
                lambda value: value["evidence"].update({"report_bytes": 0}),
                "authored_source_report_bytes",
            ),
            "evidence_candidate_bytes": (
                "hosted-summary.json",
                lambda value: value["evidence_runs"][0].update(
                    {
                        "baseline_candidate_bytes": (
                            value["evidence_runs"][0][
                                "baseline_candidate_bytes"
                            ]
                            + 1
                        )
                    }
                ),
                "summary_fidelity_identity",
            ),
            "evidence_campaign_sha": (
                "hosted-summary.json",
                lambda value: value["evidence_runs"][0].update(
                    {"campaign_sha256": "f" * 64}
                ),
                "summary_fidelity_identity",
            ),
            "summary_metrics": (
                "hosted-summary.json",
                lambda value: value["metrics"]["all"]["scores"][
                    "similarity_ppm"
                ].update({"mean": 899_999}),
                "summary_metrics",
            ),
            "summary_shard_formats": (
                "hosted-summary.json",
                lambda value: value["campaign"][
                    "shard_format_counts"
                ][0].update({"xlsx": 49}),
                "summary_shard_formats",
            ),
            "summary_font_key": (
                "hosted-summary.json",
                lambda value: value["font_pack"].update(
                    {"private_note": "secret"}
                ),
                "summary_font_pack",
            ),
            "summary_input_set": (
                "hosted-summary.json",
                lambda value: value["corpus"].update(
                    {"input_set_sha256": "f" * 64}
                ),
                "summary_corpus",
            ),
            "summary_group_topology": (
                "hosted-summary.json",
                lambda value: value["corpus"].update(
                    {"group_topology_sha256": "f" * 64}
                ),
                "summary_corpus",
            ),
            "host_tools_key": (
                "host-tools.json",
                lambda value: value.update({"private_note": "secret"}),
                "host_identity_schema",
            ),
            "build_label_key": (
                "build.json",
                lambda value: value["reproducibility"]["identities"][0][
                    "labels"
                ].update({"private_note": "secret"}),
                "build_reproducibility_identities",
            ),
        }
        for name, (filename, mutate, error) in mutations.items():
            with self.subTest(
                name=name
            ), tempfile.TemporaryDirectory() as temporary:
                artifact, baseline, lock, wrapper = self._fixture(
                    Path(temporary)
                )
                path = artifact / filename
                value = json.loads(path.read_text(encoding="utf-8"))
                mutate(value)
                self._write(path, value)
                with self.assertRaisesRegex(
                    self.checker.EvidenceError,
                    error,
                ):
                    self.checker.validate(
                        artifact,
                        self.head_sha,
                        baseline,
                        oracle_lock=lock,
                        oracle_wrapper=wrapper,
                    )

    def test_repeatability_maxima_and_transitive_source_receipts_are_exact(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, baseline, lock, wrapper = self._fixture(Path(temporary))
            repeatability_path = artifact / "repeatability.json"
            repeatability = json.loads(
                repeatability_path.read_text(encoding="utf-8")
            )
            repeatability["drift"]["similarity"][
                "max_absolute_delta_ppm"
            ] = 1
            self._write(repeatability_path, repeatability)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "repeatability_distribution",
            ):
                self.checker.validate(
                    artifact,
                    self.head_sha,
                    baseline,
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )

        with tempfile.TemporaryDirectory() as temporary:
            artifact, baseline, lock, wrapper = self._fixture(Path(temporary))
            repeatability_path = artifact / "repeatability.json"
            repeatability = json.loads(
                repeatability_path.read_text(encoding="utf-8")
            )
            repeatability["reports"]["candidate"]["bytes"] += 1
            self._write(repeatability_path, repeatability)
            self._rebind_repeatability(artifact)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "repeatability_source_report_binding",
            ):
                self.checker.validate(
                    artifact,
                    self.head_sha,
                    baseline,
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )

    def test_adoption_reuses_full_validation_and_interrupts_roll_back(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, _, lock, wrapper = self._fixture(
                Path(temporary),
                baseline_mode="candidate",
            )
            authored_path = artifact / "authored-print-gate.json"
            authored = json.loads(
                authored_path.read_text(encoding="utf-8")
            )
            authored["thresholds"]["similarity_mean_min_ppm"] = 0
            self._write(authored_path, authored)
            with self.assertRaisesRegex(
                self.checker.EvidenceError,
                "authored_thresholds",
            ):
                self.checker.build_adoption_baseline_and_receipt(
                    artifact,
                    head_sha=self.head_sha,
                    workflow_run_id=101,
                    workflow_run_attempt=2,
                    artifact_id=303,
                    artifact_name=(
                        f"render-oracle-{self.head_sha}-101-2-full-candidate"
                    ),
                    artifact_size_bytes=4096,
                    artifact_digest="sha256:" + "a" * 64,
                    artifact_repository="HyunjoJung/rxls",
                    oracle_lock=lock,
                    oracle_wrapper=wrapper,
                )

        for exception in (KeyboardInterrupt(), SystemExit(17)):
            with self.subTest(
                exception=type(exception).__name__
            ), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                destination = root / "baseline.json"
                with mock.patch.object(
                    self.checker,
                    "_fsync_directory",
                    side_effect=[exception, None],
                ):
                    with self.assertRaises(type(exception)):
                        self.checker.write_new_atomic(
                            destination,
                            b"baseline\n",
                        )
                self.assertFalse(destination.exists())
                self.assertEqual(list(root.glob(".*.tmp")), [])

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = root / "baseline.json"
            receipt = root / "receipt.json"
            original = self.checker.write_new_atomic
            calls = 0

            def interrupt_second(path: Path, payload: bytes) -> None:
                nonlocal calls
                calls += 1
                if calls == 2:
                    raise SystemExit(23)
                original(path, payload)

            with mock.patch.object(
                self.checker,
                "write_new_atomic",
                side_effect=interrupt_second,
            ):
                with self.assertRaises(SystemExit) as raised:
                    self.checker.write_adoption_pair_atomic(
                        baseline,
                        b"baseline\n",
                        receipt,
                        b"receipt\n",
                    )
            self.assertEqual(raised.exception.code, 23)
            self.assertFalse(baseline.exists())
            self.assertFalse(receipt.exists())

    def test_rejects_unauthenticated_v3_build_and_summary_vectors(self) -> None:
        mutations = (
            "schema_v2",
            "extra_build_key",
            "unpaired_manifest_pin",
            "build_contract",
            "lock_file",
            "source_commit",
            "wrapper_sha256",
            "one_identity",
            "unequal_identity",
            "identity_hash",
            "rootfs_hash",
            "config_vector",
            "manifest_vector",
            "descriptor_vector",
            "fidelity_manifest_binding",
            "authored_manifest_binding",
            "summary_manifest",
            "summary_v5",
            "summary_source",
            "summary_wrapper",
            "summary_contract",
            "changed_wrapper_file",
            "changed_lock_pin",
            "receipt_null",
            "receipt_artifact_name",
            "receipt_job_run",
            "receipt_conclusion",
            "receipt_repository",
            "receipt_evidence_size",
            "receipt_id_overflow",
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                artifact, baseline, lock, wrapper = self._fixture(Path(temporary))
                build_path = artifact / "build.json"
                summary_path = artifact / "hosted-summary.json"
                build = json.loads(build_path.read_text(encoding="utf-8"))
                summary = json.loads(summary_path.read_text(encoding="utf-8"))

                if mutation == "schema_v2":
                    build["schema"] = "rxls.render-oracle-container-build.v2"
                elif mutation == "extra_build_key":
                    build["trusted"] = True
                elif mutation == "unpaired_manifest_pin":
                    build["expected_manifest_digest"] = None
                elif mutation == "build_contract":
                    build["build_contract_sha256"] = "0" * 64
                elif mutation == "lock_file":
                    build["lock_file_sha256"] = "0" * 64
                elif mutation == "source_commit":
                    build["source_commit"] = "b" * 40
                elif mutation == "wrapper_sha256":
                    build["wrapper_sha256"] = "0" * 64
                elif mutation == "one_identity":
                    build["reproducibility"]["identities"] = build[
                        "reproducibility"
                    ]["identities"][:1]
                elif mutation == "unequal_identity":
                    build["reproducibility"]["identities"][1]["created"] = (
                        "2026-07-13T00:00:01Z"
                    )
                elif mutation == "identity_hash":
                    build["reproducibility"]["identities"][0][
                        "identity_sha256"
                    ] = "0" * 64
                    build["reproducibility"]["identities"][1][
                        "identity_sha256"
                    ] = "0" * 64
                    build["reproducibility"]["identity_sha256"] = ["0" * 64] * 2
                elif mutation == "rootfs_hash":
                    build["reproducibility"]["identities"][0][
                        "rootfs_diff_ids_sha256"
                    ] = "0" * 64
                    build["reproducibility"]["identities"][1][
                        "rootfs_diff_ids_sha256"
                    ] = "0" * 64
                    build["reproducibility"]["rootfs_diff_ids_sha256"] = [
                        "0" * 64
                    ] * 2
                elif mutation == "config_vector":
                    build["reproducibility"]["config_ids"] = build[
                        "reproducibility"
                    ]["config_ids"][:1]
                elif mutation == "manifest_vector":
                    build["reproducibility"]["manifest_digests"][1] = (
                        "sha256:" + "a" * 64
                    )
                elif mutation == "descriptor_vector":
                    build["reproducibility"]["descriptor_sizes"] = [12345]
                elif mutation == "fidelity_manifest_binding":
                    fidelity_path = artifact / "fidelity-a.json"
                    fidelity = json.loads(fidelity_path.read_text(encoding="utf-8"))
                    fidelity["evidence"]["oracle_image_manifest_digest"] = (
                        "sha256:" + "a" * 64
                    )
                    self._write(fidelity_path, fidelity)
                elif mutation == "authored_manifest_binding":
                    authored_path = artifact / "authored-print-gate.json"
                    authored = json.loads(authored_path.read_text(encoding="utf-8"))
                    authored["evidence"]["oracle_image_manifest_digest"] = (
                        "sha256:" + "a" * 64
                    )
                    self._write(authored_path, authored)
                elif mutation == "summary_manifest":
                    del summary["container"]["manifest_digest"]
                elif mutation == "summary_v5":
                    summary["schema"] = "rxls.render-oracle-hosted-campaign.v5"
                elif mutation == "summary_source":
                    summary["container"]["source_commit"] = "b" * 40
                elif mutation == "summary_wrapper":
                    summary["container"]["wrapper_sha256"] = "0" * 64
                elif mutation == "summary_contract":
                    summary["container"]["build_contract_sha256"] = "0" * 64
                elif mutation == "changed_wrapper_file":
                    wrapper.write_bytes(wrapper.read_bytes() + b"# changed\n")
                elif mutation == "changed_lock_pin":
                    lock_document = json.loads(lock.read_text(encoding="utf-8"))
                    lock_document["built_image"]["expected_id"] = (
                        "sha256:" + "a" * 64
                    )
                    self._write(lock, lock_document)
                else:
                    lock_document = json.loads(lock.read_text(encoding="utf-8"))
                    receipt = lock_document["built_image"]["bootstrap_receipt"]
                    if mutation == "receipt_null":
                        lock_document["built_image"]["bootstrap_receipt"] = None
                    elif mutation == "receipt_artifact_name":
                        receipt["artifact"]["name"] = "render-oracle-image-unbound"
                    elif mutation == "receipt_job_run":
                        receipt["job"]["run_id"] += 1
                    elif mutation == "receipt_conclusion":
                        receipt["run"]["conclusion"] = "success"
                    elif mutation == "receipt_repository":
                        receipt["repository"]["id"] += 1
                    elif mutation == "receipt_id_overflow":
                        receipt["run"]["id"] = 1 << 63
                        receipt["job"]["run_id"] = 1 << 63
                    else:
                        receipt["evidence"]["bytes"] = 0
                    self._write(lock, lock_document)

                self._write(build_path, build)
                self._write(summary_path, summary)
                with self.assertRaises(self.checker.EvidenceError):
                    self.checker.validate(
                        artifact,
                        self.head_sha,
                        baseline,
                        oracle_lock=lock,
                        oracle_wrapper=wrapper,
                    )


if __name__ == "__main__":
    unittest.main()
