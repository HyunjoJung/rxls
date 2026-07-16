#!/usr/bin/env python3
"""Tests for locked render-worker legal and CycloneDX evidence."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "render_supply_chain.py"


def _load():
    spec = importlib.util.spec_from_file_location("render_supply_chain", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RenderSupplyChainTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.supply = _load()

    def _fixture(self, root: Path) -> tuple[dict, dict]:
        source = "registry+https://github.com/rust-lang/crates.io-index"
        package_specs = (
            ("root", "rxls-render-wasm", "0.1.2", None, "MIT"),
            ("local", "rxls-render", "0.1.0", None, "MIT"),
            ("dep-a", "dep-a", "1.0.0", source, "MIT"),
            ("dep-b", "dep-b", "2.0.0", source, "Apache-2.0"),
            ("build", "build-only", "3.0.0", source, "MIT"),
            ("dev", "dev-only", "4.0.0", source, "MIT"),
        )
        packages = []
        lock_packages = []
        for package_id, name, version, package_source, license_expression in package_specs:
            package_root = root / package_id
            package_root.mkdir()
            (package_root / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
            if package_source is not None:
                (package_root / "LICENSE").write_text(
                    "Shared fixture license\n", encoding="utf-8"
                )
            packages.append(
                {
                    "id": package_id,
                    "name": name,
                    "version": version,
                    "license": license_expression,
                    "license_file": None,
                    "manifest_path": str(package_root / "Cargo.toml"),
                    "source": package_source,
                }
            )
            lock_entry = {"name": name, "version": version}
            if package_source is not None:
                lock_entry.update(
                    {
                        "source": package_source,
                        "checksum": str(len(lock_packages) + 1) * 64,
                    }
                )
            lock_packages.append(lock_entry)

        normal = [{"kind": None, "target": None}]
        metadata = {
            "workspace_members": ["root"],
            "packages": packages,
            "resolve": {
                "nodes": [
                    {
                        "id": "root",
                        "deps": [
                            {"pkg": "local", "dep_kinds": normal},
                            {"pkg": "dep-b", "dep_kinds": normal},
                            {
                                "pkg": "build",
                                "dep_kinds": [{"kind": "build", "target": None}],
                            },
                            {
                                "pkg": "dev",
                                "dep_kinds": [{"kind": "dev", "target": None}],
                            },
                        ],
                    },
                    {
                        "id": "local",
                        "deps": [{"pkg": "dep-a", "dep_kinds": normal}],
                    },
                    {"id": "dep-a", "deps": []},
                    {"id": "dep-b", "deps": []},
                    {"id": "build", "deps": []},
                    {"id": "dev", "deps": []},
                ]
            },
        }
        return metadata, {"version": 4, "package": lock_packages}

    def test_notice_covers_only_normal_target_closure_and_deduplicates_legal_texts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            metadata, lock = self._fixture(Path(temporary))
            notice, summary = self.supply.render_notice(metadata, lock, "a" * 64)
            repeated, _ = self.supply.render_notice(metadata, lock, "a" * 64)

        self.assertEqual(notice, repeated)
        self.assertEqual(summary, {"packages": 2, "legal_texts": 1})
        self.assertIn("PACKAGE: dep-a 1.0.0", notice)
        self.assertIn("PACKAGE: dep-b 2.0.0", notice)
        self.assertNotIn("build-only", notice)
        self.assertNotIn("dev-only", notice)
        self.assertNotIn(temporary, notice)
        self.assertEqual(notice.count("LEGAL TEXT SHA-256:"), 1)

    def test_sbom_is_path_neutral_and_records_exact_normal_dependency_graph(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            metadata, lock = self._fixture(Path(temporary))
            rendered, summary = self.supply.render_sbom(metadata, lock, "a" * 64)

        document = json.loads(rendered)
        component_refs = {item["bom-ref"] for item in document["components"]}
        dependency_graph = {
            item["ref"]: item["dependsOn"] for item in document["dependencies"]
        }
        self.assertEqual(summary, {"components": 3, "dependency_nodes": 4})
        self.assertEqual(
            component_refs,
            {
                "pkg:cargo/rxls-render@0.1.0",
                "pkg:cargo/dep-a@1.0.0",
                "pkg:cargo/dep-b@2.0.0",
            },
        )
        self.assertEqual(
            dependency_graph["pkg:cargo/rxls-render-wasm@0.1.2"],
            ["pkg:cargo/dep-b@2.0.0", "pkg:cargo/rxls-render@0.1.0"],
        )
        self.assertEqual(
            dependency_graph["pkg:cargo/rxls-render@0.1.0"],
            ["pkg:cargo/dep-a@1.0.0"],
        )
        self.assertNotIn("build-only", rendered)
        self.assertNotIn("dev-only", rendered)
        self.assertNotIn(temporary, rendered)

    def test_checked_notice_matches_current_locked_production_closure(self) -> None:
        manifest = ROOT / "bindings" / "render-wasm" / "Cargo.toml"
        metadata = self.supply.cargo_metadata(manifest)
        lock, lock_sha256 = self.supply.cargo_lock(manifest)
        rendered, summary = self.supply.render_notice(metadata, lock, lock_sha256)

        checked = (ROOT / "bindings" / "render-wasm" / "THIRD_PARTY_NOTICES.txt")
        self.assertEqual(checked.read_bytes(), rendered.encode("utf-8"))
        self.assertGreater(summary["packages"], 0)
        self.assertGreater(summary["legal_texts"], 0)

    def test_nested_policy_pins_local_edges_and_exact_unmaintained_exceptions(self) -> None:
        deny = tomllib.loads((ROOT / "deny.toml").read_text(encoding="utf-8"))
        binding = tomllib.loads(
            (ROOT / "bindings" / "render-wasm" / "Cargo.toml").read_text(
                encoding="utf-8"
            )
        )
        renderer = tomllib.loads(
            (ROOT / "render" / "Cargo.toml").read_text(encoding="utf-8")
        )

        self.assertIn("BSD-2-Clause", deny["licenses"]["allow"])
        self.assertEqual(
            deny["advisories"]["ignore"],
            ["RUSTSEC-2026-0192", "RUSTSEC-2026-0206"],
        )
        self.assertEqual(binding["dependencies"]["rxls"]["version"], "0.1.2")
        self.assertEqual(
            binding["dependencies"]["rxls-render"]["version"], "0.1.0"
        )
        self.assertEqual(renderer["dependencies"]["rxls"]["version"], "0.1.2")


if __name__ == "__main__":
    unittest.main()
