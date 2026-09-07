"""Regression tests for source package, lockfile, and build-tool identity drift."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import re
import shutil
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "release_identity_graph", ROOT / "scripts" / "check_release_identity.py"
)
assert SPEC is not None and SPEC.loader is not None
IDENTITY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(IDENTITY)


def copy_metadata(root: Path) -> None:
    files = ["CHANGELOG.md", "bindings/render-wasm/toolchain-lock.json", IDENTITY.POLICY]
    for directory in (".", "bindings/wasm", "render", "bindings/render-wasm", "bindings/mcp"):
        files.extend(str(Path(directory) / name) for name in ("Cargo.toml", "Cargo.lock"))
    files.extend(
        f"{directory}/package.json"
        for directory in ("bindings/wasm/npm", "bindings/render-wasm", "viewer", "extensions/vscode")
    )
    files.extend(f"{directory}/package-lock.json" for directory in ("viewer", "extensions/vscode"))
    for relative in files:
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / relative, target)


class PackageIdentityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        copy_metadata(self.root)

    def replace(self, relative: str, old: str, new: str) -> None:
        path = self.root / relative
        text = path.read_text(encoding="utf-8")
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1), encoding="utf-8")

    def assert_detected(self, text: str) -> None:
        errors = IDENTITY.validate(self.root)
        self.assertTrue(any(text in error for error in errors), errors)

    def change_cargo_version(self, relative: str, name: str, version: str = "9.9.9") -> None:
        path = self.root / relative
        text, count = re.subn(
            rf'(name = "{re.escape(name)}"\nversion = ")[^"]+',
            lambda match: match[1] + version,
            path.read_text(encoding="utf-8"), count=1,
        )
        self.assertEqual(count, 1)
        path.write_text(text, encoding="utf-8")

    def change_dependency_version(self, relative: str, name: str, version: str = "9.9.9") -> None:
        path = self.root / relative
        text, count = re.subn(
            rf'(?m)^({re.escape(name)} = (?:\{{ version = )?")[^"]+',
            lambda match: match[1] + version,
            path.read_text(encoding="utf-8"), count=1,
        )
        self.assertEqual(count, 1)
        path.write_text(text, encoding="utf-8")

    def change_json_version(self, relative: str, version: str = "9.9.9") -> None:
        path = self.root / relative
        document = json.loads(path.read_text(encoding="utf-8"))
        document["version"] = version
        path.write_text(json.dumps(document), encoding="utf-8")

    def test_current_independently_versioned_packages_pass(self) -> None:
        self.assertEqual(IDENTITY.validate(self.root), [])

    def test_renderer_manifest_and_local_lock_must_agree(self) -> None:
        self.change_cargo_version("render/Cargo.lock", "rxls-render")
        self.assert_detected("render/Cargo.lock rxls-render")

    def test_mcp_transitive_core_lock_must_match_core(self) -> None:
        self.change_cargo_version("bindings/mcp/Cargo.lock", "rxls")
        self.assert_detected("bindings/mcp/Cargo.lock rxls")

    def test_render_worker_renderer_lock_must_match_native_renderer(self) -> None:
        self.change_cargo_version("bindings/render-wasm/Cargo.lock", "rxls-render")
        self.assert_detected("bindings/render-wasm/Cargo.lock rxls-render")

    def test_duplicate_local_lock_identity_is_rejected(self) -> None:
        path = self.root / "render/Cargo.lock"
        with path.open("a", encoding="utf-8") as output:
            output.write('\n[[package]]\nname = "rxls-render"\nversion = "9.9.9"\n')
        self.assert_detected("render/Cargo.lock rxls-render")

    def test_registry_entry_cannot_replace_a_path_package(self) -> None:
        self.replace("render/Cargo.lock", 'name = "rxls-render"',
                     'name = "rxls-render"\nsource = "registry+https://example.invalid/index"')
        self.assert_detected("render/Cargo.lock rxls-render")

    def test_path_dependency_version_drift_is_rejected(self) -> None:
        self.change_dependency_version("bindings/mcp/Cargo.toml", "rxls")
        self.assert_detected("bindings/mcp/Cargo.toml rxls dependency version")

    def test_path_dependency_source_drift_is_rejected(self) -> None:
        self.replace("bindings/render-wasm/Cargo.toml", 'path = "../../render"',
                     'path = "../../other-render"')
        self.assert_detected("bindings/render-wasm/Cargo.toml rxls-render dependency path")

    def test_mcp_rust_floor_is_not_coupled_to_core(self) -> None:
        self.replace("bindings/mcp/Cargo.toml", 'rust-version = "1.88"',
                     'rust-version = "1.85"')
        self.assert_detected("bindings/mcp/Cargo.toml rust-version")

    def test_render_worker_npm_version_must_match_its_own_crate(self) -> None:
        self.change_json_version("bindings/render-wasm/package.json")
        self.assert_detected("bindings/render-wasm/package.json version")

    def test_viewer_lock_root_identity_must_match_manifest(self) -> None:
        self.change_json_version("viewer/package-lock.json")
        self.assert_detected("viewer/package-lock.json version")

    def test_extension_lock_package_identity_must_match_manifest(self) -> None:
        path = self.root / "extensions/vscode/package-lock.json"
        lock = json.loads(path.read_text(encoding="utf-8"))
        lock["packages"][""]["version"] = "9.9.9"
        path.write_text(json.dumps(lock), encoding="utf-8")
        self.assert_detected("extensions/vscode/package-lock.json packages[''] version")

    def test_wasm_bindgen_manifest_pin_must_match_cli(self) -> None:
        self.change_dependency_version("bindings/wasm/Cargo.toml", "wasm-bindgen")
        self.assert_detected("bindings/wasm/Cargo.toml wasm-bindgen")

    def test_wasm_bindgen_resolved_version_must_match_cli(self) -> None:
        self.change_cargo_version("bindings/render-wasm/Cargo.lock", "wasm-bindgen")
        self.assert_detected("bindings/render-wasm/Cargo.lock wasm-bindgen")

    def test_render_build_rust_matches_declared_floor(self) -> None:
        self.replace("bindings/render-wasm/toolchain-lock.json", '"rust": "1.85.0"',
                     '"rust": "1.88.0"')
        self.assert_detected("bindings/render-wasm/toolchain-lock.json rust")

    def test_native_renderer_can_advance_without_changing_other_package_versions(self) -> None:
        for relative in ("render/Cargo.toml", "render/Cargo.lock", "bindings/render-wasm/Cargo.lock"):
            self.change_cargo_version(relative, "rxls-render")
        self.change_dependency_version("bindings/render-wasm/Cargo.toml", "rxls-render")
        self.assertEqual(IDENTITY.validate(self.root), [])

    def test_render_worker_can_advance_while_extension_keeps_published_pin(self) -> None:
        for relative in ("bindings/render-wasm/Cargo.toml", "bindings/render-wasm/Cargo.lock"):
            self.change_cargo_version(relative, "rxls-render-wasm")
        self.change_json_version("bindings/render-wasm/package.json")
        self.assertEqual(IDENTITY.validate(self.root), [])

    def test_missing_policy_fails_closed(self) -> None:
        (self.root / IDENTITY.POLICY).unlink()
        with self.assertRaises(FileNotFoundError):
            IDENTITY.validate(self.root)

    def test_unknown_policy_schema_fails_closed(self) -> None:
        self.replace(IDENTITY.POLICY, "rxls.package-identities.v1", "rxls.package-identities.v99")
        with self.assertRaisesRegex(ValueError, "unsupported schema"):
            IDENTITY.validate(self.root)


if __name__ == "__main__":
    unittest.main()
