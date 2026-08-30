#!/usr/bin/env python3
"""Tests for locked render-worker legal and CycloneDX evidence."""

from __future__ import annotations

import gzip
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import tomllib
import unittest
from unittest import mock


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

    def test_cargo_metadata_decodes_subprocess_output_as_utf8(self) -> None:
        completed = mock.Mock(stdout="{}")
        with mock.patch.object(
            self.supply.subprocess, "run", return_value=completed
        ) as run:
            self.assertEqual(self.supply.cargo_metadata(Path("Cargo.toml")), {})

        self.assertEqual(run.call_args.kwargs["encoding"], "utf-8")
        self.assertTrue(run.call_args.kwargs["text"])

    def _write_crate(
        self,
        path: Path,
        name: str,
        version: str,
        files: dict[str, bytes],
    ) -> str:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("wb") as destination:
            with gzip.GzipFile(
                filename="", fileobj=destination, mode="wb", mtime=0
            ) as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as archive:
                    for relative, payload in sorted(files.items()):
                        member = tarfile.TarInfo(f"{name}-{version}/{relative}")
                        member.mode = 0o644
                        member.mtime = 0
                        member.size = len(payload)
                        archive.addfile(member, io.BytesIO(payload))
        return self.supply.sha256_bytes(path.read_bytes())

    def _archive_path(self, package: dict) -> Path:
        package_root = Path(package["manifest_path"]).parent
        registry_id = package_root.parent.name
        registry_root = package_root.parent.parent.parent
        return (
            registry_root
            / "cache"
            / registry_id
            / f"{package['name']}-{package['version']}.crate"
        )

    def _fixture(
        self,
        root: Path,
        *,
        legal_payload: bytes = b"Shared fixture license\n",
    ) -> tuple[dict, dict]:
        source = "registry+https://github.com/rust-lang/crates.io-index"
        registry_id = "index.crates.io-rxls-test"
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
            if package_source is None:
                package_root = root / package_id
            else:
                package_root = (
                    root / "registry" / "src" / registry_id / f"{name}-{version}"
                )
            package_root.mkdir(parents=True)
            (package_root / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
            if package_source is not None:
                (package_root / "LICENSE").write_text(
                    legal_payload.decode("utf-8"), encoding="utf-8", newline=""
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
                archive = (
                    root
                    / "registry"
                    / "cache"
                    / registry_id
                    / f"{name}-{version}.crate"
                )
                checksum = self._write_crate(
                    archive,
                    name,
                    version,
                    {"Cargo.toml": b"[package]\n", "LICENSE": legal_payload},
                )
                lock_entry.update(
                    {
                        "source": package_source,
                        "checksum": checksum,
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

    def test_notice_normalizes_display_line_endings_but_keeps_raw_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            payload = b"first\r\nsecond\rthird\n"
            metadata, lock = self._fixture(
                Path(temporary), legal_payload=payload
            )

            notice, summary = self.supply.render_notice(
                metadata,
                lock,
                "a" * 64,
            )
            repeated, _ = self.supply.render_notice(
                metadata,
                lock,
                "a" * 64,
            )

        self.assertEqual(notice, repeated)
        self.assertEqual(summary, {"packages": 2, "legal_texts": 1})
        self.assertIn(
            f"LEGAL TEXT SHA-256: {self.supply.sha256_bytes(payload)}",
            notice,
        )
        self.assertIn("first\nsecond\nthird\n----- END LEGAL TEXT -----", notice)
        self.assertNotIn("\r", notice)

    def test_notice_ignores_tampered_extracted_legal_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            metadata, lock = self._fixture(Path(temporary))
            for package in metadata["packages"]:
                if package["source"] is not None:
                    Path(package["manifest_path"]).with_name("LICENSE").write_text(
                        "tampered extracted source\n", encoding="utf-8"
                    )
            notice, _ = self.supply.render_notice(metadata, lock, "a" * 64)

        self.assertIn("Shared fixture license", notice)
        self.assertNotIn("tampered extracted source", notice)

    def test_notice_does_not_require_extracted_legal_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            metadata, lock = self._fixture(Path(temporary))
            for package in metadata["packages"]:
                if package["source"] is not None:
                    Path(package["manifest_path"]).with_name("LICENSE").unlink()
            notice, summary = self.supply.render_notice(metadata, lock, "a" * 64)

        self.assertIn("Shared fixture license", notice)
        self.assertEqual(summary, {"packages": 2, "legal_texts": 1})

    def test_notice_rejects_archive_without_legal_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            metadata, lock = self._fixture(Path(temporary))
            package = next(
                item for item in metadata["packages"] if item["id"] == "dep-a"
            )
            checksum = self._write_crate(
                self._archive_path(package),
                package["name"],
                package["version"],
                {"Cargo.toml": b"[package]\n"},
            )
            lock_entry = next(
                item
                for item in lock["package"]
                if item["name"] == package["name"]
                and item["version"] == package["version"]
            )
            lock_entry["checksum"] = checksum
            with self.assertRaisesRegex(
                self.supply.SupplyChainError,
                "dep-a 1.0.0 has no distributable legal file in its registry archive",
            ):
                self.supply.render_notice(metadata, lock, "a" * 64)

    def test_notice_rejects_missing_registry_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            metadata, lock = self._fixture(root)
            package = next(
                item for item in metadata["packages"] if item["id"] == "dep-a"
            )
            self._archive_path(package).unlink()
            with mock.patch.dict(
                self.supply.os.environ, {"CARGO_HOME": str(root)}, clear=False
            ):
                with self.assertRaisesRegex(
                    self.supply.SupplyChainError,
                    "dep-a 1.0.0 registry archive is missing",
                ):
                    self.supply.render_notice(metadata, lock, "a" * 64)

    def test_notice_rejects_registry_archive_checksum_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            metadata, lock = self._fixture(Path(temporary))
            package = next(
                item for item in metadata["packages"] if item["id"] == "dep-a"
            )
            with self._archive_path(package).open("ab") as archive:
                archive.write(b"tampered")
            with self.assertRaisesRegex(
                self.supply.SupplyChainError,
                "dep-a 1.0.0 registry archive checksum differs from Cargo.lock",
            ):
                self.supply.render_notice(metadata, lock, "a" * 64)

    def test_core_wasm_profile_uses_its_own_root_and_public_notice_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            metadata, lock = self._fixture(Path(temporary))
            metadata["packages"][0]["name"] = "rxls-wasm"
            notice, _ = self.supply.render_notice(
                metadata,
                lock,
                "a" * 64,
                crate_name="rxls-wasm",
                manifest_label=Path("bindings/wasm/Cargo.toml"),
                notice_title="RXLS WASM THIRD-PARTY NOTICES",
            )
            sbom, _ = self.supply.render_sbom(
                metadata,
                lock,
                "a" * 64,
                crate_name="rxls-wasm",
            )

        self.assertTrue(notice.startswith("RXLS WASM THIRD-PARTY NOTICES\n"))
        self.assertIn("- Manifest: bindings/wasm/Cargo.toml", notice)
        self.assertEqual(json.loads(sbom)["metadata"]["component"]["name"], "rxls-wasm")

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
        checked_bytes = checked.read_bytes()
        self.assertNotIn(b"\r", checked_bytes)
        self.assertEqual(checked_bytes, rendered.encode("utf-8"))
        self.assertIn("PACKAGE: subsetter 0.2.6", rendered)
        self.assertIn("subsetter 0.2.6/NOTICE", rendered)
        self.assertIn("PACKAGE: rustc-hash 2.1.3", rendered)
        self.assertGreater(summary["packages"], 0)
        self.assertGreater(summary["legal_texts"], 0)

    def test_checked_core_wasm_notice_matches_current_locked_production_closure(self) -> None:
        manifest = ROOT / "bindings" / "wasm" / "Cargo.toml"
        metadata = self.supply.cargo_metadata(manifest)
        lock, lock_sha256 = self.supply.cargo_lock(manifest)
        rendered, summary = self.supply.render_notice(
            metadata,
            lock,
            lock_sha256,
            crate_name="rxls-wasm",
            manifest_label=Path("bindings/wasm/Cargo.toml"),
            notice_title="RXLS WASM THIRD-PARTY NOTICES",
        )

        checked = ROOT / "bindings" / "wasm" / "THIRD_PARTY_NOTICES.txt"
        checked_bytes = checked.read_bytes()
        self.assertNotIn(b"\r", checked_bytes)
        self.assertEqual(checked_bytes, rendered.encode("utf-8"))
        self.assertIn("PACKAGE: wasm-bindgen 0.2.126", rendered)
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
        self.assertEqual(binding["dependencies"]["rxls"]["version"], "0.1.3")
        self.assertEqual(
            binding["dependencies"]["rxls-render"]["version"], "0.1.0"
        )
        self.assertEqual(renderer["dependencies"]["rxls"]["version"], "0.1.3")

    def test_tracked_renderer_locks_match_the_current_local_dependency_closure(self) -> None:
        lock_paths = (
            ROOT / "render" / "Cargo.lock",
            ROOT / "render" / "perf" / "Cargo.lock",
            ROOT / "render" / "fuzz" / "Cargo.lock",
            ROOT / "bindings" / "render-wasm" / "Cargo.lock",
        )
        for path in lock_paths:
            with self.subTest(path=path.relative_to(ROOT)):
                document = tomllib.loads(path.read_text(encoding="utf-8"))
                packages = document.get("package")
                self.assertIsInstance(packages, list)

                def package(name: str) -> dict:
                    matches = [
                        row
                        for row in packages
                        if isinstance(row, dict) and row.get("name") == name
                    ]
                    self.assertEqual(len(matches), 1, f"{path}: {name}")
                    return matches[0]

                self.assertEqual(package("rxls").get("version"), "0.1.3")
                renderer = package("rxls-render")
                self.assertEqual(renderer.get("version"), "0.1.0")
                self.assertIn("subsetter", renderer.get("dependencies", []))
                self.assertEqual(package("subsetter").get("version"), "0.2.6")
                self.assertEqual(package("rustc-hash").get("version"), "2.1.3")


if __name__ == "__main__":
    unittest.main()
