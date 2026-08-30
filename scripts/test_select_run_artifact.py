#!/usr/bin/env python3
"""Tests for attempt-aware immutable workflow artifact selection."""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = ROOT / "scripts" / "select_run_artifact.py"
SPEC = importlib.util.spec_from_file_location("select_run_artifact", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)

PREFIX = f"rxls-wasm-{'a' * 40}-123-"


def payload(artifacts):
    return {"total_count": len(artifacts), "artifacts": artifacts}


def artifact(attempt: int, **overrides):
    row = {
        "id": 1000 + attempt,
        "name": f"{PREFIX}{attempt}",
        "size_in_bytes": 2048,
        "expired": False,
        "digest": "sha256:" + "b" * 64,
    }
    row.update(overrides)
    return row


class SelectRunArtifactTests(unittest.TestCase):
    def test_current_attempt_wins(self) -> None:
        selected = MODULE.select_artifact(
            payload([artifact(1), artifact(2)]),
            name_prefix=PREFIX,
            current_attempt=2,
        )
        self.assertEqual(selected["artifact_id"], 1002)
        self.assertEqual(selected["source_attempt"], 2)

    def test_failed_job_rerun_reuses_latest_prior_attempt(self) -> None:
        selected = MODULE.select_artifact(
            payload([artifact(1)]),
            name_prefix=PREFIX,
            current_attempt=3,
        )
        self.assertEqual(selected["artifact_name"], f"{PREFIX}1")

    def test_unrelated_artifacts_are_ignored(self) -> None:
        selected = MODULE.select_artifact(
            payload(
                [
                    {**artifact(1), "name": "other-artifact"},
                    artifact(2),
                ]
            ),
            name_prefix=PREFIX,
            current_attempt=2,
        )
        self.assertEqual(selected["source_attempt"], 2)

    def test_malformed_or_untrusted_candidates_fail_closed(self) -> None:
        mutations = {
            "missing": [],
            "bad_suffix": [{**artifact(1), "name": f"{PREFIX}latest"}],
            "future": [artifact(3)],
            "duplicate": [artifact(1), artifact(1, id=2001)],
            "zero_id": [artifact(1, id=0)],
            "zero_size": [artifact(1, size_in_bytes=0)],
            "expired": [artifact(1, expired=True)],
            "missing_digest": [artifact(1, digest=None)],
            "bad_digest": [artifact(1, digest="sha256:abcd")],
        }
        for name, artifacts in mutations.items():
            with self.subTest(name=name), self.assertRaises(MODULE.SelectionError):
                MODULE.select_artifact(
                    payload(artifacts),
                    name_prefix=PREFIX,
                    current_attempt=2,
                )


if __name__ == "__main__":
    unittest.main()
