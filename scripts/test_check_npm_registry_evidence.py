#!/usr/bin/env python3
"""Tests for exact npm registry and GitHub provenance evidence."""

from __future__ import annotations

import base64
import copy
import hashlib
import importlib.util
import json
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = ROOT / "scripts" / "check_npm_registry_evidence.py"
SPEC = importlib.util.spec_from_file_location("check_npm_registry_evidence", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


PACKAGE_NAME = "@rxls/render-worker"
VERSION = "0.1.3"
REPOSITORY = "HyunjoJung/rxls"
WORKFLOW = ".github/workflows/render-package-release.yml"
GIT_SHA = "1" * 40
GIT_REF = f"refs/tags/render-v{VERSION}"
RUN_ID = "123"
RUN_ATTEMPT = "2"
ARCHIVE = b"verified npm archive"


def envelope(statement: dict) -> dict:
    payload = base64.b64encode(
        json.dumps(statement, separators=(",", ":"), sort_keys=True).encode("utf-8")
    ).decode("ascii")
    return {
        "dsseEnvelope": {
            "payloadType": "application/vnd.in-toto+json",
            "payload": payload,
            "signatures": [{"sig": "verified-by-npm", "keyid": ""}],
        }
    }


def evidence() -> tuple[dict, list[dict]]:
    digest = hashlib.sha512(ARCHIVE).digest()
    integrity = "sha512-" + base64.b64encode(digest).decode("ascii")
    subject = [
        {
            "name": f"pkg:npm/%40rxls/render-worker@{VERSION}",
            "digest": {"sha512": digest.hex()},
        }
    ]
    publish = {
        "_type": "https://in-toto.io/Statement/v0.1",
        "subject": subject,
        "predicateType": MODULE.NPM_PUBLISH,
        "predicate": {
            "name": PACKAGE_NAME,
            "version": VERSION,
            "registry": "https://registry.npmjs.org",
        },
    }
    provenance = {
        "_type": MODULE.IN_TOTO_V1,
        "subject": subject,
        "predicateType": MODULE.SLSA_PROVENANCE,
        "predicate": {
            "buildDefinition": {
                "buildType": MODULE.GITHUB_ACTIONS_BUILD_TYPE,
                "externalParameters": {
                    "workflow": {
                        "ref": GIT_REF,
                        "repository": f"https://github.com/{REPOSITORY}",
                        "path": WORKFLOW,
                    }
                },
                "internalParameters": {"github": {"event_name": "push"}},
                "resolvedDependencies": [
                    {
                        "uri": f"git+https://github.com/{REPOSITORY}@{GIT_REF}",
                        "digest": {"gitCommit": GIT_SHA},
                    }
                ],
            },
            "runDetails": {
                "builder": {"id": MODULE.GITHUB_HOSTED_BUILDER},
                "metadata": {
                    "invocationId": (
                        f"https://github.com/{REPOSITORY}/actions/runs/{RUN_ID}"
                        f"/attempts/{RUN_ATTEMPT}"
                    )
                },
            },
        },
    }
    report = {
        "invalid": [],
        "missing": [],
        "verified": [
            {
                "name": PACKAGE_NAME,
                "version": VERSION,
                "location": f"node_modules/{PACKAGE_NAME}",
                "registry": "https://registry.npmjs.org/",
                "attestations": {
                    "url": (
                        "https://registry.npmjs.org/-/npm/v1/attestations/"
                        f"{PACKAGE_NAME}@{VERSION}"
                    ),
                    "provenance": {"predicateType": MODULE.SLSA_PROVENANCE},
                },
                "attestationBundles": [
                    {"predicateType": MODULE.NPM_PUBLISH, "bundle": envelope(publish)},
                    {
                        "predicateType": MODULE.SLSA_PROVENANCE,
                        "bundle": envelope(provenance),
                    },
                ],
            }
        ],
    }
    packed = [
        {
            "name": PACKAGE_NAME,
            "version": VERSION,
            "filename": f"rxls-render-worker-{VERSION}.tgz",
            "size": len(ARCHIVE),
            "shasum": hashlib.sha1(ARCHIVE).hexdigest(),
            "integrity": integrity,
        }
    ]
    return report, packed


def validate(
    report: dict,
    packed: list[dict],
    *,
    invocation_policy: str = MODULE.CURRENT_RUN_INVOCATION,
) -> None:
    MODULE.validate_evidence(
        report,
        packed,
        archive=ARCHIVE,
        repository=REPOSITORY,
        workflow=WORKFLOW,
        git_sha=GIT_SHA,
        git_ref=GIT_REF,
        run_id=RUN_ID,
        run_attempt=RUN_ATTEMPT,
        invocation_policy=invocation_policy,
    )


def decoded_provenance(report: dict) -> dict:
    bundle = report["verified"][0]["attestationBundles"][1]["bundle"]
    payload = bundle["dsseEnvelope"]["payload"]
    return json.loads(base64.b64decode(payload))


def replace_provenance(report: dict, statement: dict) -> None:
    report["verified"][0]["attestationBundles"][1]["bundle"] = envelope(statement)


def wasm_evidence() -> tuple[dict, list[dict]]:
    report, packed = evidence()
    package_name = "rxls-wasm"
    git_ref = f"refs/tags/wasm-v{VERSION}"
    workflow = ".github/workflows/wasm-package-release.yml"
    packed[0]["name"] = package_name
    packed[0]["filename"] = f"rxls-wasm-{VERSION}.tgz"
    entry = report["verified"][0]
    entry["name"] = package_name
    entry["location"] = f"node_modules/{package_name}"
    for row in entry["attestationBundles"]:
        statement = json.loads(base64.b64decode(row["bundle"]["dsseEnvelope"]["payload"]))
        statement["subject"][0]["name"] = f"pkg:npm/{package_name}@{VERSION}"
        if row["predicateType"] == MODULE.NPM_PUBLISH:
            statement["predicate"]["name"] = package_name
        else:
            definition = statement["predicate"]["buildDefinition"]
            definition["externalParameters"]["workflow"].update(
                {"ref": git_ref, "path": workflow}
            )
            definition["resolvedDependencies"][0]["uri"] = (
                f"git+https://github.com/{REPOSITORY}@{git_ref}"
            )
        row["bundle"] = envelope(statement)
    return report, packed


class NpmRegistryEvidenceTests(unittest.TestCase):
    def test_exact_registry_evidence_passes(self) -> None:
        validate(*evidence())

    def test_pack_metadata_is_recomputed_from_the_transferred_archive(self) -> None:
        report, packed = evidence()
        with self.assertRaises(MODULE.EvidenceError):
            MODULE.validate_evidence(
                report,
                packed,
                archive=ARCHIVE + b"tampered",
                repository=REPOSITORY,
                workflow=WORKFLOW,
                git_sha=GIT_SHA,
                git_ref=GIT_REF,
                run_id=RUN_ID,
                run_attempt=RUN_ATTEMPT,
                invocation_policy=MODULE.CURRENT_RUN_INVOCATION,
            )

        for field, value in (
            ("filename", "other.tgz"),
            ("size", len(ARCHIVE) + 1),
            ("shasum", "0" * 40),
            ("integrity", "sha512-" + base64.b64encode(b"0" * 64).decode("ascii")),
        ):
            with self.subTest(field=field):
                changed = copy.deepcopy(packed)
                changed[0][field] = value
                with self.assertRaises(MODULE.EvidenceError):
                    validate(report, changed)

    def test_exact_wasm_registry_evidence_passes(self) -> None:
        report, packed = wasm_evidence()
        MODULE.validate_evidence(
            report,
            packed,
            archive=ARCHIVE,
            repository=REPOSITORY,
            workflow=".github/workflows/wasm-package-release.yml",
            git_sha=GIT_SHA,
            git_ref=f"refs/tags/wasm-v{VERSION}",
            run_id=RUN_ID,
            run_attempt=RUN_ATTEMPT,
            invocation_policy=MODULE.CURRENT_RUN_INVOCATION,
        )

    def test_package_must_match_the_publishing_workflow(self) -> None:
        report, packed = wasm_evidence()
        with self.assertRaises(MODULE.EvidenceError):
            MODULE.validate_evidence(
                report,
                packed,
                archive=ARCHIVE,
                repository=REPOSITORY,
                workflow=WORKFLOW,
                git_sha=GIT_SHA,
                git_ref=GIT_REF,
                run_id=RUN_ID,
                run_attempt=RUN_ATTEMPT,
                invocation_policy=MODULE.CURRENT_RUN_INVOCATION,
            )

    def test_existing_release_accepts_original_same_repository_invocation(self) -> None:
        report, packed = evidence()
        statement = decoded_provenance(report)
        statement["predicate"]["runDetails"]["metadata"]["invocationId"] = (
            f"https://github.com/{REPOSITORY}/actions/runs/987654321"
            "/attempts/7"
        )
        replace_provenance(report, statement)

        with self.assertRaises(MODULE.EvidenceError):
            validate(report, packed)
        validate(
            report,
            packed,
            invocation_policy=MODULE.EXISTING_RELEASE_INVOCATION,
        )

    def test_existing_release_rejects_arbitrary_invocation_urls(self) -> None:
        invalid_invocation_ids = [
            f"http://github.com/{REPOSITORY}/actions/runs/987/attempts/1",
            "https://example.invalid/HyunjoJung/rxls/actions/runs/987/attempts/1",
            "https://github.com/attacker/rxls/actions/runs/987/attempts/1",
            f"https://github.com/{REPOSITORY}/actions/workflows/release.yml/runs/987",
            f"https://github.com/{REPOSITORY}/actions/runs/0/attempts/1",
            f"https://github.com/{REPOSITORY}/actions/runs/987/attempts/0",
            f"https://github.com/{REPOSITORY}/actions/runs/987/attempts/1/",
            f"https://github.com/{REPOSITORY}/actions/runs/987/attempts/1?retry=1",
        ]
        for invocation_id in invalid_invocation_ids:
            with self.subTest(invocation_id=invocation_id):
                report, packed = evidence()
                statement = decoded_provenance(report)
                statement["predicate"]["runDetails"]["metadata"][
                    "invocationId"
                ] = invocation_id
                replace_provenance(report, statement)
                with self.assertRaises(MODULE.EvidenceError):
                    validate(
                        report,
                        packed,
                        invocation_policy=MODULE.EXISTING_RELEASE_INVOCATION,
                    )

    def test_existing_release_preserves_release_identity_bindings(self) -> None:
        mutations = []

        for field, value in [
            ("repository", "https://github.com/attacker/rxls"),
            ("path", ".github/workflows/attacker.yml"),
            ("ref", "refs/tags/render-v9.9.9"),
        ]:
            report, packed = evidence()
            statement = decoded_provenance(report)
            statement["predicate"]["buildDefinition"]["externalParameters"][
                "workflow"
            ][field] = value
            replace_provenance(report, statement)
            mutations.append((f"workflow_{field}", report, packed))

        report, packed = evidence()
        statement = decoded_provenance(report)
        statement["predicate"]["buildDefinition"]["resolvedDependencies"][0][
            "digest"
        ]["gitCommit"] = "2" * 40
        replace_provenance(report, statement)
        mutations.append(("commit", report, packed))

        report, packed = evidence()
        statement = decoded_provenance(report)
        statement["subject"][0]["digest"]["sha512"] = "00" * 64
        replace_provenance(report, statement)
        mutations.append(("archive_digest", report, packed))

        for name, report, packed in mutations:
            with self.subTest(name=name):
                with self.assertRaises(MODULE.EvidenceError):
                    validate(
                        report,
                        packed,
                        invocation_policy=MODULE.EXISTING_RELEASE_INVOCATION,
                    )

    def test_unknown_invocation_policy_fails_closed(self) -> None:
        with self.assertRaises(MODULE.EvidenceError):
            validate(*evidence(), invocation_policy="any-github-run")

    def test_registry_and_attestation_mutations_fail_closed(self) -> None:
        mutations = {}

        report, packed = evidence()
        report["invalid"].append({"name": PACKAGE_NAME})
        mutations["invalid"] = (report, packed)

        report, packed = evidence()
        report["missing"].append({"name": PACKAGE_NAME})
        mutations["missing"] = (report, packed)

        report, packed = evidence()
        report["verified"][0]["name"] = "attacker"
        mutations["name"] = (report, packed)

        report, packed = evidence()
        report["verified"][0]["attestationBundles"].pop()
        mutations["missing_bundle"] = (report, packed)

        report, packed = evidence()
        report["verified"][0]["attestationBundles"].append(
            copy.deepcopy(report["verified"][0]["attestationBundles"][1])
        )
        mutations["duplicate_bundle"] = (report, packed)

        for field, value in [
            ("repository", "https://github.com/attacker/rxls"),
            ("path", ".github/workflows/attacker.yml"),
            ("ref", "refs/heads/main"),
        ]:
            report, packed = evidence()
            statement = decoded_provenance(report)
            statement["predicate"]["buildDefinition"]["externalParameters"][
                "workflow"
            ][field] = value
            replace_provenance(report, statement)
            mutations[f"workflow_{field}"] = (report, packed)

        report, packed = evidence()
        statement = decoded_provenance(report)
        statement["predicate"]["buildDefinition"]["resolvedDependencies"][0][
            "digest"
        ]["gitCommit"] = "2" * 40
        replace_provenance(report, statement)
        mutations["commit"] = (report, packed)

        report, packed = evidence()
        statement = decoded_provenance(report)
        statement["predicate"]["runDetails"]["metadata"]["invocationId"] = (
            f"https://github.com/{REPOSITORY}/actions/runs/999/attempts/1"
        )
        replace_provenance(report, statement)
        mutations["invocation"] = (report, packed)

        report, packed = evidence()
        statement = decoded_provenance(report)
        statement["subject"][0]["digest"]["sha512"] = "00" * 64
        replace_provenance(report, statement)
        mutations["archive_digest"] = (report, packed)

        for name, (report, packed) in mutations.items():
            with self.subTest(name=name):
                with self.assertRaises(MODULE.EvidenceError):
                    validate(report, packed)


if __name__ == "__main__":
    unittest.main()
