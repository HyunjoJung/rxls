"""Regression tests for locked renderer identity in a packaged extension."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
VERIFIER = ROOT / "extensions" / "vscode" / "scripts" / "verify_vsix.py"


class VerifyVsixTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        spec = importlib.util.spec_from_file_location("verify_vsix", VERIFIER)
        assert spec is not None and spec.loader is not None
        cls.verifier = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.verifier)

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.renderer = self.root / "renderer"
        self.renderer.mkdir()
        self.vsix = self.root / "preview.vsix"
        self.manifest = {
            "name": "rxls-spreadsheet-preview",
            "publisher": "HyunjoJung",
            "version": "0.1.0",
            "engines": {"vscode": "^1.96.0"},
            "capabilities": {
                "untrustedWorkspaces": {"supported": True},
                "virtualWorkspaces": {"supported": True},
            },
            "devDependencies": {"@rxls/render-worker": "0.3.0"},
        }
        self.build_manifest = {
            "schema": "rxls.vscode-viewer.v1",
            "renderer": {
                "name": "@rxls/render-worker",
                "version": "0.3.0",
                "integrity": "sha512-locked-renderer",
            },
            "workerBundle": {
                "path": "runtime/vscode-worker.js",
                "format": "classic-single-file",
                "sha256": self.verifier.sha256(b"rxls.vscode.worker.bootstrap.v1"),
                "bundler": {
                    "name": "esbuild",
                    "version": "0.28.2",
                    "integrity": "sha512-locked-bundler",
                },
            },
        }
        self.entries = {name: b"fixture" for name in self.verifier.REQUIRED}
        self.entries["extension/media/icon.png"] = (
            b"\x89PNG\r\n\x1a\n" + b"\0" * 8 + struct.pack(">II", 128, 128)
        )
        self.entries["extension/media/viewer/runtime/vscode-worker.js"] = (
            b"rxls.vscode.worker.bootstrap.v1"
        )
        for relative in (
            "LICENSE", "THIRD_PARTY_NOTICES.txt", "package.json",
            "js/client.mjs", "js/protocol.mjs", "js/worker-runtime.mjs",
            "js/worker.mjs", "pkg/rxls_render_wasm_bg.wasm",
            "pkg/rxls_render_wasm.js",
        ):
            payload = b"locked renderer fixture"
            if relative == "package.json":
                payload = json.dumps(
                    {"name": "@rxls/render-worker", "version": "0.3.0"}
                ).encode()
            destination = self.renderer / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(payload)
            self.entries[f"extension/media/viewer/runtime/{relative}"] = payload

    def package(self) -> None:
        self.entries["extension/package.json"] = json.dumps(self.manifest).encode()
        self.entries["extension/media/viewer/build-manifest.json"] = json.dumps(
            self.build_manifest
        ).encode()
        with zipfile.ZipFile(self.vsix, "w") as archive:
            for name, payload in sorted(self.entries.items()):
                archive.writestr(zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0)), payload)

    def verify(self) -> None:
        self.package()
        with contextlib.redirect_stdout(io.StringIO()):
            self.verifier.verify(self.vsix, self.renderer, None)

    def test_accepts_renderer_selected_by_exact_manifest_pin(self) -> None:
        self.verify()

    def test_rejects_stale_renderer_build_manifest(self) -> None:
        self.build_manifest["renderer"]["version"] = "0.2.0"
        with self.assertRaisesRegex(ValueError, "viewer build manifest is invalid"):
            self.verify()

    def test_rejects_mutable_or_mismatched_dependency_pin(self) -> None:
        for version in ("^0.3.0", "0.2.0"):
            with self.subTest(version=version):
                self.manifest["devDependencies"]["@rxls/render-worker"] = version
                with self.assertRaisesRegex(ValueError, "viewer build manifest is invalid"):
                    self.verify()

    def test_rejects_substituted_renderer_bytes(self) -> None:
        self.entries["extension/media/viewer/runtime/js/client.mjs"] = b"substituted"
        with self.assertRaisesRegex(ValueError, "renderer byte mismatch"):
            self.verify()


if __name__ == "__main__":
    unittest.main()
