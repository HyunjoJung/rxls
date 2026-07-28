#!/usr/bin/env python3
"""Tests for npm tag Render Oracle prerequisite evidence."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
from pathlib import Path
import stat
import tempfile
import unittest
import warnings
import zipfile


ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "scripts" / "check_render_oracle_release_evidence.py"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "render-package-release.yml"


def _load():
    spec = importlib.util.spec_from_file_location(
        "check_render_oracle_release_evidence", CHECKER
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
        cls.head_sha = "a" * 40

    def _write(self, path: Path, value: object) -> bytes:
        payload = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()
        path.write_bytes(payload)
        return payload

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

    def _fixture(self, root: Path) -> tuple[Path, Path, Path, Path]:
        artifact = root / "artifact"
        artifact.mkdir()
        baseline = root / "reviewed-baseline.json"
        reviewed = {"schema": "rxls.render-parity-baseline.v2", "fixture": True}
        self._write(baseline, reviewed)
        reviewed_sha = self.checker._canonical_sha256(reviewed)
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
                "evidence": {
                    "oracle_build_contract_sha256": contract[
                        "build_contract_sha256"
                    ],
                    "oracle_image_config_digest": config_digest,
                    "oracle_image_manifest_digest": manifest_digest,
                    "oracle_lock_file_sha256": contract["lock_file_sha256"],
                },
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
            "evidence": {
                "oracle_build_contract_sha256": contract[
                    "build_contract_sha256"
                ],
                "oracle_image_config_digest": config_digest,
                "oracle_image_manifest_digest": manifest_digest,
                "oracle_lock_file_sha256": contract["lock_file_sha256"],
                "report_sha256": "1" * 64,
            },
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
            "snapshotter": "overlayfs",
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
            "schema": "rxls.render-oracle-hosted-campaign.v5",
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
                "build_contract_sha256": build["build_contract_sha256"],
                "identity_status": "pinned_match",
                "image_id": build["built_image_id"],
                "expected_image_id": build["built_image_id"],
                "manifest_digest": build["built_manifest_digest"],
                "expected_manifest_digest": build["built_manifest_digest"],
                "lock_file_sha256": build["lock_file_sha256"],
                "oracle_artifact_sha256": "9" * 64,
                "oracle_version": "26.2.3.2",
                "source_commit": build["source_commit"],
                "wrapper_sha256": build["wrapper_sha256"],
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
        return artifact, baseline, lock, wrapper

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
                    f"render-oracle-{self.head_sha}-101-2-full"
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
                        f"render-oracle-{self.head_sha}-102-2-full"
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
            "summary_v4",
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
                elif mutation == "summary_v4":
                    summary["schema"] = "rxls.render-oracle-hosted-campaign.v4"
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
