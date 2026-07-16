#!/usr/bin/env python3
"""Tests for the pinned render-oracle font pack."""

from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "fetch-render-fonts.py"
LOCK = ROOT / "scripts" / "render-fonts-lock.json"


def load_module():
    spec = importlib.util.spec_from_file_location("fetch_render_fonts", SCRIPT)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def payload_row(payload: bytes, **extra: object) -> dict[str, object]:
    return {
        "bytes": len(payload),
        "sha256": hashlib.sha256(payload).hexdigest(),
        **extra,
    }


def synthetic_lock(root: Path) -> tuple[dict, bytes]:
    regular = b"synthetic regular font"
    bold = b"synthetic bold font"
    license_payload = b"synthetic OFL license"
    archive_path = root / "source.zip"
    with zipfile.ZipFile(archive_path, "w") as archive:
        archive.writestr("fonts/Bold.ttf", bold)
        archive.writestr("fonts/Regular.ttf", regular)
        archive.writestr("OFL.txt", license_payload)
    archive_payload = archive_path.read_bytes()
    document = {
        "aliases": [
            {"family": "Legacy Sans", "substitute": "Fixture Sans"},
        ],
        "default_output": "local/render-fonts/test-pack",
        "license": "SIL-OFL-1.1",
        "schema": "rxls.render-fonts-lock.v1",
        "sources": [
            {
                "archive": payload_row(archive_payload, url=archive_path.as_uri()),
                "commit": "a" * 40,
                "fonts": [
                    payload_row(
                        bold,
                        family="Fixture Sans",
                        output="Fixture-Bold.ttf",
                        source_path="fonts/Bold.ttf",
                        style="normal",
                        weight=700,
                    ),
                    payload_row(
                        regular,
                        family="Fixture Sans",
                        output="Fixture-Regular.ttf",
                        source_path="fonts/Regular.ttf",
                        style="normal",
                        weight=400,
                    ),
                ],
                "id": "fixture-font",
                "license": payload_row(
                    license_payload,
                    output="fixture-OFL.txt",
                    source_path="OFL.txt",
                    url="https://example.invalid/" + "a" * 40 + "/OFL.txt",
                ),
                "release_tag": "fixture-v1",
                "repo": "fixture/fonts",
            }
        ],
    }
    encoded = (json.dumps(document, indent=2, sort_keys=True) + "\n").encode()
    return document, encoded


class RenderFontLockTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.module = load_module()

    def test_checked_in_lock_is_ofl_pinned_and_exact(self) -> None:
        lock, _ = self.module.load_lock(LOCK)
        self.assertEqual(lock["license"], "SIL-OFL-1.1")
        self.assertEqual(
            [source["id"] for source in lock["sources"]],
            [
                "arimo",
                "caladea",
                "carlito",
                "cousine",
                "noto-sans-arabic",
                "noto-sans-cjk-kr",
                "noto-sans-hebrew",
                "tinos",
            ],
        )
        fonts = [font for source in lock["sources"] for font in source["fonts"]]
        self.assertEqual(len(fonts), 26)
        self.assertEqual({font["weight"] for font in fonts}, {400, 700})
        self.assertTrue(all(len(source["commit"]) == 40 for source in lock["sources"]))
        self.assertTrue(all(len(font["sha256"]) == 64 for font in fonts))
        self.assertEqual(
            lock["aliases"],
            [
                {"family": "Arial", "substitute": "Arimo"},
                {"family": "Calibri", "substitute": "Carlito"},
                {"family": "Cambria", "substitute": "Caladea"},
                {"family": "Courier New", "substitute": "Cousine"},
                {"family": "Helvetica", "substitute": "Arimo"},
                {"family": "Helvetica Neue", "substitute": "Arimo"},
                {"family": "Liberation Mono", "substitute": "Cousine"},
                {"family": "Liberation Sans", "substitute": "Arimo"},
                {"family": "Liberation Serif", "substitute": "Tinos"},
                {"family": "Times New Roman", "substitute": "Tinos"},
            ],
        )
        selected_hashes = {
            font["output"]: font["sha256"]
            for font in fonts
            if font["output"].endswith(("-Regular.ttf", "-Bold.ttf"))
            and font["family"] in {"Arimo", "Caladea", "Carlito", "Cousine", "Tinos"}
        }
        self.assertEqual(
            selected_hashes,
            {
                "Arimo-Bold.ttf": "d7a8b187cf8444d4cfee102e8eae9e3043682fd5106d5d33ed677fe268a0e2ba",
                "Arimo-Regular.ttf": "41b22bc8f0b51f932825d37bc55b5eb6ba67dfe599a626e4aff2b43b624f9f8c",
                "Caladea-Bold.ttf": "ae3cb2dcbc925809dd29d2a44e9802211cab66be541bacbfc9c08c74b27c3742",
                "Caladea-Regular.ttf": "f1e899278b7b4491aba5b6a8253c4b04c050cc59b21865be5c37559a775153cd",
                "Carlito-Bold.ttf": "bb5d20f79b82599ec72983597437373a80f2d2085fa91fc144fd74e876a594db",
                "Carlito-Regular.ttf": "f6418f708baede9789daef5d458c0f53d2a888af9820e8062934e504fedc6595",
                "Cousine-Bold.ttf": "331215ec6445f41e98d8971251bb6237e722907f4101faae990583accfe79545",
                "Cousine-Regular.ttf": "5a57f0184000371cb22fe3fcea4c500354cab1a69efaf7beaf3e6eca6ecfefea",
                "Tinos-Bold.ttf": "393269dbab8899f938db19783eca5eac92eb431f7ae0ab45b8349ca895f1a06b",
                "Tinos-Regular.ttf": "60a0e8ef0c04dd5dd69ffe91025fa2ae5836cbd35600a82ba031977557e2cb61",
            },
        )

    def test_lock_rejects_duplicate_or_unsafe_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            lock, _ = synthetic_lock(Path(raw))
            lock["sources"][0]["archive"]["url"] = (
                "https://github.com/fixture/fonts/releases/download/v1/source.zip"
            )
            lock["sources"][0]["fonts"][1]["output"] = "Fixture-Bold.ttf"
            with self.assertRaisesRegex(self.module.FontPackError, "duplicate_output"):
                self.module.validate_lock(lock)
            lock["sources"][0]["fonts"][1]["output"] = "../escape.ttf"
            with self.assertRaisesRegex(self.module.FontPackError, "unsafe_path"):
                self.module.validate_lock(lock)

    def test_lock_rejects_alias_reordering_duplicates_unknown_targets_and_extra_fields(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            lock, _ = synthetic_lock(Path(raw))
            lock["sources"][0]["archive"]["url"] = (
                "https://github.com/fixture/fonts/releases/download/v1/source.zip"
            )
            lock["aliases"] = [
                {"family": "Zulu", "substitute": "Fixture Sans"},
                {"family": "Alpha", "substitute": "Fixture Sans"},
            ]
            with self.assertRaisesRegex(self.module.FontPackError, "alias_order"):
                self.module.validate_lock(lock)
            lock["aliases"] = [
                {"family": "Alpha", "substitute": "Missing Family"},
            ]
            with self.assertRaisesRegex(self.module.FontPackError, "alias_substitute"):
                self.module.validate_lock(lock)
            lock["aliases"] = [
                {
                    "family": "Alpha",
                    "substitute": "Fixture Sans",
                    "unbounded": True,
                },
            ]
            with self.assertRaisesRegex(self.module.FontPackError, "alias_row"):
                self.module.validate_lock(lock)


class RenderFontMaterializationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.module = load_module()

    def test_acquire_and_offline_verify_are_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            lock, lock_bytes = synthetic_lock(root)
            destination = root / "pack"

            first = self.module.acquire(lock, lock_bytes, destination)
            first_manifest = (destination / "manifest.json").read_bytes()
            verified = self.module.verify(lock, lock_bytes, destination)
            second = self.module.acquire(lock, lock_bytes, destination)

            self.assertEqual(first, verified)
            self.assertEqual(first, second)
            self.assertEqual(first_manifest, (destination / "manifest.json").read_bytes())
            self.assertEqual(first["schema"], "rxls.render-font-pack.v1")
            self.assertEqual(len(first["fonts"]), 2)
            self.assertEqual(
                first["aliases"],
                [{"family": "Legacy Sans", "substitute": "Fixture Sans"}],
            )
            configuration = (destination / "fonts.conf").read_bytes()
            self.assertIn(b'<dir prefix="relative">fonts</dir>', configuration)
            self.assertIn(b"<family>Legacy Sans</family>", configuration)
            self.assertIn(b"<family>Fixture Sans</family>", configuration)
            self.assertNotIn(b"<family>Noto Sans CJK KR</family>", configuration)
            self.assertGreaterEqual(configuration.count(b"<family>Fixture Sans</family>"), 4)

    def test_verify_rejects_font_manifest_and_file_set_tampering(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            lock, lock_bytes = synthetic_lock(root)
            destination = root / "pack"
            self.module.acquire(lock, lock_bytes, destination)

            font = destination / "fonts" / "Fixture-Regular.ttf"
            font.write_bytes(font.read_bytes() + b"tamper")
            with self.assertRaisesRegex(self.module.FontPackError, "font_identity"):
                self.module.verify(lock, lock_bytes, destination)

            self.module.acquire(lock, lock_bytes, destination)
            (destination / "unexpected.txt").write_text("unexpected", encoding="utf-8")
            with self.assertRaisesRegex(self.module.FontPackError, "pack_file_set"):
                self.module.verify(lock, lock_bytes, destination)

            self.module.acquire(lock, lock_bytes, destination)
            manifest_path = destination / "manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["aliases"][0]["substitute"] = "Tampered Sans"
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(self.module.FontPackError, "manifest_aliases"):
                self.module.verify(lock, lock_bytes, destination)

    def test_cli_destination_guard_keeps_payloads_under_local(self) -> None:
        with self.assertRaisesRegex(
            self.module.FontPackError, "destination_outside_local"
        ):
            self.module.resolve_destination("/tmp/rxls-render-fonts")


if __name__ == "__main__":
    unittest.main()
