#!/usr/bin/env python3
"""Tests for exact crates.io cargo-publish dry-run evidence."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "check_cargo_publish_dry_run.py"
SPEC = importlib.util.spec_from_file_location("check_cargo_publish_dry_run", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


GIT_SHA = "1234567890abcdef1234567890abcdef12345678"
CARGO_VERBOSE = """cargo 1.96.1 (356927216 2026-06-26)
release: 1.96.1
commit-hash: 356927216a2d746168cf76e5e88cc3f4b58e029d
commit-date: 2026-06-26
host: x86_64-unknown-linux-gnu
libgit2: ignored
"""
RUSTC_VERBOSE = """rustc 1.96.1 (31fca3adb 2026-06-26)
binary: rustc
commit-hash: 31fca3adb283cc9dfd56b49cdee9a96eb9c96ffd
commit-date: 2026-06-26
host: x86_64-unknown-linux-gnu
release: 1.96.1
LLVM version: ignored
"""


class FakeRunner:
    def __init__(self, publish_returncode: int = 0) -> None:
        self.publish_returncode = publish_returncode
        self.commands: list[list[str]] = []

    def __call__(self, argv, **kwargs):
        command = list(argv)
        self.commands.append(command)
        self.assert_invocation(kwargs)
        if command == ["cargo", "--version", "--verbose"]:
            return subprocess.CompletedProcess(command, 0, stdout=CARGO_VERBOSE, stderr="")
        if command == ["rustc", "--version", "--verbose"]:
            return subprocess.CompletedProcess(command, 0, stdout=RUSTC_VERBOSE, stderr="")
        if command == list(MODULE.PUBLISH_ARGV):
            return subprocess.CompletedProcess(
                command,
                self.publish_returncode,
                stdout="captured cargo output containing /private/workspace\n",
                stderr="captured cargo errors containing /private/workspace\n",
            )
        raise AssertionError(f"unexpected command: {command!r}")

    @staticmethod
    def assert_invocation(kwargs) -> None:
        assert kwargs["check"] is False
        assert kwargs["stdout"] is subprocess.PIPE
        assert kwargs["stderr"] is subprocess.PIPE
        assert kwargs["text"] is True
        assert "shell" not in kwargs


class CargoPublishEvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.manifest = self.root / "Cargo.toml"
        self.manifest.write_text(
            '[package]\nname = "rxls"\nversion = "0.1.3"\n',
            encoding="utf-8",
        )
        self.package = self.root / "target" / "package"
        self.package.mkdir(parents=True)
        self.crate = self.package / "rxls-0.1.3.crate"
        self.crate.write_bytes(b"immutable crate bytes")
        self.receipt = self.package / MODULE.RECEIPT_NAME

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def _record(self, runner: FakeRunner | None = None):
        selected = runner or FakeRunner()
        payload = MODULE.run_and_write(
            self.manifest,
            GIT_SHA,
            self.receipt,
            runner=selected,
        )
        return selected, payload

    def _write_mutation(self, mutate) -> Path:
        _, payload = self._record()
        mutated = json.loads(json.dumps(payload))
        mutate(mutated)
        self.receipt.write_text(
            json.dumps(mutated, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return self.receipt

    def test_runner_executes_only_exact_argv_and_writes_path_neutral_receipt(self) -> None:
        runner, payload = self._record()
        self.assertEqual(
            runner.commands,
            [
                ["cargo", "--version", "--verbose"],
                ["rustc", "--version", "--verbose"],
                list(MODULE.PUBLISH_ARGV),
            ],
        )
        self.assertEqual(payload, json.loads(self.receipt.read_text(encoding="utf-8")))
        self.assertEqual(payload["argv"], list(MODULE.PUBLISH_ARGV))
        self.assertEqual(payload["registry"], "crates-io")
        self.assertIs(payload["passed"], True)
        serialized = json.dumps(payload, sort_keys=True)
        self.assertNotIn(str(self.root), serialized)
        self.assertNotIn("captured cargo output", serialized)
        self.assertNotIn("captured cargo errors", serialized)

    def test_nonzero_publish_fails_and_removes_stale_receipt(self) -> None:
        self.receipt.write_text("stale\n", encoding="utf-8")
        runner = FakeRunner(publish_returncode=101)
        with self.assertRaisesRegex(MODULE.EvidenceError, "cargo_publish_dry_run"):
            MODULE.run_and_write(
                self.manifest,
                GIT_SHA,
                self.receipt,
                runner=runner,
            )
        self.assertFalse(self.receipt.exists())
        self.assertEqual(runner.commands[-1], list(MODULE.PUBLISH_ARGV))

    def test_verifier_recomputes_the_adjacent_crate_and_toolchain(self) -> None:
        self._record()
        verified = MODULE.verify_file(
            self.manifest,
            GIT_SHA,
            self.receipt,
            runner=FakeRunner(),
        )
        self.assertEqual(verified["crate"]["bytes"], len(self.crate.read_bytes()))

        self.crate.write_bytes(b"different crate bytes")
        with self.assertRaisesRegex(MODULE.EvidenceError, "receipt_crate_binding"):
            MODULE.verify_file(
                self.manifest,
                GIT_SHA,
                self.receipt,
                runner=FakeRunner(),
            )

    def test_rejects_false_status_and_wrong_release_bindings(self) -> None:
        mutations = {
            "passed": lambda payload: payload.__setitem__("passed", False),
            "version": lambda payload: payload.__setitem__("version", "0.1.2"),
            "git_rev": lambda payload: payload.__setitem__("git_rev", "0" * 40),
            "crate_digest": lambda payload: payload["crate"].__setitem__(
                "sha256", "0" * 64
            ),
            "crate_size": lambda payload: payload["crate"].__setitem__("bytes", 1),
            "registry": lambda payload: payload.__setitem__("registry", "private"),
            "argv": lambda payload: payload.__setitem__(
                "argv", ["cargo", "publish", "--dry-run", "--locked"]
            ),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                self._write_mutation(mutate)
                with self.assertRaises(MODULE.EvidenceError):
                    MODULE.verify_file(
                        self.manifest,
                        GIT_SHA,
                        self.receipt,
                        runner=FakeRunner(),
                    )

    def test_rejects_wrong_types_unknown_keys_and_tool_identity(self) -> None:
        mutations = {
            "passed_int": lambda payload: payload.__setitem__("passed", 1),
            "crate_bytes_bool": lambda payload: payload["crate"].__setitem__(
                "bytes", True
            ),
            "argv_scalar": lambda payload: payload.__setitem__(
                "argv", "cargo publish --dry-run --locked --registry crates-io"
            ),
            "top_level_key": lambda payload: payload.__setitem__("output", "hidden"),
            "crate_key": lambda payload: payload["crate"].__setitem__("path", "hidden"),
            "cargo_identity": lambda payload: payload["toolchain"]["cargo"].__setitem__(
                "commit_hash", "0" * 40
            ),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                self._write_mutation(mutate)
                with self.assertRaises(MODULE.EvidenceError):
                    MODULE.verify_file(
                        self.manifest,
                        GIT_SHA,
                        self.receipt,
                        runner=FakeRunner(),
                    )

    def test_rejects_duplicate_json_keys_and_uppercase_expected_sha(self) -> None:
        self.receipt.write_text(
            '{"schema":"one","schema":"two"}\n', encoding="utf-8"
        )
        with self.assertRaisesRegex(MODULE.EvidenceError, "receipt_duplicate_key"):
            MODULE.verify_file(
                self.manifest,
                GIT_SHA,
                self.receipt,
                runner=FakeRunner(),
            )
        with self.assertRaisesRegex(MODULE.EvidenceError, "git_rev"):
            MODULE.verify_file(
                self.manifest,
                GIT_SHA.upper(),
                self.receipt,
                runner=FakeRunner(),
            )

    def test_runner_requires_the_receipt_next_to_target_package_crate(self) -> None:
        with self.assertRaisesRegex(MODULE.EvidenceError, "receipt_output_location"):
            MODULE.run_and_write(
                self.manifest,
                GIT_SHA,
                self.root / MODULE.RECEIPT_NAME,
                runner=FakeRunner(),
            )


if __name__ == "__main__":
    unittest.main()
