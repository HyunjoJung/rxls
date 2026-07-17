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
RENDER_PACKAGE_WASM_BINDGEN_BUILD_RUST = "1.88.0"
RENDER_PACKAGE_WASM_BINDGEN_VERSION = "0.2.126"


def _without_commented_lines(text: str) -> str:
    """Remove fully commented lines while preserving YAML indentation and blocks."""

    return "\n".join(
        "" if line.lstrip().startswith("#") else line for line in text.splitlines()
    )


def _yaml_blocks(text: str, header: str, indent: int) -> list[str]:
    """Return active YAML blocks beginning with an exact, indentation-scoped header."""

    lines = _without_commented_lines(text).splitlines()
    target = " " * indent + header
    starts = [index for index, line in enumerate(lines) if line.rstrip() == target]
    blocks: list[str] = []
    for start in starts:
        end = len(lines)
        for index in range(start + 1, len(lines)):
            line = lines[index]
            if not line.strip():
                continue
            current_indent = len(line) - len(line.lstrip(" "))
            if current_indent <= indent:
                end = index
                break
        blocks.append("\n".join(lines[start:end]))
    return blocks


def _single_yaml_block(
    path: Path,
    text: str,
    header: str,
    indent: int,
    label: str,
    errors: list[str],
) -> str:
    blocks = _yaml_blocks(text, header, indent)
    if len(blocks) != 1:
        errors.append(f"{path}: expected exactly one active {label}")
        return ""
    return blocks[0]


def _normalized_active_commands(text: str) -> list[str]:
    active = _without_commented_lines(text)
    normalized = re.sub(r"[ \t]*\\\r?\n[ \t]*", " ", active)
    return [line.strip() for line in normalized.splitlines() if line.strip()]


def _audit_exact_wasm_bindgen_install(
    path: Path,
    workflow_text: str,
    install_step: str,
    build_step: str,
    build_command: str,
    label: str,
) -> list[str]:
    """Require an exact wasm-bindgen CLI rebuilt into an isolated temporary root."""

    errors: list[str] = []
    expected_root = (
        'tool_root="$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION"'
    )
    expected_remove = 'rm -rf "$tool_root"'
    expected_mkdir = 'mkdir -p "$tool_root"'
    expected_rustup = (
        'rustup toolchain install "$WASM_BINDGEN_BUILD_RUST" --profile minimal'
    )
    expected_cargo = (
        'cargo "+$WASM_BINDGEN_BUILD_RUST" install wasm-bindgen-cli '
        '--version "$WASM_BINDGEN_VERSION" --locked --root "$tool_root"'
    )
    expected_version = (
        'test "$("$tool_root/bin/wasm-bindgen" --version)" = '
        '"wasm-bindgen $WASM_BINDGEN_VERSION"'
    )
    expected_github_path = 'echo "$tool_root/bin" >> "$GITHUB_PATH"'
    expected_path_export = (
        'export PATH="$RUNNER_TEMP/rxls-wasm-bindgen-cli-'
        '$WASM_BINDGEN_VERSION/bin:$PATH"'
    )
    expected_resolution = (
        'test "$(command -v wasm-bindgen)" = '
        '"$RUNNER_TEMP/rxls-wasm-bindgen-cli-'
        '$WASM_BINDGEN_VERSION/bin/wasm-bindgen"'
    )
    step_commands = _normalized_active_commands(install_step)
    build_commands = _normalized_active_commands(build_step)
    workflow_commands = _normalized_active_commands(workflow_text)
    step_installs = [
        command
        for command in step_commands
        if "install wasm-bindgen-cli" in command
    ]
    workflow_installs = [
        command
        for command in workflow_commands
        if "install wasm-bindgen-cli" in command
    ]
    step_roots = [
        command for command in step_commands if command.startswith("tool_root=")
    ]
    step_github_paths = [
        command for command in step_commands if "$GITHUB_PATH" in command
    ]
    build_path_exports = [
        command for command in build_commands if command.startswith("export PATH=")
    ]
    required_install_commands = (
        "set -euo pipefail",
        'test -n "$RUNNER_TEMP"',
        expected_root,
        expected_remove,
        expected_mkdir,
        expected_rustup,
        expected_cargo,
        expected_version,
        expected_github_path,
    )
    if step_commands.count("shell: bash") != 1:
        errors.append(f"{path}: {label} must run under an explicit Bash shell")
    if any(step_commands.count(command) != 1 for command in required_install_commands):
        errors.append(
            f"{path}: {label} must create one fresh RUNNER_TEMP tool root and "
            "verify the exact build-only Rust/wasm-bindgen tool"
        )
    if step_installs != [expected_cargo]:
        errors.append(
            f"{path}: {label} must install wasm-bindgen-cli only into its "
            "fresh dedicated root"
        )
    if workflow_installs != [expected_cargo]:
        errors.append(
            f"{path}: workflow must contain exactly one active, isolated, pinned "
            "wasm-bindgen-cli install"
        )
    if step_roots != [expected_root] or step_github_paths != [expected_github_path]:
        errors.append(
            f"{path}: {label} must expose only the fresh RUNNER_TEMP tool bin "
            "through GITHUB_PATH"
        )
    install_positions = [
        step_commands.index(command)
        for command in required_install_commands
        if step_commands.count(command) == 1
    ]
    if len(install_positions) != len(required_install_commands) or install_positions != sorted(
        install_positions
    ):
        errors.append(
            f"{path}: {label} must clean the temporary root before installing and "
            "export it only after exact-version verification"
        )
    scoped_commands = (
        "set -euo pipefail",
        'test -n "$RUNNER_TEMP"',
        expected_path_export,
        expected_resolution,
        build_command,
    )
    if build_commands.count("shell: bash") != 1:
        errors.append(f"{path}: {label} build must run under an explicit Bash shell")
    if build_path_exports != [expected_path_export]:
        errors.append(
            f"{path}: {label} build must prepend only the isolated tool bin to PATH"
        )
    if any(build_commands.count(command) != 1 for command in scoped_commands):
        errors.append(
            f"{path}: {label} must prepend only the isolated tool bin to PATH, "
            "verify command resolution, and build exactly once"
        )
    build_positions = [
        build_commands.index(command)
        for command in scoped_commands
        if build_commands.count(command) == 1
    ]
    if len(build_positions) != len(scoped_commands) or build_positions != sorted(
        build_positions
    ):
        errors.append(
            f"{path}: {label} must export and verify the isolated PATH before building"
        )
    if any("--force" in command for command in workflow_installs):
        errors.append(f"{path}: forced wasm-bindgen-cli installation is forbidden")
    return errors


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
    """Require scoped, fail-closed host and OCI rendering identity gates."""

    errors: list[str] = []
    active = _without_commented_lines(text)

    pull_request = _single_yaml_block(
        path, active, "pull_request:", 2, "pull_request trigger", errors
    )
    for trigger_path in (
        '      - "scripts/render-oracle-container/**"',
        '      - "scripts/run-render-oracle-container.py"',
        '      - "scripts/test_render_oracle_container.py"',
    ):
        if trigger_path not in pull_request.splitlines():
            errors.append(
                f"{path}: pull requests must trigger hardening for {trigger_path.strip()[2:]}"
            )

    pdf_job = _single_yaml_block(path, active, "pdf:", 2, "pdf job", errors)
    pdf_runners = re.findall(r"^\s{4}runs-on:\s*(\S+)\s*$", pdf_job, re.MULTILINE)
    if pdf_runners != ["ubuntu-24.04"]:
        errors.append(f"{path}: PDF hardening must use only ubuntu-24.04")
    if 'python-version: "3.13.14"' not in pdf_job:
        errors.append(
            f"{path}: PDF hardening must match the render-oracle Python identity"
        )
    pdf_policy_step = _single_yaml_block(
        path,
        pdf_job,
        "- name: Enforce hosted workflow policy",
        6,
        "PDF policy step",
        errors,
    )
    if "run: python3 scripts/check_workflow_policy.py" not in pdf_policy_step:
        errors.append(f"{path}: PDF job must actively enforce hosted workflow policy")

    host_bootstrap = _single_yaml_block(
        path,
        pdf_job,
        "- name: Capture an unpinned host identity and fail closed",
        6,
        "host identity bootstrap step",
        errors,
    )
    for snippet, message in {
        'if [[ "$EXPECTED_IDENTITY" != "null" ]]; then': (
            "host bootstrap must run only while the reviewed identity is absent"
        ),
        "sudo apt-get update": "host bootstrap must refresh its package source",
        "sudo apt-get install --yes --no-install-recommends libcairo2 poppler-utils": (
            "host bootstrap must install only the declared comparison tools"
        ),
        'echo "Review and pin the uploaded host identity before this gate can pass." >&2': (
            "host bootstrap must explain the deliberate failure"
        ),
    }.items():
        if snippet not in host_bootstrap:
            errors.append(f"{path}: {message}")
    host_bootstrap_commands = _normalized_active_commands(host_bootstrap)
    for command, message in {
        (
            "python3 -m pip install --disable-pip-version-check --force-reinstall "
            "--no-deps --only-binary=:all: --require-hashes --requirement "
            "scripts/render-oracle-host-requirements.txt"
        ): "host bootstrap must install the exact hash-locked Python wheel closure",
        (
            "python3 scripts/render-oracle-host-tools.py verify --scope all "
            "--bootstrap-identities --output target/poppler-identity.json"
        ): "host bootstrap must emit complete typed identity evidence",
    }.items():
        if command not in host_bootstrap_commands:
            errors.append(f"{path}: {message}")
    if host_bootstrap_commands.count("exit 1") != 1:
        errors.append(f"{path}: unpinned host identity capture must fail closed")

    strict_host = _single_yaml_block(
        path,
        pdf_job,
        "- name: Verify the pinned Poppler PDF gate and complete native closure",
        6,
        "strict Poppler verification step",
        errors,
    )
    strict_commands = _normalized_active_commands(strict_host)
    for command, message in {
        "python3 scripts/render-oracle-host-tools.py apt-specs --scope poppler": (
            "strict PDF gate must install the pinned Poppler closure"
        ),
        'sudo apt-get install --yes --no-install-recommends "${SYSTEM_PACKAGES[@]}"': (
            "strict PDF gate must install only exact locked package specs"
        ),
        (
            "python3 scripts/render-oracle-host-tools.py verify --scope poppler "
            "--output target/poppler-identity.json"
        ): "strict PDF gate must verify and record the complete Poppler closure",
    }.items():
        if command not in strict_commands:
            errors.append(f"{path}: {message}")
    bootstrap_index = pdf_job.find("Capture an unpinned host identity and fail closed")
    strict_index = pdf_job.find("Verify the pinned Poppler PDF gate")
    if bootstrap_index < 0 or strict_index < 0 or bootstrap_index >= strict_index:
        errors.append(f"{path}: host bootstrap must precede the strict PDF gate")

    image_job = _single_yaml_block(
        path, active, "oracle-image:", 2, "oracle-image job", errors
    )
    image_runners = re.findall(
        r"^\s{4}runs-on:\s*(\S+)\s*$", image_job, re.MULTILINE
    )
    if image_runners != ["ubuntu-24.04"]:
        errors.append(f"{path}: oracle-image job must use only ubuntu-24.04")
    if "    name: locked LibreOffice oracle image" not in image_job.splitlines():
        errors.append(f"{path}: oracle-image job must retain its reviewed identity")
    image_policy_step = _single_yaml_block(
        path,
        image_job,
        "- name: Enforce hosted workflow policy",
        6,
        "oracle-image policy step",
        errors,
    )
    if "run: python3 scripts/check_workflow_policy.py" not in image_policy_step:
        errors.append(
            f"{path}: oracle-image job must actively enforce hosted workflow policy"
        )
    image_build = _single_yaml_block(
        path,
        image_job,
        "- name: Build and verify the locked oracle image",
        6,
        "oracle-image build step",
        errors,
    )
    for snippet, message in {
        'if [[ "$EXPECTED_IMAGE_ID" == "null" ]]; then': (
            "oracle image bootstrap must run only while the pin is absent"
        ),
        "BOOTSTRAP_ARGS+=(--bootstrap-identities)": (
            "oracle image bootstrap must pass the explicit bootstrap argument"
        ),
        'assert evidence["image_identity_status"] == "bootstrap_capture_required", evidence': (
            "unpinned image evidence must have the bootstrap status"
        ),
        'assert evidence["expected_image_id"] is None, evidence': (
            "unpinned image evidence must not claim a reviewed identity"
        ),
        "raise SystemExit(1)": "unpinned oracle image capture must fail closed",
        'assert evidence["image_identity_status"] == "pinned_match", evidence': (
            "pinned oracle image evidence must require pinned_match"
        ),
        'assert evidence["expected_image_id"] == expected == evidence["built_image_id"], evidence': (
            "pinned oracle image evidence must match expected and built identities"
        ),
    }.items():
        if snippet not in image_build:
            errors.append(f"{path}: {message}")
    image_commands = _normalized_active_commands(image_build)
    for command, message in {
        (
            'python3 scripts/run-render-oracle-container.py verify-lock "${BOOTSTRAP_ARGS[@]}"'
        ): "oracle image gate must verify the reproducible build contract",
        (
            "python3 scripts/run-render-oracle-container.py build --engine docker "
            "--image rxls-render-oracle:lo-26.2.3 --execute "
            '"${BOOTSTRAP_ARGS[@]}" > target/render-oracle-image-build.json'
        ): "oracle image gate must execute and record the reproducible OCI build",
    }.items():
        if command not in image_commands:
            errors.append(f"{path}: {message}")
    if image_build.count('"${BOOTSTRAP_ARGS[@]}"') != 2:
        errors.append(
            f"{path}: verify-lock and build must consume the same bootstrap argument array"
        )
    image_upload = _single_yaml_block(
        path,
        image_job,
        "- name: Upload oracle image identity evidence",
        6,
        "oracle-image evidence upload step",
        errors,
    )
    if (
        "if: always()" not in image_upload
        or "path: target/render-oracle-image-build.json" not in image_upload
        or "if-no-files-found: error" not in image_upload
    ):
        errors.append(f"{path}: oracle-image identity evidence must always upload")

    apt_lines = [line for line in active.splitlines() if "apt-get " in line]
    if (
        len(apt_lines) != 4
        or sum("apt-get update" in line for line in apt_lines) != 2
        or sum(
            'apt-get install --yes --no-install-recommends "${SYSTEM_PACKAGES[@]}"'
            in line
            for line in apt_lines
        )
        != 1
        or sum(
            "apt-get install --yes --no-install-recommends libcairo2 poppler-utils"
            in line
            for line in apt_lines
        )
        != 1
    ):
        errors.append(
            f"{path}: PDF apt inputs must be the fail-closed bootstrap or exact lock"
        )
    if "poppler-version.txt" in active or "command -v pdfinfo |" in active:
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


def audit_render_browser_workflow(path: Path, text: str) -> list[str]:
    """Require the browser lane to build wasm-bindgen with its exact Rust pin."""

    errors: list[str] = []
    active = _without_commented_lines(text)
    for name, value in {
        "WASM_BINDGEN_BUILD_RUST": RENDER_PACKAGE_WASM_BINDGEN_BUILD_RUST,
        "WASM_BINDGEN_VERSION": RENDER_PACKAGE_WASM_BINDGEN_VERSION,
    }.items():
        assignment = re.compile(
            rf"^\s*{re.escape(name)}:\s*[\"']?{re.escape(value)}[\"']?\s*$",
            re.MULTILINE,
        )
        if len(assignment.findall(active)) != 1:
            errors.append(f"{path}: expected exact {name}={value}")

    worker_job = _single_yaml_block(
        path, active, "worker-wasm:", 2, "worker-wasm job", errors
    )
    metadata_step = _single_yaml_block(
        path,
        worker_job,
        "- name: Verify publishable package and pinned toolchain metadata",
        6,
        "browser toolchain metadata step",
        errors,
    )
    if (
        "l.wasmBindgen.buildRust !== process.env.WASM_BINDGEN_BUILD_RUST"
        not in metadata_step
    ):
        errors.append(
            f"{path}: browser metadata gate must bind wasm-bindgen to its build Rust pin"
        )
    install_step = _single_yaml_block(
        path,
        worker_job,
        "- name: Install exact wasm-bindgen CLI",
        6,
        "browser wasm-bindgen install step",
        errors,
    )
    build_step = _single_yaml_block(
        path,
        worker_job,
        "- name: Build exact wasm32 package",
        6,
        "browser wasm package build step",
        errors,
    )
    errors.extend(
        _audit_exact_wasm_bindgen_install(
            path,
            active,
            install_step,
            build_step,
            "npm run build:wasm",
            "browser wasm-bindgen install step",
        )
    )
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
        "WASM_BINDGEN_BUILD_RUST": RENDER_PACKAGE_WASM_BINDGEN_BUILD_RUST,
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
    active = _without_commented_lines(text)
    verify_job = _single_yaml_block(
        path, active, "verify:", 2, "render package verify job", errors
    )
    build_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Build the exact worker/WASM package",
        6,
        "render package wasm-bindgen build step",
        errors,
    )
    errors.extend(
        _audit_exact_wasm_bindgen_install(
            path,
            active,
            build_step,
            build_step,
            "npm --prefix bindings/render-wasm run build:wasm",
            "render package wasm-bindgen build step",
        )
    )
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
        elif path.name == "render-browser.yml":
            errors.extend(audit_render_browser_workflow(relative, text))
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
