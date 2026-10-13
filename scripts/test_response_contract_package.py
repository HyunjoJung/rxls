#!/usr/bin/env python3
"""Focused checks for the internal response-contract package members."""

import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ResponseContractPackageTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location(
            "check_render_package", ROOT / "scripts/check_render_package.py"
        )
        cls.checker = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.checker)

    def test_internal_module_and_declaration_are_required_not_public(self):
        self.assertIn("js/response-contracts.mjs", self.checker.EXPECTED_FILES)
        self.assertIn("js/response-contracts.d.mts", self.checker.EXPECTED_FILES)
        self.assertEqual(
            self.checker.DECLARATION_MODULES["js/response-contracts.mjs"],
            "js/response-contracts.d.mts",
        )
        self.assertNotIn("./response-contracts", self.checker.EXPECTED_EXPORTS)

    def test_internal_runtime_and_declaration_exports_match(self):
        # Other modules are deliberately synthetic: this is a focused unit test
        # of registration and declaration checks, not an installed-package gate.
        payloads = {}
        for runtime, declaration in self.checker.DECLARATION_MODULES.items():
            payloads[runtime] = b"export const fixture = 1;\n"
            payloads[declaration] = b"export const fixture: number;\n"
        for relative in ("js/response-contracts.mjs", "js/response-contracts.d.mts"):
            payloads[relative] = (ROOT / "bindings/render-wasm" / relative).read_bytes()
        self.assertEqual(self.checker._declaration_errors(payloads, "fixture"), [])
        del payloads["js/response-contracts.d.mts"]
        errors = self.checker._declaration_errors(payloads, "fixture")
        self.assertTrue(any("response-contracts.d.mts" in error for error in errors))

    def test_mismatched_internal_declaration_is_rejected(self):
        payloads = {}
        for runtime, declaration in self.checker.DECLARATION_MODULES.items():
            payloads[runtime] = b"export const fixture = 1;\n"
            payloads[declaration] = b"export const fixture: number;\n"
        payloads["js/response-contracts.mjs"] = (
            ROOT / "bindings/render-wasm/js/response-contracts.mjs"
        ).read_bytes()
        errors = self.checker._declaration_errors(payloads, "fixture")
        self.assertTrue(any("runtime declarations differ for js/response-contracts.mjs" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
