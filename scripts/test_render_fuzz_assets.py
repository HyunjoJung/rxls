#!/usr/bin/env python3
"""Verify renderer fuzz target registration and exact seed integrity."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]
FUZZ_ROOT = ROOT / "render" / "fuzz"
MANIFEST = FUZZ_ROOT / "seeds" / "manifest.json"


class RenderFuzzAssetTests(unittest.TestCase):
    def test_targets_and_generated_seeds_are_exact(self) -> None:
        cargo = tomllib.loads((FUZZ_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        binaries = {
            entry["name"]: entry["path"]
            for entry in cargo["bin"]
        }
        payload = json.loads(MANIFEST.read_text(encoding="utf-8"))
        self.assertEqual(payload["schema"], "rxls.render-fuzz-seeds.v1")
        self.assertEqual(payload["license"], "generated-test-inputs")

        targets = {entry["target"]: entry for entry in payload["targets"]}
        self.assertEqual(set(targets), set(binaries))
        self.assertEqual(len(targets), 6)
        expected_files: set[Path] = set()
        for target, entry in targets.items():
            self.assertEqual(
                binaries[target],
                f"fuzz_targets/{target}.rs",
            )
            target_path = FUZZ_ROOT / "fuzz_targets" / f"{target}.rs"
            self.assertTrue(target_path.is_file())
            self.assertGreaterEqual(len(entry["seeds"]), 2)
            for seed in entry["seeds"]:
                self.assertEqual(Path(seed["name"]).name, seed["name"])
                path = FUZZ_ROOT / "seeds" / target / seed["name"]
                expected_files.add(path)
                data = path.read_bytes()
                self.assertLessEqual(len(data), 64 << 10)
                self.assertEqual(len(data), seed["bytes"])
                self.assertEqual(hashlib.sha256(data).hexdigest(), seed["sha256"])

        actual_files = {
            path
            for path in (FUZZ_ROOT / "seeds").glob("*/*")
            if path.is_file()
        }
        self.assertEqual(actual_files, expected_files)

    def test_each_target_caps_raw_fuzzer_input(self) -> None:
        support = (FUZZ_ROOT / "fuzz_targets" / "support.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn("MAX_FUZZ_INPUT_BYTES: usize = 64 << 10", support)
        for target in json.loads(MANIFEST.read_text(encoding="utf-8"))["targets"]:
            source = (
                FUZZ_ROOT / "fuzz_targets" / f"{target['target']}.rs"
            ).read_text(encoding="utf-8")
            self.assertIn("support::input(data)", source)


if __name__ == "__main__":
    unittest.main()
