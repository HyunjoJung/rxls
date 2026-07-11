#!/usr/bin/env python3
"""Tests for public release hygiene and artifact manifests."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import tomllib
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
HYGIENE = ROOT / "scripts" / "public_hygiene_audit.py"
MANIFEST = ROOT / "scripts" / "release_manifest.py"


class ReleaseToolTests(unittest.TestCase):
    def test_wasm_cdylib_is_isolated_from_native_artifacts(self) -> None:
        root_manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        binding_manifest = tomllib.loads(
            (ROOT / "bindings" / "wasm" / "Cargo.toml").read_text(encoding="utf-8")
        )

        self.assertEqual(root_manifest["lib"]["crate-type"], ["rlib"])
        self.assertEqual(root_manifest["bin"][0]["name"], "rxls")
        self.assertEqual(binding_manifest["lib"]["crate-type"], ["cdylib", "rlib"])
        self.assertFalse(binding_manifest["package"]["publish"])
        self.assertFalse(
            binding_manifest["dependencies"]["rxls"]["default-features"]
        )

    def test_hygiene_detects_secret_local_path_and_internal_trace(self) -> None:
        module = _load("public_hygiene_audit", HYGIENE)
        text = "\n".join(
            [
                "token=" + "sk-" + "a" * 24,
                "path=" + "C:" + r"\Users\joe\private",
                "docs=" + "rxls" + "-internal-docs",
            ]
        )

        kinds = {finding.kind for finding in module.scan_text("sample.txt", text)}

        self.assertEqual(
            kinds, {"openai_api_key", "windows_home_path", "internal_docs_trace"}
        )

    def test_hygiene_scans_office_member_text(self) -> None:
        module = _load("public_hygiene_audit", HYGIENE)
        with tempfile.TemporaryDirectory() as tmp:
            package = Path(tmp) / "sample.xlsx"
            with zipfile.ZipFile(package, "w") as archive:
                archive.writestr(
                    "xl/workbook.xml",
                    "<workbook><path>" + "/home/" + "joe/private</path></workbook>",
                )

            findings = module.scan_office_package(package, "sample.xlsx")

        self.assertEqual([finding.kind for finding in findings], ["linux_home_path"])

    def test_hygiene_rejects_backslash_office_member_traversal(self) -> None:
        module = _load("public_hygiene_audit", HYGIENE)
        with tempfile.TemporaryDirectory() as tmp:
            package = Path(tmp) / "sample.xlsx"
            with zipfile.ZipFile(package, "w") as archive:
                archive.writestr(r"..\secret.xml", "<clean/>")

            findings = module.scan_office_package(package, "sample.xlsx")

        self.assertEqual([finding.kind for finding in findings], ["unsafe_office_member"])

    def test_release_manifest_is_sorted_and_checksummed(self) -> None:
        module = _load("release_manifest", MANIFEST)
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            first = base / "z.crate"
            second = base / "a.crate"
            first.write_bytes(b"z")
            second.write_bytes(b"a")
            hygiene = base / "hygiene.json"
            hygiene.write_text(
                json.dumps(
                    {
                        "schema": "rxls.public-hygiene-audit.v1",
                        "passed": True,
                        "findings": [],
                    }
                ),
                encoding="utf-8",
            )

            result = module.release_manifest(
                [first, second], "0.1.0", "a" * 40, hygiene
            )

        self.assertEqual(
            [artifact["name"] for artifact in result["artifacts"]],
            ["a.crate", "z.crate"],
        )
        self.assertEqual(
            result["artifacts"][0]["sha256"],
            "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb",
        )

    def test_release_manifest_rejects_failed_hygiene(self) -> None:
        module = _load("release_manifest", MANIFEST)
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            artifact = base / "rxls.crate"
            artifact.write_bytes(b"crate")
            hygiene = base / "hygiene.json"
            hygiene.write_text(
                json.dumps(
                    {
                        "schema": "rxls.public-hygiene-audit.v1",
                        "passed": False,
                        "findings": [{"kind": "secret"}],
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(ValueError, "did not pass"):
                module.release_manifest([artifact], "0.1.0", "b" * 40, hygiene)

    def test_release_manifest_accepts_semver_prerelease_and_build(self) -> None:
        module = _load("release_manifest", MANIFEST)
        for version in [
            "0.0.0",
            "1.2.3-rc.1+build.5",
            "1.0.0-0.3.7",
            "1.0.0-x.7.z.92",
            "1.0.0+001",
        ]:
            with self.subTest(version=version):
                self.assertIsNotNone(module.SEMVER.fullmatch(version))

    def test_release_manifest_rejects_invalid_semver(self) -> None:
        module = _load("release_manifest", MANIFEST)
        for version in [
            "1.2",
            "01.2.3",
            "1.02.3",
            "1.2.3-01",
            "1.2.3-rc..1",
            "1.2.3+",
        ]:
            with self.subTest(version=version):
                self.assertIsNone(module.SEMVER.fullmatch(version))


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


if __name__ == "__main__":
    unittest.main()
