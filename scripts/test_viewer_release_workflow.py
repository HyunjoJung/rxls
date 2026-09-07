#!/usr/bin/env python3
"""Regression tests for verification-only viewer builds and Pages promotion."""

from __future__ import annotations

import importlib.util
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "viewer-pages.yml"
SPEC = importlib.util.spec_from_file_location(
    "viewer_workflow_policy", ROOT / "scripts" / "check_workflow_policy.py"
)
assert SPEC is not None and SPEC.loader is not None
POLICY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(POLICY)
SHA = "1234567890abcdef1234567890abcdef12345678"


def step_script(workflow: str, name: str) -> str:
    marker = f"      - name: {name}\n"
    if marker not in workflow:
        raise AssertionError(f"missing workflow step: {name}")
    step = workflow.split(marker, 1)[1]
    step = re.split(r"(?m)^(?:      - |  [A-Za-z])", step, maxsplit=1)[0]
    if "        run: |\n" not in step:
        raise AssertionError(f"missing shell script: {name}")
    return "\n".join(
        line[10:] if line.startswith("          ") else line
        for line in step.split("        run: |\n", 1)[1].splitlines()
    )


class ViewerWorkflowPolicyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.workflow = WORKFLOW.read_text(encoding="utf-8")

    def test_repository_viewer_boundary_passes(self) -> None:
        self.assertEqual(
            POLICY.audit_viewer_release_boundary(WORKFLOW, self.workflow), []
        )

    def test_boundary_rejects_unsafe_dispatch_and_publication_mutations(self) -> None:
        mutations = {
            "default_deploy": ("default: false", "default: true"),
            "untyped_deploy": ("type: boolean", "type: string"),
            "unconditional_request": (
                "github.event_name == 'push' || (github.event_name == 'workflow_dispatch' && inputs.deploy == true)",
                "true",
            ),
            "missing_canonical_repository": (
                '          test "$GITHUB_REPOSITORY" = "HyunjoJung/rxls"\n',
                "",
            ),
            "missing_main_ref": (
                '          test "$GITHUB_REF" = "refs/heads/main"\n',
                "",
            ),
            "missing_remote_sha": (
                '          test "$remote_main" = "$(printf \'%s\\trefs/heads/main\' "$EXPECTED_SHA")"\n',
                "",
            ),
            "unconditional_pages_configuration": (
                "        if: steps.mode.outputs.deploy_pages == 'true'\n",
                "",
            ),
            "unconditional_pages_upload": (
                "      - name: Upload GitHub Pages artifact\n        if: steps.mode.outputs.deploy_pages == 'true'\n",
                "      - name: Upload GitHub Pages artifact\n",
            ),
            "unconditional_deployment_job": (
                "    if: needs.build.outputs.deploy_pages == 'true'\n",
                "",
            ),
            "verify_write_permission": (
                "      contents: read\n",
                "      contents: read\n      pages: write\n",
            ),
            "verify_cancels_deployment": (
                "  group: viewer-${{ (github.repository == 'HyunjoJung/rxls' && github.ref == 'refs/heads/main' && (github.event_name == 'push' || inputs.deploy == true)) && 'pages' || format('verify-{0}', github.ref) }}",
                "  group: pages",
            ),
            "missing_pinned_browser_gate": (
                "          npm --prefix viewer run test:browser\n",
                "",
            ),
        }
        for name, (before, after) in mutations.items():
            with self.subTest(mutation=name):
                self.assertIn(before, self.workflow)
                changed = self.workflow.replace(before, after, 1)
                self.assertTrue(
                    POLICY.audit_viewer_release_boundary(WORKFLOW, changed), name
                )

    def test_pages_actions_cannot_escape_their_guarded_steps(self) -> None:
        for action in (
            "actions/configure-pages@",
            "actions/upload-pages-artifact@",
            "actions/deploy-pages@",
        ):
            with self.subTest(action=action):
                action_line = next(
                    line for line in self.workflow.splitlines() if f"uses: {action}" in line
                )
                changed = self.workflow.replace(
                    action_line,
                    "        run: 'true'\n"
                    "      - name: Escaped publication action\n" + action_line,
                    1,
                )
                self.assertTrue(
                    POLICY.audit_viewer_release_boundary(WORKFLOW, changed), action
                )

    def test_final_promotion_guard_cannot_be_removed_skipped_or_delayed(self) -> None:
        marker = "      - name: Recheck exact canonical main before deployment\n"
        start = self.workflow.index(marker)
        end = self.workflow.index("      - name: Deploy GitHub Pages artifact\n", start)
        guard = self.workflow[start:end]
        removed = self.workflow[:start] + self.workflow[end:]
        mutations = {
            "removed": removed,
            "delayed_until_after_publication": removed + guard,
            "skipped": self.workflow.replace(marker, marker + "        if: false\n", 1),
            "errors_ignored": self.workflow.replace(
                marker, marker + "        continue-on-error: true\n", 1
            ),
            "wrong_revision": self.workflow[:start] + guard.replace(
                "EXPECTED_SHA: ${{ github.sha }}", "EXPECTED_SHA: unreviewed"
            ) + self.workflow[end:],
        }
        for name, changed in mutations.items():
            with self.subTest(mutation=name):
                self.assertTrue(
                    POLICY.audit_viewer_release_boundary(WORKFLOW, changed), name
                )


@unittest.skipUnless(shutil.which("bash"), "workflow shell tests require bash")
class ViewerWorkflowShellTests(unittest.TestCase):
    def setUp(self) -> None:
        self.workflow = WORKFLOW.read_text(encoding="utf-8")

    def run_step(self, name: str, **environment: str):
        script = step_script(self.workflow, name)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "output"
            calls = root / "calls"
            env = dict(
                os.environ,
                EXPECTED_SHA=SHA,
                GITHUB_REPOSITORY="HyunjoJung/rxls",
                GITHUB_REF="refs/heads/main",
                REQUEST_DEPLOYMENT="false",
                REMOTE_SHA=SHA,
                REMOTE_EXIT="0",
                GITHUB_OUTPUT=str(output),
                COMMAND_LOG=str(calls),
            )
            env.update(environment)
            stub = """
git() {
  printf '%s\\n' "$*" >> "$COMMAND_LOG"
  if [[ "$*" != 'ls-remote --exit-code https://github.com/HyunjoJung/rxls.git refs/heads/main' ]]; then
    return 91
  fi
  printf '%s\\trefs/heads/main\\n' "$REMOTE_SHA"
  return "$REMOTE_EXIT"
}
"""
            result = subprocess.run(
                [shutil.which("bash"), "-c", stub + script],
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            return (
                result,
                output.read_text(encoding="utf-8") if output.exists() else "",
                calls.read_text(encoding="utf-8") if calls.exists() else "",
            )

    def test_default_verification_never_queries_publication_state(self) -> None:
        for repository, ref in (
            ("HyunjoJung/rxls", "refs/heads/main"),
            ("HyunjoJung/rxls", "refs/heads/contribution"),
            ("contributor/rxls", "refs/heads/main"),
        ):
            with self.subTest(repository=repository, ref=ref):
                result, output, calls = self.run_step(
                    "Select viewer publication mode",
                    GITHUB_REPOSITORY=repository,
                    GITHUB_REF=ref,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output, "deploy_pages=false\n")
                self.assertEqual(calls, "")

    def test_explicit_canonical_main_deployment_is_selected(self) -> None:
        result, output, calls = self.run_step(
            "Select viewer publication mode", REQUEST_DEPLOYMENT="true"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "deploy_pages=true\n")
        self.assertEqual(len(calls.splitlines()), 1)

    def test_deployment_selection_fails_closed(self) -> None:
        for mutation in (
            {"GITHUB_REPOSITORY": "contributor/rxls"},
            {"GITHUB_REF": "refs/heads/contribution"},
            {"GITHUB_REF": "refs/tags/v0.1.3"},
            {"REMOTE_SHA": "f" * 40},
            {"REMOTE_EXIT": "128"},
        ):
            with self.subTest(mutation=mutation):
                result, output, _ = self.run_step(
                    "Select viewer publication mode",
                    REQUEST_DEPLOYMENT="true",
                    **mutation,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("deploy_pages=true", output)

    def test_final_promotion_rechecks_canonical_main_and_exact_sha(self) -> None:
        result, _, calls = self.run_step("Recheck exact canonical main before deployment")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls.splitlines()), 1)
        for mutation in (
            {"GITHUB_REPOSITORY": "contributor/rxls"},
            {"GITHUB_REF": "refs/heads/contribution"},
            {"REMOTE_SHA": "f" * 40},
            {"REMOTE_EXIT": "128"},
        ):
            with self.subTest(mutation=mutation):
                result, _, _ = self.run_step(
                    "Recheck exact canonical main before deployment", **mutation
                )
                self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
