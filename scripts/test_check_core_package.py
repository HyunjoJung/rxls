from __future__ import annotations

import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import tomllib
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
VERSION = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))[
    "package"
]["version"]
MANIFEST = f"""
[package]
name = "rxls"
version = "{VERSION}"
rust-version = "1.85"
{FEATURES}
{DEPENDENCIES}
"""

# This inventory is intentionally independent from the production checker. A
# missing checker entry must fail the test instead of silently reducing its
# generated test cases.
EXPECTED_CI_RELEASE_ONLY_SCRIPTS = {
    "check_cargo_publish_dry_run.py",
    "check_npm_registry_evidence.py",
    "check_workflow_policy.py",
    "reconcile_github_release.py",
    "test_check_cargo_publish_dry_run.py",
    "test_check_npm_registry_evidence.py",
    "test_reconcile_github_release.py",
    "test_release_tools.py",
    "test_workflow_policy.py",
    "test_verify_vscode_vsix.py",
}
EXPECTED_HOSTED_ORACLE_STORAGE_SCRIPTS = {
    "render-oracle-build-storage.sh",
    "test_render_oracle_build_storage.py",
}


def write_crate(
    path: Path,
    extra: dict[str, bytes] | None = None,
    duplicate: str | None = None,
    version: str = VERSION,
    root: str | None = None,
) -> None:
    files = {
        "Cargo.lock": b"lock",
        "Cargo.toml": MANIFEST.replace(
            f'version = "{VERSION}"', f'version = "{version}"', 1
        ).encode(),
        "Cargo.toml.orig": b"original",
        "LICENSE": b"MIT",
        "README.md": b"rxls",
        "src/lib.rs": b"#![forbid(unsafe_code)]",
    }
    files.update(extra or {})
    package_root = root or f"rxls-{version}"
    with tarfile.open(path, "w:gz") as package:
        for relative, payload in files.items():
            info = tarfile.TarInfo(f"{package_root}/{relative}")
            info.size = len(payload)
            package.addfile(info, io.BytesIO(payload))
        if duplicate is not None:
            payload = b"duplicate"
            info = tarfile.TarInfo(f"{package_root}/{duplicate}")
            info.size = len(payload)
            package.addfile(info, io.BytesIO(payload))


class CorePackageGateTests(unittest.TestCase):
    def test_accepts_version_owned_by_source_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            manifest = Path(directory) / "Cargo.toml"
            manifest.write_text(
                '[package]\nname = "rxls"\nversion = "1.2.3"\n',
                encoding="utf-8",
            )
            crate = Path(directory) / "rxls.crate"
            write_crate(crate, version="1.2.3")
            errors, report = MODULE.validate(crate, manifest)
        self.assertEqual(errors, [])
        self.assertEqual(report["package"], {"name": "rxls", "version": "1.2.3"})

    def test_rejects_archive_version_different_from_source_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(crate, version="9.9.9")
            errors, report = MODULE.validate(crate)
        self.assertIn("package identity does not match source manifest", errors)
        self.assertFalse(report["passed"])

    def test_rejects_archive_root_different_from_source_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(crate, root="rxls-9.9.9")
            errors, _ = MODULE.validate(crate)
        self.assertIn("archive root does not match source package identity", errors)

    def test_rejects_missing_or_invalid_source_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "rxls.crate"
            write_crate(crate)
            manifest = Path(directory) / "Cargo.toml"
            for content in (
                None,
                "invalid TOML",
                'package = "not a table"',
                '[package]\nname = "other"\nversion = "1.2.3"\n',
                '[package]\nname = "rxls"\nversion = "1.2.3-rc.1"\n',
            ):
                with self.subTest(content=content):
                    if content is not None:
                        manifest.write_text(content, encoding="utf-8")
                    errors, report = MODULE.validate(crate, manifest)
                    self.assertIn(
                        "source manifest must identify an rxls stable release", errors
                    )
                    self.assertFalse(report["passed"])

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

    def test_rejects_release_only_scripts(self) -> None:
        self.assertEqual(
            MODULE.FORBIDDEN_RELEASE_SCRIPTS,
            EXPECTED_CI_RELEASE_ONLY_SCRIPTS,
        )
        for name in sorted(EXPECTED_CI_RELEASE_ONLY_SCRIPTS):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                crate = Path(directory) / "rxls.crate"
                write_crate(crate, {f"scripts/{name}": b"hosted release tooling"})
                errors, _ = MODULE.validate(crate)
            self.assertIn(
                f"release-only script entered the core package: {name}",
                errors,
            )

    def test_manifest_excludes_ci_and_release_only_scripts(self) -> None:
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        excluded = set(manifest["package"]["exclude"])
        expected = {
            f"scripts/{name}" for name in EXPECTED_CI_RELEASE_ONLY_SCRIPTS
        }
        self.assertEqual(expected - excluded, set())

    def test_manifest_excludes_hosted_oracle_storage_scripts(self) -> None:
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        excluded = set(manifest["package"]["exclude"])
        expected = {
            f"scripts/{name}" for name in EXPECTED_HOSTED_ORACLE_STORAGE_SCRIPTS
        }
        self.assertEqual(expected - excluded, set())

    def test_rejects_hosted_oracle_storage_scripts(self) -> None:
        for name in sorted(EXPECTED_HOSTED_ORACLE_STORAGE_SCRIPTS):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                crate = Path(directory) / "rxls.crate"
                write_crate(crate, {f"scripts/{name}": b"hosted oracle storage tooling"})
                errors, report = MODULE.validate(crate)
                self.assertIn(
                    f"render-only script entered the core package: {name}",
                    errors,
                )
                self.assertFalse(report["passed"])

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
            "check-ooxml-row-oracle.py",
            "generate-ooxml-row-oracle.py",
            "render_parity_geometry_gate.py",
            "render-oracle-host-profile.xcu",
            "render-oracle-host-requirements.txt",
            "render-oracle-host-tools-lock.json",
            "render-oracle-host-tools.py",
            "smoke-render-oracle-runtime.py",
            "summarize-render-oracle-failure.py",
            "test_check_ooxml_row_oracle.py",
            "test_generate_ooxml_row_oracle.py",
            "test_render_oracle_runtime_smoke.py",
            "test_render_oracle_host_tools.py",
            "test_summarize_render_oracle_failure.py",
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
