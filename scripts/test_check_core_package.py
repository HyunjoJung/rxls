from __future__ import annotations

import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "check_core_package", ROOT / "scripts" / "check_core_package.py"
)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


FEATURES = """
[features]
chrono = ["dep:chrono"]
cli = []
default = ["xlsx", "cli"]
full = ["xlsx", "xlsb", "ods", "serde", "chrono"]
ods = ["dep:zip", "dep:quick-xml"]
serde = ["dep:serde"]
xlsb = ["xlsx"]
xlsx = ["dep:zip", "dep:quick-xml"]
"""
DEPENDENCIES = "\n".join(
    f'[dependencies.{name}]\nversion = "1"'
    for name in sorted(MODULE.ALLOWED_DEPENDENCIES)
)
MANIFEST = f"""
[package]
name = "rxls"
version = "0.1.2"
rust-version = "1.85"
{FEATURES}
{DEPENDENCIES}
"""


def write_crate(
    path: Path,
    extra: dict[str, bytes] | None = None,
    duplicate: str | None = None,
) -> None:
    files = {
        "Cargo.lock": b"lock",
        "Cargo.toml": MANIFEST.encode(),
        "Cargo.toml.orig": b"original",
        "LICENSE": b"MIT",
        "README.md": b"rxls",
        "src/lib.rs": b"#![forbid(unsafe_code)]",
    }
    files.update(extra or {})
    with tarfile.open(path, "w:gz") as package:
        for relative, payload in files.items():
            info = tarfile.TarInfo(f"rxls-0.1.2/{relative}")
            info.size = len(payload)
            package.addfile(info, io.BytesIO(payload))
        if duplicate is not None:
            payload = b"duplicate"
            info = tarfile.TarInfo(f"rxls-0.1.2/{duplicate}")
            info.size = len(payload)
            package.addfile(info, io.BytesIO(payload))


class CorePackageGateTests(unittest.TestCase):
    def test_accepts_bounded_core_only_package(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(crate)
            errors, report = MODULE.validate(crate)
        self.assertEqual(errors, [])
        self.assertTrue(report["passed"])
        self.assertEqual(report["dependencies"], sorted(MODULE.ALLOWED_DEPENDENCIES))

    def test_rejects_render_tree_and_internal_plan(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(
                crate,
                {
                    "render/src/lib.rs": b"heavy",
                    "ROADMAP-private.md": b"internal",
                },
            )
            errors, _ = MODULE.validate(crate)
        self.assertIn("forbidden package subtree: render", errors)
        self.assertIn("internal planning document entered the package", errors)

    def test_rejects_render_only_script(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(
                crate,
                {"scripts/libreoffice-render-parity.py": b"heavy oracle"},
            )
            errors, _ = MODULE.validate(crate)
        self.assertIn(
            "render-only script entered the core package: libreoffice-render-parity.py",
            errors,
        )

    def test_rejects_absolute_fidelity_gate_script(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(
                crate,
                {"scripts/check-render-fidelity-targets.py": b"render-only gate"},
            )
            errors, _ = MODULE.validate(crate)
        self.assertIn(
            "render-only script entered the core package: check-render-fidelity-targets.py",
            errors,
        )

    def test_rejects_authored_print_oracle_scripts(self) -> None:
        for name in (
            "check-authored-print-parity.py",
            "test_check_authored_print_parity.py",
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                crate = Path(directory) / "rxls.crate"
                write_crate(crate, {f"scripts/{name}": b"render-only print oracle"})
                errors, _ = MODULE.validate(crate)
            self.assertIn(
                f"render-only script entered the core package: {name}",
                errors,
            )

    def test_rejects_reviewed_render_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(
                crate,
                {"scripts/render-parity-baseline-full.json": b"hosted evidence"},
            )
            errors, _ = MODULE.validate(crate)
        self.assertIn(
            "render-only script entered the core package: "
            "render-parity-baseline-full.json",
            errors,
        )

    def test_rejects_render_oracle_script_subtree(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(
                crate,
                {"scripts/render-oracle-container/Containerfile": b"FROM pinned"},
            )
            errors, _ = MODULE.validate(crate)
        self.assertIn(
            "render-only script subtree entered the core package: render-oracle-container",
            errors,
        )

    def test_rejects_host_oracle_identity_files(self) -> None:
        names = (
            "render-oracle-host-profile.xcu",
            "render-oracle-host-requirements.txt",
            "render-oracle-host-tools-lock.json",
            "render-oracle-host-tools.py",
            "test_render_oracle_host_tools.py",
        )
        for name in names:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                crate = Path(directory) / "rxls.crate"
                write_crate(crate, {f"scripts/{name}": b"render-only identity"})
                errors, _ = MODULE.validate(crate)
            self.assertIn(
                f"render-only script entered the core package: {name}",
                errors,
            )

    def test_rejects_unpacked_size_over_budget(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(crate, {"tests/oversized.bin": b"0" * (MODULE.MAX_UNPACKED_BYTES + 1)})
            errors, _ = MODULE.validate(crate)
        self.assertTrue(any("unpacked bytes" in error for error in errors))

    def test_rejects_duplicate_member_paths(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(crate, duplicate="README.md")
            errors, _ = MODULE.validate(crate)
        self.assertIn("archive contains a duplicate member path", errors)


if __name__ == "__main__":
    unittest.main()
