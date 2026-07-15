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
            "cargo install --path target/package/rxls-0.1.2 --locked --root target/installed-product",
            workflow,
        )
        self.assertIn('installed="target/installed-product/bin/', workflow)


if __name__ == "__main__":
    unittest.main()
