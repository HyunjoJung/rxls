#!/usr/bin/env python3
"""Enforce immutable GitHub Actions and reproducible release tool versions."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


ACTION_RE = re.compile(
    r"^\s*(?:-\s*)?uses:\s*[\"']?(?P<spec>[^\s\"'#]+)[\"']?"
    r"(?:\s+#\s*(?P<comment>.+?))?\s*$"
)
FULL_SHA_RE = re.compile(r"[0-9a-f]{40}")
REMOTE_ACTION_RE = re.compile(r"[^/\s]+/[^@\s]+@.+")
RELEASE_VERSIONS = {
    "RELEASE_RUST_VERSION": "1.96.1",
    "FUZZ_NIGHTLY_VERSION": "nightly-2026-07-10",
    "CARGO_FUZZ_VERSION": "0.13.2",
}
RENDER_ORACLE_PYTHON_VERSION = "3.13.14"
RENDER_ORACLE_FULL_CASES = "800"
RENDER_ORACLE_FULL_REPEATS = "2"
RENDER_ORACLE_FULL_SHARDS = "4"
RENDER_ORACLE_MAX_PARALLEL_SHARDS = "2"
RENDER_PACKAGE_NODE_VERSION = "24.18.0"
RENDER_PACKAGE_NPM_VERSION = "11.16.0"
RENDER_PACKAGE_WASM_BINDGEN_VERSION = "0.2.126"


def audit_action_pins(path: Path, text: str) -> list[str]:
    """Return policy violations for remote action references in one workflow."""

    errors: list[str] = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        if "uses:" not in line:
            continue
        match = ACTION_RE.match(line)
        if match is None:
            errors.append(f"{path}:{line_number}: unreadable uses entry")
            continue
        spec = match.group("spec")
        if spec.startswith("./"):
            continue
        if not REMOTE_ACTION_RE.fullmatch(spec):
            errors.append(f"{path}:{line_number}: invalid remote action {spec!r}")
            continue
        action, ref = spec.rsplit("@", 1)
        if FULL_SHA_RE.fullmatch(ref) is None:
            errors.append(
                f"{path}:{line_number}: {action} must use a full immutable commit SHA"
            )
        comment = match.group("comment")
        if comment is None or not comment.strip():
            errors.append(
                f"{path}:{line_number}: pinned action {action} needs a version comment"
            )
    return errors


def _cargo_fuzz_commands(text: str) -> list[tuple[int, str]]:
    lines = text.splitlines()
    commands: list[tuple[int, str]] = []
    index = 0
    while index < len(lines):
        line = lines[index]
        if re.search(r"\bcargo\s+install\s+cargo-fuzz\b", line):
            start = index + 1
            command = line.strip()
            while command.rstrip().endswith("\\") and index + 1 < len(lines):
                index += 1
                command = command.rstrip()[:-1] + " " + lines[index].strip()
            commands.append((start, command))
        index += 1
    return commands


def _audit_exact_assignments(
    path: Path, text: str, names: tuple[str, ...]
) -> list[str]:
    errors: list[str] = []
    for name in names:
        expected = RELEASE_VERSIONS[name]
        assignment = re.compile(
            rf"^\s*{re.escape(name)}:\s*[\"']?{re.escape(expected)}[\"']?\s*$",
            re.MULTILINE,
        )
        if assignment.search(text) is None:
            errors.append(f"{path}: expected exact {name}={expected}")
    return errors


def audit_fuzz_tools(
    path: Path, text: str, required_assignments: tuple[str, ...]
) -> list[str]:
    """Return violations for a workflow that installs and invokes cargo-fuzz."""

    errors = _audit_exact_assignments(path, text, required_assignments)

    commands = _cargo_fuzz_commands(text)
    if not commands:
        errors.append(f"{path}: fuzzing workflow must install cargo-fuzz")
    errors.extend(audit_tool_commands(path, text))

    return errors


def audit_tool_commands(path: Path, text: str) -> list[str]:
    """Reject mutable nightly/cargo-fuzz commands in any hosted workflow."""

    errors: list[str] = []
    for line_number, command in _cargo_fuzz_commands(text):
        if not re.search(
            r"--version(?:=|\s+)(?:[\"']?\$\{?CARGO_FUZZ_VERSION\}?|"
            + re.escape(RELEASE_VERSIONS["CARGO_FUZZ_VERSION"])
            + r")[\"']?(?:\s|$)",
            command,
        ):
            errors.append(
                f"{path}:{line_number}: cargo-fuzz install must use exact version "
                f'{RELEASE_VERSIONS["CARGO_FUZZ_VERSION"]}'
            )

    if re.search(r"rustup\s+toolchain\s+install\s+nightly(?:\s|$)", text):
        errors.append(f"{path}: workflow must not install mutable nightly")
    if re.search(r"cargo\s+\+nightly(?:\s|$)", text):
        errors.append(f"{path}: workflow must not invoke mutable nightly")
    return errors


def audit_release_versions(path: Path, text: str) -> list[str]:
    """Return violations for release toolchain and cargo-fuzz version pins."""

    return audit_fuzz_tools(path, text, tuple(RELEASE_VERSIONS))


def audit_fuzz_workflow(path: Path, text: str) -> list[str]:
    """Return violations for the standalone hosted fuzz workflow."""

    return audit_fuzz_tools(
        path, text, ("FUZZ_NIGHTLY_VERSION", "CARGO_FUZZ_VERSION")
    )


def audit_render_oracle_workflow(path: Path, text: str) -> list[str]:
    """Require exact identities and bounded pilot/full rendering campaigns."""

    errors: list[str] = []
    required = {
        f'python-version: "{RENDER_ORACLE_PYTHON_VERSION}"': (
            "must pin the complete Python patch version"
        ),
        "--no-deps": "must disable dependency resolution for the hashed closure",
        "--force-reinstall": "must materialize the exact wheel contents on every run",
        "--only-binary=:all:": "must install binary wheels only",
        "--require-hashes": "must require wheel hashes",
        "--requirement scripts/render-oracle-host-requirements.txt": (
            "must install the checked-in comparison closure"
        ),
        "scripts/render-oracle-host-tools.py verify": (
            "must verify the hosted comparison identity"
        ),
        "scripts/render-oracle-host-tools.py apt-specs --scope all": (
            "normal installs must use the pinned native package closure"
        ),
        "--output target/render-oracle-hosted/host-tools.json": (
            "must emit path-neutral hosted identity evidence"
        ),
        'assert document["image_identity_status"] == "pinned_match"': (
            "normal oracle builds must require pinned_match"
        ),
        'assert host_tools["identity_status"] == "pinned_match"': (
            "normal campaigns must require the pinned host identity"
        ),
        "if: always()": "must upload bootstrap or mismatch identity evidence",
        'RXLS_ORACLE_CAMPAIGN: ${{ github.event_name == \'workflow_dispatch\' && inputs.campaign || \'pilot\' }}': (
            "push and schedule runs must stay on the bounded pilot"
        ),
        'test "$(git rev-parse HEAD)" = "$GITHUB_SHA"': (
            "must verify the exact checked-out commit"
        ),
        "timeout-minutes: ${{ github.event_name == 'workflow_dispatch' && inputs.campaign == 'full' && 330 || 120 }}": (
            "must keep the pilot at 120 minutes and bound explicit full campaigns at 330"
        ),
        '--profile "$RXLS_ORACLE_CAMPAIGN"': (
            "must generate and verify the selected deterministic profile"
        ),
        'row.get("name") == "pdffonts"': (
            "must select pdffonts from verified host identity evidence"
        ),
        '--pdffonts-binary-sha256 "$PDFFONTS_SHA256"': (
            "must bind PDF font inspection to the verified host binary"
        ),
        '--shard-count "$shard_count"': (
            "full campaigns must use the harness content-identity sharder"
        ),
        'if int(row["sha256"][:16], 16) % 4 == shard_index': (
            "must preflight the same deterministic content-identity shards"
        ),
        "assert all(180 <= len(rows) <= 220 for rows in shards)": (
            "full shards must remain balanced and bounded"
        ),
        '40 <= sum(row["format"] == format_name for row in rows) <= 60': (
            "every full shard must remain balanced by format"
        ),
        "python3 scripts/merge-render-parity-reports.py": (
            "must fail closed while merging complete full-corpus shards"
        ),
        "python3 scripts/check-render-fidelity-targets.py": (
            "must enforce the absolute rendering-fidelity gate"
        ),
        "python3 scripts/compare-render-parity-runs.py": (
            "must compare the two complete same-SHA full campaigns"
        ),
        "python3 scripts/check-render-parity-baseline.py": (
            "must ratchet each complete hosted full campaign against reviewed evidence"
        ),
        "python3 scripts/check-authored-print-parity.py": (
            "must enforce the dedicated authored-print differential gate"
        ),
        "--format xlsx": (
            "authored-print evidence must stay on the attested OOXML lane"
        ),
        "--required-feature print-settings": (
            "authored-print evidence must require explicit print settings"
        ),
        "--print-mode authored": (
            "authored-print evidence must preserve workbook print intent"
        ),
        "--baseline scripts/render-parity-baseline-full.json": (
            "must use the checked-in reviewed full-campaign baseline"
        ),
        "--campaign-manifest local/render-corpus-generated/full/manifest.json": (
            "must bind ratchets to the generated 800-workbook hosted corpus"
        ),
        "--require-hosted-full-800": (
            "must reject acquired-corpus or incorrectly sized baseline evidence"
        ),
        '--candidate-baseline "target/render-oracle-hosted/baseline-candidate-${label}.json"': (
            "must preserve path-neutral baseline candidates for review"
        ),
        "for label in a b; do": (
            "must apply the reviewed ratchet to both same-SHA full campaigns"
        ),
        'test "$(cat target/render-oracle-hosted/gate-status.txt)" = "0"': (
            "must fail closed after detailed campaign reports are removed"
        ),
        'rm -- "${shard_reports[@]}"': (
            "must remove detailed shard reports after exact merging"
        ),
        "for report_path in report_paths:": (
            "must remove detailed campaign reports after aggregation"
        ),
        "authored_report_path.unlink()": (
            "must remove the detailed authored-print report after aggregation"
        ),
        'assert authored_gate["schema"] == "rxls.authored-print-parity.v1"': (
            "must verify the aggregate authored-print gate schema"
        ),
        'assert authored_gate["passed"] is True': (
            "must reject failed authored-print aggregate evidence"
        ),
        'authored_gate["evidence"]["oracle_libreoffice_artifact_sha256"]': (
            "must bind authored-print evidence to the locked LibreOffice artifact"
        ),
        'authored_gate["evidence"]["pdffonts_sha256"] == pdffonts_sha256': (
            "must bind authored-print text evidence to the pinned PDF inspector"
        ),
        '"schema": "rxls.render-oracle-hosted-campaign.v4"': (
            "must emit the aggregate-only hosted campaign contract"
        ),
        '"acquired_corpus_included": False': (
            "must distinguish the 800-case hosted corpus from acquired-corpus evidence"
        ),
        '"scope": "project_generated_hosted_acceptance"': (
            "must label the bounded hosted acceptance corpus explicitly"
        ),
        'assert warning_policy["unclassified_codes"] == []': (
            "must reject every warning code absent from the reviewed baseline"
        ),
        '"reviewed_baseline_available": all(': (
            "must distinguish a reviewed ratchet from a bootstrap candidate"
        ),
        'gate["evidence"]["oracle_build_contract_sha256"]': (
            "must bind absolute-gate evidence to the exact container build"
        ),
        'gate["evidence"]["oracle_image_config_digest"]': (
            "must bind absolute-gate evidence to the pinned OCI image"
        ),
        'gate["evidence"]["pdffonts_sha256"] == pdffonts_sha256': (
            "must bind absolute-gate font inspection to the pinned host tool"
        ),
        "compression-level: 9": "must bound aggregate artifact transfer size",
    }
    for snippet, message in required.items():
        if snippet not in text:
            errors.append(f"{path}: {message}")
    if re.search(r"python-version:\s*[\"']?3\.13[\"']?\s*$", text, re.MULTILINE):
        errors.append(f"{path}: mutable Python minor selectors are forbidden")
    if "runtime_verified_unpinned" in text or "runtime_verified" in text:
        errors.append(f"{path}: normal oracle gates must not accept unpinned identities")
    if re.search(r"check-render-parity-baseline\.py(?s:.*?)--create", text):
        errors.append(
            f"{path}: hosted gates must not auto-approve their own reviewed baseline"
        )

    exact_assignments = {
        "FULL_CASE_COUNT": RENDER_ORACLE_FULL_CASES,
        "FULL_REPEAT_COUNT": RENDER_ORACLE_FULL_REPEATS,
        "FULL_SHARD_COUNT": RENDER_ORACLE_FULL_SHARDS,
        "MAX_PARALLEL_SHARDS": RENDER_ORACLE_MAX_PARALLEL_SHARDS,
    }
    for name, value in exact_assignments.items():
        assignment = re.compile(
            rf"^\s*{re.escape(name)}:\s*[\"']?{re.escape(value)}[\"']?\s*$",
            re.MULTILINE,
        )
        if len(assignment.findall(text)) != 1:
            errors.append(f"{path}: expected exact {name}={value}")

    campaign_input = re.search(
        r"(?ms)^\s{6}campaign:\s*$"
        r"(?P<body>.*?)(?=^\s{6}bootstrap_identities:\s*$)",
        text,
    )
    if campaign_input is None:
        errors.append(f"{path}: missing workflow_dispatch pilot/full campaign choice")
    else:
        body = campaign_input.group("body")
        if (
            "type: choice" not in body
            or "default: pilot" not in body
            or len(re.findall(r"^\s+- pilot\s*$", body, re.MULTILINE)) != 1
            or len(re.findall(r"^\s+- full\s*$", body, re.MULTILINE)) != 1
        ):
            errors.append(
                f"{path}: workflow_dispatch campaign must be an exact pilot/full choice"
            )

    if re.search(
        r"--max-(?:similarity|blur|mask)-drift-ppm(?:=|\s)", text
    ):
        errors.append(
            f"{path}: same-SHA drift thresholds must use the calibrated checked-in defaults"
        )
    if text.count('test "$FULL_REPEAT_COUNT" = "2"') != 1:
        errors.append(f"{path}: full mode must require exactly two same-SHA campaigns")
    if text.count('test "$FULL_SHARD_COUNT" = "4"') != 1:
        errors.append(f"{path}: full mode must require exactly four deterministic shards")
    if text.count('test "$MAX_PARALLEL_SHARDS" = "2"') != 1:
        errors.append(f"{path}: full mode must cap concurrent shard processes at two")
    if len(
        re.findall(
            r"^\s*python3 scripts/check-render-fidelity-targets\.py\s+\\$",
            text,
            re.MULTILINE,
        )
    ) != 2:
        errors.append(f"{path}: pilot/full evidence needs one absolute gate per campaign")

    upload = re.search(
        r"(?ms)^\s+- name: Upload path-neutral aggregate identities only\s*$"
        r".*?^\s+path:\s*\|\s*$\n(?P<paths>(?:\s+target/[^\n]+\n)+)"
        r"\s+compression-level:\s*9\s*$",
        text,
    )
    allowed_artifacts = {
        "target/render-oracle-hosted/authored-print-gate.json",
        "target/render-oracle-hosted/baseline-candidate-a.json",
        "target/render-oracle-hosted/baseline-candidate-b.json",
        "target/render-oracle-hosted/baseline-gate-a.json",
        "target/render-oracle-hosted/baseline-gate-b.json",
        "target/render-oracle-hosted/build.json",
        "target/render-oracle-hosted/fidelity-a.json",
        "target/render-oracle-hosted/fidelity-b.json",
        "target/render-oracle-hosted/hosted-summary.json",
        "target/render-oracle-hosted/host-tools.json",
        "target/render-oracle-hosted/repeatability.json",
        "target/render-oracle-hosted/renderer.json",
    }
    if upload is None:
        errors.append(f"{path}: aggregate-only artifact allowlist is missing")
    else:
        uploaded = {
            line.strip() for line in upload.group("paths").splitlines() if line.strip()
        }
        if uploaded != allowed_artifacts:
            errors.append(f"{path}: hosted artifacts must use the exact aggregate-only allowlist")

    apt_lines = [line for line in text.splitlines() if "apt-get " in line]
    bootstrap_matches = re.finditer(
        r'if \[\[ "\$RXLS_IDENTITY_BOOTSTRAP" == "1" \]\]; then\n'
        r'(?P<body>(?:\s+[^\n]*\n)+?)\s+fi',
        text,
    )
    bootstrap_bodies = [match.group("body") for match in bootstrap_matches]
    unpinned_installs = [
        line
        for line in apt_lines
        if "install" in line and '"${SYSTEM_PACKAGES[@]}"' not in line
    ]
    unpinned_is_bootstrap_only = any(
        all(line.strip() in body for line in unpinned_installs)
        for body in bootstrap_bodies
    )
    if (
        len(apt_lines) != 3
        or len(unpinned_installs) != 1
        or not unpinned_is_bootstrap_only
        or text.count('"${SYSTEM_PACKAGES[@]}"') != 1
    ):
        errors.append(
            f"{path}: apt must use bootstrap-only top-level packages or the exact pinned closure"
        )
    if "bootstrap_identities:" not in text or "--bootstrap-identities" not in text:
        errors.append(f"{path}: missing deliberate identity bootstrap path")
    if text.count("python3 -m pip install") != 1:
        errors.append(f"{path}: comparison dependencies need one hashed pip install")
    if text.count("if: ${{ env.RXLS_IDENTITY_BOOTSTRAP != '1' }}") < 4:
        errors.append(f"{path}: bootstrap runs must not execute parity campaign gates")
    return errors


def audit_render_hardening_workflow(path: Path, text: str) -> list[str]:
    """Require the PDF hardening lane to consume the same exact Poppler lock."""

    errors: list[str] = []
    for snippet, message in {
        "runs-on: ubuntu-24.04": "PDF hardening must use the locked host family",
        "scripts/render-oracle-host-tools.py verify": (
            "PDF hardening must verify the hosted-tool lock"
        ),
        "--scope poppler": "PDF hardening must verify the complete Poppler closure",
        "--output target/poppler-identity.json": (
            "PDF hardening must emit path-neutral Poppler evidence"
        ),
        "scripts/render-oracle-host-tools.py apt-specs --scope poppler": (
            "PDF hardening must install the pinned Poppler package closure"
        ),
        'sudo apt-get install --yes --no-install-recommends "${SYSTEM_PACKAGES[@]}"': (
            "PDF hardening must install only exact locked package specs"
        ),
    }.items():
        if snippet not in text:
            errors.append(f"{path}: {message}")
    apt_lines = [line for line in text.splitlines() if "apt-get " in line]
    if len(apt_lines) != 2 or any("poppler-utils" in line for line in apt_lines):
        errors.append(f"{path}: PDF hardening apt inputs must come only from the exact lock")
    if "poppler-version.txt" in text or "command -v pdfinfo |" in text:
        errors.append(f"{path}: path-bearing Poppler evidence is forbidden")
    return errors


def audit_codeql_workflow(path: Path, text: str) -> list[str]:
    """Require explicit CodeQL builds for every shipped Rust surface."""

    errors: list[str] = []
    normalized = re.sub(r"[ \t]*\\\r?\n[ \t]*", " ", text)
    commands = (
        "cargo build --all-targets --all-features --locked",
        "cargo build --manifest-path render/Cargo.toml --all-targets --locked",
        "cargo build --manifest-path bindings/render-wasm/Cargo.toml --all-targets --locked",
    )
    for command in commands:
        if normalized.count(command) != 1:
            errors.append(f"{path}: CodeQL must build exactly once with `{command}`")
    if "github/codeql-action/autobuild@" in text:
        errors.append(f"{path}: CodeQL autobuild cannot replace explicit nested builds")
    init_index = normalized.find("github/codeql-action/init@")
    analyze_index = normalized.find("github/codeql-action/analyze@")
    build_indices = [normalized.find(command) for command in commands]
    if (
        init_index < 0
        or analyze_index < 0
        or any(index < 0 for index in build_indices)
        or init_index >= min(build_indices)
        or max(build_indices) >= analyze_index
    ):
        errors.append(f"{path}: explicit Rust builds must run between CodeQL init and analysis")
    return errors


def audit_render_package_release_workflow(path: Path, text: str) -> list[str]:
    """Require a verification-only dispatch and protected, exact-tag npm publish."""

    errors: list[str] = []
    required = {
        'tags:\n      - "render-v*"': "must use the render-package-only tag namespace",
        "workflow_dispatch:": "must provide a verification-only manual dry run",
        'test "$GITHUB_REF_NAME" = "render-v$version"': (
            "must bind publication to the exact package version tag"
        ),
        'test "$GITHUB_REPOSITORY" = "HyunjoJung/rxls"': (
            "must reject publication from repository forks"
        ),
        'git merge-base --is-ancestor "$GITHUB_SHA" origin/main': (
            "must require the tagged commit to be on public main"
        ),
        "require_successful_run ci.yml .github/workflows/ci.yml push CI": (
            "must require exact-SHA push CI"
        ),
        "require_successful_run codeql.yml .github/workflows/codeql.yml push CodeQL": (
            "must require exact-SHA push CodeQL"
        ),
        (
            "render-hardening.yml \\\n"
            "            .github/workflows/render-hardening.yml \\\n"
            "            workflow_dispatch"
        ): (
            "must require an exact-SHA dispatched render-hardening run"
        ),
        ".github/workflows/render-browser.yml": (
            "must require the exact-SHA push render-browser path"
        ),
        "'[.head_sha, .event, .conclusion, .status, .path] | @tsv'": (
            "must revalidate hosted run SHA, event, conclusion, status, and path"
        ),
        "--workflow render-oracle.yml": (
            "must require a successful exact-SHA Render Oracle run"
        ),
        '&& "$event" == "workflow_dispatch"': (
            "must accept full-oracle evidence only from deliberate dispatch"
        ),
        '&& "$run_path" == ".github/workflows/render-oracle.yml"': (
            "must validate the Render Oracle workflow path"
        ),
        'artifact_name="render-oracle-${GITHUB_SHA}-full"': (
            "must select only the exact-SHA full-campaign artifact"
        ),
        'actions/runs/$run_id/artifacts': (
            "must inspect the selected run's artifact metadata"
        ),
        '"$digest" =~ ^sha256:[0-9a-f]{64}$': (
            "must require an immutable artifact digest"
        ),
        "scripts/check_render_oracle_release_evidence.py": (
            "must inspect full campaign and reviewed baseline-ratchet evidence"
        ),
        "--reviewed-baseline scripts/render-parity-baseline-full.json": (
            "must bind oracle ratchets to the checked reviewed baseline"
        ),
        "oracle-prerequisite.json": (
            "must preserve and reverify aggregate oracle prerequisite evidence"
        ),
        "python3 scripts/check_render_package.py": (
            "must enforce the bounded package/archive contract"
        ),
        "EmbarkStudios/cargo-deny-action@3c6349835b2b7b196a839186cb8b78e02f7b5f25": (
            "must use the pinned nested advisory and license gate"
        ),
        "manifest-path: bindings/render-wasm/Cargo.toml": (
            "cargo-deny must audit the nested render-WASM manifest"
        ),
        "arguments: --config deny.toml --locked --all-features": (
            "cargo-deny must use the root policy and locked complete feature graph"
        ),
        "scripts/render_supply_chain.py notice": (
            "must verify the checked third-party notice against the locked closure"
        ),
        "--check bindings/render-wasm/THIRD_PARTY_NOTICES.txt": (
            "must validate the exact checked legal notice"
        ),
        '"$output/render-worker-sbom.cdx.json"': (
            "must generate nested CycloneDX evidence beside the package candidate"
        ),
        'cmp --silent \\': "must prove deterministic nested CycloneDX generation",
        "render-worker-sbom.cdx.json.sha256": (
            "must checksum and reverify nested CycloneDX evidence"
        ),
        "path: target/render-package/*": (
            "must upload the nested supply-chain evidence with the candidate"
        ),
        "python3 scripts/test_check_render_package.py": (
            "must run the focused immutable package tests"
        ),
        "python3 scripts/test_render_supply_chain.py": (
            "must run the focused nested supply-chain tests"
        ),
        "python3 scripts/test_check_render_oracle_release_evidence.py": (
            "must run the focused oracle-evidence tests"
        ),
        "npm publish --dry-run --ignore-scripts --access public": (
            "must execute the registry publication dry run"
        ),
        "sha256sum --check": "must reverify the immutable candidate checksum",
        "actions/download-artifact@": (
            "must transfer the verified candidate rather than rebuild it for publication"
        ),
        "digest-mismatch: error": "must fail closed on artifact transport drift",
        "if: github.event_name == 'push'": (
            "the publication job must not run for workflow_dispatch"
        ),
        "environment: npm-render-worker": (
            "registry mutation must pass through the protected deployment environment"
        ),
        "id-token: write": "npm publication must mint short-lived provenance identity",
        "registry-url: \"https://registry.npmjs.org\"": (
            "publication must target the public npm registry explicitly"
        ),
        "package-manager-cache: false": (
            "release jobs must not restore mutable package-manager caches"
        ),
        "NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}": (
            "the first-package bootstrap token must be scoped to the publish step"
        ),
        "npm publish \\": "must contain a real tag-only publication command",
        "npm view \"$spec\" version dist.integrity repository.url --json": (
            "must verify the published registry identity and integrity"
        ),
        "npm install --ignore-scripts \"$spec\"": (
            "must execute an exact registry-installed consumer"
        ),
    }
    for snippet, message in required.items():
        if snippet not in text:
            errors.append(f"{path}: {message}")

    exact_assignments = {
        "NODE_VERSION": RENDER_PACKAGE_NODE_VERSION,
        "NPM_VERSION": RENDER_PACKAGE_NPM_VERSION,
        "WASM_BINDGEN_VERSION": RENDER_PACKAGE_WASM_BINDGEN_VERSION,
    }
    for name, value in exact_assignments.items():
        assignment = re.compile(
            rf"^\s*{re.escape(name)}:\s*[\"']?{re.escape(value)}[\"']?\s*$",
            re.MULTILINE,
        )
        if len(assignment.findall(text)) != 1:
            errors.append(f"{path}: expected exact {name}={value}")

    if text.count("NODE_AUTH_TOKEN:") != 1 or text.count("secrets.NPM_TOKEN") != 1:
        errors.append(f"{path}: npm bootstrap credentials must appear only on publish")
    if text.count("if: github.event_name == 'push'") != 2:
        errors.append(
            f"{path}: only the hosted prerequisites and publish job may be tag-only"
        )
    if text.count("package-manager-cache: false") != 2 or re.search(
        r"package-manager-cache:\s*true", text
    ):
        errors.append(f"{path}: both release jobs must disable npm caching")
    if re.search(r"^\s*pull_request:\s*$", text, re.MULTILINE):
        errors.append(f"{path}: pull requests must never enter the registry release workflow")
    if re.search(r"\bnpm\s+publish\b[^\n]*--force\b", text):
        errors.append(f"{path}: forced npm publication is forbidden")
    if len(re.findall(r"^\s*npm publish\b", text, re.MULTILINE)) != 2:
        errors.append(f"{path}: expected exactly one dry-run and one real npm publish")
    if text.count("scripts/render_supply_chain.py sbom") != 3:
        errors.append(
            f"{path}: expected two deterministic SBOM generations and one exact validation"
        )
    hosted_gate_calls = re.findall(
        r"^\s*require_successful_run(?:\s|\\)", text, re.MULTILINE
    )
    if len(hosted_gate_calls) != 4:
        errors.append(f"{path}: expected exact-SHA CI, CodeQL, hardening, and browser gates")
    deny_index = text.find("EmbarkStudios/cargo-deny-action@")
    build_index = text.find("npm --prefix bindings/render-wasm run build:wasm")
    if deny_index < 0 or build_index < 0 or deny_index > build_index:
        errors.append(f"{path}: nested dependency policy must run before building WASM")
    return errors


def audit_repository(root: Path) -> list[str]:
    workflow_root = root / ".github" / "workflows"
    workflows = sorted((*workflow_root.glob("*.yml"), *workflow_root.glob("*.yaml")))
    if not workflows:
        return [f"{workflow_root}: no workflows found"]

    errors: list[str] = []
    for path in workflows:
        text = path.read_text(encoding="utf-8")
        errors.extend(audit_action_pins(path.relative_to(root), text))
        relative = path.relative_to(root)
        if path.name == "fuzz.yml":
            errors.extend(audit_fuzz_workflow(relative, text))
        elif path.name != "release.yml":
            errors.extend(audit_tool_commands(relative, text))
        if path.name == "render-oracle.yml":
            errors.extend(audit_render_oracle_workflow(relative, text))
        elif path.name == "render-hardening.yml":
            errors.extend(audit_render_hardening_workflow(relative, text))
        elif path.name == "render-package-release.yml":
            errors.extend(audit_render_package_release_workflow(relative, text))
        elif path.name == "codeql.yml":
            errors.extend(audit_codeql_workflow(relative, text))

    release = workflow_root / "release.yml"
    if not release.is_file():
        errors.append(f"{release.relative_to(root)}: missing release workflow")
    else:
        errors.extend(
            audit_release_versions(
                release.relative_to(root), release.read_text(encoding="utf-8")
            )
        )
    render_package_release = workflow_root / "render-package-release.yml"
    if not render_package_release.is_file():
        errors.append(
            f"{render_package_release.relative_to(root)}: missing render package release workflow"
        )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root", type=Path, default=Path(__file__).resolve().parents[1]
    )
    args = parser.parse_args()

    errors = audit_repository(args.root.resolve())
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print("workflow policy passed: immutable action SHAs and exact release tools")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
