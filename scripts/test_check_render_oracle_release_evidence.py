#!/usr/bin/env python3
"""Tests for npm tag Render Oracle prerequisite evidence."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "scripts" / "check_render_oracle_release_evidence.py"


def _load():
    spec = importlib.util.spec_from_file_location(
        "check_render_oracle_release_evidence", CHECKER
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RenderOracleReleaseEvidenceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.checker = _load()
        cls.head_sha = "a" * 40

    def _write(self, path: Path, value: object) -> bytes:
        payload = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()
        path.write_bytes(payload)
        return payload

    def _fixture(self, root: Path) -> tuple[Path, Path]:
        artifact = root / "artifact"
        artifact.mkdir()
        baseline = root / "reviewed-baseline.json"
        reviewed = {"schema": "rxls.render-parity-baseline.v2", "fixture": True}
        self._write(baseline, reviewed)
        reviewed_sha = self.checker._canonical_sha256(reviewed)
        campaign = {
            "schema": "rxls.render-parity-campaign.v1",
            "kind": "project_generated_hosted_full",
            "profile": "full",
            "case_count": 800,
            "format_counts": {"ods": 200, "xls": 200, "xlsb": 200, "xlsx": 200},
            "feature_counts": {},
            "manifest_sha256": "b" * 64,
            "input_set_sha256": "c" * 64,
        }
        warning_policy = {
            "candidate_code_count": 0,
            "candidate_counts_sha256": "d" * 64,
            "reviewed_code_count": 0,
            "reviewed_counts_sha256": "e" * 64,
            "reviewed_codes_sha256": "f" * 64,
            "unclassified_codes": [],
        }
        candidates = []
        gates = []
        for label in ("a", "b"):
            candidate = {
                "schema": "rxls.render-parity-baseline.v2",
                "input_files": 800,
                "input_set_sha256": "c" * 64,
                "warning_counts": {},
                "campaign": campaign,
            }
            candidate_payload = self._write(
                artifact / f"baseline-candidate-{label}.json", candidate
            )
            gate = {
                "schema": "rxls.render-parity-baseline-check.v1",
                "passed": True,
                "failures": [],
                "baseline_sha256": reviewed_sha,
                "candidate_sha256": self.checker._canonical_sha256(candidate),
                "warning_policy": warning_policy,
                "campaign": {
                    "case_count": 800,
                    "kind": "project_generated_hosted_full",
                    "manifest_sha256": "b" * 64,
                    "sha256": self.checker._canonical_sha256(campaign),
                },
            }
            gate_payload = self._write(
                artifact / f"baseline-gate-{label}.json", gate
            )
            candidates.append((candidate, candidate_payload))
            gates.append((gate, gate_payload))

        fidelities = []
        for label in ("a", "b"):
            fidelity = {
                "schema": "rxls.render-fidelity-targets.v1",
                "passed": True,
                "failures": [],
                "coverage": {"report_workbooks": 800},
                "metrics": {"similarity_ppm": 999_000},
                "thresholds": {"similarity_ppm": 950_000},
            }
            payload = self._write(artifact / f"fidelity-{label}.json", fidelity)
            fidelities.append((fidelity, payload))
        authored = {
            "schema": "rxls.authored-print-parity.v1",
            "passed": True,
            "failures": [],
            "coverage": {"workbooks": 100, "pages": 400},
            "evidence": {"report_sha256": "1" * 64},
            "expected": {"workbooks": 100},
            "metrics": {"similarity_ppm": 999_000},
            "thresholds": {"similarity_ppm": 950_000},
        }
        authored_payload = self._write(artifact / "authored-print-gate.json", authored)
        repeatability = {
            "schema": "rxls.libreoffice-render-repeatability.v1",
            "status": "pass",
            "failures": [],
            "coverage": {"workbooks": 800},
            "thresholds_ppm": {"maximum": 20_000},
        }
        repeatability_payload = self._write(
            artifact / "repeatability.json", repeatability
        )
        build = {
            "image_identity_status": "pinned_match",
            "expected_image_id": "sha256:" + "2" * 64,
            "built_image_id": "sha256:" + "2" * 64,
        }
        self._write(artifact / "build.json", build)
        host_tools = {
            "identity_status": "pinned_match",
            "captured_identity_sha256": "3" * 64,
            "expected_identity_sha256": "3" * 64,
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
                    "campaign_sha256": self.checker._canonical_sha256(campaign),
                    "sha256": self.checker._sha256(candidate_payload),
                    "warning_counts": {},
                }
            )
            baseline_gates.append(
                {
                    "baseline_sha256": reviewed_sha,
                    "candidate_sha256": gate["candidate_sha256"],
                    "failures": [],
                    "passed": True,
                    "sha256": self.checker._sha256(gate_payload),
                    "warning_policy": warning_policy,
                }
            )
            evidence_runs.append(
                {
                    "fidelity_gate_sha256": self.checker._sha256(fidelity_payload),
                    "report_bytes": 1234,
                    "report_sha256": str(index + 5) * 64,
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
            "schema": "rxls.render-oracle-hosted-campaign.v4",
            "head_sha": self.head_sha,
            "campaign": {
                "mode": "full",
                "case_count": 800,
                "repetitions": 2,
                "shard_count": 4,
                "parallel_shards": 2,
                "shard_case_counts": [200, 200, 200, 200],
            },
            "summary": {"files": 800, "by_status": {"compared": 800}},
            "corpus": {
                "profile": "full",
                "case_count": 800,
                "rights_tier": "S",
                "redistribution": "allowed",
            },
            "renderer": renderer,
            "host_tools": host_tools,
            "container": {
                "identity_status": "pinned_match",
                "image_id": build["built_image_id"],
                "expected_image_id": build["built_image_id"],
            },
            "baseline_ratcheting": {
                "applies": True,
                "passed": True,
                "reviewed_baseline_available": True,
                "candidate_baselines": baseline_candidates,
                "gates": baseline_gates,
                "reviewed_warning_policy": warning_policy,
            },
            "evidence_runs": evidence_runs,
            "fidelity": fidelity_summaries,
            "authored_print": authored_summary,
            "repeatability": repeatability_summary,
        }
        self._write(artifact / "hosted-summary.json", summary)
        return artifact, baseline

    def test_accepts_exact_full_ratchet_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact, baseline = self._fixture(Path(temporary))

            report = self.checker.validate(artifact, self.head_sha, baseline)

        self.assertTrue(report["passed"])
        self.assertEqual(report["full_cases"], 800)
        self.assertEqual(report["ratchets"], 2)

    def test_rejects_failed_mismatched_missing_and_path_bearing_evidence(self) -> None:
        mutations = ("failed", "head", "missing", "path", "baseline")
        for mutation in mutations:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                artifact, baseline = self._fixture(Path(temporary))
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
                    self.checker.validate(artifact, self.head_sha, baseline)


if __name__ == "__main__":
    unittest.main()
