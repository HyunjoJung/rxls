#!/usr/bin/env python3
"""Validate npm's cryptographically verified registry/provenance report."""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import re
from pathlib import Path
from typing import Any
from urllib.parse import unquote


SLSA_PROVENANCE = "https://slsa.dev/provenance/v1"
NPM_PUBLISH = "https://github.com/npm/attestation/tree/main/specs/publish/v0.1"
IN_TOTO_V1 = "https://in-toto.io/Statement/v1"
GITHUB_ACTIONS_BUILD_TYPE = (
    "https://slsa-framework.github.io/github-actions-buildtypes/workflow/v1"
)
GITHUB_HOSTED_BUILDER = "https://github.com/actions/runner/github-hosted"
SHA_RE = re.compile(r"[0-9a-f]{40}")
POSITIVE_INTEGER_RE = re.compile(r"[1-9][0-9]*")
CURRENT_RUN_INVOCATION = "current-run"
EXISTING_RELEASE_INVOCATION = "existing-release"
INVOCATION_POLICIES = {CURRENT_RUN_INVOCATION, EXISTING_RELEASE_INVOCATION}
WORKFLOW_RELEASES = {
    ".github/workflows/render-package-release.yml": {
        "package": "@rxls/render-worker",
        "tag_prefix": "render-v",
    },
    ".github/workflows/wasm-package-release.yml": {
        "package": "rxls-wasm",
        "tag_prefix": "wasm-v",
    },
}


class EvidenceError(ValueError):
    """Raised when registry evidence is incomplete or differs."""


def _require(condition: bool, label: str) -> None:
    if not condition:
        raise EvidenceError(label)


def _object(value: Any, label: str) -> dict[str, Any]:
    _require(isinstance(value, dict), label)
    return value


def _array(value: Any, label: str) -> list[Any]:
    _require(isinstance(value, list), label)
    return value


def _decode_dsse(bundle: dict[str, Any], label: str) -> dict[str, Any]:
    envelope = _object(bundle.get("dsseEnvelope"), f"{label}.dsseEnvelope")
    _require(
        envelope.get("payloadType") == "application/vnd.in-toto+json",
        f"{label}.payloadType",
    )
    signatures = _array(envelope.get("signatures"), f"{label}.signatures")
    _require(
        bool(signatures)
        and all(
            isinstance(signature, dict)
            and isinstance(signature.get("sig"), str)
            and bool(signature["sig"])
            for signature in signatures
        ),
        f"{label}.signatures",
    )
    payload = envelope.get("payload")
    _require(isinstance(payload, str) and bool(payload), f"{label}.payload")
    try:
        decoded = base64.b64decode(payload, validate=True)
        statement = json.loads(decoded)
    except (binascii.Error, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise EvidenceError(f"{label}.payload") from error
    return _object(statement, f"{label}.statement")


def _validate_archive_identity(
    package: dict[str, Any], archive: bytes, package_name: str, version: str
) -> str:
    expected_filename = (
        f"{package_name.removeprefix('@').replace('/', '-')}-{version}.tgz"
    )
    _require(package.get("filename") == expected_filename, "pack.filename")
    _require(package.get("size") == len(archive), "pack.size")
    _require(package.get("shasum") == hashlib.sha1(archive).hexdigest(), "pack.shasum")
    expected_integrity = "sha512-" + base64.b64encode(
        hashlib.sha512(archive).digest()
    ).decode("ascii")
    _require(package.get("integrity") == expected_integrity, "pack.integrity")
    return hashlib.sha512(archive).hexdigest()


def _validate_subject(
    statement: dict[str, Any], package_name: str, version: str, sha512: str, label: str
) -> None:
    subjects = _array(statement.get("subject"), f"{label}.subject")
    _require(len(subjects) == 1, f"{label}.subject_count")
    subject = _object(subjects[0], f"{label}.subject[0]")
    expected_purl = f"pkg:npm/{package_name}@{version}"
    _require(unquote(subject.get("name", "")) == expected_purl, f"{label}.subject_name")
    digest = _object(subject.get("digest"), f"{label}.subject_digest")
    _require(digest == {"sha512": sha512}, f"{label}.subject_digest")


def _validate_invocation_id(
    invocation_id: Any,
    *,
    repository: str,
    run_id: str,
    run_attempt: str,
    invocation_policy: str,
) -> None:
    """Bind provenance to this run or an original run in the same repository."""

    _require(invocation_policy in INVOCATION_POLICIES, "invocation_policy")
    _require(isinstance(invocation_id, str), "provenance.invocationId")
    match = re.fullmatch(
        (
            rf"https://github\.com/{re.escape(repository)}/actions/runs/"
            r"([1-9][0-9]*)/attempts/([1-9][0-9]*)"
        ),
        invocation_id,
    )
    _require(match is not None, "provenance.invocationId")
    if invocation_policy == CURRENT_RUN_INVOCATION:
        _require(
            match.groups() == (run_id, run_attempt),
            "provenance.invocationId",
        )


def validate_evidence(
    report: Any,
    packed: Any,
    *,
    archive: bytes,
    repository: str,
    workflow: str,
    git_sha: str,
    git_ref: str,
    run_id: str,
    run_attempt: str,
    invocation_policy: str,
) -> None:
    """Validate one dependency-free installed package and its npm attestations."""

    report = _object(report, "report")
    _require(set(report) == {"invalid", "missing", "verified"}, "report.keys")
    _require(_array(report["invalid"], "report.invalid") == [], "report.invalid")
    _require(_array(report["missing"], "report.missing") == [], "report.missing")
    verified = _array(report["verified"], "report.verified")
    _require(len(verified) == 1, "report.verified_count")

    packed_rows = _array(packed, "pack")
    _require(len(packed_rows) == 1, "pack.count")
    package = _object(packed_rows[0], "pack[0]")
    package_name = package.get("name")
    version = package.get("version")
    _require(isinstance(package_name, str) and bool(package_name), "pack.name")
    _require(isinstance(version, str) and bool(version), "pack.version")
    sha512 = _validate_archive_identity(package, archive, package_name, version)

    _require(SHA_RE.fullmatch(git_sha) is not None, "git_sha")
    _require(POSITIVE_INTEGER_RE.fullmatch(run_id) is not None, "run_id")
    _require(POSITIVE_INTEGER_RE.fullmatch(run_attempt) is not None, "run_attempt")
    _require(invocation_policy in INVOCATION_POLICIES, "invocation_policy")
    release = WORKFLOW_RELEASES.get(workflow)
    _require(release is not None, "workflow")
    _require(package_name == release["package"], "pack.name")
    expected_ref = f"refs/tags/{release['tag_prefix']}{version}"
    _require(git_ref == expected_ref, "git_ref")
    _require(repository == "HyunjoJung/rxls", "repository")

    entry = _object(verified[0], "report.verified[0]")
    _require(entry.get("name") == package_name, "verified.name")
    _require(entry.get("version") == version, "verified.version")
    _require(
        entry.get("location") == f"node_modules/{package_name}", "verified.location"
    )
    _require(entry.get("registry") == "https://registry.npmjs.org/", "verified.registry")
    attestations = _object(entry.get("attestations"), "verified.attestations")
    provenance = _object(
        attestations.get("provenance"), "verified.attestations.provenance"
    )
    _require(provenance == {"predicateType": SLSA_PROVENANCE}, "verified.provenance")
    attestation_url = attestations.get("url")
    _require(
        isinstance(attestation_url, str)
        and attestation_url.startswith(
            "https://registry.npmjs.org/-/npm/v1/attestations/"
        ),
        "verified.attestation_url",
    )

    bundles = _array(entry.get("attestationBundles"), "verified.attestationBundles")
    by_type: dict[str, dict[str, Any]] = {}
    for index, row in enumerate(bundles):
        row = _object(row, f"verified.attestationBundles[{index}]")
        predicate_type = row.get("predicateType")
        _require(
            predicate_type in {NPM_PUBLISH, SLSA_PROVENANCE},
            "verified.attestation_predicate_type",
        )
        _require(predicate_type not in by_type, "verified.duplicate_attestation")
        by_type[predicate_type] = _object(row.get("bundle"), "verified.bundle")
    _require(set(by_type) == {NPM_PUBLISH, SLSA_PROVENANCE}, "verified.attestation_types")

    publish = _decode_dsse(by_type[NPM_PUBLISH], "publish")
    _require(
        publish.get("_type") in {"https://in-toto.io/Statement/v0.1", IN_TOTO_V1},
        "publish.type",
    )
    _require(publish.get("predicateType") == NPM_PUBLISH, "publish.predicateType")
    _validate_subject(publish, package_name, version, sha512, "publish")
    publish_predicate = _object(publish.get("predicate"), "publish.predicate")
    _require(publish_predicate.get("name") == package_name, "publish.name")
    _require(publish_predicate.get("version") == version, "publish.version")
    _require(
        publish_predicate.get("registry") == "https://registry.npmjs.org",
        "publish.registry",
    )

    slsa = _decode_dsse(by_type[SLSA_PROVENANCE], "provenance")
    _require(slsa.get("_type") == IN_TOTO_V1, "provenance.type")
    _require(slsa.get("predicateType") == SLSA_PROVENANCE, "provenance.predicateType")
    _validate_subject(slsa, package_name, version, sha512, "provenance")
    predicate = _object(slsa.get("predicate"), "provenance.predicate")
    build_definition = _object(
        predicate.get("buildDefinition"), "provenance.buildDefinition"
    )
    _require(
        build_definition.get("buildType") == GITHUB_ACTIONS_BUILD_TYPE,
        "provenance.buildType",
    )
    external = _object(
        build_definition.get("externalParameters"),
        "provenance.externalParameters",
    )
    source_workflow = _object(external.get("workflow"), "provenance.workflow")
    _require(
        source_workflow
        == {
            "ref": git_ref,
            "repository": f"https://github.com/{repository}",
            "path": workflow,
        },
        "provenance.workflow",
    )
    internal = _object(
        build_definition.get("internalParameters"), "provenance.internalParameters"
    )
    github = _object(internal.get("github"), "provenance.github")
    _require(github.get("event_name") == "push", "provenance.event_name")
    dependencies = _array(
        build_definition.get("resolvedDependencies"),
        "provenance.resolvedDependencies",
    )
    matching_dependencies = [
        dependency
        for dependency in dependencies
        if isinstance(dependency, dict)
        and dependency.get("uri")
        == f"git+https://github.com/{repository}@{git_ref}"
        and dependency.get("digest") == {"gitCommit": git_sha}
    ]
    _require(len(matching_dependencies) == 1, "provenance.git_dependency")

    run_details = _object(predicate.get("runDetails"), "provenance.runDetails")
    builder = _object(run_details.get("builder"), "provenance.builder")
    _require(builder == {"id": GITHUB_HOSTED_BUILDER}, "provenance.builder")
    metadata = _object(run_details.get("metadata"), "provenance.metadata")
    _validate_invocation_id(
        metadata.get("invocationId"),
        repository=repository,
        run_id=run_id,
        run_attempt=run_attempt,
        invocation_policy=invocation_policy,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--audit", type=Path, required=True)
    parser.add_argument("--pack", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--workflow", required=True)
    parser.add_argument("--git-sha", required=True)
    parser.add_argument("--git-ref", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument(
        "--invocation-policy",
        choices=sorted(INVOCATION_POLICIES),
        required=True,
        help=(
            "require provenance from the current run, or allow the original "
            "same-repository run for an already-published immutable release"
        ),
    )
    args = parser.parse_args()
    try:
        validate_evidence(
            json.loads(args.audit.read_text(encoding="utf-8")),
            json.loads(args.pack.read_text(encoding="utf-8")),
            archive=args.archive.read_bytes(),
            repository=args.repository,
            workflow=args.workflow,
            git_sha=args.git_sha,
            git_ref=args.git_ref,
            run_id=args.run_id,
            run_attempt=args.run_attempt,
            invocation_policy=args.invocation_policy,
        )
    except (EvidenceError, OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"npm registry evidence failed: {error}") from error
    print("npm registry evidence passed: exact package and GitHub provenance")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
