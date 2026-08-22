#!/usr/bin/env python3
"""Tests for the hosted workflow supply-chain policy."""

from __future__ import annotations

import importlib.util
import os
import re
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
POLICY = ROOT / "scripts" / "check_workflow_policy.py"
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
CODEQL_WORKFLOW = ROOT / ".github" / "workflows" / "codeql.yml"
FUZZ_WORKFLOW = ROOT / ".github" / "workflows" / "fuzz.yml"
RENDER_ORACLE_WORKFLOW = ROOT / ".github" / "workflows" / "render-oracle.yml"
RENDER_HARDENING_WORKFLOW = ROOT / ".github" / "workflows" / "render-hardening.yml"
RENDER_BROWSER_WORKFLOW = ROOT / ".github" / "workflows" / "render-browser.yml"
RENDER_PACKAGE_RELEASE_WORKFLOW = (
    ROOT / ".github" / "workflows" / "render-package-release.yml"
)


def _load():
    spec = importlib.util.spec_from_file_location("check_workflow_policy", POLICY)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class WorkflowPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.policy = _load()

    def test_repository_workflows_pass(self) -> None:
        self.assertEqual(self.policy.audit_repository(ROOT), [])

    def test_ci_and_release_pin_the_registry_semver_gate(self) -> None:
        for workflow_path in (
            CI_WORKFLOW,
            ROOT / ".github" / "workflows" / "release.yml",
        ):
            original = workflow_path.read_text(encoding="utf-8")
            with self.subTest(workflow=workflow_path.name, state="valid"):
                self.assertEqual(
                    self.policy.audit_semver_gate(workflow_path.name, original), []
                )
            mutations = {
                "version": original.replace(
                    'CARGO_SEMVER_CHECKS_VERSION: "0.49.0"',
                    'CARGO_SEMVER_CHECKS_VERSION: "latest"',
                    1,
                ),
                "install": original.replace(
                    'cargo install cargo-semver-checks --version "$CARGO_SEMVER_CHECKS_VERSION" --locked',
                    "cargo install cargo-semver-checks",
                    1,
                ),
                "baseline": original.replace(
                    "--baseline-version 0.1.2", "--baseline-version 0.1.3", 1
                ),
                "release_type": original.replace(
                    "--release-type patch", "--release-type minor", 1
                ),
                "feature_mode": original.replace(
                    "--only-explicit-features", "--all-features", 1
                ),
            }
            for name, workflow in mutations.items():
                with self.subTest(workflow=workflow_path.name, mutation=name):
                    self.assertTrue(
                        self.policy.audit_semver_gate(workflow_path.name, workflow)
                    )

    def test_ci_keeps_cli_ods_feature_surface_warning_clean(self) -> None:
        original = CI_WORKFLOW.read_text(encoding="utf-8")
        self.assertEqual(
            self.policy.audit_ci_feature_matrix(CI_WORKFLOW.name, original), []
        )
        for command in self.policy.ADDITIONAL_FEATURE_CLIPPY_COMMANDS:
            with self.subTest(command=command):
                removed = original.replace(command, "", 1)
                self.assertNotEqual(removed, original)
                self.assertTrue(
                    self.policy.audit_ci_feature_matrix(CI_WORKFLOW.name, removed)
                )

    def test_mutable_action_ref_is_rejected(self) -> None:
        errors = self.policy.audit_action_pins(
            Path(".github/workflows/example.yml"),
            "steps:\n  - uses: actions/checkout@v7 # v7.0.1\n",
        )

        self.assertTrue(any("full immutable commit SHA" in error for error in errors))

    def test_action_pin_without_version_comment_is_rejected(self) -> None:
        errors = self.policy.audit_action_pins(
            Path(".github/workflows/example.yml"),
            "steps:\n  - uses: actions/checkout@" + "a" * 40 + "\n",
        )

        self.assertTrue(any("needs a version comment" in error for error in errors))

    def test_setup_node_requires_reviewed_identity_and_comment(self) -> None:
        path = Path(".github/workflows/example.yml")
        valid = (
            "steps:\n"
            "  - uses: actions/setup-node@"
            "820762786026740c76f36085b0efc47a31fe5020 # v7.0.0\n"
        )
        self.assertEqual(self.policy.audit_action_pins(path, valid), [])

        mutations = {
            "commit": valid.replace(
                "820762786026740c76f36085b0efc47a31fe5020", "a" * 40
            ),
            "comment": valid.replace("# v7.0.0", "# v6.5.0"),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertTrue(self.policy.audit_action_pins(path, workflow))

    def test_pull_request_checkouts_require_exact_head_and_immediate_verifier(
        self,
    ) -> None:
        expression = "${{ github.event.pull_request.head.sha || github.sha }}"
        valid = f"""
on:
  pull_request:
jobs:
  test:
    steps:
      - uses: actions/checkout@{"a" * 40} # v7.0.1
        with:
          ref: {expression}
      - name: Verify exact source revision
        shell: bash
        env:
          EXPECTED_SHA: {expression}
        run: test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"
      - run: true
"""
        path = Path(".github/workflows/example.yml")
        self.assertEqual(self.policy.audit_pr_head_checkouts(path, valid), [])

        trigger_forms = (
            "on:\n  pull_request: {}",
            'on:\n  "pull_request": {}',
            "on: pull_request",
            "on: [push, pull_request]",
            "on: {push: {}, pull_request: {}}",
            '"on": {"pull_request": {}}',
        )
        for trigger in trigger_forms:
            with self.subTest(trigger=trigger):
                workflow = valid.replace("on:\n  pull_request:", trigger, 1)
                self.assertEqual(
                    self.policy.audit_pr_head_checkouts(path, workflow),
                    [],
                )

        mutations = (
            valid.replace(
                f"        with:\n          ref: {expression}\n",
                "",
            ),
            valid.replace(expression, "${{ github.sha }}", 1),
            valid.replace(
                "      - name: Verify exact source revision\n",
                "      # - name: Verify exact source revision\n",
            ),
            valid.replace(
                'run: test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"',
                'run: test "$(git rev-parse HEAD)" = "$GITHUB_SHA"',
            ),
            valid.replace(
                "      - name: Verify exact source revision\n",
                "      - run: true\n      - name: Verify exact source revision\n",
            ),
            valid.replace(
                "        shell: bash\n",
                "        continue-on-error: true\n        shell: bash\n",
                1,
            ),
            valid.replace(
                "        shell: bash\n",
                "        if: ${{ false }}\n        shell: bash\n",
                1,
            ),
            valid.replace(
                "        with:\n",
                "        if: ${{ false }}\n        with:\n",
                1,
            ),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.assertTrue(self.policy.audit_pr_head_checkouts(path, mutation))

    def test_pull_request_checkout_accepts_only_exact_hardened_verifier(self) -> None:
        expression = "${{ github.event.pull_request.head.sha || github.sha }}"
        hardened = f"""
on:
  pull_request:
jobs:
  test:
    steps:
      - uses: actions/checkout@{"a" * 40} # v7.0.1
        with:
          ref: {expression}
      - name: Verify exact source revision
        shell: bash
        env:
          EXPECTED_SHA: {expression}
        run: |
          set -euo pipefail
          test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"
          git diff --exit-code
          git diff --cached --exit-code
"""
        path = Path(".github/workflows/example.yml")
        self.assertEqual(self.policy.audit_pr_head_checkouts(path, hardened), [])

        for command in (
            "          set -euo pipefail\n",
            "          git diff --exit-code\n",
            "          git diff --cached --exit-code\n",
        ):
            with self.subTest(command=command):
                weakened = hardened.replace(command, "", 1)
                self.assertTrue(self.policy.audit_pr_head_checkouts(path, weakened))

    def test_flow_map_pull_request_cannot_bypass_exact_head_guards(self) -> None:
        expression = "${{ github.event.pull_request.head.sha || github.sha }}"
        valid = f"""
on:
  pull_request: {{}}
jobs:
  test:
    steps:
      - uses: actions/checkout@{"a" * 40} # v7.0.1
        with:
          ref: {expression}
      - name: Verify exact source revision
        shell: bash
        env:
          EXPECTED_SHA: {expression}
        run: test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"
"""
        without_ref = valid.replace(
            f"        with:\n          ref: {expression}\n",
            "",
        )
        verifier = f"""      - name: Verify exact source revision
        shell: bash
        env:
          EXPECTED_SHA: {expression}
        run: test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"
"""
        without_ref_or_verifier = without_ref.replace(verifier, "")
        path = Path(".github/workflows/example.yml")

        self.assertTrue(self.policy.audit_pr_head_checkouts(path, without_ref))
        self.assertTrue(
            self.policy.audit_pr_head_checkouts(path, without_ref_or_verifier)
        )

    def test_each_pull_request_job_requires_its_own_guarded_checkout(self) -> None:
        expression = "${{ github.event.pull_request.head.sha || github.sha }}"
        guarded_steps = f"""    steps:
      - uses: actions/checkout@{"a" * 40} # v7.0.1
        with:
          ref: {expression}
      - name: Verify exact source revision
        shell: bash
        env:
          EXPECTED_SHA: {expression}
        run: test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"
"""
        valid = (
            "on: pull_request\n"
            "jobs:\n"
            "  linux:\n" + guarded_steps + "  macos:\n" + guarded_steps
        )
        path = Path(".github/workflows/example.yml")
        self.assertEqual(self.policy.audit_pr_head_checkouts(path, valid), [])

        mutations = (
            valid.replace(
                "  macos:\n" + guarded_steps,
                "  macos:\n    steps:\n      - run: true\n",
            ),
            valid.replace(
                "  macos:\n" + guarded_steps,
                "  macos:\n    uses: owner/repository/.github/workflows/test.yml@main\n",
            ),
            valid.replace(
                "  macos:\n" + guarded_steps,
                "  macos: {runs-on: ubuntu-latest, steps: []}\n",
            ),
            valid.replace(
                "jobs:\n",
                "jobs: {linux: {runs-on: ubuntu-latest, steps: []}}\nignored:\n",
                1,
            ),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.assertTrue(self.policy.audit_pr_head_checkouts(path, mutation))

    def test_pull_request_target_is_explicitly_rejected(self) -> None:
        for trigger in (
            "on: pull_request_target",
            "on: [push, pull_request_target]",
            'on: {"pull_request_target": {}}',
        ):
            with self.subTest(trigger=trigger):
                errors = self.policy.audit_pr_head_checkouts(
                    Path(".github/workflows/example.yml"),
                    trigger + "\njobs: {}\n",
                )
                self.assertTrue(
                    any("pull_request_target is forbidden" in error for error in errors)
                )

    def test_non_pull_request_checkout_does_not_require_head_expression(self) -> None:
        text = (
            "on:\n  workflow_dispatch:\n  # pull_request: {}\njobs:\n  test:\n    steps:\n"
            f"      - uses: actions/checkout@{'a' * 40} # v7.0.1\n"
        )
        self.assertEqual(
            self.policy.audit_pr_head_checkouts(
                Path(".github/workflows/manual.yml"), text
            ),
            [],
        )

    def test_unversioned_release_cargo_fuzz_is_rejected(self) -> None:
        text = """
env:
  RELEASE_RUST_VERSION: "1.96.1"
  FUZZ_NIGHTLY_VERSION: "nightly-2026-07-10"
  CARGO_FUZZ_VERSION: "0.13.2"
steps:
  - run: cargo install cargo-fuzz --locked
"""

        errors = self.policy.audit_release_versions(Path("release.yml"), text)

        self.assertTrue(any("cargo-fuzz install must use" in error for error in errors))

    def test_exact_release_cargo_fuzz_is_accepted(self) -> None:
        text = """
env:
  RELEASE_RUST_VERSION: "1.96.1"
  FUZZ_NIGHTLY_VERSION: "nightly-2026-07-10"
  CARGO_FUZZ_VERSION: "0.13.2"
steps:
  - run: cargo install cargo-fuzz --version "$CARGO_FUZZ_VERSION" --locked
"""

        self.assertEqual(
            self.policy.audit_release_versions(Path("release.yml"), text), []
        )

    def test_core_release_binds_dry_run_and_public_provenance(self) -> None:
        original = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        self.assertEqual(
            self.policy.audit_core_release_evidence(Path("release.yml"), original),
            [],
        )
        runner = (
            "          python3 scripts/check_cargo_publish_dry_run.py run \\\n"
            "            --manifest Cargo.toml \\\n"
            '            --git-sha "$GITHUB_SHA" \\\n'
            "            --output target/package/"
            "release-cargo-publish-dry-run.json\n"
        )
        dist_verifier = (
            "          python3 scripts/check_cargo_publish_dry_run.py verify \\\n"
            "            --manifest Cargo.toml \\\n"
            '            --git-sha "$GITHUB_SHA" \\\n'
            "            --receipt dist/release-cargo-publish-dry-run.json\n"
        )
        mutations = {
            "bare_dry_run": original.replace(
                runner,
                "          cargo publish --dry-run --locked --registry crates-io\n",
                1,
            ),
            "heredoc_receipt": original.replace(
                runner,
                "          python3 - <<'PY'\n"
                "          from pathlib import Path\n"
                "          Path('target/package/release-cargo-publish-dry-run.json')"
                ".write_text('{}')\n"
                "          PY\n",
                1,
            ),
            "runner_output_detached": original.replace(
                "--output target/package/release-cargo-publish-dry-run.json",
                "--output target/release-cargo-publish-dry-run.json",
                1,
            ),
            "candidate_verifier": original.replace(
                dist_verifier,
                "          true\n",
                1,
            ),
            "candidate_comparison_verifier": original.replace(
                "--receipt target/baseline-release/release-cargo-publish-dry-run.json",
                "--receipt target/baseline-release/unverified.json",
                1,
            ),
            "tag_authorization_verifier": original.replace(
                "--receipt target/attested-candidate-release/"
                "release-cargo-publish-dry-run.json",
                "--receipt target/attested-candidate-release/unverified.json",
                1,
            ),
            "post_download_verifier": original.replace(
                '--receipt "$smoke/assets/release-cargo-publish-dry-run.json"',
                '--receipt "$smoke/assets/unverified.json"',
                1,
            ),
            "candidate_manifest_upload": original.replace(
                "            target/reproducibility/rxls-release-candidate-manifest.json\n",
                "",
                1,
            ),
            "tag_comparison": original.replace(
                "          cp target/publication-attestation/rxls-tag-release-comparison.json dist/\n",
                "",
                1,
            ),
            "manifest_input_count": original.replace(
                "[[ ${#artifacts[@]} -eq 50 ]]",
                "[[ ${#artifacts[@]} -eq 49 ]]",
                1,
            ),
            "candidate_file_count": original.replace(
                "--expected-files 48",
                "--expected-files 47",
                1,
            ),
            "publication_file_count": original.replace(
                "--expected-files 52",
                "--expected-files 51",
                1,
            ),
            "github_release_reconciler": original.replace(
                "python3 scripts/reconcile_github_release.py \\\n",
                "true \\\n",
                1,
            ),
            "github_release_revision": original.replace(
                '--target-commitish "$GITHUB_SHA" \\\n',
                '--target-commitish "$GITHUB_REF_NAME" \\\n',
                1,
            ),
            "github_release_inventory": original.replace(
                "            --dist dist \\\n            --expected-files 52 \\\n",
                "            --dist dist \\\n            --expected-files 51 \\\n",
                1,
            ),
            "github_release_failure_bypass": original.replace(
                "      - name: Create or update GitHub release\n",
                "      - name: Create or update GitHub release\n        continue-on-error: true\n",
                1,
            ),
            "release_main_ancestor_only": original.replace(
                'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"',
                'git merge-base --is-ancestor "$GITHUB_SHA" origin/main',
                1,
            ),
            "release_history_shallow": original.replace(
                "fetch-depth: 0", "fetch-depth: 1", 1
            ),
            "release_checkout_credentials": original.replace(
                "persist-credentials: false", "persist-credentials: true", 1
            ),
            "release_identity_not_first": original.replace(
                "          persist-credentials: false\n"
                "      - name: Validate release identity\n",
                "          persist-credentials: false\n"
                "      - run: python3 scripts/check_release_identity.py\n"
                "      - name: Validate release identity\n",
                1,
            ),
            "release_shallow_probe": original.replace(
                'test "$(git rev-parse --is-shallow-repository)" = "false"',
                "true",
                1,
            ),
            "release_commit_count": original.replace(
                'test "$(git rev-list --count HEAD)" = "1"',
                'test "$(git rev-list --count HEAD)" -ge "1"',
                1,
            ),
            "release_root_count": original.replace(
                'test "$(git rev-list --max-parents=0 --count HEAD)" = "1"',
                'test "$(git rev-list --max-parents=0 --count HEAD)" -ge "1"',
                1,
            ),
            "release_render_harness_dependency": original.replace(
                '            "numpy==2.4.4" \\\n',
                '            "numpy==2.4.3" \\\n',
                1,
            ),
            "candidate_attempt_name": original.replace(
                "name: rxls-${{ steps.release.outputs.version }}-release-${{ github.run_attempt }}",
                "name: rxls-${{ steps.release.outputs.version }}-release",
                1,
            ),
            "baseline_attempt_download": original.replace(
                '--name "rxls-${version}-release-${baseline_run_attempt}"',
                '--name "rxls-${version}-release"',
                1,
            ),
            "comparison_attempt_download": original.replace(
                '--name "rxls-${version}-release-${selected_attempt}"',
                '--name "rxls-${version}-release"',
                1,
            ),
            "comparison_attempt_current": original.replace(
                '[[ "$current_attempt" == "$selected_attempt" ]] || continue',
                "true",
                1,
            ),
            "release_concurrency_cancel": original.replace(
                "cancel-in-progress: false",
                "cancel-in-progress: true",
                1,
            ),
            "publish_registry_unbound": original.replace(
                "cargo publish --locked --registry crates-io",
                "cargo publish --locked",
                1,
            ),
            "publish_token_in_argv": original.replace(
                "cargo publish --locked --registry crates-io",
                'cargo publish --locked --token "${{ secrets.CARGO_REGISTRY_TOKEN }}"',
                1,
            ),
            "publish_tag_revalidation": original.replace(
                '          git fetch origin "refs/tags/$GITHUB_REF_NAME" --no-tags\n'
                '          test "$(git rev-parse \'FETCH_HEAD^{commit}\')" = "$GITHUB_SHA"\n',
                "",
                1,
            ),
            "github_release_wildcard": original.replace(
                "python3 scripts/reconcile_github_release.py \\\n",
                'gh release upload "$tag" dist/* --clobber \\\n',
                1,
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(mutation=name):
                self.assertNotEqual(workflow, original)
                self.assertTrue(
                    self.policy.audit_core_release_evidence(
                        Path("release.yml"), workflow
                    )
                )

    def test_github_release_reconciler_invariants_are_mutation_guarded(self) -> None:
        path = Path("scripts/reconcile_github_release.py")
        original = (ROOT / path).read_text(encoding="utf-8")
        self.assertEqual(
            self.policy.audit_github_release_reconciler(path, original), []
        )
        mutations = {
            "local_count": original.replace(
                "if len(entries) != expected_files:", "if False:", 1
            ),
            "stale_asset_delete": original.replace(
                "client.delete_release_asset(asset_id)", "pass", 1
            ),
            "replacement_empty_guard": original.replace(
                "if remaining_assets != []:", "if False:", 1
            ),
            "upload_all": original.replace(
                "client.upload_release_asset(release_id, local_assets[name])",
                "pass",
                1,
            ),
            "release_state": original.replace(
                '{"draft": False, "prerelease": False}',
                '{"draft": True, "prerelease": True}',
                1,
            ),
            "published_state_verification": original.replace(
                "require_published=True", "require_published=False", 1
            ),
            "asset_count": original.replace(
                "if len(remote_assets) != len(local_assets):", "if False:", 1
            ),
            "uploaded_state": original.replace(
                'if raw.get("state") != "uploaded":', "if False:", 1
            ),
            "byte_size": original.replace("size != local.size", "False", 1),
            "remote_digest": original.replace(
                "if digest != local.digest:", "if False:", 1
            ),
            "exact_name_set": original.replace(
                "if seen_names != set(local_assets):", "if False:", 1
            ),
            "target_sha": original.replace(
                "if SHA_RE.fullmatch(target_commitish) is None:", "if False:", 1
            ),
            "tag_commit_preflight": original.replace(
                "if client.get_tag_commit_sha(tag) != target_commitish:",
                "if False:",
                1,
            ),
            "exact_release_noop": original.replace(
                "        return\n    immutable = release.get",
                "        pass\n    immutable = release.get",
                1,
            ),
            "immutable_release": original.replace(
                "if immutable is True:", "if False:", 1
            ),
            "published_release_guard": original.replace(
                'if release.get("draft") is not True:', "if False:", 1
            ),
            "external_dependency": original.replace(
                "import urllib.request", "import requests", 1
            ),
        }
        for name, source in mutations.items():
            with self.subTest(mutation=name):
                self.assertNotEqual(source, original)
                self.assertTrue(
                    self.policy.audit_github_release_reconciler(path, source)
                )

    def test_mutable_fuzz_workflow_toolchain_is_rejected(self) -> None:
        text = """
env:
  FUZZ_NIGHTLY_VERSION: nightly
  CARGO_FUZZ_VERSION: "0.13.2"
steps:
  - run: |
      rustup toolchain install nightly
      cargo install cargo-fuzz --version "$CARGO_FUZZ_VERSION" --locked
      cargo +nightly fuzz build
"""

        errors = self.policy.audit_fuzz_workflow(Path("fuzz.yml"), text)

        self.assertTrue(
            any("expected exact FUZZ_NIGHTLY_VERSION" in error for error in errors)
        )
        self.assertTrue(
            any("must not install mutable nightly" in error for error in errors)
        )
        self.assertTrue(
            any("must not invoke mutable nightly" in error for error in errors)
        )

    def test_exact_fuzz_workflow_tools_are_accepted(self) -> None:
        text = FUZZ_WORKFLOW.read_text(encoding="utf-8")

        self.assertEqual(self.policy.audit_fuzz_workflow(Path("fuzz.yml"), text), [])

    def test_fuzz_dispatch_bridge_rejects_accidental_or_unbound_oracle_runs(
        self,
    ) -> None:
        original = FUZZ_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "oracle_default": original.replace(
                "        default: fuzz",
                "        default: render-oracle",
                1,
            ),
            "ordinary_fuzz_skipped": original.replace(
                "    if: ${{ github.event_name != 'workflow_dispatch' || inputs.target == 'fuzz' }}",
                "    if: ${{ inputs.target == 'fuzz' }}",
                1,
            ),
            "oracle_on_pr": original.replace(
                "    if: ${{ github.event_name == 'workflow_dispatch' && inputs.target == 'render-oracle' }}",
                "    if: ${{ inputs.target == 'render-oracle' }}",
                1,
            ),
            "write_permission": original.replace(
                "    permissions:\n      contents: read\n"
                "    uses: ./.github/workflows/render-oracle.yml",
                "    permissions:\n      contents: write\n"
                "    uses: ./.github/workflows/render-oracle.yml",
                1,
            ),
            "mutable_source": original.replace(
                "      source_sha: ${{ github.sha }}",
                "      source_sha: ${{ github.ref }}",
                1,
            ),
            "diagnostic_campaign_removed": original.replace(
                "          - ooxml-row-diagnostic\n",
                "",
                1,
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, original)
                self.assertTrue(
                    self.policy.audit_fuzz_workflow(Path("fuzz.yml"), workflow)
                )

    def test_mutable_tools_are_rejected_in_any_workflow(self) -> None:
        text = """
steps:
  - run: |
      rustup toolchain install nightly
      cargo install cargo-fuzz --version 0.13.1 --locked
      cargo +nightly fuzz build
"""

        errors = self.policy.audit_tool_commands(Path("ci.yml"), text)

        self.assertEqual(len(errors), 3)

    def test_repository_audit_reports_missing_workflows(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            errors = self.policy.audit_repository(Path(tmp))

        self.assertTrue(any("no workflows found" in error for error in errors))

    def test_render_oracle_rejects_mutable_python_pip_apt_and_identity_status(
        self,
    ) -> None:
        original = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "python": original.replace(
                'python-version: "3.13.14"', 'python-version: "3.13"'
            ),
            "pip": original.replace("            --require-hashes \\\n", ""),
            "apt": original.replace(
                'sudo apt-get "${APT_OPTIONS[@]}" install \\',
                "sudo apt-get install \\",
                1,
            ),
            "apt_live_source_parts": original.replace(
                '-o "Dir::Etc::sourceparts=-"',
                '-o "Dir::Etc::sourceparts=/etc/apt/sources.list.d"',
                1,
            ),
            "apt_snapshot_generator": original.replace(
                "python3 scripts/render-oracle-host-tools.py apt-sources \\",
                "printf '%s\\n' 'deb https://archive.ubuntu.com/ubuntu noble main' \\",
                1,
            ),
            "identity": original.replace(
                'assert document["image_identity_status"] == "pinned_match"',
                'assert document["image_identity_status"] in {"pinned_match", "runtime_verified"}',
            ),
            "buildx_version": original.replace(
                "          version: v0.35.0",
                "          version: latest",
                1,
            ),
            "buildkit_image": original.replace(
                "moby/buildkit:v0.31.2@sha256:",
                "moby/buildkit:latest@sha256:",
                1,
            ),
            "reproducibility": original.replace(
                '          assert reproducibility["config_ids"] == [image_id, image_id], reproducibility',
                '          # assert reproducibility["config_ids"] == [image_id, image_id], reproducibility',
                1,
            ),
            "manifest_reproducibility": original.replace(
                '          assert reproducibility["manifest_digests"] == [manifest_digest, manifest_digest], reproducibility',
                '          # assert reproducibility["manifest_digests"] == [manifest_digest, manifest_digest], reproducibility',
                1,
            ),
            "bootstrap_reuse": original.replace(
                '              assert document["image_identity_status"] == "bootstrap_capture_required"',
                '              assert document["image_identity_status"] in {"bootstrap_capture_required", "pinned_match"}',
                1,
            ),
            "runtime_identity_schema": original.replace(
                "rxls.render-oracle-container-execution.v3",
                "rxls.render-oracle-container-execution.v2",
                1,
            ),
            "runtime_manifest_digest": original.replace(
                '              assert {row["image"]["manifest_digest"] for row in adapters} == {',
                '              # assert {row["image"]["manifest_digest"] for row in adapters} == {',
                1,
            ),
            "configuration_identity_schema": original.replace(
                'oracle_lock["schema"] == "rxls.render-oracle-container-identity.v2"',
                'oracle_lock["schema"] == "rxls.render-oracle-container-identity.v1"',
                1,
            ),
            "fidelity_manifest_digest": original.replace(
                '                  gate["evidence"]["oracle_image_manifest_digest"]',
                '                  gate["evidence"]["oracle_image_config_digest"]',
                1,
            ),
            "summary_source_commit": original.replace(
                '                  "source_commit": build["source_commit"],',
                '                  "source_commit": "unbound",',
                1,
            ),
            "summary_wrapper_identity": original.replace(
                '                  "wrapper_sha256": build["wrapper_sha256"],',
                '                  "wrapper_sha256": renderer["sha256"],',
                1,
            ),
            "summary_schema_v6": original.replace(
                "rxls.render-oracle-hosted-campaign.v7",
                "rxls.render-oracle-hosted-campaign.v6",
                1,
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, original)
                errors = self.policy.audit_render_oracle_workflow(
                    Path("render-oracle.yml"), workflow
                )
                self.assertTrue(errors)

    def test_checked_in_render_oracle_reproducibility_policy_passes(self) -> None:
        text = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        self.assertEqual(
            self.policy.audit_render_oracle_workflow(Path("render-oracle.yml"), text),
            [],
        )

    def test_oracle_image_retry_contract_is_bounded_and_fail_closed(self) -> None:
        cases = (
            (
                Path("render-oracle.yml"),
                RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8"),
                "- name: Build and inspect the locked oracle image",
                "target/render-oracle-hosted/build.json",
                "target/render-oracle-hosted/build.stderr",
            ),
            (
                Path("render-hardening.yml"),
                RENDER_HARDENING_WORKFLOW.read_text(encoding="utf-8"),
                "- name: Build and verify the locked oracle image",
                "target/render-oracle-image-build.json",
                "target/render-oracle-image-build.stderr",
            ),
        )
        mutations = {
            "unbounded_attempts": (
                "for build_attempt in 1 2 3; do",
                "for build_attempt in 1 2 3 4; do",
            ),
            "stale_output": ("rm -f target/", "test -e target/"),
            "discard_status": ("build_status=$?", "build_status=1"),
            "retry_every_curl_failure": (
                r"curl: \((5|6|7|18|28|35|52|55|56|92)\)",
                r"curl: \([0-9]+\)",
            ),
            "retry_not_found": (
                "(408|429|500|502|503|504)",
                "(404|408|429|500|502|503|504)",
            ),
            "long_backoff": (
                "retry_delay_seconds=$((build_attempt * 5))",
                "retry_delay_seconds=$((build_attempt * 60))",
            ),
            "continue_after_success": (
                "\n              break\n",
                "\n              continue\n",
            ),
            "retry_integrity_failure": (
                'if ! retryable_oracle_download_failure "$build_log"; then\n'
                '              exit "$build_status"',
                'if ! retryable_oracle_download_failure "$build_log"; then\n'
                "              :",
            ),
            "mask_final_failure": ('exit "$build_status"', "exit 0"),
        }
        for path, workflow, header, output_path, log_path in cases:
            extraction_errors: list[str] = []
            step = self.policy._single_yaml_block(
                path,
                workflow,
                header,
                6,
                "locked oracle image build step",
                extraction_errors,
            )
            self.assertEqual(extraction_errors, [])
            errors: list[str] = []
            self.policy._audit_oracle_build_retry(
                path,
                step,
                output_path,
                log_path,
                errors,
            )
            self.assertEqual(errors, [])
            for name, (source, replacement) in mutations.items():
                with self.subTest(workflow=path.name, mutation=name):
                    mutated = step.replace(source, replacement, 1)
                    self.assertNotEqual(mutated, step)
                    errors = []
                    self.policy._audit_oracle_build_retry(
                        path,
                        mutated,
                        output_path,
                        log_path,
                        errors,
                    )
                    self.assertTrue(errors)

    @unittest.skipIf(os.name == "nt", "executes an Ubuntu workflow shell")
    def test_oracle_image_retry_executes_only_for_transient_downloads(self) -> None:
        cases = (
            (
                "render-oracle",
                RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8"),
                "- name: Build and inspect the locked oracle image",
                Path("target/render-oracle-hosted/build.json"),
            ),
            (
                "render-hardening",
                RENDER_HARDENING_WORKFLOW.read_text(encoding="utf-8"),
                "- name: Build and verify the locked oracle image",
                Path("target/render-oracle-image-build.json"),
            ),
        )
        scenarios = {
            "integrity": (2, ["1"]),
            "transient": (0, ["1", "2"]),
            "exhausted": (2, ["1", "2", "3"]),
        }
        mock_function = textwrap.dedent(
            """\
            build_oracle_image() {
              mock_attempt=$((mock_attempt + 1))
              printf '%s\\n' "$mock_attempt" >> "$MOCK_ATTEMPTS_FILE"
              case "$MOCK_MODE" in
                integrity)
                  if [[ "$mock_attempt" -eq 1 ]]; then
                    printf '%s\\n' \
                      'render_oracle_error:image_reproducibility_mismatch' >&2
                    return 2
                  fi
                  ;;
                transient)
                  if [[ "$mock_attempt" -eq 1 ]]; then
                    printf '%s\\n' \
                      "$MOCK_ORACLE_URL" \
                      'curl: (18) transfer closed with bytes remaining to read' >&2
                    return 2
                  fi
                  ;;
                exhausted)
                  printf '%s\\n' \
                    "$MOCK_ORACLE_URL" \
                    'curl: (22) The requested URL returned error: 500' >&2
                  return 2
                  ;;
              esac
              return 0
            }
            """
        )
        oracle_url = (
            "https://download.documentfoundation.org/libreoffice/stable/26.2.3/"
            "deb/x86_64/LibreOffice_26.2.3_Linux_x86-64_deb.tar.gz"
        )
        for workflow_name, workflow, header, output_path in cases:
            extraction_errors: list[str] = []
            step = self.policy._single_yaml_block(
                Path(f"{workflow_name}.yml"),
                workflow,
                header,
                6,
                "locked oracle image build step",
                extraction_errors,
            )
            self.assertEqual(extraction_errors, [])
            run_marker = "        run: |\n"
            self.assertIn(run_marker, step)
            run_script = textwrap.dedent(step.split(run_marker, 1)[1])
            retry_start = run_script.index("build_oracle_image() {")
            retry_end = run_script.index("python3 - <<'PY'", retry_start)
            retry_script = run_script[retry_start:retry_end]
            retry_script, replacements = re.subn(
                r"(?ms)^build_oracle_image\(\) \{\n.*?^\}\n",
                mock_function,
                retry_script,
                count=1,
            )
            self.assertEqual(replacements, 1)
            retry_script = retry_script.replace(
                'sleep "$retry_delay_seconds"',
                ': "$retry_delay_seconds"',
            )
            for scenario, (expected_status, expected_attempts) in scenarios.items():
                with self.subTest(workflow=workflow_name, scenario=scenario):
                    with tempfile.TemporaryDirectory() as raw:
                        root = Path(raw)
                        (root / output_path).parent.mkdir(parents=True)
                        attempts_path = root / "attempts.txt"
                        environment = os.environ.copy()
                        environment.update(
                            {
                                "MOCK_ATTEMPTS_FILE": str(attempts_path),
                                "MOCK_MODE": scenario,
                                "MOCK_ORACLE_URL": oracle_url,
                            }
                        )
                        result = subprocess.run(
                            ["bash"],
                            input=(
                                f"set -euo pipefail\nmock_attempt=0\n{retry_script}"
                            ),
                            text=True,
                            cwd=root,
                            env=environment,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                            check=False,
                        )
                        self.assertEqual(
                            result.returncode,
                            expected_status,
                            result.stderr,
                        )
                        self.assertEqual(
                            attempts_path.read_text(encoding="utf-8").splitlines(),
                            expected_attempts,
                        )

    def test_oracle_build_jobs_reject_unreviewed_step_surface(self) -> None:
        oracle = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        hardening = RENDER_HARDENING_WORKFLOW.read_text(encoding="utf-8")
        action_sha = "a" * 40
        injected_steps = {
            "sha_pinned_build_push": (
                f"      - uses: docker/build-push-action@{action_sha} # v6.18.0\n"
            ),
            "local_composite": (
                "      - uses: ./.github/actions/unreviewed-oracle-build\n"
            ),
            "injected_remote": (f"      - uses: actions/cache@{action_sha} # v4.3.0\n"),
            "extra_make_step": (
                "      - name: Alternate oracle build\n        run: make oracle-image\n"
            ),
            "download_chmod_execute_step": (
                "      - name: Download alternate build tool\n"
                "        run: |\n"
                "          curl --fail --location --output /tmp/tool "
                "https://example.invalid/tool\n"
                "          chmod +x /tmp/tool\n"
                "          /tmp/tool\n"
            ),
        }
        setup_header = "      - name: Set up the pinned Buildx client\n"
        for workflow_name, original, audit in (
            (
                "render-oracle.yml",
                oracle,
                self.policy.audit_render_oracle_workflow,
            ),
            (
                "render-hardening.yml",
                hardening,
                self.policy.audit_render_hardening_workflow,
            ),
        ):
            self.assertEqual(audit(Path(workflow_name), original), [])
            for case, injected in injected_steps.items():
                with self.subTest(workflow=workflow_name, case=case):
                    mutated = original.replace(
                        setup_header,
                        injected + setup_header,
                        1,
                    )
                    self.assertNotEqual(mutated, original)
                    self.assertTrue(audit(Path(workflow_name), mutated))
            build_invocation = (
                "          python3 scripts/run-render-oracle-container.py build \\\n"
            )
            mutated_build_block = original.replace(
                build_invocation,
                "          echo unreviewed-build-block-mutation\n" + build_invocation,
                1,
            )
            self.assertNotEqual(mutated_build_block, original)
            self.assertTrue(
                audit(Path(workflow_name), mutated_build_block),
                workflow_name,
            )

        reusable_oracle = oracle.replace(
            "    steps:\n",
            "    uses: owner/repository/.github/workflows/build.yml@"
            f"{action_sha} # v1.0.0\n"
            "    steps:\n",
            1,
        )
        self.assertTrue(
            self.policy.audit_render_oracle_workflow(
                Path("render-oracle.yml"), reusable_oracle
            )
        )
        image_start = hardening.index("  oracle-image:\n")
        image_end = hardening.index("  performance:\n", image_start)
        image_job = hardening[image_start:image_end]
        reusable_image_job = image_job.replace(
            "    steps:\n",
            "    uses: owner/repository/.github/workflows/build.yml@"
            f"{action_sha} # v1.0.0\n"
            "    steps:\n",
            1,
        )
        self.assertNotEqual(image_job, reusable_image_job)
        reusable_hardening = (
            hardening[:image_start] + reusable_image_job + hardening[image_end:]
        )
        self.assertTrue(
            self.policy.audit_render_hardening_workflow(
                Path("render-hardening.yml"), reusable_hardening
            )
        )

    def test_oracle_workflows_reject_overlayfs_snapshotter_reintroduction(self) -> None:
        for workflow_path, audit in (
            (RENDER_ORACLE_WORKFLOW, self.policy.audit_render_oracle_workflow),
            (RENDER_HARDENING_WORKFLOW, self.policy.audit_render_hardening_workflow),
        ):
            original = workflow_path.read_text(encoding="utf-8")
            mutated = original.replace(
                "--oci-worker-snapshotter=native",
                "--oci-worker-snapshotter=overlayfs",
                1,
            )
            self.assertNotEqual(mutated, original)
            errors = audit(Path(workflow_path.name), mutated)
            self.assertTrue(
                any("native snapshotting" in error for error in errors),
                (workflow_path.name, errors),
            )

    def test_oracle_workflows_reject_unreviewed_execution_context(self) -> None:
        oracle = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        hardening = RENDER_HARDENING_WORKFLOW.read_text(encoding="utf-8")

        def add_workflow_env(workflow: str, assignment: str) -> str:
            if "\nenv:\n" in workflow:
                return workflow.replace(
                    "\nenv:\n",
                    f"\nenv:\n  {assignment}\n",
                    1,
                )
            return workflow.replace(
                "\njobs:\n",
                f"\nenv:\n  {assignment}\n\njobs:\n",
                1,
            )

        for workflow_name, original, job_name, next_job, audit in (
            (
                "render-oracle.yml",
                oracle,
                "locked-linux-oracle",
                None,
                self.policy.audit_render_oracle_workflow,
            ),
            (
                "render-hardening.yml",
                hardening,
                "oracle-image",
                "performance",
                self.policy.audit_render_hardening_workflow,
            ),
        ):
            job_start = original.index(f"  {job_name}:\n")
            job_end = (
                len(original)
                if next_job is None
                else original.index(f"  {next_job}:\n", job_start)
            )
            job = original[job_start:job_end]
            if "    env:\n" in job:
                job_docker_host = job.replace(
                    "    env:\n",
                    "    env:\n      DOCKER_HOST: unix:///tmp/unreviewed-docker.sock\n",
                    1,
                )
            else:
                job_docker_host = job.replace(
                    "    steps:\n",
                    "    env:\n"
                    "      DOCKER_HOST: unix:///tmp/unreviewed-docker.sock\n"
                    "    steps:\n",
                    1,
                )
            context_mutations = {
                "workflow_docker_host": add_workflow_env(
                    original,
                    "DOCKER_HOST: unix:///tmp/unreviewed-docker.sock",
                ),
                "workflow_bash_env": add_workflow_env(
                    original,
                    "BASH_ENV: /tmp/unreviewed-bash-env",
                ),
                "oracle_job_docker_host": (
                    original[:job_start] + job_docker_host + original[job_end:]
                ),
                "job_default_shell": (
                    original[:job_start]
                    + job.replace(
                        "    steps:\n",
                        "    defaults:\n"
                        "      run:\n"
                        "        shell: bash -c 'source /tmp/tool; exec bash -e {0}'\n"
                        "    steps:\n",
                        1,
                    )
                    + original[job_end:]
                ),
                "privileged_job_container": (
                    original[:job_start]
                    + job.replace(
                        "    steps:\n",
                        "    container:\n"
                        "      image: docker:latest\n"
                        "      options: --privileged\n"
                        "    steps:\n",
                        1,
                    )
                    + original[job_end:]
                ),
                "outside_steps_permissions": original.replace(
                    "  contents: read",
                    "  contents: write",
                    1,
                ),
            }
            self.assertEqual(audit(Path(workflow_name), original), [])
            for case, mutated in context_mutations.items():
                with self.subTest(workflow=workflow_name, case=case):
                    self.assertNotEqual(mutated, original)
                    errors = audit(Path(workflow_name), mutated)
                    self.assertTrue(
                        any("reviewed SHA-256" in error for error in errors),
                        errors,
                    )

    def test_render_oracle_rejects_weakened_full_campaign_contract(self) -> None:
        original = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "case_count": original.replace(
                'FULL_CASE_COUNT: "800"', 'FULL_CASE_COUNT: "799"'
            ),
            "repeat_count": original.replace(
                'FULL_REPEAT_COUNT: "2"', 'FULL_REPEAT_COUNT: "1"'
            ),
            "shard_count": original.replace(
                'FULL_SHARD_COUNT: "4"', 'FULL_SHARD_COUNT: "8"'
            ),
            "parallelism": original.replace(
                'MAX_PARALLEL_SHARDS: "2"', 'MAX_PARALLEL_SHARDS: "4"'
            ),
            "balance": original.replace(
                "(46, 47, 39, 54),",
                "(46, 48, 39, 53),",
            ),
            "timeout": original.replace(
                "&& 330 || 120",
                "&& 360 || 120",
                1,
            ),
            "scheduled_profile": original.replace(
                "(github.event_name == 'workflow_dispatch' || "
                "github.event_name == 'workflow_call') && inputs.campaign || 'pilot'",
                "inputs.campaign",
                1,
            ),
            "head_sha": original.replace(
                'test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"',
                "git rev-parse HEAD",
                1,
            ),
            "pdffonts_identity": original.replace(
                '--pdffonts-binary-sha256 "$PDFFONTS_SHA256"',
                "",
            ),
            "host_tools_closure": original.replace(
                '--host-tools-identity-sha256 "$HOST_TOOLS_IDENTITY_SHA256"',
                "",
            ),
            "persisted_checkout_credentials": original.replace(
                "          persist-credentials: false\n",
                "",
                1,
            ),
            "native_pdf_smoke": original.replace(
                "pdf::tests::project_font_pack_type0_pdf_exposes_exact_poppler_word_tokens",
                "pdf::tests::unreviewed_smoke",
                1,
            ),
            "native_pdf_smoke_poppler_optional": original.replace(
                '          RXLS_REQUIRE_POPPLER: "1"',
                '          RXLS_REQUIRE_POPPLER: "0"',
                1,
            ),
            "native_common_raster": original.replace(
                "          command -v pdftoppm\n",
                "",
                1,
            ),
            "runtime_smoke_removed": original.replace(
                "      - name: Smoke the locked oracle runtime\n",
                "      - name: Unreviewed runtime step\n",
                1,
            ),
            "runtime_smoke_scope": original.replace(
                "env.RXLS_ORACLE_CAMPAIGN == 'pilot'",
                "env.RXLS_ORACLE_CAMPAIGN == 'full'",
                1,
            ),
            "runtime_smoke_fixture": original.replace(
                "--manifest local/render-corpus-generated/pilot/manifest.json",
                "--manifest local/unreviewed/manifest.json",
                1,
            ),
            "merge": original.replace(
                "python3 scripts/merge-render-parity-reports.py",
                "python3 scripts/unverified-merge.py",
            ),
            "absolute_gate": original.replace(
                "python3 scripts/check-render-fidelity-targets.py \\\n",
                "python3 scripts/unchecked-fidelity.py \\\n",
                1,
            ),
            "absolute_gate_diagnostics": original.replace(
                "| tee target/render-oracle-hosted/fidelity-a.json",
                "> target/render-oracle-hosted/fidelity-a.json",
                1,
            ),
            "repeat_gate": original.replace(
                "python3 scripts/compare-render-parity-runs.py",
                "python3 scripts/unchecked-repeat.py",
            ),
            "baseline_gate": original.replace(
                "python3 scripts/check-render-parity-baseline.py",
                "python3 scripts/unchecked-baseline.py",
            ),
            "authored_print_gate": original.replace(
                "python3 scripts/check-authored-print-parity.py",
                "python3 scripts/unchecked-authored-print.py",
            ),
            "authored_print_mode": original.replace(
                "--print-mode authored",
                "--print-mode single-page-sheets",
            ),
            "authored_print_filter": original.replace(
                "--required-feature print-settings",
                "--required-feature formulas",
            ),
            "authored_print_page_contract": original.replace(
                "              == expected_authored_print_pages\n",
                "              == expected_authored_print * 4\n",
                1,
            ),
            "authored_print_cleanup": original.replace(
                "          authored_report_path.unlink()",
                "          pass  # detailed authored report retained",
            ),
            "baseline_scope": original.replace(
                "--require-hosted-full-800",
                "--accept-any-corpus",
            ),
            "baseline_self_approval": original.replace(
                "--require-hosted-full-800 \\\n",
                "--require-hosted-full-800 \\\n                --create \\\n",
            ),
            "gate_status": original.replace(
                'test "$(cat target/render-oracle-hosted/gate-status.txt)" = "0"',
                "true",
            ),
            "corpus_scope": original.replace(
                '"acquired_corpus_included": False',
                '"acquired_corpus_included": True',
            ),
            "unclassified_warning": original.replace(
                'assert warning_policy["unclassified_codes"] == []',
                "pass",
            ),
            "drift_threshold": original.replace(
                "--output target/render-oracle-hosted/repeatability.json \\\n",
                "--output target/render-oracle-hosted/repeatability.json \\\n"
                "              --max-similarity-drift-ppm 1000000 \\\n",
            ),
            "raw_artifact": original.replace(
                "            target/render-oracle-upload/renderer.json\n",
                "            target/render-oracle-upload/renderer.json\n"
                "            target/render-oracle-hosted/parity-report-a.json\n",
            ),
            "raw_authored_artifact": original.replace(
                "            target/render-oracle-upload/authored-print-gate.json\n",
                "            target/render-oracle-upload/authored-print-gate.json\n"
                "            target/render-oracle-hosted/authored-print-report.json\n",
            ),
            "upload_after_failure": original.replace(
                "        if: ${{ success() }}",
                "        if: always()",
            ),
            "unstaged_upload": original.replace(
                "target/render-oracle-upload",
                "target/render-oracle-hosted",
            ),
            "failure_sanitizer_removed": original.replace(
                "python3 scripts/summarize-render-oracle-failure.py",
                "python3 scripts/unreviewed-failure-summary.py",
            ),
            "failure_sanitizer_test_removed": original.replace(
                "          python3 scripts/test_summarize_render_oracle_failure.py\n",
                "",
            ),
            "failure_condition_weakened": original.replace(
                "if: ${{ failure() && env.RXLS_IDENTITY_BOOTSTRAP != '1' }}",
                "if: always()",
            ),
            "failure_input_root_widened": original.replace(
                "--input-root target/render-oracle-hosted",
                "--input-root .",
            ),
            "failure_artifact_unbound": original.replace(
                (
                    "name: render-oracle-failure-${{ github.event_name == "
                    "'workflow_call' && inputs.source_sha || "
                    "github.event.pull_request.head.sha || github.sha }}-"
                    "${{ github.run_id }}-${{ github.run_attempt }}"
                ),
                "name: render-oracle-failure-${{ github.run_id }}",
            ),
            "failure_raw_report_uploaded": original.replace(
                (
                    "path: target/render-oracle-failure/"
                    "render-oracle-failure-summary.json"
                ),
                "path: target/render-oracle-hosted/parity-report-a.json",
            ),
            "failure_upload_before_sanitizer": original.replace(
                "steps.render_oracle_failure_evidence.outcome == 'success'",
                "steps.render_oracle_failure_evidence.outcome != 'cancelled'",
            ),
            "failure_independent_validation_removed": original.replace(
                "          validate_failure_summary(\n",
                "          dict(\n",
                1,
            ),
            "failure_overview_is_blocking": original.replace(
                "        continue-on-error: true\n",
                "",
                1,
            ),
            "failure_overview_precedes_upload": original.replace(
                "- name: Upload sanitized Render Oracle failure summary",
                "- name: TEMPORARY FAILURE STEP",
                1,
            )
            .replace(
                "- name: Append bounded Render Oracle failure overview",
                "- name: Upload sanitized Render Oracle failure summary",
                1,
            )
            .replace(
                "- name: TEMPORARY FAILURE STEP",
                "- name: Append bounded Render Oracle failure overview",
                1,
            ),
            "failure_overview_uploads_full_json": original.replace(
                "          python3 - \"$GITHUB_STEP_SUMMARY\" <<'PY'",
                (
                    "          cat target/render-oracle-failure/"
                    "render-oracle-failure-summary.json "
                    '>> "$GITHUB_STEP_SUMMARY"\n'
                    "          python3 - /dev/null <<'PY'"
                ),
            ),
            "bootstrap_path_substring_allowed": original.replace(
                'assert "path" not in normalized_key',
                'assert not normalized_key.startswith("path")',
                1,
            ),
            "aggregate_path_substring_allowed": original.replace(
                'or "path" not in normalized_key',
                'or not normalized_key.endswith("path")',
                1,
            ),
            "retention_exception_near_match": original.replace(
                '== ("metric_policy", "paths_or_content_retained")',
                '== ("metric_policy", "path_or_content_retained")',
                1,
            ),
            "retention_exception_true_allowed": original.replace(
                "                          and item is False\n",
                "                          and item in (False, True)\n",
                1,
            ),
            "retention_exception_non_bool_allowed": original.replace(
                "                          and item is False\n",
                "                          and item == False\n",
                1,
            ),
            "retention_exception_wrong_artifact": original.replace(
                'aggregate_path.name == "repeatability.json"',
                'aggregate_path.name.endswith(".json")',
                1,
            ),
            "retention_exception_list_alias": original.replace(
                "item, (*key_path, index), allow_retention_policy",
                "item, key_path, allow_retention_policy",
                1,
            ),
            "path_traversal_allowed": original.replace(
                "                  assert traversal.search(value) is None\n",
                "",
            ),
            "relative_artifact_name_allowed": original.replace(
                "                  assert artifact_extension.search(value) is None\n",
                "",
            ),
            "aggregate_extra_key_allowed": original.replace(
                "                  assert set(document) == expected_keys\n",
                "",
                1,
            ),
            "late_clean_removed": original.replace(
                "      - name: Verify evidence source remained exact and clean\n",
                "      - name: Unreviewed late step\n",
                1,
            ),
            "late_untracked_allowed": original.replace(
                '          test -z "$(git status --porcelain=v1 --untracked-files=all)"\n',
                "",
                1,
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertTrue(
                    self.policy.audit_render_oracle_workflow(
                        Path("render-oracle.yml"), workflow
                    )
                )

    def test_render_oracle_rejects_weakened_pinned_type0_pdf_gate(self) -> None:
        original = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "step_removed": original.replace(
                "- name: Run the project-native Type0 PDF Poppler smoke",
                "- name: Unreviewed Type0 PDF smoke",
                1,
            ),
            "manifest_reassigned": original.replace(
                "          RXLS_TEST_FONT_PACK_MANIFEST: "
                "${{ github.workspace }}/local/render-fonts/pack/manifest.json\n",
                "          RXLS_TEST_FONT_PACK_MANIFEST: "
                "${{ github.workspace }}/local/unverified/manifest.json\n",
                1,
            ),
            "workspace_guard_removed": original.replace(
                '          [[ "$RXLS_TEST_FONT_PACK_MANIFEST" = '
                '"$GITHUB_WORKSPACE/"* ]]\n',
                "",
                1,
            ),
            "poppler_optional": original.replace(
                '          RXLS_REQUIRE_POPPLER: "1"\n',
                '          RXLS_REQUIRE_POPPLER: "0"\n',
                1,
            ),
            "raw_descriptor_test_removed": original.replace(
                "embed::tests::pinned_arimo_and_noto_faces_match_libreoffice_descriptor_metrics",
                "embed::tests::unreviewed_descriptor_metrics",
                1,
            ),
            "scaled_descriptor_test_removed": original.replace(
                "pdf::tests::pinned_arimo_and_noto_descriptors_match_libreoffice_pdf_metrics",
                "pdf::tests::unreviewed_descriptor_metrics",
                1,
            ),
            "poppler_box_test_removed": original.replace(
                "pdf::tests::pinned_type0_poppler_boxes_follow_libreoffice_descriptor_metrics",
                "pdf::tests::unreviewed_poppler_boxes",
                1,
            ),
            "discovery_assertion_removed": original.replace(
                "            | grep -Fqx "
                "'pdf::tests::pinned_type0_poppler_boxes_follow_libreoffice_descriptor_metrics: test'\n",
                "            | true\n",
                1,
            ),
            "exact_filter_removed": original.replace(
                "            --lib "
                "pdf::tests::pinned_type0_poppler_boxes_follow_libreoffice_descriptor_metrics \\\n"
                "            -- --exact\n",
                "            --lib "
                "pdf::tests::pinned_type0_poppler_boxes_follow_libreoffice_descriptor_metrics \\\n"
                "            --\n",
                1,
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, original)
                errors = self.policy.audit_render_oracle_workflow(
                    Path("render-oracle.yml"), workflow
                )
                self.assertTrue(
                    any(
                        "pinned Type0 PDF descriptor and Poppler gate" in error
                        for error in errors
                    ),
                    errors,
                )

    def test_render_oracle_rejects_weakened_pinned_font_cli_regression(self) -> None:
        original = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "step_removed": original.replace(
                "- name: Run the pinned-font SinglePageSheets CLI geometry regression",
                "- name: Unreviewed pinned-font regression",
                1,
            ),
            "timeout_widened": original.replace(
                "        timeout-minutes: 15\n",
                "        timeout-minutes: 30\n",
                1,
            ),
            "manifest_reassigned": original.replace(
                "RXLS_TEST_FONT_PACK_MANIFEST: "
                "${{ github.workspace }}/local/render-fonts/pack/manifest.json\n"
                "          RXLS_TEST_FONT_FAMILY: Arimo",
                "RXLS_TEST_FONT_PACK_MANIFEST: "
                "${{ github.workspace }}/local/unverified/manifest.json\n"
                "          RXLS_TEST_FONT_FAMILY: Arimo",
                1,
            ),
            "manifest_made_relative": original.replace(
                "RXLS_TEST_FONT_PACK_MANIFEST: "
                "${{ github.workspace }}/local/render-fonts/pack/manifest.json\n"
                "          RXLS_TEST_FONT_FAMILY: Arimo",
                "RXLS_TEST_FONT_PACK_MANIFEST: "
                "local/render-fonts/pack/manifest.json\n"
                "          RXLS_TEST_FONT_FAMILY: Arimo",
                1,
            ),
            "workspace_guard_removed": original.replace(
                "          RXLS_TEST_FONT_FAMILY: Arimo\n"
                "        run: |\n"
                "          set -euo pipefail\n"
                '          [[ "$RXLS_TEST_FONT_PACK_MANIFEST" = '
                '"$GITHUB_WORKSPACE/"* ]]\n',
                "          RXLS_TEST_FONT_FAMILY: Arimo\n"
                "        run: |\n"
                "          set -euo pipefail\n",
                1,
            ),
            "family_reassigned": original.replace(
                "          RXLS_TEST_FONT_FAMILY: Arimo\n",
                "          RXLS_TEST_FONT_FAMILY: Carlito\n",
                1,
            ),
            "ctl_test_filter_widened": original.replace(
                "            --lib "
                "layout::tests::pinned_calc_ctl_base_face_produces_the_verified_mixed_rtl_row_height \\\n",
                "            --lib layout::tests::pinned_calc_ctl_base_face \\\n",
                1,
            ),
            "test_filter_widened": original.replace(
                "            --test printing "
                "cli_single_page_terminal_drawing_keeps_every_geometry_contract_in_sync \\\n",
                "            --test printing cli_single_page \\\n",
                1,
            ),
            "discovery_assertion_removed": original.replace(
                "            | grep -Fqx "
                "'cli_single_page_terminal_drawing_keeps_every_geometry_contract_in_sync: test'\n",
                "            | true\n",
                1,
            ),
            "exact_filter_removed": original.replace(
                "            -- --exact\n"
                "      - name: Build and inspect the locked oracle image",
                "            --\n"
                "      - name: Build and inspect the locked oracle image",
                1,
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, original)
                errors = self.policy.audit_render_oracle_workflow(
                    Path("render-oracle.yml"), workflow
                )
                self.assertTrue(
                    any(
                        "pinned-font SinglePageSheets CLI regression" in error
                        for error in errors
                    ),
                    errors,
                )

    def test_render_oracle_tracks_shared_gate_dependencies(self) -> None:
        original = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        dependencies = {
            "scripts/render_parity_geometry_gate.py": (
                "shared render-parity geometry gate"
            ),
            "scripts/strict_json_contract.py": ("shared type-exact JSON contract"),
        }
        for dependency, expected_error in dependencies.items():
            trigger = f'      - "{dependency}"\n'
            with self.subTest(dependency=dependency):
                self.assertEqual(original.count(trigger), 1)
                weakened = original.replace(trigger, "", 1)
                errors = self.policy.audit_render_oracle_workflow(
                    Path("render-oracle.yml"), weakened
                )
                self.assertTrue(
                    any(expected_error in error for error in errors),
                    errors,
                )

    def test_render_oracle_rejects_weakened_ooxml_row_diagnostic_contract(
        self,
    ) -> None:
        original = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "case_count": original.replace(
                'OOXML_ROW_DIAGNOSTIC_CASE_COUNT: "34"',
                'OOXML_ROW_DIAGNOSTIC_CASE_COUNT: "33"',
                1,
            ),
            "candidate_mode": original.replace(
                '            test "$RXLS_BASELINE_MODE" = "verify"\n',
                "",
                1,
            ),
            "identity_bootstrap": original.replace(
                '            test "$RXLS_IDENTITY_BOOTSTRAP" = "0"\n',
                "",
                1,
            ),
            "unreviewed_generator": original.replace(
                "python3 scripts/generate-ooxml-row-oracle.py --generate",
                "python3 scripts/unreviewed-row-generator.py --generate",
                1,
            ),
            "manifest_identity": original.replace(
                "088db320a0d35494fa8e0a8c33ba95e12a824cfe1b7163c2071cf70528c5d0a2",
                "0" * 64,
                1,
            ),
            "lane_filter": original.replace(
                "lane_args+=(--format xlsx --required-feature ooxml-implicit-row)",
                "lane_args+=(--format xlsx)",
                1,
            ),
            "unreviewed_reducer": original.replace(
                "python3 scripts/check-ooxml-row-oracle.py \\",
                "python3 scripts/unreviewed-row-reducer.py \\",
                1,
            ),
            "release_minimizer_pollution": original.replace(
                "env.RXLS_ORACLE_CAMPAIGN != 'ooxml-row-diagnostic'",
                "env.RXLS_ORACLE_CAMPAIGN != 'never'",
                1,
            ),
            "raw_report_retained": original.replace(
                "          report_path.unlink()\n",
                "",
                1,
            ),
            "aggregate_revalidation_removed": original.replace(
                '          checker["_validate_output"](aggregate)\n',
                "",
                1,
            ),
            "raw_report_uploaded": original.replace(
                "            target/render-oracle-upload/ooxml-row-oracle.json\n",
                "            target/render-oracle-upload/ooxml-row-oracle.json\n"
                "            target/render-oracle-hosted/parity-report-a.json\n",
                1,
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, original)
                self.assertTrue(
                    self.policy.audit_render_oracle_workflow(
                        Path("render-oracle.yml"), workflow
                    )
                )

    def test_render_oracle_path_guards_reject_substring_keys(self) -> None:
        workflow = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        blocks = []
        cursor = 0
        for terminator in (
            "\n\n          root = pathlib.Path",
            "\n\n          baseline_gate_keys =",
        ):
            start = workflow.index("          def reject_path_bearing_strings", cursor)
            end = workflow.index(terminator, start)
            namespace = {
                "re": re,
                "traversal": re.compile(r"(?:^|[\\/])\.\.(?:$|[\\/])"),
                "artifact_extension": re.compile(
                    r"\.(?:xls|xlsx|xlsb|xlsm|ods|fods|pdf|png|svg)\Z",
                    re.IGNORECASE,
                ),
            }
            exec(textwrap.dedent(workflow[start:end]), namespace)
            blocks.append(namespace["reject_path_bearing_strings"])
            cursor = end

        self.assertEqual(len(blocks), 2)
        for guard in blocks:
            for adversarial_key in (
                "source_path_sha256",
                "host_path_digest",
            ):
                with self.subTest(
                    guard=guard.__code__.co_firstlineno,
                    adversarial_key=adversarial_key,
                ):
                    with self.assertRaises(AssertionError):
                        guard({adversarial_key: 0})

        bootstrap_guard, aggregate_guard = blocks
        approved = {"metric_policy": {"paths_or_content_retained": False}}
        with self.assertRaises(AssertionError):
            bootstrap_guard(approved)
        with self.assertRaises(AssertionError):
            aggregate_guard(approved)
        aggregate_guard(approved, allow_retention_policy=True)
        for near_match in (
            {"metric_policy": {"paths_or_content_retained": True}},
            {"metric_policy": [{"paths_or_content_retained": False}]},
            {"other_policy": {"paths_or_content_retained": False}},
        ):
            with self.subTest(near_match=near_match):
                with self.assertRaises(AssertionError):
                    aggregate_guard(near_match, allow_retention_policy=True)

    def test_render_oracle_pr_campaigns_are_same_repo_label_guarded(self) -> None:
        original = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        pilot_label = "rxls-render-oracle-pilot"
        full_label = "rxls-render-oracle-full"
        head_expression = self.policy.ORACLE_SOURCE_SHA_EXPRESSION
        expected_condition = (
            "${{ github.event_name != 'pull_request' || "
            "(github.event.action == 'labeled' && "
            f"(github.event.label.name == '{pilot_label}' || "
            f"github.event.label.name == '{full_label}') && "
            "github.event.pull_request.head.repo.full_name == github.repository) }}"
        )
        campaign_expression = self.policy.ORACLE_CAMPAIGN_EXPRESSION
        timeout_expression = self.policy.ORACLE_TIMEOUT_EXPRESSION
        bootstrap_expression = self.policy.ORACLE_BOOTSTRAP_EXPRESSION
        baseline_expression = self.policy.ORACLE_BASELINE_MODE_EXPRESSION
        hardened_verifier = (
            "        run: |\n"
            "          set -euo pipefail\n"
            '          test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"\n'
            "          git diff --exit-code\n"
            "          git diff --cached --exit-code\n"
        )

        self.assertIn("  pull_request:\n    types: [labeled]\n", original)
        self.assertIn(f"    if: {expected_condition}\n", original)
        self.assertIn(f"    timeout-minutes: {timeout_expression}\n", original)
        self.assertEqual(original.count(f"ref: {head_expression}"), 1)
        self.assertEqual(original.count(f"EXPECTED_SHA: {head_expression}"), 3)
        self.assertEqual(original.count(f"EXPECTED_SOURCE_SHA: {head_expression}"), 1)
        self.assertEqual(original.count(f"EXPECTED_HEAD_SHA: {head_expression}"), 2)
        self.assertEqual(original.count(hardened_verifier), 2)
        self.assertEqual(
            original.count(f"RXLS_ORACLE_CAMPAIGN: {campaign_expression}"),
            1,
        )
        self.assertEqual(
            original.count(f"RXLS_IDENTITY_BOOTSTRAP: {bootstrap_expression}"),
            1,
        )
        self.assertIn(
            "name: render-oracle-"
            f"{head_expression}-"
            "${{ github.run_id }}-${{ github.run_attempt }}-"
            f"{campaign_expression}-"
            f"{baseline_expression}",
            original,
        )

        mutations = {
            "unfiltered_pr_events": original.replace(
                "    types: [labeled]",
                "    types: [opened, synchronize, labeled]",
                1,
            ),
            "broad_label": original.replace(
                f"github.event.label.name == '{full_label}'",
                "github.event.label.name != ''",
                1,
            ),
            "pilot_label_removed": original.replace(
                f"github.event.label.name == '{pilot_label}'",
                "false",
                1,
            ),
            "fork_allowed": original.replace(
                " && github.event.pull_request.head.repo.full_name == github.repository",
                "",
                1,
            ),
            "merge_checkout": original.replace(
                f"          ref: {head_expression}\n",
                "          ref: ${{ github.sha }}\n",
                1,
            ),
            "merge_build_identity": original.replace(
                f"          EXPECTED_SOURCE_SHA: {head_expression}\n",
                "          EXPECTED_SOURCE_SHA: ${{ github.sha }}\n",
                1,
            ),
            "merge_summary_identity": original.replace(
                f"          EXPECTED_HEAD_SHA: {head_expression}\n",
                "          EXPECTED_HEAD_SHA: ${{ github.sha }}\n",
                1,
            ),
            "full_selects_pilot": original.replace(
                f"github.event.label.name == '{full_label}' && 'full'",
                f"github.event.label.name == '{full_label}' && 'pilot'",
                1,
            ),
            "pilot_selects_full": original.replace(
                f"github.event.label.name == '{pilot_label}' && 'pilot'",
                f"github.event.label.name == '{pilot_label}' && 'full'",
                1,
            ),
            "pilot_gets_full_timeout": original.replace(
                "&& 330 || 120",
                "&& 330 || 330",
                1,
            ),
            "pr_bootstrap": original.replace(
                f"RXLS_IDENTITY_BOOTSTRAP: {bootstrap_expression}",
                (
                    "RXLS_IDENTITY_BOOTSTRAP: "
                    "${{ github.event_name == 'pull_request' && '1' || '0' }}"
                ),
                1,
            ),
            "non_strict_verifier": original.replace(
                "          set -euo pipefail\n",
                "",
                1,
            ),
            "dirty_worktree_allowed": original.replace(
                "          git diff --exit-code\n",
                "",
                1,
            ),
            "dirty_index_allowed": original.replace(
                "          git diff --cached --exit-code\n",
                "",
                1,
            ),
            "unbound_artifact": original.replace(
                f"name: render-oracle-{head_expression}-",
                "name: render-oracle-${{ github.sha }}-",
                1,
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, original)
                self.assertTrue(
                    self.policy.audit_render_oracle_workflow(
                        Path("render-oracle.yml"), workflow
                    )
                )

    def test_render_oracle_campaign_artifacts_are_aggregate_only(self) -> None:
        text = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn('--profile "$RXLS_ORACLE_CAMPAIGN"', text)
        self.assertIn("run_full_campaign a", text)
        self.assertIn("run_full_campaign b", text)
        self.assertIn("scripts/merge-render-parity-reports.py", text)
        self.assertIn("scripts/compare-render-parity-runs.py", text)
        self.assertIn("scripts/check-render-parity-baseline.py", text)
        self.assertIn("scripts/check-authored-print-parity.py", text)
        self.assertIn("--print-mode authored", text)
        self.assertIn("--required-feature print-settings", text)
        self.assertIn(
            '"pages_per_workbook_by_scale_mode"\n          ] == {"fit": 1, "scale": 4}',
            text,
        )
        self.assertIn("--require-hosted-full-800", text)
        self.assertIn('"acquired_corpus_included": False', text)
        self.assertIn("generate-ooxml-row-oracle.py --generate", text)
        self.assertIn("--required-feature ooxml-implicit-row", text)
        self.assertIn("scripts/check-ooxml-row-oracle.py", text)
        self.assertIn(
            "target/render-oracle-upload/ooxml-row-oracle.json",
            text,
        )
        self.assertIn("report_path.unlink()", text)
        self.assertNotIn(
            "            target/render-oracle-hosted/parity-report-a.json\n",
            text,
        )
        self.assertNotIn(
            "            target/render-oracle-hosted/authored-print-report.json\n",
            text,
        )
        self.assertNotIn("            local/render-corpus-generated", text)
        self.assertIn(
            "python3 scripts/summarize-render-oracle-failure.py",
            text,
        )
        self.assertIn(
            "python3 scripts/test_summarize_render_oracle_failure.py",
            text,
        )
        self.assertIn(
            "test_failure_summary_validator_is_bound_private_and_fail_closed",
            text,
        )
        self.assertIn(
            "### Sanitized Render Oracle failure evidence",
            text,
        )
        self.assertNotIn(
            "cat target/render-oracle-failure/render-oracle-failure-summary.json",
            text,
        )
        self.assertIn(
            "path: target/render-oracle-failure/render-oracle-failure-summary.json",
            text,
        )
        self.assertIn(
            "steps.render_oracle_failure_evidence.outcome == 'success'",
            text,
        )
        self.assertIn("validate_failure_summary(", text)
        self.assertIn("id: render_oracle_failure_upload", text)
        self.assertIn(
            "steps.render_oracle_failure_upload.outcome == 'success'",
            text,
        )
        self.assertIn("continue-on-error: true", text)
        self.assertLess(
            text.index("- name: Upload sanitized Render Oracle failure summary"),
            text.index("- name: Append bounded Render Oracle failure overview"),
        )
        self.assertIn(
            '== ("metric_policy", "paths_or_content_retained")',
            text,
        )
        self.assertIn("and item is False", text)
        self.assertIn(
            'aggregate_path.name == "repeatability.json"',
            text,
        )
        self.assertNotIn(
            "path: target/render-oracle-hosted/parity-report-a.json",
            text,
        )

    def test_render_hardening_requires_poppler_semantics_before_printing(
        self,
    ) -> None:
        original = RENDER_HARDENING_WORKFLOW.read_text(encoding="utf-8")
        semantic_selector = (
            "pdf::tests::clipped_ods_paragraph_group_retains_full_semantics_"
            "without_changing_paint"
        )
        semantic_gate = (
            "          cargo test --locked --manifest-path render/Cargo.toml \\\n"
            f"            --lib {semantic_selector} \\\n"
            "            -- --exact --list \\\n"
            f"            | grep -Fqx '{semantic_selector}: test'\n"
            "          cargo test --locked --manifest-path render/Cargo.toml \\\n"
            f"            --lib {semantic_selector} \\\n"
            "            -- --exact\n"
        )
        printing_selector = (
            "deterministic_pdf_reopens_has_exact_page_count_and_extractable_text"
        )
        printing_gate = (
            "          cargo test --locked --manifest-path render/Cargo.toml \\\n"
            f"            --test printing {printing_selector} \\\n"
            "            -- --exact --list \\\n"
            f"            | grep -Fqx '{printing_selector}: test'\n"
            "          cargo test --locked --manifest-path render/Cargo.toml \\\n"
            f"            --test printing {printing_selector} \\\n"
            "            -- --exact\n"
        )
        focused_error = "pinned Poppler exact-test gate"
        mutations = {
            "poppler_optional": (
                original.replace(
                    '      RXLS_REQUIRE_POPPLER: "1"\n',
                    '      RXLS_REQUIRE_POPPLER: "0"\n',
                    1,
                ),
                "must fail closed on the pinned Poppler tools",
            ),
            "semantic_selector_deleted": (
                original.replace(semantic_selector, "unreviewed_semantic_test"),
                focused_error,
            ),
            "semantic_discovery_relaxed": (
                original.replace(
                    f"            | grep -Fqx '{semantic_selector}: test'\n",
                    "            | true\n",
                    1,
                ),
                focused_error,
            ),
            "semantic_execution_relaxed": (
                original.replace(
                    f"            --lib {semantic_selector} \\\n"
                    "            -- --exact\n",
                    f"            --lib {semantic_selector} \\\n"
                    "            --\n",
                    1,
                ),
                focused_error,
            ),
            "semantic_runs_after_printing": (
                original.replace(
                    semantic_gate + printing_gate,
                    printing_gate + semantic_gate,
                    1,
                ),
                focused_error,
            ),
            "printing_selector_deleted": (
                original.replace(printing_selector, "unreviewed_printing_test"),
                focused_error,
            ),
        }
        for name, (workflow, expected_error) in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, original)
                errors = self.policy.audit_render_hardening_workflow(
                    Path("render-hardening.yml"), workflow
                )
                self.assertTrue(
                    any(expected_error in error for error in errors),
                    errors,
                )

    def test_render_hardening_rejects_mutable_apt_and_path_bearing_evidence(
        self,
    ) -> None:
        original = RENDER_HARDENING_WORKFLOW.read_text(encoding="utf-8")
        mutations = (
            original.replace(
                '-o "Dir::Etc::sourceparts=-"',
                '-o "Dir::Etc::sourceparts=/etc/apt/sources.list.d"',
                1,
            ),
            original.replace(
                "python3 scripts/render-oracle-host-tools.py apt-sources \\",
                "printf '%s\\n' 'deb https://archive.ubuntu.com/ubuntu noble main' \\",
                1,
            ),
            original.replace(
                'sudo apt-get "${APT_OPTIONS[@]}" update',
                "sudo apt-get update",
                1,
            ),
            original.replace("--scope poppler", "--scope all"),
            original.replace("poppler-identity.json", "poppler-version.txt"),
            original.replace(
                'if [[ "$EXPECTED_IDENTITY" != "null" ]]; then',
                'if [[ "$EXPECTED_IDENTITY" == "null" ]]; then',
            ),
            original.replace("            --require-hashes \\\n", "", 1),
            original.replace(
                '          echo "Review and pin the uploaded host identity before this gate can pass." >&2\n'
                "          exit 1\n",
                '          echo "bootstrap accepted"\n',
            ),
            original.replace(
                "              raise SystemExit(1)\n",
                '              print("bootstrap accepted")\n',
            ),
            original.replace(
                '          assert evidence["image_identity_status"] == "pinned_match", evidence\n',
                '          assert evidence["image_identity_status"] != "mismatch", evidence\n',
            ),
            original.replace(
                "            | grep -Fqx "
                "'deterministic_pdf_reopens_has_exact_page_count_and_extractable_text: test'\n",
                "            | true\n",
                1,
            ),
        )
        for workflow in mutations:
            with self.subTest(workflow=workflow):
                errors = self.policy.audit_render_hardening_workflow(
                    Path("render-hardening.yml"), workflow
                )
                self.assertTrue(errors)

    def test_render_hardening_runs_policy_mutation_suite_fail_closed(self) -> None:
        original = RENDER_HARDENING_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "trigger_removed": (
                original.replace(
                    '      - "scripts/test_workflow_policy.py"\n',
                    "",
                    1,
                ),
                "pull requests must trigger hardening",
            ),
            "checker_removed": (
                original.replace(
                    "          python3 scripts/check_workflow_policy.py\n",
                    "",
                    1,
                ),
                "focused mutation suite",
            ),
            "mutation_suite_removed": (
                original.replace(
                    "          python3 scripts/test_workflow_policy.py\n",
                    "",
                    1,
                ),
                "focused mutation suite",
            ),
            "shell_weakened": (
                original.replace(
                    "          set -euo pipefail\n"
                    "          python3 scripts/check_workflow_policy.py\n"
                    "          python3 scripts/test_workflow_policy.py\n",
                    "          set +e\n"
                    "          python3 scripts/check_workflow_policy.py\n"
                    "          python3 scripts/test_workflow_policy.py\n",
                    1,
                ),
                "focused mutation suite",
            ),
        }
        for name, (workflow, expected_error) in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, original)
                errors = self.policy.audit_render_hardening_workflow(
                    Path("render-hardening.yml"), workflow
                )
                self.assertTrue(
                    any(expected_error in error for error in errors),
                    errors,
                )

    def test_render_hardening_rejects_unscoped_or_commented_oci_guards(self) -> None:
        original = RENDER_HARDENING_WORKFLOW.read_text(encoding="utf-8")
        image_start = original.index("  oracle-image:\n")
        image_end = original.index("  performance:\n", image_start)

        def mutate_image(old: str, new: str) -> str:
            image_job = original[image_start:image_end]
            mutated_job = image_job.replace(old, new, 1)
            self.assertNotEqual(image_job, mutated_job)
            return original[:image_start] + mutated_job + original[image_end:]

        mutations = {
            "container_trigger_commented": original.replace(
                '      - "scripts/render-oracle-container/**"',
                '      # - "scripts/render-oracle-container/**"',
                1,
            ),
            "runner_trigger_commented": original.replace(
                '      - "scripts/run-render-oracle-container.py"',
                '      # - "scripts/run-render-oracle-container.py"',
                1,
            ),
            "container_test_trigger_commented": original.replace(
                '      - "scripts/test_render_oracle_container.py"',
                '      # - "scripts/test_render_oracle_container.py"',
                1,
            ),
            "oci_runner": mutate_image(
                "    runs-on: ubuntu-24.04", "    runs-on: ubuntu-latest"
            ),
            "oci_policy_step": mutate_image(
                "        run: python3 scripts/check_workflow_policy.py",
                "        run: true",
            ),
            "oci_buildx_version": mutate_image(
                "          version: v0.35.0",
                "          version: latest",
            ),
            "oci_buildkit_image": mutate_image(
                "moby/buildkit:v0.31.2@sha256:",
                "moby/buildkit:latest@sha256:",
            ),
            "oci_build_step_scope": mutate_image(
                "      - name: Build and verify the locked oracle image",
                "      - name: Describe the locked oracle image",
            ),
            "oci_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          docker buildx build .\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_env_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          env docker buildx build .\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_command_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          command docker build .\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_sudo_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          sudo -u root docker buildx build .\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_assignment_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          DOCKER_BUILDKIT=1 docker build .\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_bake_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          docker buildx bake .\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_shell_c_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          bash -c 'docker build .'\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_eval_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          eval 'docker buildx build .'\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_static_variable_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          DOCKER_COMMAND=docker\n"
                "          DOCKER_SUBCOMMAND=build\n"
                '          "$DOCKER_COMMAND" "$DOCKER_SUBCOMMAND" .\n'
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_backtick_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          IMAGE_ID=`docker build .`\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_python_inline_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          python3 -c 'import subprocess; "
                'subprocess.run(["docker", "build", "."])\'\n'
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_perl_inline_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          perl -e 'system(\"docker build .\")'\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "oci_sh_c_direct_build_bypass": mutate_image(
                "          python3 scripts/run-render-oracle-container.py build \\\n",
                "          sh -c 'docker build .'\n"
                "          python3 scripts/run-render-oracle-container.py build \\\n",
            ),
            "bootstrap_argument_commented": mutate_image(
                "            BOOTSTRAP_ARGS+=(--bootstrap-identities)",
                "            # BOOTSTRAP_ARGS+=(--bootstrap-identities)",
            ),
            "verify_bootstrap_use_commented": mutate_image(
                '            "${BOOTSTRAP_ARGS[@]}"',
                '            # "${BOOTSTRAP_ARGS[@]}"',
            ),
            "bootstrap_status_commented": mutate_image(
                '              assert evidence["image_identity_status"] == "bootstrap_capture_required", evidence',
                '              # assert evidence["image_identity_status"] == "bootstrap_capture_required", evidence',
            ),
            "reproducible_config_ids_commented": mutate_image(
                '          assert reproducibility["config_ids"] == [evidence["built_image_id"]] * 2, reproducibility',
                '          # assert reproducibility["config_ids"] == [evidence["built_image_id"]] * 2, reproducibility',
            ),
            "reproducible_manifest_digests_commented": mutate_image(
                '          assert reproducibility["manifest_digests"] == [evidence["built_manifest_digest"]] * 2, reproducibility',
                '          # assert reproducibility["manifest_digests"] == [evidence["built_manifest_digest"]] * 2, reproducibility',
            ),
            "reproducible_identities_commented": mutate_image(
                '          assert len(reproducibility["identities"]) == 2, reproducibility',
                '          # assert len(reproducibility["identities"]) == 2, reproducibility',
            ),
            "bootstrap_identity_commented": mutate_image(
                '              assert evidence["expected_image_id"] is None, evidence',
                '              # assert evidence["expected_image_id"] is None, evidence',
            ),
            "bootstrap_failure_commented": mutate_image(
                "              raise SystemExit(1)",
                "              # raise SystemExit(1)",
            ),
            "pinned_status_commented": mutate_image(
                '          assert evidence["image_identity_status"] == "pinned_match", evidence',
                '          # assert evidence["image_identity_status"] == "pinned_match", evidence',
            ),
            "pinned_identity_commented": mutate_image(
                '          assert evidence["expected_image_id"] == expected == evidence["built_image_id"], evidence',
                '          # assert evidence["expected_image_id"] == expected == evidence["built_image_id"], evidence',
            ),
            "source_commit_commented": mutate_image(
                '          assert evidence["source_commit"] == expected_source, evidence',
                '          # assert evidence["source_commit"] == expected_source, evidence',
            ),
            "wrapper_identity_commented": mutate_image(
                '          assert evidence["wrapper_sha256"] == live_wrapper_sha256 == lock["wrapper"]["sha256"], evidence',
                '          # assert evidence["wrapper_sha256"] == live_wrapper_sha256 == lock["wrapper"]["sha256"], evidence',
            ),
            "receipt_artifact_name_unbound": mutate_image(
                "          name: render-oracle-image-${{ github.event.pull_request.head.sha || github.sha }}-${{ github.run_id }}-${{ github.run_attempt }}",
                "          name: render-oracle-image-${{ github.run_id }}-${{ github.run_attempt }}",
            ),
            "host_bootstrap_argument_commented": original.replace(
                "            --bootstrap-identities \\\n",
                "            # --bootstrap-identities \\\n",
                1,
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, original)
                errors = self.policy.audit_render_hardening_workflow(
                    Path("render-hardening.yml"), workflow
                )
                self.assertTrue(errors)

    def test_direct_docker_build_detection_ignores_comments_and_echoes(self) -> None:
        inactive = """
steps:
  - run: |
      # env docker buildx build .
      true  # sudo docker build .
      echo "command docker buildx build ."
      printf '%s\\n' 'DOCKER_BUILDKIT=1 docker build .'
      command -v docker build
      cat <<'EOF'
      docker build .
      EOF
"""
        self.assertEqual(self.policy._direct_docker_build_commands(inactive), [])

        workflows = (
            "steps:\n  - run: env docker buildx build .\n",
            "steps:\n  - run: /usr/bin/env PINNED=1 command docker build .\n",
            "steps:\n  - run: sudo --preserve-env -u root docker buildx build .\n",
            "steps:\n  - run: DOCKER_BUILDKIT=1 docker build .\n",
            "steps:\n  - run: 'docker build .'\n",
            "steps:\n  - run: env -S 'docker build .'\n",
            "steps:\n  - run: docker --context default build .\n",
            "steps:\n  - run: docker buildx bake .\n",
            'steps:\n  - run: docker buildx "$SUBCOMMAND" .\n',
            'steps:\n  - run: docker buildx "${SUBCOMMAND:-build}" .\n',
            "steps:\n  - run: bash -c 'docker build .'\n",
            "steps:\n  - run: bash /tmp/generated-build-script.sh\n",
            'steps:\n  - run: sh -c "$BUILD_COMMAND"\n',
            'steps:\n  - run: eval "$BUILD_COMMAND"\n',
            "steps:\n  - run: IMAGE_ID=`docker build .`\n",
            "steps:\n  - run: echo `docker buildx build .`\n",
            (
                "steps:\n  - run: python3 -c 'import subprocess; "
                'subprocess.run(["docker", "build", "."])\'\n'
            ),
            "steps:\n  - run: perl -e 'system(\"docker build .\")'\n",
            "steps:\n  - run: find . -exec docker build {} ;\n",
            "steps:\n  - run: timeout 30 docker build .\n",
            "steps:\n  - run: . /tmp/generated-build-script.sh\n",
            'steps:\n  - run: "$UNKNOWN_COMMAND" .\n',
            "steps:\n  - run: 'docker build .\n",
            'steps:\n  - run: |\n      DOCKER=docker\n      SUBCOMMAND=build\n      "$DOCKER" "$SUBCOMMAND" .\n',
            "steps:\n  - run: |\n      COMMAND='docker buildx bake'\n      $COMMAND .\n",
            "steps:\n  - run: |\n      COMMAND=dock\n      COMMAND+=er\n      $COMMAND build .\n",
            "steps:\n  - run: >-\n      docker buildx\n      build .\n",
            "steps: [{run: docker build .}]\n",
            "steps:\n  - {run: docker build .}\n",
            "steps:\n  - <<: *docker-build-step\n",
            "steps:\n  - run: ${{ format('{0} {1}', 'docker', 'build') }}\n",
        )
        for workflow in workflows:
            with self.subTest(workflow=workflow):
                self.assertTrue(self.policy._direct_docker_build_commands(workflow))

        safe = """
steps:
  - run: |
      bash -n scripts/render-oracle-container/oracle-entrypoint.sh
      python3 scripts/run-render-oracle-container.py build --engine docker
      docker version
      docker buildx version
      printf '%s\\n' 'bash -c "docker build ."'
"""
        self.assertEqual(self.policy._direct_docker_build_commands(safe), [])

    def test_checked_in_render_browser_policy_passes(self) -> None:
        text = RENDER_BROWSER_WORKFLOW.read_text(encoding="utf-8")

        self.assertEqual(
            self.policy.audit_render_browser_workflow(Path("render-browser.yml"), text),
            [],
        )
        self.assertEqual(text.count("rxls-render-worker-0.1.3.tgz"), 2)
        self.assertNotIn("rxls-render-worker-0.1.2.tgz", text)

    def test_render_browser_rejects_mutable_or_commented_wasm_build_tools(self) -> None:
        original = RENDER_BROWSER_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "main_push_path_filter": original.replace(
                "  push:\n    branches: [main]\n",
                "  push:\n    branches: [main]\n    paths:\n      - 'src/**'\n",
                1,
            ),
            "reviewed_baseline_trigger": original.replace(
                '      - "scripts/render-parity-baseline-full.json"\n',
                "",
            ),
            "runner": original.replace(
                "    runs-on: ubuntu-24.04",
                "    runs-on: ubuntu-latest",
                1,
            ),
            "persisted_checkout_credentials": original.replace(
                "          persist-credentials: false\n",
                "",
                1,
            ),
            "build_rust": original.replace(
                'WASM_BINDGEN_BUILD_RUST: "1.88.0"',
                'WASM_BINDGEN_BUILD_RUST: "1.88"',
            ),
            "metadata": original.replace(
                "l.wasmBindgen.buildRust !== process.env.WASM_BINDGEN_BUILD_RUST || ",
                "",
            ),
            "step_scope": original.replace(
                "      - name: Install exact wasm-bindgen CLI",
                "      - name: Install mutable wasm-bindgen CLI",
            ),
            "rustup_toolchain": original.replace(
                'rustup toolchain install "$WASM_BINDGEN_BUILD_RUST" --profile minimal',
                'rustup toolchain install "$RENDER_MSRV" --profile minimal',
            ),
            "runner_temp_guard": original.replace(
                '          test -n "$RUNNER_TEMP"',
                "          true",
            ),
            "cached_tool_root": original.replace(
                'tool_root="$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION"',
                'tool_root="$CARGO_HOME"',
            ),
            "cached_tool_root_after_pin": original.replace(
                '          tool_root="$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION"',
                '          tool_root="$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION"\n'
                '          tool_root="$CARGO_HOME"',
            ),
            "missing_fresh_root_cleanup": original.replace(
                '          rm -rf "$tool_root"',
                "          true",
            ),
            "cargo_toolchain": original.replace(
                'cargo "+$WASM_BINDGEN_BUILD_RUST" install \\\n',
                'cargo "+$RENDER_MSRV" install \\\n',
            ),
            "cargo_unqualified": original.replace(
                'cargo "+$WASM_BINDGEN_BUILD_RUST" install \\\n',
                "cargo install \\\n",
            ),
            "cargo_commented": original.replace(
                '          cargo "+$WASM_BINDGEN_BUILD_RUST" install \\\n',
                '          # cargo "+$WASM_BINDGEN_BUILD_RUST" install \\\n',
            ),
            "cargo_default_root": original.replace(
                '            wasm-bindgen-cli --version "$WASM_BINDGEN_VERSION" --locked \\\n'
                '            --root "$tool_root"',
                '            wasm-bindgen-cli --version "$WASM_BINDGEN_VERSION" --locked',
            ),
            "cargo_force": original.replace(
                '            --root "$tool_root"',
                '            --root "$tool_root" --force',
            ),
            "github_path_missing": original.replace(
                '          echo "$tool_root/bin" >> "$GITHUB_PATH"',
                '          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"',
            ),
            "github_path_cached_alternative": original.replace(
                '          echo "$tool_root/bin" >> "$GITHUB_PATH"',
                '          echo "$tool_root/bin" >> "$GITHUB_PATH"\n'
                '          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"',
            ),
            "isolated_path_export_missing": original.replace(
                '          export PATH="$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION/bin:$PATH"',
                '          export PATH="$CARGO_HOME/bin:$PATH"',
            ),
            "isolated_resolution_missing": original.replace(
                '          test "$(command -v wasm-bindgen)" = \\\n'
                '            "$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION/bin/wasm-bindgen"',
                "          command -v wasm-bindgen",
            ),
            "cached_path_after_verification": original.replace(
                '            "$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION/bin/wasm-bindgen"\n'
                "          npm run build:wasm",
                '            "$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION/bin/wasm-bindgen"\n'
                '          export PATH="$CARGO_HOME/bin:$PATH"\n'
                "          npm run build:wasm",
            ),
            "installed_browser_websocket_flag": original.replace(
                "            node --experimental-websocket \\\n"
                '            "$GITHUB_WORKSPACE/bindings/render-wasm/tests/browser/run.mjs"',
                '            node "$GITHUB_WORKSPACE/bindings/render-wasm/tests/browser/run.mjs"',
            ),
            "direct_browser_pipefail": original.replace(
                "      - name: Exercise worker under strict CSP in pinned Chromium\n"
                "        working-directory: bindings/render-wasm\n"
                "        shell: bash\n"
                "        run: |\n"
                "          set -euo pipefail\n",
                "      - name: Exercise worker under strict CSP in pinned Chromium\n"
                "        working-directory: bindings/render-wasm\n"
                "        shell: bash\n"
                "        run: |\n",
            ),
            "installed_browser_pipefail": original.replace(
                "      - name: Pack and consume the publishable artifact\n"
                "        working-directory: bindings/render-wasm\n"
                "        shell: bash\n"
                "        run: |\n"
                "          set -euo pipefail\n",
                "      - name: Pack and consume the publishable artifact\n"
                "        working-directory: bindings/render-wasm\n"
                "        shell: bash\n"
                "        run: |\n",
            ),
            "browser_pipeline_shell": original.replace(
                "      - name: Exercise worker under strict CSP in pinned Chromium\n"
                "        working-directory: bindings/render-wasm\n"
                "        shell: bash\n",
                "      - name: Exercise worker under strict CSP in pinned Chromium\n"
                "        working-directory: bindings/render-wasm\n"
                "        shell: sh\n",
            ),
            "sandbox_owner": original.replace(
                '          sudo chown root:root "$chrome_root/chrome_sandbox"\n',
                "",
                1,
            ),
            "sandbox_mode": original.replace(
                '          sudo chmod 4755 "$chrome_root/chrome_sandbox"\n',
                '          sudo chmod 0755 "$chrome_root/chrome_sandbox"\n',
                1,
            ),
            "sandbox_owner_check": original.replace(
                '          test "$(stat --format=%u "$chrome_root/chrome_sandbox")" = "0"\n',
                "",
                1,
            ),
            "sandbox_mode_check": original.replace(
                '          test "$(stat --format=%a "$chrome_root/chrome_sandbox")" = "4755"\n',
                "",
                1,
            ),
            "sandbox_export": original.replace(
                '          echo "CHROME_DEVEL_SANDBOX=$GITHUB_WORKSPACE/target/render-chrome/chrome-linux64/chrome_sandbox" >> "$GITHUB_ENV"\n',
                "",
                1,
            ),
            "runtime_ldd": original.replace(
                '          ldd "$chrome_root/chrome" | tee "$RUNNER_TEMP/rxls-chromium-ldd.txt"\n',
                "",
                1,
            ),
            "runtime_not_found": original.replace(
                '          if grep -Fq "not found" "$RUNNER_TEMP/rxls-chromium-ldd.txt"; then\n'
                "            exit 1\n"
                "          fi\n",
                "",
                1,
            ),
            "runtime_pass_artifact": original.replace(
                "          printf '%s\\n' \"PASS pinned Chromium runtime closure resolved\" \\\n"
                "            > target/render-browser-evidence/chromium-runtime.txt\n",
                "",
                1,
            ),
            "source_browser_stderr": original.replace(
                "          npm run test:browser 2>&1 | tee ",
                "          npm run test:browser | tee ",
                1,
            ),
            "installed_browser_stderr": original.replace(
                "            2>&1 | tee ../render-browser-evidence/installed-package-chromium.log",
                "            | tee ../render-browser-evidence/installed-package-chromium.log",
                1,
            ),
            "late_expected_head": original.replace(
                "      - name: Verify evidence source remained exact and clean\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.event.pull_request.head.sha || github.sha }}\n",
                "      - name: Verify evidence source remained exact and clean\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.sha }}\n",
                1,
            ),
            "late_head_check": original.replace(
                "      - name: Verify evidence source remained exact and clean\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.event.pull_request.head.sha || github.sha }}\n"
                "        run: |\n"
                "          set -euo pipefail\n"
                '          test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"\n',
                "      - name: Verify evidence source remained exact and clean\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.event.pull_request.head.sha || github.sha }}\n"
                "        run: |\n"
                "          set -euo pipefail\n",
                1,
            ),
            "late_unstaged_check": original.replace(
                "      - name: Verify evidence source remained exact and clean\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.event.pull_request.head.sha || github.sha }}\n"
                "        run: |\n"
                "          set -euo pipefail\n"
                '          test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"\n'
                "          git diff --exit-code\n",
                "      - name: Verify evidence source remained exact and clean\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.event.pull_request.head.sha || github.sha }}\n"
                "        run: |\n"
                "          set -euo pipefail\n"
                '          test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"\n',
                1,
            ),
            "late_staged_check": original.replace(
                "          git diff --cached --exit-code\n"
                "      - name: Upload browser-rendering evidence\n",
                "      - name: Upload browser-rendering evidence\n",
                1,
            ),
            "summary_extra_field": original.replace(
                "      - name: Build path-neutral exact-SHA browser evidence\n"
                "        shell: bash\n",
                "      - name: Build path-neutral exact-SHA browser evidence\n"
                "        continue-on-error: true\n"
                "        shell: bash\n",
                1,
            ),
            "summary_expected_head": original.replace(
                "      - name: Build path-neutral exact-SHA browser evidence\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.event.pull_request.head.sha || github.sha }}\n",
                "      - name: Build path-neutral exact-SHA browser evidence\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.sha }}\n",
                1,
            ),
            "summary_strict_shell": original.replace(
                "      - name: Build path-neutral exact-SHA browser evidence\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.event.pull_request.head.sha || github.sha }}\n"
                "        run: |\n"
                "          set -euo pipefail\n",
                "      - name: Build path-neutral exact-SHA browser evidence\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.event.pull_request.head.sha || github.sha }}\n"
                "        run: |\n",
                1,
            ),
            "summary_head_check": original.replace(
                "      - name: Build path-neutral exact-SHA browser evidence\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.event.pull_request.head.sha || github.sha }}\n"
                "        run: |\n"
                "          set -euo pipefail\n"
                '          test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"\n',
                "      - name: Build path-neutral exact-SHA browser evidence\n"
                "        shell: bash\n"
                "        env:\n"
                "          EXPECTED_SHA: ${{ github.event.pull_request.head.sha || github.sha }}\n"
                "        run: |\n"
                "          set -euo pipefail\n",
                1,
            ),
            "summary_verifier": original.replace(
                "          python3 scripts/check_render_browser_release_evidence.py verify \\\n",
                "          python3 scripts/check_render_browser_release_evidence.py build \\\n",
                1,
            ),
            "summary_run_attempt_binding": original.replace(
                '            --workflow-run-attempt "$GITHUB_RUN_ATTEMPT" \\\n',
                '            --workflow-run-attempt "1" \\\n',
                1,
            ),
            "summary_source_adjacency": original.replace(
                "          git diff --cached --exit-code\n"
                "      - name: Build path-neutral exact-SHA browser evidence\n",
                "          git diff --cached --exit-code\n"
                "      - name: Unexpected source-summary interstitial\n"
                "        run: true\n"
                "      - name: Build path-neutral exact-SHA browser evidence\n",
                1,
            ),
            "summary_upload_adjacency": original.replace(
                "          git diff --cached --exit-code\n"
                "      - name: Upload browser-rendering evidence\n",
                "          git diff --cached --exit-code\n"
                "      - name: Unexpected summary-upload interstitial\n"
                "        run: true\n"
                "      - name: Upload browser-rendering evidence\n",
                1,
            ),
            "summary_upload_success": original.replace(
                "      - name: Upload browser-rendering evidence\n"
                "        if: ${{ success() }}\n",
                "      - name: Upload browser-rendering evidence\n"
                "        if: ${{ always() }}\n",
                1,
            ),
            "summary_upload_name": original.replace(
                "          name: render-browser-${{ github.event.pull_request.head.sha || github.sha }}-${{ github.run_id }}-${{ github.run_attempt }}\n",
                "          name: render-browser-${{ github.sha }}\n",
                1,
            ),
            "summary_upload_path": original.replace(
                "          path: target/render-browser-evidence/browser-summary.json\n",
                "          path: target/render-browser-evidence/\n",
                1,
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, original)
                self.assertTrue(
                    self.policy.audit_render_browser_workflow(
                        Path("render-browser.yml"), workflow
                    )
                )

    def test_checked_in_render_package_release_policy_passes(self) -> None:
        text = RENDER_PACKAGE_RELEASE_WORKFLOW.read_text(encoding="utf-8")

        self.assertEqual(
            self.policy.audit_render_package_release_workflow(
                Path("render-package-release.yml"), text
            ),
            [],
        )

    def test_render_package_release_rejects_unsafe_publication_paths(self) -> None:
        original = RENDER_PACKAGE_RELEASE_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "verify_job_continue_on_error": original.replace(
                "    timeout-minutes: 30\n    permissions:\n",
                "    timeout-minutes: 30\n"
                "    continue-on-error: true\n"
                "    permissions:\n",
                1,
            ),
            "identity_step_continue_on_error": original.replace(
                "      - name: Validate event and package identity\n"
                "        id: package\n",
                "      - name: Validate event and package identity\n"
                "        continue-on-error: true\n"
                "        id: package\n",
                1,
            ),
            "publish_job_continue_on_error": original.replace(
                "    timeout-minutes: 15\n    environment: npm-render-worker\n",
                "    timeout-minutes: 15\n"
                "    continue-on-error: true\n"
                "    environment: npm-render-worker\n",
                1,
            ),
            "identity_disables_errexit": original.replace(
                "      - name: Validate event and package identity\n"
                "        id: package\n"
                "        shell: bash\n"
                "        run: |\n"
                "          set -euo pipefail\n",
                "      - name: Validate event and package identity\n"
                "        id: package\n"
                "        shell: bash\n"
                "        run: |\n"
                "          set -euo pipefail\n"
                "          set +e\n",
                1,
            ),
            "hosted_ci_gate_fail_open": original.replace(
                "          require_successful_run ci.yml .github/workflows/ci.yml push CI\n",
                "          require_successful_run ci.yml .github/workflows/ci.yml push CI || true\n",
                1,
            ),
            "identity_checks_fail_open": original.replace(
                'test "$GITHUB_REPOSITORY" = "HyunjoJung/rxls"',
                'test "$GITHUB_REPOSITORY" = "HyunjoJung/rxls" || true',
            ).replace(
                'test "$GITHUB_REF_TYPE" = "tag"',
                'test "$GITHUB_REF_TYPE" = "tag" || true',
            ).replace(
                'test "$GITHUB_REF_NAME" = "render-v$version"',
                'test "$GITHUB_REF_NAME" = "render-v$version" || true',
            ).replace(
                'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"',
                'test "$(git rev-parse origin/main)" = "$GITHUB_SHA" || true',
            ).replace(
                'test "$(git rev-parse \'FETCH_HEAD^{commit}\')" = "$GITHUB_SHA"',
                'test "$(git rev-parse \'FETCH_HEAD^{commit}\')" = "$GITHUB_SHA" || true',
            ),
            "tag": original.replace(
                'test "$GITHUB_REF_NAME" = "render-v$version"', "true"
            ),
            "main": original.replace(
                'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"', "true"
            ),
            "commented_main": original.replace(
                '            test "$(git rev-parse origin/main)" = "$GITHUB_SHA"',
                '            # test "$(git rev-parse origin/main)" = "$GITHUB_SHA"',
                1,
            ),
            "publish_tag_revalidation": original.replace(
                '          git fetch origin "refs/tags/$GITHUB_REF_NAME" --no-tags\n'
                '          test "$(git rev-parse \'FETCH_HEAD^{commit}\')" = "$GITHUB_SHA"\n',
                "",
                1,
            ),
            "ci_gate": original.replace(
                "require_successful_run ci.yml .github/workflows/ci.yml push CI",
                "true",
            ),
            "codeql_gate": original.replace(
                "require_successful_run codeql.yml .github/workflows/codeql.yml push CodeQL",
                "true",
            ),
            "hardening_gate": original.replace(
                ".github/workflows/render-hardening.yml",
                ".github/workflows/ci.yml",
                1,
            ),
            "hardening_event": original.replace(
                "render-hardening.yml \\\n"
                "            .github/workflows/render-hardening.yml \\\n"
                "            workflow_dispatch",
                "render-hardening.yml \\\n"
                "            .github/workflows/render-hardening.yml \\\n"
                "            push",
                1,
            ),
            "browser_gate": original.replace(
                ".github/workflows/render-browser.yml",
                ".github/workflows/ci.yml",
                1,
            ),
            "browser_artifact_name": original.replace(
                'browser_artifact_name="render-browser-${GITHUB_SHA}-${browser_run_id}-${browser_run_attempt}"',
                'browser_artifact_name="render-browser-${GITHUB_SHA}"',
                1,
            ),
            "browser_artifact_run_scope": original.replace(
                "actions/runs/$browser_run_id/artifacts",
                "actions/artifacts",
                1,
            ),
            "browser_artifact_exact_one": original.replace(
                'test "${#matching_browser_artifacts[@]}" = "1"',
                'test "${#matching_browser_artifacts[@]}" -ge "1"',
                1,
            ),
            "browser_artifact_id": original.replace(
                '"$browser_artifact_id" =~ ^[1-9][0-9]*$',
                '-n "$browser_artifact_id"',
                1,
            ),
            "browser_artifact_size": original.replace(
                '&& "$size_bytes" -le 1048576',
                '&& "$size_bytes" -gt 0',
                1,
            ),
            "browser_artifact_digest": original.replace(
                '&& "$digest" =~ ^sha256:[0-9a-f]{64}$',
                '&& -n "$digest"',
                1,
            ),
            "browser_verifier": original.replace(
                "python3 scripts/check_render_browser_release_evidence.py download",
                "python3 scripts/check_render_package.py",
                1,
            ),
            "browser_artifact_name_arg": original.replace(
                '--artifact-name "$browser_artifact_name"',
                '--artifact-name "render-browser-${GITHUB_SHA}"',
                1,
            ),
            "browser_head_arg": original.replace(
                '--head-sha "$GITHUB_SHA" \\\n'
                "            --platform linux \\\n"
                '            --workflow-run-id "$browser_run_id"',
                '--head-sha "$browser_run_id" \\\n'
                "            --platform linux \\\n"
                '            --workflow-run-id "$browser_run_id"',
                1,
            ),
            "browser_platform_arg": original.replace(
                "--platform linux",
                "--platform darwin",
                1,
            ),
            "browser_run_arg": original.replace(
                '--workflow-run-id "$browser_run_id"',
                '--workflow-run-id "$SELECTED_RUN_ID"',
                1,
            ),
            "browser_attempt_arg": original.replace(
                '--workflow-run-attempt "$browser_run_attempt"',
                '--workflow-run-attempt "1"',
                1,
            ),
            "browser_receipt_output": original.replace(
                "--output target/render-package/browser-prerequisite.json",
                "--output target/render-package/browser.json",
                1,
            ),
            "browser_dry_run_binding": original.replace(
                "browser-proven package differs from release candidate",
                "browser package check skipped",
                1,
            ),
            "browser_publish_binding": original.replace(
                "Render Browser prerequisite evidence differs",
                "browser receipt check skipped",
                1,
            ),
            "run_api_fields": original.replace(
                "[.head_sha, .event, .conclusion, .status, .path, .run_attempt]",
                "[.head_sha, .conclusion]",
            ),
            "oracle_workflow": original.replace(
                "for oracle_workflow in fuzz.yml render-oracle.yml; do",
                "for oracle_workflow in ci.yml; do",
            ),
            "oracle_event": original.replace(
                '&& "$event" == "workflow_dispatch"', '&& "$event" == "push"'
            ),
            "oracle_path": original.replace(
                '"$run_path" == ".github/workflows/fuzz.yml"',
                '"$run_path" == ".github/workflows/ci.yml"',
            ),
            "oracle_profile": original.replace(
                'artifact_name="render-oracle-${GITHUB_SHA}-${run_id}-${run_attempt}-full-verify"',
                'artifact_name="render-oracle-${GITHUB_SHA}-${run_id}-${run_attempt}-full-candidate"',
            ),
            "oracle_run_attempt": original.replace(
                '&& "$run_attempt" =~ ^[1-9][0-9]*$',
                '&& -n "$run_attempt"',
            ),
            "oracle_artifact_api": original.replace(
                "actions/runs/$run_id/artifacts", "actions/artifacts"
            ),
            "oracle_digest": original.replace(
                '"$digest" =~ ^sha256:[0-9a-f]{64}$', '"$digest" != ""'
            ),
            "oracle_validator": original.replace(
                "scripts/check_render_oracle_release_evidence.py",
                "scripts/check_render_package.py",
            ),
            "oracle_baseline": original.replace(
                "--reviewed-baseline scripts/render-parity-baseline-full.json",
                "--reviewed-baseline /tmp/candidate.json",
            ),
            "manual_pack_push_guard": original.replace(
                '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n'
                '            ARCHIVE="$archive" python3 - <<\'PY\'\n',
                "          if true; then\n"
                '            ARCHIVE="$archive" python3 - <<\'PY\'\n',
                1,
            ),
            "manual_dispatch_guard": original.replace(
                '            test "$GITHUB_EVENT_NAME" = "workflow_dispatch"\n',
                "            true\n",
                1,
            ),
            "manual_browser_binding_order": original.replace(
                'Path("target/render-package/browser-prerequisite.json")',
                'Path("target/render-package/browser-prerequisite-deferred.json")',
                1,
            ).replace(
                '            test "$GITHUB_EVENT_NAME" = "workflow_dispatch"\n',
                '            test "$GITHUB_EVENT_NAME" = "workflow_dispatch"\n'
                '            echo \'Path("target/render-package/browser-prerequisite.json")\'\n',
                1,
            ),
            "manual_prefix_independent_event_guard": original.replace(
                "          python3 scripts/render_supply_chain.py sbom \\\n",
                '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n'
                "            python3 scripts/render_supply_chain.py sbom \\\n",
                1,
            ).replace(
                '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n'
                '            ARCHIVE="$archive" python3 - <<\'PY\'\n',
                "          fi\n"
                '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n'
                '            ARCHIVE="$archive" python3 - <<\'PY\'\n',
                1,
            ),
            "manual_suffix_independent_event_guard": original.replace(
                "          fi\n"
                "          npm publish --dry-run --ignore-scripts --access public \"$archive\" \\\n",
                "          fi\n"
                '          if [[ "${{ github.event_name }}" == "push" ]]; then\n'
                "            npm publish --dry-run --ignore-scripts --access public \"$archive\" \\\n",
                1,
            ).replace(
                "          NODE\n"
                "      - name: Upload verified package candidate\n",
                "          NODE\n"
                "          fi\n"
                "      - name: Upload verified package candidate\n",
                1,
            ),
            "manual_downstream_inside_event_guard": original.replace(
                "          fi\n"
                "          npm publish --dry-run --ignore-scripts --access public \"$archive\" \\\n",
                "          npm publish --dry-run --ignore-scripts --access public \"$archive\" \\\n",
                1,
            ).replace(
                "          NODE\n"
                "      - name: Upload verified package candidate\n",
                "          NODE\n"
                "          fi\n"
                "      - name: Upload verified package candidate\n",
                1,
            ),
            "manual_sbom_inside_event_guard": original.replace(
                "          python3 scripts/render_supply_chain.py sbom \\\n",
                '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n'
                "            python3 scripts/render_supply_chain.py sbom \\\n",
                1,
            ).replace(
                '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n'
                '            ARCHIVE="$archive" python3 - <<\'PY\'\n',
                '            ARCHIVE="$archive" python3 - <<\'PY\'\n',
                1,
            ),
            "manual_browser_read_outside_event_guard": original.replace(
                '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n'
                '            ARCHIVE="$archive" python3 - <<\'PY\'\n',
                '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n'
                "            true\n"
                "          fi\n"
                '          ARCHIVE="$archive" python3 - <<\'PY\'\n',
                1,
            ).replace(
                "          PY\n"
                "          else\n"
                '            test "$GITHUB_EVENT_NAME" = "workflow_dispatch"\n'
                "            echo \"workflow_dispatch verified the locally rebuilt package without publication prerequisites\"\n"
                "          fi\n",
                "          PY\n"
                '          if [[ "$GITHUB_EVENT_NAME" != "push" ]]; then\n'
                '            test "$GITHUB_EVENT_NAME" = "workflow_dispatch"\n'
                "            echo \"workflow_dispatch verified the locally rebuilt package without publication prerequisites\"\n"
                "          fi\n",
                1,
            ),
            "manual_policy_shell_guard": original.replace(
                "      - name: Enforce workflow and package policy\n"
                "        run: |\n"
                '          test "$(node --version)" = "v$NODE_VERSION"\n',
                "      - name: Enforce workflow and package policy\n"
                "        run: |\n"
                '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n'
                '            test "$(node --version)" = "v$NODE_VERSION"\n',
                1,
            ).replace(
                "          npm --prefix bindings/render-wasm test\n"
                "      - name: Build the exact worker/WASM package\n",
                "          npm --prefix bindings/render-wasm test\n"
                "          fi\n"
                "      - name: Build the exact worker/WASM package\n",
                1,
            ),
            "manual_build_shell_guard": original.replace(
                "      - name: Build the exact worker/WASM package\n"
                "        shell: bash\n"
                "        run: |\n"
                "          set -euo pipefail\n",
                "      - name: Build the exact worker/WASM package\n"
                "        shell: bash\n"
                "        run: |\n"
                "          set -euo pipefail\n"
                '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n',
                1,
            ).replace(
                "          npm --prefix bindings/render-wasm run build:wasm\n"
                "      - name: Pack, inspect, dry-run, and consume\n",
                "          npm --prefix bindings/render-wasm run build:wasm\n"
                "          fi\n"
                "      - name: Pack, inspect, dry-run, and consume\n",
                1,
            ),
            "extra_manual_trigger": original.replace(
                "  workflow_dispatch:\n"
                "  push:\n",
                "  workflow_dispatch:\n"
                "  schedule:\n"
                '    - cron: "0 0 * * *"\n'
                "  push:\n",
                1,
            ),
            "verify_job_tag_only": original.replace(
                "  verify:\n"
                "    name: Verify immutable npm artifact\n",
                "  verify:\n"
                "    name: Verify immutable npm artifact\n"
                "    if: github.event_name == 'push'\n",
                1,
            ),
            "nested_policy_skips_dispatch": original.replace(
                "      - name: Audit nested Rust advisories, licenses, and sources\n"
                "        uses: EmbarkStudios/cargo-deny-action@",
                "      - name: Audit nested Rust advisories, licenses, and sources\n"
                "        if: ${{ github.event_name != 'workflow_dispatch' }}\n"
                "        uses: EmbarkStudios/cargo-deny-action@",
                1,
            ),
            "nested_policy_continue_on_error": original.replace(
                "      - name: Audit nested Rust advisories, licenses, and sources\n"
                "        uses: EmbarkStudios/cargo-deny-action@"
                "3c6349835b2b7b196a839186cb8b78e02f7b5f25 # v2.1.1\n"
                "        with:\n",
                "      - name: Audit nested Rust advisories, licenses, and sources\n"
                "        uses: EmbarkStudios/cargo-deny-action@"
                "3c6349835b2b7b196a839186cb8b78e02f7b5f25 # v2.1.1\n"
                "        continue-on-error: true\n"
                "        with:\n",
                1,
            ),
            "publish_guard_relocated_to_policy": original.replace(
                "  publish:\n"
                "    name: Publish protected tag to npm\n"
                "    if: github.event_name == 'push'\n",
                "  publish:\n"
                "    name: Publish protected tag to npm\n"
                "    if: always()\n",
                1,
            ).replace(
                "      - name: Enforce workflow and package policy\n"
                "        run: |\n",
                "      - name: Enforce workflow and package policy\n"
                "        if: github.event_name == 'push'\n"
                "        run: |\n",
                1,
            ),
            "dispatch_publish": original.replace(
                "if: github.event_name == 'push'", "if: always()", 1
            ),
            "environment": original.replace(
                "environment: npm-render-worker", "environment: unprotected"
            ),
            "oidc": original.replace("id-token: write", "id-token: none"),
            "cache": original.replace(
                "package-manager-cache: false", "package-manager-cache: true", 1
            ),
            "force": original.replace(
                "--ignore-scripts --access public",
                "--ignore-scripts --access public --force",
                1,
            ),
            "credential": original.replace(
                "NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}",
                "NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}\n"
                "          SECOND_TOKEN: ${{ secrets.NPM_TOKEN }}",
            ),
            "registry_preflight": original.replace(
                "      - name: Detect an identical immutable registry release",
                "      - name: Skip immutable registry preflight",
            ),
            "registry_mismatch": original.replace(
                "existing immutable registry version differs from the verified candidate",
                "existing release accepted without comparison",
            ),
            "registry_error_class": original.replace(
                "if ! grep -Eq '(^|[[:space:]])E404([[:space:]]|$)' \"$error_log\"; then",
                "if false; then",
            ),
            "registry_idempotency": original.replace(
                "if: steps.registry.outputs.already_published != 'true'",
                "if: always()",
            ),
            "registry_provenance": original.replace(
                "https://slsa.dev/provenance/v1",
                "https://example.invalid/provenance",
                1,
            ),
            "registry_attestation_query": original.replace(
                "version dist.integrity repository.url dist.attestations --json",
                "version dist.integrity repository.url --json",
                1,
            ),
            "registry_signature_audit": original.replace(
                "npm audit signatures --json --include-attestations",
                "npm audit --json",
            ),
            "registry_evidence_validator": original.replace(
                'python3 "$GITHUB_WORKSPACE/scripts/check_npm_registry_evidence.py"',
                "true",
            ),
            "registry_evidence_workflow": original.replace(
                "--workflow .github/workflows/render-package-release.yml",
                "--workflow .github/workflows/attacker.yml",
            ),
            "registry_evidence_sha": original.replace(
                '--git-sha "$GITHUB_SHA"', '--git-sha "$GITHUB_REF_NAME"'
            ),
            "registry_evidence_ref": original.replace(
                '--git-ref "$GITHUB_REF"', '--git-ref "$GITHUB_SHA"'
            ),
            "registry_invocation_state": original.replace(
                "ALREADY_PUBLISHED: ${{ steps.registry.outputs.already_published }}",
                'ALREADY_PUBLISHED: "true"',
            ),
            "registry_current_invocation_policy": original.replace(
                '          invocation_policy="current-run"\n',
                '          invocation_policy="existing-release"\n',
                1,
            ),
            "registry_existing_invocation_policy": original.replace(
                '            invocation_policy="existing-release"\n',
                '            invocation_policy="current-run"\n',
                1,
            ),
            "registry_invocation_policy_branch": original.replace(
                '          if [[ "$ALREADY_PUBLISHED" == "true" ]]; then\n',
                "          if true; then\n",
                1,
            ),
            "registry_invocation_policy_fail_closed": original.replace(
                '          elif [[ "$ALREADY_PUBLISHED" != "false" ]]; then\n',
                "          else\n",
                1,
            ),
            "registry_invocation_policy_argument": original.replace(
                '--invocation-policy "$invocation_policy"',
                "--invocation-policy existing-release",
                1,
            ),
            "registry_evidence_test": original.replace(
                "python3 scripts/test_check_npm_registry_evidence.py",
                "true",
            ),
            "nested_manifest": original.replace(
                "manifest-path: bindings/render-wasm/Cargo.toml",
                "manifest-path: Cargo.toml",
                1,
            ),
            "root_deny_policy": original.replace(
                "arguments: --config deny.toml --locked --all-features",
                "arguments: --locked --all-features",
            ),
            "notice": original.replace(
                "--check bindings/render-wasm/THIRD_PARTY_NOTICES.txt",
                "--output target/notice.txt",
            ),
            "sbom_determinism": original.replace(
                "cmp --silent \\", "cmp --silently \\", 1
            ),
            "wasm_build_rust": original.replace(
                'WASM_BINDGEN_BUILD_RUST: "1.88.0"',
                'WASM_BINDGEN_BUILD_RUST: "1.88"',
            ),
            "wasm_build_step_scope": original.replace(
                "      - name: Build the exact worker/WASM package",
                "      - name: Build a mutable worker/WASM package",
            ),
            "wasm_rustup_toolchain": original.replace(
                'rustup toolchain install "$WASM_BINDGEN_BUILD_RUST" --profile minimal',
                'rustup toolchain install "$RENDER_MSRV" --profile minimal',
            ),
            "wasm_runner_temp_guard": original.replace(
                '          test -n "$RUNNER_TEMP"',
                "          true",
            ),
            "wasm_cached_tool_root": original.replace(
                'tool_root="$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION"',
                'tool_root="$CARGO_HOME"',
            ),
            "wasm_cached_tool_root_after_pin": original.replace(
                '          tool_root="$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION"',
                '          tool_root="$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION"\n'
                '          tool_root="$CARGO_HOME"',
            ),
            "wasm_missing_fresh_root_cleanup": original.replace(
                '          rm -rf "$tool_root"',
                "          true",
            ),
            "wasm_cargo_toolchain": original.replace(
                'cargo "+$WASM_BINDGEN_BUILD_RUST" install \\\n',
                'cargo "+$RENDER_MSRV" install \\\n',
            ),
            "wasm_cargo_unqualified": original.replace(
                'cargo "+$WASM_BINDGEN_BUILD_RUST" install \\\n',
                "cargo install \\\n",
            ),
            "wasm_cargo_commented": original.replace(
                '          cargo "+$WASM_BINDGEN_BUILD_RUST" install \\\n',
                '          # cargo "+$WASM_BINDGEN_BUILD_RUST" install \\\n',
            ),
            "wasm_cargo_default_root": original.replace(
                '            wasm-bindgen-cli --version "$WASM_BINDGEN_VERSION" --locked \\\n'
                '            --root "$tool_root"',
                '            wasm-bindgen-cli --version "$WASM_BINDGEN_VERSION" --locked',
            ),
            "wasm_cargo_force": original.replace(
                '            --root "$tool_root"',
                '            --root "$tool_root" --force',
            ),
            "wasm_github_path_missing": original.replace(
                '          echo "$tool_root/bin" >> "$GITHUB_PATH"',
                '          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"',
            ),
            "wasm_github_path_cached_alternative": original.replace(
                '          echo "$tool_root/bin" >> "$GITHUB_PATH"',
                '          echo "$tool_root/bin" >> "$GITHUB_PATH"\n'
                '          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"',
            ),
            "wasm_isolated_path_export_missing": original.replace(
                '          export PATH="$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION/bin:$PATH"',
                '          export PATH="$CARGO_HOME/bin:$PATH"',
            ),
            "wasm_isolated_resolution_missing": original.replace(
                '          test "$(command -v wasm-bindgen)" = \\\n'
                '            "$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION/bin/wasm-bindgen"',
                "          command -v wasm-bindgen",
            ),
            "wasm_cached_path_after_verification": original.replace(
                '            "$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION/bin/wasm-bindgen"\n'
                "          npm --prefix bindings/render-wasm run build:wasm",
                '            "$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION/bin/wasm-bindgen"\n'
                '          export PATH="$CARGO_HOME/bin:$PATH"\n'
                "          npm --prefix bindings/render-wasm run build:wasm",
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                errors = self.policy.audit_render_package_release_workflow(
                    Path("render-package-release.yml"), workflow
                )
                self.assertTrue(errors)

    def test_checked_in_codeql_explicitly_builds_every_rust_surface(self) -> None:
        text = CODEQL_WORKFLOW.read_text(encoding="utf-8")

        self.assertEqual(
            self.policy.audit_codeql_workflow(Path("codeql.yml"), text), []
        )

    def test_codeql_rejects_dropped_root_renderer_or_render_wasm_build(self) -> None:
        original = CODEQL_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "main_push_path_filter": original.replace(
                "  push:\n    branches: [main]\n",
                "  push:\n    branches: [main]\n    paths-ignore:\n      - '**/*.md'\n",
                1,
            ),
            "root": original.replace(
                "cargo build --all-targets --all-features --locked",
                "cargo build --all-features --locked",
            ),
            "renderer": original.replace(
                "cargo build --manifest-path render/Cargo.toml --all-targets --locked",
                "cargo build --manifest-path render/Cargo.toml --locked",
            ),
            "render_wasm": original.replace(
                "cargo build --manifest-path bindings/render-wasm/Cargo.toml \\\n"
                "            --all-targets --locked",
                "cargo build --manifest-path bindings/render-wasm/Cargo.toml --locked",
            ),
            "autobuild": original.replace(
                "      - name: Build",
                "      - uses: github/codeql-action/autobuild@"
                + "a" * 40
                + " # v4.37.0\n\n      - name: Build",
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                errors = self.policy.audit_codeql_workflow(Path("codeql.yml"), workflow)
                self.assertTrue(errors)

    def test_installed_product_lane_covers_linux_macos_and_windows(self) -> None:
        workflow = CI_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn("installed-product:", workflow)
        for runner in ("ubuntu-latest", "macos-latest", "windows-latest"):
            self.assertIn(f"os: {runner}", workflow)
        self.assertIn("cargo build --locked", workflow)
        self.assertIn(
            "CARGO_BIN_EXE_rxls: target/debug/${{ matrix.executable }}", workflow
        )
        self.assertIn("cargo test --test cli --locked", workflow)
        self.assertIn("cargo package --locked", workflow)
        self.assertIn(
            "python3 scripts/check_core_package.py target/package/rxls-0.1.3.crate",
            workflow,
        )
        self.assertIn(
            "cargo install --path target/package/rxls-0.1.3 --locked --root target/installed-product",
            workflow,
        )
        self.assertIn('installed="target/installed-product/bin/', workflow)

    def test_ci_package_lanes_use_the_source_release_identity(self) -> None:
        workflow = CI_WORKFLOW.read_text(encoding="utf-8")

        self.assertEqual(
            workflow.count(
                "python3 scripts/check_core_package.py target/package/rxls-0.1.3.crate"
            ),
            2,
        )
        self.assertNotIn("target/package/rxls-0.1.2", workflow)


if __name__ == "__main__":
    unittest.main()
