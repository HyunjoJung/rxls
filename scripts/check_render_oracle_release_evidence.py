#!/usr/bin/env python3
"""Validate full, exact-SHA Render Oracle evidence before npm publication."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import sys
from typing import Any


EXPECTED_FILES = frozenset(
    {
        "authored-print-gate.json",
        "baseline-candidate-a.json",
        "baseline-candidate-b.json",
        "baseline-gate-a.json",
        "baseline-gate-b.json",
        "build.json",
        "fidelity-a.json",
        "fidelity-b.json",
        "host-tools.json",
        "hosted-summary.json",
        "renderer.json",
        "repeatability.json",
    }
)
MAX_FILE_BYTES = 16 * 1024 * 1024
MAX_TOTAL_BYTES = 48 * 1024 * 1024
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
HEAD_SHA_RE = re.compile(r"^[0-9a-f]{40}$")


class EvidenceError(ValueError):
    """Raised when hosted release evidence is absent or inconsistent."""


def _require(condition: bool, code: str) -> None:
    if not condition:
        raise EvidenceError(code)


def _object_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise EvidenceError("duplicate_json_key")
        result[key] = value
    return result


def _read_json(path: Path) -> tuple[dict[str, Any], bytes]:
    _require(path.is_file() and not path.is_symlink(), "evidence_file_type")
    payload = path.read_bytes()
    _require(0 < len(payload) <= MAX_FILE_BYTES, "evidence_file_size")
    try:
        document = json.loads(payload, object_pairs_hook=_object_pairs)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise EvidenceError("evidence_invalid_json") from error
    _require(isinstance(document, dict), "evidence_not_object")
    return document, payload


def _sha256(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def _canonical_sha256(value: object) -> str:
    payload = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")
    return _sha256(payload)


def _path_neutral(value: object) -> None:
    if isinstance(value, dict):
        _require("path" not in value, "path_bearing_key")
        for item in value.values():
            _path_neutral(item)
    elif isinstance(value, list):
        for item in value:
            _path_neutral(item)
    elif isinstance(value, str):
        lowered = value.lower()
        _require(not value.startswith("/"), "absolute_path")
        _require(re.match(r"^[A-Za-z]:[\\/]", value) is None, "windows_path")
        _require(not lowered.startswith("file://"), "file_uri")
        _require("local/render-corpus" not in lowered, "corpus_path")
        _require("payload/" not in lowered, "payload_path")


def _hash_matches(value: object) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _validate_baseline_gate(
    gate: dict[str, Any],
    candidate: dict[str, Any],
    candidate_payload: bytes,
    reviewed_baseline_sha256: str,
) -> None:
    _require(gate.get("schema") == "rxls.render-parity-baseline-check.v1", "gate_schema")
    _require(gate.get("passed") is True and gate.get("failures") == [], "ratchet_failed")
    _require(gate.get("baseline_sha256") == reviewed_baseline_sha256, "baseline_identity")
    _require(
        gate.get("candidate_sha256") == _canonical_sha256(candidate),
        "candidate_identity",
    )
    _require(_sha256(candidate_payload) == _canonical_sha256(candidate), "candidate_encoding")
    campaign = gate.get("campaign")
    _require(isinstance(campaign, dict), "gate_campaign")
    _require(campaign.get("case_count") == 800, "gate_case_count")
    _require(campaign.get("kind") == "project_generated_hosted_full", "gate_kind")
    _require(_hash_matches(campaign.get("manifest_sha256")), "gate_manifest_identity")
    warning_policy = gate.get("warning_policy")
    _require(isinstance(warning_policy, dict), "warning_policy")
    _require(warning_policy.get("unclassified_codes") == [], "unreviewed_warning")
    reviewed_count = warning_policy.get("reviewed_code_count")
    candidate_count = warning_policy.get("candidate_code_count")
    _require(
        isinstance(reviewed_count, int)
        and isinstance(candidate_count, int)
        and reviewed_count >= candidate_count >= 0,
        "warning_policy_counts",
    )


def _validate_candidate(candidate: dict[str, Any]) -> dict[str, Any]:
    _require(candidate.get("schema") == "rxls.render-parity-baseline.v2", "candidate_schema")
    _require(candidate.get("input_files") == 800, "candidate_case_count")
    campaign = candidate.get("campaign")
    _require(isinstance(campaign, dict), "candidate_campaign")
    _require(campaign.get("schema") == "rxls.render-parity-campaign.v1", "campaign_schema")
    _require(campaign.get("kind") == "project_generated_hosted_full", "campaign_kind")
    _require(campaign.get("profile") == "full", "campaign_profile")
    _require(campaign.get("case_count") == 800, "campaign_case_count")
    _require(
        campaign.get("format_counts")
        == {"ods": 200, "xls": 200, "xlsb": 200, "xlsx": 200},
        "campaign_format_counts",
    )
    _require(_hash_matches(campaign.get("manifest_sha256")), "campaign_manifest")
    _require(
        campaign.get("input_set_sha256") == candidate.get("input_set_sha256")
        and _hash_matches(candidate.get("input_set_sha256")),
        "campaign_input_identity",
    )
    _require(isinstance(candidate.get("warning_counts"), dict), "candidate_warnings")
    return campaign


def validate(
    artifact_dir: Path,
    head_sha: str,
    reviewed_baseline: Path,
    *,
    workflow_run_id: int | None = None,
    artifact_digest: str | None = None,
) -> dict[str, object]:
    _require(HEAD_SHA_RE.fullmatch(head_sha) is not None, "head_sha")
    if workflow_run_id is not None:
        _require(workflow_run_id > 0, "workflow_run_id")
    if artifact_digest is not None:
        _require(
            re.fullmatch(r"sha256:[0-9a-f]{64}", artifact_digest) is not None,
            "artifact_digest",
        )
    artifact_dir = artifact_dir.resolve()
    _require(artifact_dir.is_dir() and not artifact_dir.is_symlink(), "artifact_directory")
    members = list(artifact_dir.iterdir())
    _require(all(item.is_file() and not item.is_symlink() for item in members), "artifact_member_type")
    _require({item.name for item in members} == EXPECTED_FILES, "artifact_file_set")
    _require(
        sum(item.stat().st_size for item in members) <= MAX_TOTAL_BYTES,
        "artifact_total_size",
    )

    documents: dict[str, dict[str, Any]] = {}
    payloads: dict[str, bytes] = {}
    for name in sorted(EXPECTED_FILES):
        document, payload = _read_json(artifact_dir / name)
        _path_neutral(document)
        documents[name] = document
        payloads[name] = payload

    reviewed, _ = _read_json(reviewed_baseline)
    _require(reviewed.get("schema") == "rxls.render-parity-baseline.v2", "reviewed_schema")
    reviewed_sha256 = _canonical_sha256(reviewed)

    candidates = []
    gates = []
    for label in ("a", "b"):
        candidate = documents[f"baseline-candidate-{label}.json"]
        campaign = _validate_candidate(candidate)
        gate = documents[f"baseline-gate-{label}.json"]
        _validate_baseline_gate(
            gate,
            candidate,
            payloads[f"baseline-candidate-{label}.json"],
            reviewed_sha256,
        )
        _require(
            gate["campaign"]["manifest_sha256"] == campaign["manifest_sha256"],
            "gate_campaign_identity",
        )
        candidates.append(candidate)
        gates.append(gate)
    _require(candidates[0]["campaign"] == candidates[1]["campaign"], "campaign_repeatability")
    _require(candidates[0]["warning_counts"] == candidates[1]["warning_counts"], "warning_repeatability")

    fidelities = [documents["fidelity-a.json"], documents["fidelity-b.json"]]
    for fidelity in fidelities:
        _require(fidelity.get("schema") == "rxls.render-fidelity-targets.v1", "fidelity_schema")
        _require(fidelity.get("passed") is True and fidelity.get("failures") == [], "fidelity_failed")

    authored = documents["authored-print-gate.json"]
    _require(authored.get("schema") == "rxls.authored-print-parity.v1", "authored_schema")
    _require(authored.get("passed") is True and authored.get("failures") == [], "authored_failed")
    _require(
        authored.get("coverage", {}).get("workbooks") == 100
        and authored.get("coverage", {}).get("pages") == 400,
        "authored_coverage",
    )

    repeatability = documents["repeatability.json"]
    _require(
        repeatability.get("schema") == "rxls.libreoffice-render-repeatability.v1",
        "repeatability_schema",
    )
    _require(
        repeatability.get("status") == "pass" and repeatability.get("failures") == [],
        "repeatability_failed",
    )
    _require(
        repeatability.get("coverage", {}).get("workbooks") == 800,
        "repeatability_coverage",
    )

    build = documents["build.json"]
    _require(build.get("image_identity_status") == "pinned_match", "image_identity")
    _require(
        build.get("expected_image_id") == build.get("built_image_id"),
        "image_identity_mismatch",
    )
    host_tools = documents["host-tools.json"]
    _require(host_tools.get("identity_status") == "pinned_match", "host_identity")
    _require(
        host_tools.get("captured_identity_sha256")
        == host_tools.get("expected_identity_sha256"),
        "host_identity_mismatch",
    )
    renderer = documents["renderer.json"]
    _require(_hash_matches(renderer.get("sha256")), "renderer_identity")

    summary = documents["hosted-summary.json"]
    _require(summary.get("schema") == "rxls.render-oracle-hosted-campaign.v4", "summary_schema")
    _require(summary.get("head_sha") == head_sha, "summary_head_sha")
    campaign = summary.get("campaign", {})
    _require(
        campaign.get("mode") == "full"
        and campaign.get("case_count") == 800
        and campaign.get("repetitions") == 2
        and campaign.get("shard_count") == 4
        and campaign.get("parallel_shards") == 2,
        "summary_campaign",
    )
    shard_counts = campaign.get("shard_case_counts")
    _require(
        isinstance(shard_counts, list)
        and len(shard_counts) == 4
        and sum(shard_counts) == 800
        and all(isinstance(count, int) and 180 <= count <= 220 for count in shard_counts),
        "summary_shards",
    )
    _require(
        summary.get("summary", {}).get("files") == 800
        and summary.get("summary", {}).get("by_status") == {"compared": 800},
        "summary_coverage",
    )
    _require(
        summary.get("corpus", {}).get("profile") == "full"
        and summary.get("corpus", {}).get("case_count") == 800
        and summary.get("corpus", {}).get("rights_tier") == "S"
        and summary.get("corpus", {}).get("redistribution") == "allowed",
        "summary_corpus",
    )
    _require(summary.get("renderer") == renderer, "summary_renderer")
    _require(summary.get("host_tools") == host_tools, "summary_host_tools")
    _require(
        summary.get("container", {}).get("identity_status") == "pinned_match"
        and summary.get("container", {}).get("image_id") == build.get("built_image_id")
        and summary.get("container", {}).get("expected_image_id") == build.get("built_image_id"),
        "summary_container",
    )

    baseline_summary = summary.get("baseline_ratcheting")
    _require(isinstance(baseline_summary, dict), "summary_baseline")
    _require(
        baseline_summary.get("applies") is True
        and baseline_summary.get("passed") is True
        and baseline_summary.get("reviewed_baseline_available") is True,
        "summary_ratchet",
    )
    summary_gates = baseline_summary.get("gates")
    summary_candidates = baseline_summary.get("candidate_baselines")
    _require(
        isinstance(summary_gates, list)
        and len(summary_gates) == 2
        and isinstance(summary_candidates, list)
        and len(summary_candidates) == 2,
        "summary_ratchet_runs",
    )
    for index, label in enumerate(("a", "b")):
        gate = gates[index]
        candidate = candidates[index]
        expected_gate_summary = {
            "baseline_sha256": gate["baseline_sha256"],
            "candidate_sha256": gate["candidate_sha256"],
            "failures": gate["failures"],
            "passed": gate["passed"],
            "sha256": _sha256(payloads[f"baseline-gate-{label}.json"]),
            "warning_policy": gate["warning_policy"],
        }
        _require(summary_gates[index] == expected_gate_summary, "summary_gate_identity")
        _require(
            summary_candidates[index].get("sha256")
            == _sha256(payloads[f"baseline-candidate-{label}.json"])
            and summary_candidates[index].get("campaign_sha256")
            == _canonical_sha256(candidate["campaign"])
            and summary_candidates[index].get("warning_counts")
            == candidate["warning_counts"],
            "summary_candidate_identity",
        )
    _require(
        baseline_summary.get("reviewed_warning_policy") == gates[0]["warning_policy"]
        == gates[1]["warning_policy"],
        "summary_warning_policy",
    )

    evidence_runs = summary.get("evidence_runs")
    _require(isinstance(evidence_runs, list) and len(evidence_runs) == 2, "summary_evidence_runs")
    for index, label in enumerate(("a", "b")):
        _require(
            evidence_runs[index].get("fidelity_gate_sha256")
            == _sha256(payloads[f"fidelity-{label}.json"])
            and _hash_matches(evidence_runs[index].get("report_sha256"))
            and isinstance(evidence_runs[index].get("report_bytes"), int)
            and evidence_runs[index]["report_bytes"] > 0,
            "summary_fidelity_identity",
        )
        expected_fidelity = {
            key: fidelities[index][key]
            for key in ("coverage", "metrics", "passed", "thresholds")
        }
        _require(summary.get("fidelity", [])[index] == expected_fidelity, "summary_fidelity")
    expected_authored = {
        key: authored[key]
        for key in ("coverage", "evidence", "expected", "metrics", "passed", "thresholds")
    }
    expected_authored["sha256"] = _sha256(payloads["authored-print-gate.json"])
    _require(summary.get("authored_print") == expected_authored, "summary_authored")
    expected_repeatability = {
        key: repeatability[key]
        for key in ("coverage", "status", "thresholds_ppm")
    }
    expected_repeatability["sha256"] = _sha256(payloads["repeatability.json"])
    _require(summary.get("repeatability") == expected_repeatability, "summary_repeatability")

    report: dict[str, object] = {
        "schema": "rxls.render-worker-release-prerequisites.v1",
        "head_sha": head_sha,
        "full_cases": 800,
        "ratchets": 2,
        "reviewed_baseline_sha256": reviewed_sha256,
        "passed": True,
    }
    if workflow_run_id is not None:
        report["workflow_run_id"] = workflow_run_id
    if artifact_digest is not None:
        report["artifact_digest"] = artifact_digest
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact-dir", type=Path, required=True)
    parser.add_argument("--head-sha", required=True)
    parser.add_argument("--reviewed-baseline", type=Path, required=True)
    parser.add_argument("--workflow-run-id", type=int, required=True)
    parser.add_argument("--artifact-digest", required=True)
    parser.add_argument("--write-report", type=Path)
    args = parser.parse_args()
    try:
        report = validate(
            args.artifact_dir,
            args.head_sha,
            args.reviewed_baseline,
            workflow_run_id=args.workflow_run_id,
            artifact_digest=args.artifact_digest,
        )
        if args.write_report is not None:
            args.write_report.parent.mkdir(parents=True, exist_ok=True)
            args.write_report.write_text(
                json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
    except (EvidenceError, OSError) as error:
        print(f"render release prerequisites: {error}", file=sys.stderr)
        return 1
    print(
        "render release prerequisites: "
        f"head_sha={report['head_sha']} full_cases=800 ratchets=2 passed=true"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
