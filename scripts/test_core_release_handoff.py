"""Offline tests for the shared, non-publishing release handoff verifier."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("core_release_handoff", ROOT / "scripts/core_release_handoff.py")
handoff = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(handoff)


class HandoffTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.env = {
            "ARTIFACT_ID": "456", "ARTIFACT_DIGEST": "a" * 64,
            "SOURCE_ATTEMPT": "1", "VERIFIED_VERSION": "0.1.4",
            "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2",
            "GITHUB_SHA": "b" * 40, "GITHUB_REPOSITORY_ID": "789",
            "GITHUB_REPOSITORY": "HyunjoJung/rxls",
        }
        self.metadata = {
            "id": 456, "name": f"rxls-0.1.4-publication-{'b' * 40}-123-1",
            "digest": f"sha256:{'a' * 64}", "expired": False,
            "workflow_run": {"id": 123, "head_sha": "b" * 40, "repository_id": 789, "head_repository_id": 789},
        }
        for relative in ("dist", "target/package", "target/publication"):
            (self.root / relative).mkdir(parents=True)
        self.metadata_path = self.root / "target/publication/artifact.json"
        self.metadata_path.write_text(json.dumps(self.metadata), encoding="utf-8")
        (self.root / "Cargo.toml").write_text('[package]\nname="rxls"\nversion="0.1.4"\n', encoding="utf-8")
        self.archive = self.root / "dist/rxls-0.1.4.crate"
        self.archive.write_bytes(b"verified archive")
        self.commands = []

    def runner(self, argv, root):
        self.assertEqual(root, self.root)
        self.commands.append(argv)
        if argv == ["cargo", "package", "--locked"]:
            (root / "target/package/rxls-0.1.4.crate").write_bytes(self.archive.read_bytes())

    def test_shared_verifier_orders_real_gates_and_never_authorizes_publication(self):
        result = handoff.verify(self.root, self.env, self.runner)
        self.assertTrue(result["passed"])
        self.assertFalse(result["publication_allowed"])
        self.assertEqual(result["artifact_id"], 456)
        self.assertEqual(result["source_attempt"], 1)
        self.assertEqual(result["git_rev"], "b" * 40)
        self.assertEqual([command[1] for command in self.commands], [
            "scripts/check_workflow_policy.py", "scripts/check_release_identity.py",
            "scripts/check_cargo_publish_dry_run.py", "scripts/release_manifest.py",
            "scripts/check_core_package.py", "package",
        ])
        self.assertIn("52", self.commands[3])
        self.assertIn("--git-sha", self.commands[2])
        self.assertEqual(self.commands[-1], ["cargo", "package", "--locked"])
        self.assertNotIn("publish", [argument for command in self.commands for argument in command])

    def test_different_repacked_bytes_fail(self):
        def mismatch(argv, root):
            self.runner(argv, root)
            if argv[0] == "cargo":
                (root / "target/package/rxls-0.1.4.crate").write_bytes(b"different")
        with self.assertRaisesRegex(handoff.HandoffError, "repacked crate"):
            handoff.verify(self.root, self.env, mismatch)

    def test_every_failed_evidence_gate_prevents_repack(self):
        for failed in range(5):
            self.commands = []
            def fail(argv, root):
                if len(self.commands) == failed:
                    raise subprocess.CalledProcessError(1, argv)
                self.runner(argv, root)
            with self.subTest(gate=failed), self.assertRaises(subprocess.CalledProcessError):
                handoff.verify(self.root, self.env, fail)
            self.assertFalse(any(argv[0] == "cargo" for argv in self.commands))

    def test_metadata_and_manifest_mismatch_fail_before_commands(self):
        for env in (
            {**self.env, "ARTIFACT_ID": "999"},
            {**self.env, "GITHUB_REPOSITORY": "other/rxls"},
            {**self.env, "SOURCE_ATTEMPT": "3"},
            {**self.env, "ARTIFACT_DIGEST": None},
        ):
            with self.subTest(env=env), self.assertRaises(handoff.HandoffError):
                handoff.verify(self.root, env, self.runner)
        (self.root / "Cargo.toml").write_text('[package]\nversion="0.1.5"\n')
        with self.assertRaisesRegex(handoff.HandoffError, "manifest version"):
            handoff.verify(self.root, self.env, self.runner)
        self.assertEqual(self.commands, [])

    def test_evidence_files_are_bounded_regular_and_duplicate_free(self):
        self.metadata_path.write_text('{"id":1,"id":2}')
        with self.assertRaisesRegex(handoff.HandoffError, "duplicate"):
            handoff.load_metadata(self.metadata_path)
        self.metadata_path.write_bytes(b"x" * (handoff.MAX_METADATA_BYTES + 1))
        with self.assertRaisesRegex(handoff.HandoffError, "byte budget"):
            handoff.load_metadata(self.metadata_path)
        self.metadata_path.unlink()
        self.metadata_path.symlink_to(self.archive)
        with self.assertRaisesRegex(handoff.HandoffError, "regular"):
            handoff.load_metadata(self.metadata_path)

    def test_subprocess_has_checked_status_timeout_and_no_shell(self):
        with mock.patch.object(handoff.subprocess, "run") as run:
            handoff.checked_command(["cargo", "package", "--locked"], self.root)
        run.assert_called_once_with(
            ["cargo", "package", "--locked"], cwd=self.root,
            check=True, timeout=handoff.COMMAND_TIMEOUT,
        )


if __name__ == "__main__":
    unittest.main()
