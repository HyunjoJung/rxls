#!/usr/bin/env python3
"""Tests for public release hygiene and artifact manifests."""

from __future__ import annotations

import importlib.util
import io
import json
import re
import sys
import tarfile
import tempfile
import tomllib
import unittest
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
HYGIENE = ROOT / "scripts" / "public_hygiene_audit.py"
MANIFEST = ROOT / "scripts" / "release_manifest.py"
IDENTITY = ROOT / "scripts" / "check_release_identity.py"
WASM_PACKAGE = ROOT / "scripts" / "check_wasm_package.py"
SBOM = ROOT / "scripts" / "generate-sbom.py"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"


class ReleaseToolTests(unittest.TestCase):
    def test_release_evidence_is_quiet_then_hygiene_checked_after_assembly(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        self.assertEqual(workflow.count("cargo test --quiet --all-features"), 4)
        copy_index = workflow.index("cp target/release-*-evidence.txt dist/")
        hygiene_index = workflow.index(
            "python3 scripts/public_hygiene_audit.py --json dist"
        )
        manifest_index = workflow.index("python3 scripts/release_manifest.py")
        self.assertLess(copy_index, hygiene_index)
        self.assertLess(hygiene_index, manifest_index)

    def test_release_smokes_authored_and_edited_workbooks_independently(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn(
            "api_journeys --all-features --locked -- target/api-journeys-edited.xlsx",
            workflow,
        )
        self.assertIn("target/release-libreoffice-edit-smoke.json", workflow)
        self.assertIn("dist/release-libreoffice-edit-smoke.json", workflow)
        self.assertIn("name: Smoke package-edited XLSM through LibreOffice", workflow)
        self.assertIn(
            'manifest_files("local/public-corpus/manifest.json", {".xlsm"})',
            workflow,
        )
        self.assertIn(
            "from scripts.verify_libreoffice_xlsm import inspect_package",
            workflow,
        )
        self.assertIn(
            '"$xlsm_source" target/libreoffice-package-edited.xlsm',
            workflow,
        )
        self.assertNotIn("libreoffice -env:UserInstallation", workflow)
        self.assertIn("soffice -env:UserInstallation", workflow)
        self.assertIn("xlsm:Calc MS Excel 2007 VBA XML", workflow)
        self.assertIn("target/release-libreoffice-xlsm-edit-smoke.json", workflow)
        self.assertIn("scripts/verify_libreoffice_xlsm.py", workflow)
        self.assertIn("target/release-libreoffice-xlsm-preservation.json", workflow)
        self.assertIn("soffice --version | tee target/release-libreoffice-version.txt", workflow)
        self.assertIn("target/release-libreoffice-xlsx-smoke.txt", workflow)
        self.assertIn("target/release-libreoffice-xlsm-smoke.txt", workflow)
        sanitize_index = workflow.index("scripts/sanitize_release_logs.py")
        self.assertLess(
            workflow.index("scripts/verify_libreoffice_xlsm.py"), sanitize_index
        )
        self.assertLess(
            sanitize_index, workflow.index("name: Generate focused formula")
        )
        self.assertIn("dist/release-libreoffice-xlsm-edit-smoke.json", workflow)
        self.assertIn("dist/release-libreoffice-xlsm-preservation.json", workflow)
        for name in (
            "release-libreoffice-version.txt",
            "release-libreoffice-xlsx-smoke.txt",
            "release-libreoffice-xlsm-smoke.txt",
        ):
            self.assertIn(f"dist/{name}", workflow)
        self.assertEqual(workflow.count('if warnings != []:'), 1)

    def test_release_measures_generated_medium_and_edit_save_workloads(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn(
            "--output target/performance/medium.xlsx --payload-mib 16", workflow
        )
        self.assertIn(
            "--repeat 3 --max-seconds 5 --max-rss-mib 512", workflow
        )
        self.assertIn(
            "--repeat 3 --max-seconds 10 --max-rss-mib 768", workflow
        )
        self.assertIn("--operation edit-save", workflow)
        self.assertIn("dist/release-performance-medium.json", workflow)
        self.assertIn("dist/release-performance-edit.json", workflow)

    def test_release_bundles_native_node_and_browser_wasm_parity_evidence(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn("tee target/wasm-node-smoke.json", workflow)
        self.assertIn("tee target/wasm-browser-smoke.json", workflow)
        for name in (
            "wasm-native-report.json",
            "wasm-node-smoke.json",
            "wasm-browser-smoke.json",
        ):
            self.assertIn(f"dist/{name}", workflow)

    def test_release_verifies_post_publication_install_docs_and_assets(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        publish_index = workflow.index("name: Publish to crates.io")
        release_index = workflow.index("name: Create or update GitHub release")
        smoke_index = workflow.index(
            "name: Verify published crate, WASM, docs, assets, and checksums"
        )
        self.assertLess(publish_index, smoke_index)
        self.assertLess(release_index, smoke_index)
        self.assertIn('rxls = "=$version"', workflow)
        self.assertIn('"https://docs.rs/rxls/$version/rxls/"', workflow)
        download_index = workflow.rindex("gh release download")
        checksum_index = workflow.index("sha256sum --check ./*.sha256")
        wasm_install_index = workflow.index(
            '"$smoke/assets/rxls-wasm-$version.tgz"'
        )
        self.assertLess(download_index, checksum_index)
        self.assertLess(checksum_index, wasm_install_index)
        self.assertNotIn('--pattern "rxls-$version.crate"', workflow)
        self.assertIn('--verify-bundle "$smoke/assets"', workflow)
        self.assertIn("--expected-files 47", workflow)
        self.assertIn("npm install --ignore-scripts --no-audit --no-fund", workflow)
        self.assertIn('require("rxls-wasm")', workflow)
        self.assertIn("assert.equal(maxInputBytes(), 32 * 1024 * 1024)", workflow)
        self.assertIn("JSON.parse(reportJson(bytes))", workflow)
        self.assertIn(
            '"$smoke/wasm-consumer/node_modules/rxls-wasm"', workflow
        )
        self.assertIn("bindings/wasm/tests/browser-smoke.mjs", workflow)

    def test_release_runs_and_bundles_all_fuzz_targets(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn("name: Run release fuzz campaign", workflow)
        self.assertIn("-max_total_time=120 -timeout=10 -rss_limit_mb=2048", workflow)
        self.assertIn("for target_name in parse author edit formula", workflow)
        self.assertIn('FUZZ_NIGHTLY_VERSION: "nightly-2026-07-10"', workflow)
        self.assertIn('CARGO_FUZZ_VERSION: "0.13.2"', workflow)
        self.assertIn("scripts/fuzz_seeds.py materialize", workflow)
        self.assertIn("scripts/fuzz_seeds.py replay", workflow)
        self.assertIn("scripts/record_toolchain_versions.py", workflow)
        self.assertLess(
            workflow.index("scripts/fuzz_seeds.py replay"),
            workflow.index("for target_name in parse author edit formula"),
        )
        self.assertIn("python3 scripts/sanitize_release_logs.py", workflow)
        for target_name in ("parse", "author", "edit", "formula"):
            self.assertIn(f"dist/fuzz-{target_name}.log", workflow)
        for name in (
            "fuzz-seed-manifest.json",
            "fuzz-seed-replay.json",
            "release-toolchain-versions.json",
        ):
            self.assertIn(f"dist/{name}", workflow)

    def test_second_clean_run_compares_every_release_artifact(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn("baseline_run_id:", workflow)
        self.assertIn("actions: read", workflow)
        self.assertIn("name: Compare clean release candidates", workflow)
        self.assertIn("scripts/compare_release_bundles.py", workflow)
        self.assertIn("rxls-release-reproducibility.json", workflow)
        self.assertIn(
            "if: always() && github.event_name == 'workflow_dispatch' && inputs.baseline_run_id != ''",
            workflow,
        )

    def test_tag_publication_requires_immutable_exact_sha_candidate_attestation(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        gate_index = workflow.index(
            "name: Require exact-SHA two-candidate publication attestation"
        )
        publish_index = workflow.index("name: Publish to crates.io")
        release_index = workflow.index("name: Create or update GitHub release")
        self.assertLess(gate_index, publish_index)
        self.assertLess(gate_index, release_index)
        self.assertIn("rxls.release-candidate-attestation.v1", workflow)
        self.assertIn('comparison.get("passed") is not True', workflow)
        self.assertIn('comparison.get("git_rev") != expected_sha', workflow)
        self.assertIn('attestation.get("comparison_run_id")', workflow)
        self.assertIn('record.get("sha256")', workflow)
        self.assertIn('"release_manifest": {', workflow)
        self.assertIn('attestation["release_manifest"]', workflow)
        self.assertIn('baseline_sha" == "$GITHUB_SHA', workflow)
        self.assertGreaterEqual(
            workflow.count('baseline_path" == ".github/workflows/release.yml"'), 2
        )
        self.assertIn(
            "reproducibility-${{ github.sha }}-${{ github.run_attempt }}", workflow
        )
        candidate_download_index = workflow.index(
            '--name "rxls-${version}-release"', gate_index
        )
        tag_compare_index = workflow.index(
            "target/attested-candidate-release dist", gate_index
        )
        self.assertLess(candidate_download_index, tag_compare_index)
        self.assertLess(tag_compare_index, publish_index)

    def test_tag_publication_requires_exact_sha_ci_and_codeql(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        hosted_gate = workflow.index("name: Require successful exact-SHA CI and CodeQL runs")
        attestation_gate = workflow.index(
            "name: Require exact-SHA two-candidate publication attestation"
        )
        publish_index = workflow.index("name: Publish to crates.io")
        self.assertLess(hosted_gate, attestation_gate)
        self.assertLess(hosted_gate, publish_index)
        self.assertIn(
            "require_successful_push ci.yml .github/workflows/ci.yml CI", workflow
        )
        self.assertIn(
            "require_successful_push codeql.yml .github/workflows/codeql.yml CodeQL",
            workflow,
        )
        self.assertIn('--commit "$GITHUB_SHA"', workflow)
        self.assertIn('"$event" == "push"', workflow)

    def test_release_smokes_exact_local_and_registry_crate_distributions(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        local_index = workflow.index("name: Smoke exact packaged crate distribution")
        assembly_index = workflow.index("name: Generate release evidence")
        publish_index = workflow.index("name: Publish to crates.io")
        registry_index = workflow.index("name: Smoke published crates.io distribution")
        self.assertLess(local_index, assembly_index)
        self.assertLess(publish_index, registry_index)
        self.assertIn("--crate target/package/rxls-0.1.2.crate", workflow)
        self.assertIn("--registry-version 0.1.2", workflow)
        self.assertIn("dist/release-crate-distribution-smoke.json", workflow)
        registry_upload = workflow.index(
            "name: Upload published crates.io distribution evidence"
        )
        self.assertGreater(registry_upload, registry_index)
        self.assertIn("target/release-crates-io-distribution-smoke.json", workflow)

    def test_hosted_release_bundle_contract_is_exactly_47_files(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        self.assertEqual(workflow.count("--expected-files 47"), 3)
        generation = workflow.index("name: Generate release evidence")
        manifest_start = workflow.index(
            "python3 scripts/release_manifest.py", generation
        )
        verifier_start = workflow.index(
            "python3 scripts/release_manifest.py", manifest_start + 1
        )
        manifest_block = workflow[manifest_start:verifier_start]
        actual = set(re.findall(r'dist/([^"\\\s]+)', manifest_block))
        expected = {
            "rxls-${version}.crate",
            "rxls-${version}.crate.sha256",
            "rxls-wasm-${version}.tgz",
            "rxls-wasm-${version}.tgz.sha256",
            "wasm-size-report.json",
            "wasm-native-report.json",
            "wasm-node-smoke.json",
            "wasm-browser-smoke.json",
            "rxls-sbom.cdx.json",
            "release-performance-small.json",
            "release-performance-medium.json",
            "release-performance-edit.json",
            "release-performance-largest.json",
            "release-libreoffice-smoke.json",
            "release-libreoffice-edit-smoke.json",
            "release-libreoffice-xlsm-edit-smoke.json",
            "release-libreoffice-xlsm-preservation.json",
            "release-libreoffice-version.txt",
            "release-libreoffice-xlsx-smoke.txt",
            "release-libreoffice-xlsm-smoke.txt",
            "public-hygiene-source.json",
            "public-corpus-baseline.json",
            "public-corpus-expectations.json",
            "release-corpus-report.txt",
            "release-xls-parity-full.txt",
            "release-xls-parity-summary.txt",
            "release-ooxml-parity-full.txt",
            "release-ooxml-parity-summary.txt",
            "release-xlsb-parity-full.txt",
            "release-xlsb-parity-summary.txt",
            "release-ods-parity-full.txt",
            "release-ods-parity-summary.txt",
            "release-formula-evidence.txt",
            "release-evaluation-evidence.txt",
            "release-edit-unit-evidence.txt",
            "release-edit-integration-evidence.txt",
            "fuzz-build.log",
            "fuzz-parse.log",
            "fuzz-author.log",
            "fuzz-edit.log",
            "fuzz-formula.log",
            "fuzz-seed-manifest.json",
            "fuzz-seed-replay.json",
            "release-toolchain-versions.json",
            "release-crate-distribution-smoke.json",
            "public-hygiene.json",
            "rxls-release-manifest.json",
        }
        self.assertEqual(len(expected), 47)
        self.assertEqual(actual, expected)
        for name in (
            "release-libreoffice-version.txt",
            "release-libreoffice-xlsx-smoke.txt",
            "release-libreoffice-xlsm-smoke.txt",
        ):
            self.assertEqual(workflow.count(f"dist/{name}"), 1)

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

    def test_release_identity_matches_native_wasm_and_locks(self) -> None:
        module = _load("check_release_identity", IDENTITY)

        self.assertEqual(module.validate(ROOT), [])

    def test_release_identity_reports_every_version_mismatch(self) -> None:
        module = _load("check_release_identity_mismatch", IDENTITY)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            wasm = root / "bindings" / "wasm"
            wasm.mkdir(parents=True)
            (root / "Cargo.toml").write_text(
                '[package]\nname = "rxls"\nversion = "0.1.2"\nrust-version = "1.85"\n',
                encoding="utf-8",
            )
            (root / "Cargo.lock").write_text(
                'version = 4\n[[package]]\nname = "rxls"\nversion = "0.1.1"\n',
                encoding="utf-8",
            )
            (wasm / "Cargo.toml").write_text(
                '[package]\nname = "rxls-wasm"\nversion = "0.1.0"\n'
                'rust-version = "1.84"\npublish = false\n'
                '[dependencies]\nrxls = { path = "../..", default-features = false }\n',
                encoding="utf-8",
            )
            (wasm / "Cargo.lock").write_text(
                'version = 4\n[[package]]\nname = "rxls"\nversion = "0.1.1"\n'
                '[[package]]\nname = "rxls-wasm"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (root / "CHANGELOG.md").write_text(
                "## [0.1.2]\n"
                "[Unreleased]: https://github.com/HyunjoJung/rxls/compare/v0.1.2...HEAD\n"
                "[0.1.2]: https://github.com/HyunjoJung/rxls/releases/tag/v0.1.2\n",
                encoding="utf-8",
            )
            npm = wasm / "npm"
            npm.mkdir()
            (npm / "package.json").write_text(
                json.dumps(
                    {
                        "name": "rxls-wasm",
                        "version": "0.1.2",
                        "main": "./node/rxls_wasm.js",
                        "types": "./node/rxls_wasm.d.ts",
                        "engines": {"node": ">=20"},
                        "files": [
                            "node",
                            "web",
                            "demo",
                            "README.md",
                            "LICENSE",
                        ],
                    }
                ),
                encoding="utf-8",
            )

            errors = module.validate(root)

        self.assertEqual(len(errors), 5)
        self.assertTrue(any("root Cargo.lock rxls" in error for error in errors))
        self.assertTrue(any("WASM Cargo.toml rxls-wasm" in error for error in errors))
        self.assertTrue(any("WASM Cargo.lock rxls" in error for error in errors))
        self.assertTrue(any("WASM Cargo.lock rxls-wasm" in error for error in errors))
        self.assertTrue(any("rust-version" in error for error in errors))

    def test_sbom_references_are_stable_and_cover_native_and_wasm_roots(self) -> None:
        module = _load("generate_sbom", SBOM)
        native_home = "/" + "Users" + "/alice"
        linux_home = "/" + "home" + "/runner"
        native_id = f"path+file://{native_home}/work/rxls#0.1.2"
        wasm_id = f"path+file://{linux_home}/work/rxls/bindings/wasm#0.1.2"
        dependency = {
            "id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
            "name": "serde",
            "version": "1.0.0",
            "license": "MIT OR Apache-2.0",
            "source": "registry+https://github.com/rust-lang/crates.io-index",
        }
        payload = module.make_sbom(
            [
                {
                    "workspace_members": [native_id],
                    "packages": [
                        {"id": native_id, "name": "rxls", "version": "0.1.2", "license": "MIT"},
                        dependency,
                    ],
                },
                {
                    "workspace_members": [wasm_id],
                    "packages": [
                        {"id": wasm_id, "name": "rxls-wasm", "version": "0.1.2", "license": "MIT"},
                        dependency,
                    ],
                },
            ]
        )

        rendered = json.dumps(payload, sort_keys=True)
        root_names = {
            item["name"]
            for item in payload["metadata"]["component"]["components"]
        }
        self.assertNotIn(native_home, rendered)
        self.assertNotIn(linux_home, rendered)
        self.assertEqual(root_names, {"rxls", "rxls-wasm"})
        self.assertEqual(len(payload["components"]), 1)

    def test_wasm_package_verifier_checks_exports_and_budgets(self) -> None:
        module = _load("check_wasm_package", WASM_PACKAGE)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            package = root / "package"
            package.mkdir()
            metadata = json.loads(
                (ROOT / "bindings" / "wasm" / "npm" / "package.json").read_text(
                    encoding="utf-8"
                )
            )
            (package / "package.json").write_text(
                json.dumps(metadata), encoding="utf-8"
            )
            (package / "README.md").write_text("readme", encoding="utf-8")
            (package / "LICENSE").write_text("license", encoding="utf-8")
            (package / "demo").mkdir()
            (package / "demo" / "index.html").write_text("demo", encoding="utf-8")
            (package / "demo" / "app.js").write_text("demo", encoding="utf-8")
            declarations = " ".join(module.REQUIRED_TYPES)
            for target in ("node", "web"):
                output = package / target
                output.mkdir()
                (output / "rxls_wasm.js").write_text("// glue", encoding="utf-8")
                (output / "rxls_wasm.d.ts").write_text(
                    declarations
                    + (" export default function init(): Promise<void>;" if target == "web" else ""),
                    encoding="utf-8",
                )
                (output / "rxls_wasm_bg.wasm").write_bytes(b"\0asmfixture")
                (output / "rxls_wasm_bg.wasm.d.ts").write_text(
                    "export const memory: WebAssembly.Memory;", encoding="utf-8"
                )
            (package / "web" / "package.json").write_text(
                '{"type":"module"}', encoding="utf-8"
            )

            errors, report = module.validate(package)

            node_declarations = package / "node" / "rxls_wasm.d.ts"
            node_declarations.write_text(
                declarations + " export default function init(): Promise<void>;",
                encoding="utf-8",
            )
            node_type_errors, _ = module.validate(package)
            node_declarations.write_text(declarations, encoding="utf-8")

            web_declarations = package / "web" / "rxls_wasm.d.ts"
            web_declarations.write_text(declarations, encoding="utf-8")
            web_type_errors, _ = module.validate(package)
            web_declarations.write_text(
                declarations + " export default function init(): Promise<void>;",
                encoding="utf-8",
            )

            stale = package / "wasm-size-report.json"
            stale.write_text("{}", encoding="utf-8")
            stale_errors, _ = module.validate(package)
            stale.unlink()

            archive = root / "rxls-wasm-0.1.2.tgz"
            with tarfile.open(archive, "w:gz") as bundle:
                for relative in module.REQUIRED_FILES:
                    bundle.add(package / relative, arcname=f"package/{relative}")
            archive_errors, _ = module.validate(package, archive)

            bad_archive = root / "rxls-wasm-0.1.2-bad.tgz"
            with tarfile.open(bad_archive, "w:gz") as bundle:
                for relative in module.REQUIRED_FILES:
                    bundle.add(package / relative, arcname=f"package/{relative}")
                bundle.add(
                    package / "README.md", arcname="package/wasm-size-report.json"
                )
            bad_archive_errors, _ = module.validate(package, bad_archive)

        self.assertEqual(errors, [])
        self.assertTrue(report["passed"])
        self.assertEqual(report["schema"], "rxls.wasm-bundle-budget.v1")
        self.assertIn(
            "node declarations must not advertise browser initialization",
            node_type_errors,
        )
        self.assertIn(
            "web declarations must advertise browser initialization",
            web_type_errors,
        )
        self.assertIn("unexpected package file: wasm-size-report.json", stale_errors)
        self.assertEqual(archive_errors, [])
        self.assertIn(
            "npm archive contains unexpected file: wasm-size-report.json",
            bad_archive_errors,
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

    def test_hygiene_can_scan_assembled_distribution_paths(self) -> None:
        module = _load("public_hygiene_audit_dist", HYGIENE)
        with tempfile.TemporaryDirectory(dir=ROOT) as tmp:
            directory = Path(tmp)
            linux_home = "/" + "home" + "/runner"
            (directory / "sbom.json").write_text(
                json.dumps({"bom-ref": f"path+file://{linux_home}/work/rxls#0.1.2"}),
                encoding="utf-8",
            )
            findings = module.audit_paths([str(directory)], ROOT)

        self.assertTrue(any(finding.kind == "linux_home_path" for finding in findings))

    def test_hygiene_scans_published_archive_members(self) -> None:
        module = _load("public_hygiene_audit_archive", HYGIENE)
        with tempfile.TemporaryDirectory() as tmp:
            package = Path(tmp) / "rxls-wasm.tgz"
            payload = ("workspace=/" + "home/runner/private\n").encode()
            with tarfile.open(package, "w:gz") as archive:
                info = tarfile.TarInfo("package/generated.js")
                info.size = len(payload)
                archive.addfile(info, io.BytesIO(payload))

            findings = module.scan_tar_package(package, "dist/rxls-wasm.tgz")

        self.assertEqual([finding.kind for finding in findings], ["linux_home_path"])

    def test_hygiene_rejects_unsafe_published_archive_member(self) -> None:
        module = _load("public_hygiene_audit_unsafe_archive", HYGIENE)
        with tempfile.TemporaryDirectory() as tmp:
            package = Path(tmp) / "rxls.crate"
            with tarfile.open(package, "w:gz") as archive:
                info = tarfile.TarInfo(r"..\secret.txt")
                info.size = 0
                archive.addfile(info, io.BytesIO())

            findings = module.scan_tar_package(package, "dist/rxls.crate")

        self.assertEqual(
            [finding.kind for finding in findings], ["unsafe_archive_member"]
        )

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

    def test_release_bundle_verifier_enforces_exact_47_file_coverage(self) -> None:
        module = _load("release_manifest_verify", MANIFEST)
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            artifacts = []
            for index in range(45):
                artifact = base / f"artifact-{index:02d}.txt"
                artifact.write_text(f"artifact {index}\n", encoding="utf-8")
                artifacts.append(artifact)
            hygiene = base / "public-hygiene.json"
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
            payload = module.release_manifest(
                artifacts, "0.1.2", "c" * 40, hygiene
            )
            (base / module.MANIFEST_NAME).write_text(
                json.dumps(payload, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )

            verified = module.verify_release_bundle(
                base,
                expected_files=47,
                version="0.1.2",
                git_rev="c" * 40,
            )

            self.assertEqual(verified["version"], "0.1.2")
            (base / "unexpected.txt").write_text("unexpected\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "48 files; expected 47"):
                module.verify_release_bundle(base, expected_files=47)

    def test_release_bundle_verifier_rejects_manifest_checksum_drift(self) -> None:
        module = _load("release_manifest_drift", MANIFEST)
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            artifact = base / "artifact.txt"
            artifact.write_text("original\n", encoding="utf-8")
            hygiene = base / "public-hygiene.json"
            hygiene.write_text(
                '{"schema":"rxls.public-hygiene-audit.v1",'
                '"passed":true,"findings":[]}\n',
                encoding="utf-8",
            )
            payload = module.release_manifest(
                [artifact], "0.1.2", "d" * 40, hygiene
            )
            (base / module.MANIFEST_NAME).write_text(
                json.dumps(payload), encoding="utf-8"
            )
            artifact.write_text("changed\n", encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "size differs|SHA-256 differs"):
                module.verify_release_bundle(base, expected_files=3)

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
