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
        }
        for name, workflow in mutations.items():
            with self.subTest(name=name):
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
        )
        for workflow in mutations:
            with self.subTest(workflow=workflow):
                errors = self.policy.audit_render_hardening_workflow(
                    Path("render-hardening.yml"), workflow
                )
                self.assertTrue(errors)

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
                "[.head_sha, .event, .conclusion, .status, .path]",
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
                'artifact_name="render-oracle-${GITHUB_SHA}-full"',
                'artifact_name="render-oracle-${GITHUB_SHA}-pilot"',
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
