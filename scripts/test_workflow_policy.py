#!/usr/bin/env python3
"""Tests for the hosted workflow supply-chain policy."""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
POLICY = ROOT / "scripts" / "check_workflow_policy.py"
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
CODEQL_WORKFLOW = ROOT / ".github" / "workflows" / "codeql.yml"
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

    def test_mutable_action_ref_is_rejected(self) -> None:
        errors = self.policy.audit_action_pins(
            Path(".github/workflows/example.yml"),
            "steps:\n  - uses: actions/checkout@v7 # v7.0.0\n",
        )

        self.assertTrue(any("full immutable commit SHA" in error for error in errors))

    def test_action_pin_without_version_comment_is_rejected(self) -> None:
        errors = self.policy.audit_action_pins(
            Path(".github/workflows/example.yml"),
            "steps:\n  - uses: actions/checkout@" + "a" * 40 + "\n",
        )

        self.assertTrue(any("needs a version comment" in error for error in errors))

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
      - uses: actions/checkout@{"a" * 40} # v7.0.0
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
                "      - run: true\n"
                "      - name: Verify exact source revision\n",
            ),
            valid.replace(
                "        shell: bash\n",
                "        continue-on-error: true\n"
                "        shell: bash\n",
                1,
            ),
            valid.replace(
                "        shell: bash\n",
                "        if: ${{ false }}\n"
                "        shell: bash\n",
                1,
            ),
            valid.replace(
                "        with:\n",
                "        if: ${{ false }}\n"
                "        with:\n",
                1,
            ),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.assertTrue(
                    self.policy.audit_pr_head_checkouts(path, mutation)
                )

    def test_flow_map_pull_request_cannot_bypass_exact_head_guards(self) -> None:
        expression = "${{ github.event.pull_request.head.sha || github.sha }}"
        valid = f"""
on:
  pull_request: {{}}
jobs:
  test:
    steps:
      - uses: actions/checkout@{"a" * 40} # v7.0.0
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
      - uses: actions/checkout@{"a" * 40} # v7.0.0
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
            "  linux:\n"
            + guarded_steps
            + "  macos:\n"
            + guarded_steps
        )
        path = Path(".github/workflows/example.yml")
        self.assertEqual(self.policy.audit_pr_head_checkouts(path, valid), [])

        mutations = (
            valid.replace("  macos:\n" + guarded_steps, "  macos:\n    steps:\n      - run: true\n"),
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
            f"      - uses: actions/checkout@{'a' * 40} # v7.0.0\n"
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

        self.assertTrue(any("expected exact FUZZ_NIGHTLY_VERSION" in error for error in errors))
        self.assertTrue(any("must not install mutable nightly" in error for error in errors))
        self.assertTrue(any("must not invoke mutable nightly" in error for error in errors))

    def test_exact_fuzz_workflow_tools_are_accepted(self) -> None:
        text = """
env:
  FUZZ_NIGHTLY_VERSION: "nightly-2026-07-10"
  CARGO_FUZZ_VERSION: "0.13.2"
steps:
  - run: cargo install cargo-fuzz --version "$CARGO_FUZZ_VERSION" --locked
  - run: cargo +"$FUZZ_NIGHTLY_VERSION" fuzz build
"""

        self.assertEqual(
            self.policy.audit_fuzz_workflow(Path("fuzz.yml"), text), []
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

    def test_render_oracle_rejects_mutable_python_pip_apt_and_identity_status(self) -> None:
        original = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
            "python": original.replace('python-version: "3.13.14"', 'python-version: "3.13"'),
            "pip": original.replace("            --require-hashes \\\n", ""),
            "apt": original.replace(
                'sudo apt-get install --yes --no-install-recommends '
                '"${SYSTEM_PACKAGES[@]}"',
                "sudo apt-get install --yes --no-install-recommends poppler-utils",
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
            "summary_schema_v4": original.replace(
                "rxls.render-oracle-hosted-campaign.v5",
                "rxls.render-oracle-hosted-campaign.v4",
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
            self.policy.audit_render_oracle_workflow(
                Path("render-oracle.yml"), text
            ),
            [],
        )

    def test_oracle_build_jobs_reject_unreviewed_step_surface(self) -> None:
        oracle = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")
        hardening = RENDER_HARDENING_WORKFLOW.read_text(encoding="utf-8")
        action_sha = "a" * 40
        injected_steps = {
            "sha_pinned_build_push": (
                "      - uses: docker/build-push-action@"
                f"{action_sha} # v6.18.0\n"
            ),
            "local_composite": (
                "      - uses: ./.github/actions/unreviewed-oracle-build\n"
            ),
            "injected_remote": (
                f"      - uses: actions/cache@{action_sha} # v4.3.0\n"
            ),
            "extra_make_step": (
                "      - name: Alternate oracle build\n"
                "        run: make oracle-image\n"
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
                "          echo unreviewed-build-block-mutation\n"
                + build_invocation,
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
            hardening[:image_start]
            + reusable_image_job
            + hardening[image_end:]
        )
        self.assertTrue(
            self.policy.audit_render_hardening_workflow(
                Path("render-hardening.yml"), reusable_hardening
            )
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
                    "    env:\n"
                    "      DOCKER_HOST: unix:///tmp/unreviewed-docker.sock\n",
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
                "assert all(180 <= len(rows) <= 220 for rows in shards)",
                "assert shards",
            ),
            "timeout": original.replace(
                "inputs.campaign == 'full' && 330 || 120",
                "inputs.campaign == 'full' && 360 || 120",
            ),
            "scheduled_profile": original.replace(
                "github.event_name == 'workflow_dispatch' && inputs.campaign || 'pilot'",
                "inputs.campaign",
            ),
            "head_sha": original.replace(
                'test "$(git rev-parse HEAD)" = "$GITHUB_SHA"',
                "git rev-parse HEAD",
            ),
            "pdffonts_identity": original.replace(
                '--pdffonts-binary-sha256 "$PDFFONTS_SHA256"',
                "",
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
                "            target/render-oracle-hosted/renderer.json\n",
                "            target/render-oracle-hosted/renderer.json\n"
                "            target/render-oracle-hosted/parity-report-a.json\n",
            ),
            "raw_authored_artifact": original.replace(
                "            target/render-oracle-hosted/authored-print-gate.json\n",
                "            target/render-oracle-hosted/authored-print-gate.json\n"
                "            target/render-oracle-hosted/authored-print-report.json\n",
            ),
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
                self.assertTrue(
                    self.policy.audit_render_oracle_workflow(
                        Path("render-oracle.yml"), workflow
                    )
                )

    def test_render_oracle_campaign_artifacts_are_aggregate_only(self) -> None:
        text = RENDER_ORACLE_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn("--profile \"$RXLS_ORACLE_CAMPAIGN\"", text)
        self.assertIn("run_full_campaign a", text)
        self.assertIn("run_full_campaign b", text)
        self.assertIn("scripts/merge-render-parity-reports.py", text)
        self.assertIn("scripts/compare-render-parity-runs.py", text)
        self.assertIn("scripts/check-render-parity-baseline.py", text)
        self.assertIn("scripts/check-authored-print-parity.py", text)
        self.assertIn("--print-mode authored", text)
        self.assertIn("--required-feature print-settings", text)
        self.assertIn("--require-hosted-full-800", text)
        self.assertIn('"acquired_corpus_included": False', text)
        self.assertNotIn(
            "            target/render-oracle-hosted/parity-report-a.json\n",
            text,
        )
        self.assertNotIn(
            "            target/render-oracle-hosted/authored-print-report.json\n",
            text,
        )
        self.assertNotIn("            local/render-corpus-generated", text)

    def test_render_hardening_rejects_mutable_apt_and_path_bearing_evidence(self) -> None:
        original = RENDER_HARDENING_WORKFLOW.read_text(encoding="utf-8")
        mutations = (
            original.replace(
                "          mkdir -p target\n",
                "          sudo apt-get update\n          mkdir -p target\n",
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
                '              raise SystemExit(1)\n',
                '              print("bootstrap accepted")\n',
            ),
            original.replace(
                '          assert evidence["image_identity_status"] == "pinned_match", evidence\n',
                '          assert evidence["image_identity_status"] != "mismatch", evidence\n',
            ),
        )
        for workflow in mutations:
            with self.subTest(workflow=workflow):
                errors = self.policy.audit_render_hardening_workflow(
                    Path("render-hardening.yml"), workflow
                )
                self.assertTrue(errors)

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
            "steps:\n  - run: docker buildx \"$SUBCOMMAND\" .\n",
            "steps:\n  - run: docker buildx \"${SUBCOMMAND:-build}\" .\n",
            "steps:\n  - run: bash -c 'docker build .'\n",
            "steps:\n  - run: bash /tmp/generated-build-script.sh\n",
            "steps:\n  - run: sh -c \"$BUILD_COMMAND\"\n",
            "steps:\n  - run: eval \"$BUILD_COMMAND\"\n",
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
            "steps:\n  - run: \"$UNKNOWN_COMMAND\" .\n",
            "steps:\n  - run: 'docker build .\n",
            "steps:\n  - run: |\n      DOCKER=docker\n      SUBCOMMAND=build\n      \"$DOCKER\" \"$SUBCOMMAND\" .\n",
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
                self.assertTrue(
                    self.policy._direct_docker_build_commands(workflow)
                )

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
            self.policy.audit_render_browser_workflow(
                Path("render-browser.yml"), text
            ),
            [],
        )

    def test_render_browser_rejects_mutable_or_commented_wasm_build_tools(self) -> None:
        original = RENDER_BROWSER_WORKFLOW.read_text(encoding="utf-8")
        mutations = {
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
            "tag": original.replace('test "$GITHUB_REF_NAME" = "render-v$version"', "true"),
            "main": original.replace(
                'git merge-base --is-ancestor "$GITHUB_SHA" origin/main', "true"
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
            "run_api_fields": original.replace(
                "[.head_sha, .event, .conclusion, .status, .path, .run_attempt]",
                "[.head_sha, .conclusion]",
            ),
            "oracle_workflow": original.replace(
                "--workflow render-oracle.yml", "--workflow ci.yml"
            ),
            "oracle_event": original.replace(
                '&& "$event" == "workflow_dispatch"', '&& "$event" == "push"'
            ),
            "oracle_path": original.replace(
                '&& "$run_path" == ".github/workflows/render-oracle.yml"',
                '&& "$run_path" == ".github/workflows/ci.yml"',
            ),
            "oracle_profile": original.replace(
                'artifact_name="render-oracle-${GITHUB_SHA}-${run_id}-${run_attempt}-full"',
                'artifact_name="render-oracle-${GITHUB_SHA}-${run_id}-${run_attempt}-pilot"',
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
                "--ignore-scripts --access public", "--ignore-scripts --access public --force", 1
            ),
            "credential": original.replace(
                "NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}",
                "NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}\n"
                "          SECOND_TOKEN: ${{ secrets.NPM_TOKEN }}",
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
            "sbom_determinism": original.replace("cmp --silent \\", "cmp --silently \\", 1),
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
                errors = self.policy.audit_codeql_workflow(
                    Path("codeql.yml"), workflow
                )
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
            "python3 scripts/check_core_package.py target/package/rxls-0.1.2.crate",
            workflow,
        )
        self.assertIn(
            "cargo install --path target/package/rxls-0.1.2 --locked --root target/installed-product",
            workflow,
        )
        self.assertIn('installed="target/installed-product/bin/', workflow)


if __name__ == "__main__":
    unittest.main()
