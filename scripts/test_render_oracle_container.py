#!/usr/bin/env python3
"""Tests for the isolated Linux LibreOffice render-oracle wrapper."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "run-render-oracle-container.py"
CONTAINER_DIR = ROOT / "scripts" / "render-oracle-container"


def load_module():
    spec = importlib.util.spec_from_file_location("render_oracle_container", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def sha256(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def write_font_pack(root: Path) -> Path:
    font = b"deterministic fixture font"
    license_payload = b"fixture OFL license"
    configuration = b'<fontconfig><dir prefix="relative">fonts</dir></fontconfig>\n'
    font_path = root / "fonts" / "Fixture-Regular.ttf"
    license_path = root / "licenses" / "OFL.txt"
    font_path.parent.mkdir(parents=True)
    license_path.parent.mkdir(parents=True)
    font_path.write_bytes(font)
    license_path.write_bytes(license_payload)
    (root / "fonts.conf").write_bytes(configuration)
    identity = {
        "fonts": [
            {
                "bytes": len(font),
                "family": "Fixture",
                "output": "fonts/Fixture-Regular.ttf",
                "sha256": sha256(font),
                "style": "normal",
                "weight": 400,
            }
        ],
        "fonts_conf_sha256": sha256(configuration),
        "licenses": [
            {
                "bytes": len(license_payload),
                "output": "licenses/OFL.txt",
                "sha256": sha256(license_payload),
            }
        ],
    }
    manifest = {
        **identity,
        "pack_sha256": sha256(MODULE.canonical_json_bytes(identity)),
        "schema": MODULE.FONT_PACK_SCHEMA,
        "total_bytes": len(font) + len(license_payload) + len(configuration),
    }
    (root / "manifest.json").write_bytes(MODULE.canonical_json_bytes(manifest))
    return root


def output_manifest(
    source: bytes,
    extension: str,
    lock_sha256: str,
    pdf: bytes,
    font_pack_sha256: str = "9" * 64,
    single_page_sheets: bool = True,
) -> dict:
    return {
        "artifact": {
            "bytes": len(pdf),
            "path": "oracle/oracle.pdf",
            "sha256": sha256(pdf),
        },
        "export": {
            "filter": "calc_pdf_Export",
            "single_page_sheets": single_page_sheets,
        },
        "font_pack_sha256": font_pack_sha256,
        "lock_sha256": lock_sha256,
        "oracle": {
            "artifact_sha256": MODULE.LIBREOFFICE_ARTIFACT_SHA256,
            "name": "LibreOffice",
            "version": "26.2.3.2",
        },
        "schema": MODULE.OUTPUT_SCHEMA,
        "source": {
            "bytes": len(source),
            "path": f"source/input{extension}",
            "sha256": sha256(source),
        },
    }


def make_tar(
    path: Path,
    manifest: dict,
    pdf: bytes,
    *,
    extra: list[tuple[str, bytes, str]] | None = None,
) -> None:
    entries = [
        ("oracle-manifest.json", MODULE.canonical_json_bytes(manifest), "file"),
        ("oracle.pdf", pdf, "file"),
        *(extra or []),
    ]
    with tarfile.open(path, mode="w") as bundle:
        for name, payload, kind in entries:
            info = tarfile.TarInfo(name)
            info.mtime = 0
            if kind == "symlink":
                info.type = tarfile.SYMTYPE
                info.linkname = payload.decode()
                info.size = 0
                bundle.addfile(info)
            else:
                info.size = len(payload)
                bundle.addfile(info, io.BytesIO(payload))


def image_inspect(lock_sha256: str, *, mutate: dict[str, str] | None = None) -> bytes:
    labels = {
        **MODULE.EXPECTED_IMAGE_LABELS,
        "org.rxls.render-oracle.lock-sha256": lock_sha256,
    }
    labels.update(mutate or {})
    return json.dumps(
        [
            {
                "Architecture": "amd64",
                "Config": {"Labels": labels},
                "Id": "sha256:" + "a" * 64,
            }
        ]
    ).encode()


class FakeRunner:
    def __init__(
        self,
        lock_sha256: str,
        archive: Path | None = None,
        *,
        start_status: str = "ok",
        label_mutation: dict[str, str] | None = None,
    ) -> None:
        self.lock_sha256 = lock_sha256
        self.archive = archive
        self.start_status = start_status
        self.label_mutation = label_mutation
        self.commands: list[list[str]] = []

    def run(
        self,
        command,
        *,
        timeout_seconds,
        output_limit_bytes,
        stdout_path=None,
    ):
        command = list(command)
        self.commands.append(command)
        if command[1:3] == ["image", "inspect"]:
            return MODULE.CommandResult(
                "ok",
                0,
                image_inspect(
                    self.lock_sha256,
                    mutate=self.label_mutation,
                ),
            )
        if command[1] == "create":
            return MODULE.CommandResult("ok", 0, b"container-id\n")
        if command[1] == "start":
            if self.start_status == "ok":
                assert stdout_path is not None and self.archive is not None
                Path(stdout_path).write_bytes(self.archive.read_bytes())
                return MODULE.CommandResult("ok", 0)
            return MODULE.CommandResult(self.start_status, None)
        if command[1] == "rm":
            return MODULE.CommandResult("ok", 0)
        if command[1] == "build":
            return MODULE.CommandResult("ok", 0)
        raise AssertionError(f"unexpected command: {command!r}")


class RenderOracleContainerTests(unittest.TestCase):
    def test_checked_in_lock_and_assets_verify(self) -> None:
        document, payload, digest = MODULE.load_lock()
        self.assertEqual(document["schema"], MODULE.LOCK_SCHEMA)
        self.assertEqual(digest, MODULE.build_contract_sha256(document))
        expected = document["built_image"]["expected_id"]
        if expected is None:
            self.assertEqual(digest, sha256(payload))
        else:
            self.assertRegex(expected, r"^sha256:[0-9a-f]{64}$")
            self.assertNotEqual(digest, sha256(payload))

    def test_normal_lock_gate_rejects_null_image_pin_but_bootstrap_is_explicit(self) -> None:
        lock, _, _ = MODULE.load_lock()
        rejected = subprocess.run(
            [sys.executable, str(SCRIPT), "verify-lock"],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if lock["built_image"]["expected_id"] is None:
            self.assertEqual(rejected.returncode, 2)
            self.assertIn(b"image_pin_required", rejected.stderr)
        else:
            self.assertEqual(rejected.returncode, 0, rejected.stderr.decode())
            self.assertEqual(
                json.loads(rejected.stdout)["expected_image_id"],
                lock["built_image"]["expected_id"],
            )
        accepted = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "verify-lock",
                "--bootstrap-identities",
            ],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(accepted.returncode, 0, accepted.stderr.decode())
        self.assertEqual(
            json.loads(accepted.stdout)["expected_image_id"],
            lock["built_image"]["expected_id"],
        )

    def test_optional_image_pin_does_not_change_the_build_contract(self) -> None:
        document, _, digest = MODULE.load_lock()
        pinned = json.loads(json.dumps(document))
        pinned["built_image"]["expected_id"] = "sha256:" + "a" * 64
        MODULE.validate_lock(pinned)
        self.assertEqual(MODULE.build_contract_sha256(pinned), digest)
        pinned["built_image"]["expected_id"] = "not-an-image-id"
        with self.assertRaisesRegex(
            MODULE.OracleContainerError, "lock_built_image_id"
        ):
            MODULE.validate_lock(pinned)

    def test_containerfile_has_exact_architecture_artifact_and_snapshot_pins(self) -> None:
        lock, _, _ = MODULE.load_lock()
        containerfile = (CONTAINER_DIR / "Containerfile").read_text()
        base = lock["base_image"]
        artifact = lock["libreoffice"]["artifact"]
        self.assertIn(
            f"FROM --platform=linux/amd64 {base['reference']}",
            containerfile,
        )
        self.assertIn(artifact["url"], containerfile)
        self.assertIn(str(artifact["bytes"]), containerfile)
        self.assertIn(artifact["sha256"], containerfile)
        self.assertIn(lock["debian_snapshot"]["timestamp"], containerfile)
        self.assertIn(
            f"ARG SOURCE_DATE_EPOCH={lock['built_image']['source_date_epoch']}",
            containerfile,
        )
        self.assertIn("/var/log/dpkg.log", containerfile)
        self.assertIn("/var/log/apt/*", containerfile)
        self.assertNotRegex(containerfile, r"^FROM\s+[^\n]+:(?:latest|bookworm-slim)\s*$")

    def test_image_build_command_locks_the_reproducible_config_epoch(self) -> None:
        lock, _, contract = MODULE.load_lock()
        command = MODULE.build_build_command("docker", "local/oracle:test", contract)
        self.assertIn(
            f"SOURCE_DATE_EPOCH={lock['built_image']['source_date_epoch']}",
            command,
        )

    def test_profile_allows_embedded_charts_but_blocks_execution_and_links(self) -> None:
        profile_path = CONTAINER_DIR / "profile" / "registrymodifications.xcu"
        root = ET.parse(profile_path).getroot()
        oor = "{http://openoffice.org/2001/registry}"
        settings: dict[tuple[str, str], str] = {}
        for item in root.findall("item"):
            path = item.attrib[f"{oor}path"]
            for prop in item.findall("prop"):
                name = prop.attrib[f"{oor}name"]
                value = prop.findtext("value")
                self.assertIsNotNone(value)
                key = (path, name)
                self.assertNotIn(key, settings)
                settings[key] = value

        scripting = "/org.openoffice.Office.Common/Security/Scripting"
        self.assertEqual(settings[(scripting, "DisableActiveContent")], "false")
        for name in (
            "DisableMacrosExecution",
            "DisablePythonRuntime",
            "DisableOLEAutomation",
            "BlockUntrustedRefererLinks",
            "CheckDocumentEvents",
        ):
            self.assertEqual(settings[(scripting, name)], "true")
        self.assertEqual(settings[(scripting, "MacroSecurityLevel")], "3")
        self.assertEqual(
            settings[("/org.openoffice.Office.Calc/Content/Update", "Link")],
            "0",
        )

        entrypoint = (CONTAINER_DIR / "oracle-entrypoint.sh").read_text()
        self.assertIn("SinglePageSheets", entrypoint)
        self.assertIn("UserInstallation=file://", entrypoint)
        self.assertNotIn("curl ", entrypoint)

    def test_create_command_contains_every_isolation_and_resource_bound(self) -> None:
        limits = MODULE.ResourceLimits(
            timeout_seconds=45,
            cpus=1.5,
            memory_mib=768,
            pids=64,
            nofile=128,
            evidence_mib=32,
            runtime_mib=64,
            tmp_mib=64,
            max_source_mib=8,
        )
        config = MODULE.RenderConfig(
            source=Path("source.xlsx"),
            font_pack=Path("fonts"),
            corpus=Path("corpus"),
            evidence_dir=Path("evidence"),
            run_id="unit-test",
            limits=limits,
        )
        command = MODULE.build_create_command(
            "docker",
            "sha256:" + "a" * 64,
            config,
            source_mount=Path("/safe/source.xlsx"),
            font_mount=Path("/safe/fonts"),
            corpus_mount=Path("/safe/corpus"),
            source_bytes=7,
            source_sha256="b" * 64,
            extension=".xlsx",
            lock_sha256="c" * 64,
            font_pack_sha256="d" * 64,
        )
        pairs = list(zip(command, command[1:]))
        self.assertIn(("--network", "none"), pairs)
        self.assertIn("--read-only", command)
        self.assertIn(("--cap-drop", "ALL"), pairs)
        self.assertIn(("--security-opt", "no-new-privileges=true"), pairs)
        self.assertIn(("--pids-limit", "64"), pairs)
        self.assertIn(("--cpus", "1.50"), pairs)
        self.assertIn(("--memory", "768m"), pairs)
        self.assertIn(("--memory-swap", "768m"), pairs)
        self.assertIn("nofile=128:128", command)
        self.assertIn("fsize=33554432:33554432", command)
        self.assertIn(("--ipc", "private"), pairs)
        self.assertIn(("--shm-size", "64m"), pairs)
        self.assertIn(("--user", "65534:65534"), pairs)
        tmpfs = [
            command[index + 1]
            for index, item in enumerate(command)
            if item == "--tmpfs"
        ]
        self.assertEqual(len(tmpfs), 3)
        self.assertTrue(
            any(
                "/oracle/evidence:" in item and "size=33554432" in item
                for item in tmpfs
            )
        )
        self.assertTrue(any("uid=65534" in item and "gid=65534" in item for item in tmpfs))
        mounts = [
            command[index + 1]
            for index, item in enumerate(command)
            if item == "--mount"
        ]
        self.assertEqual(len(mounts), 3)
        self.assertTrue(all(item.endswith(",readonly") for item in mounts))
        self.assertTrue(any("target=/oracle/source/input.xlsx" in item for item in mounts))
        self.assertTrue(any("target=/oracle/fonts" in item for item in mounts))
        self.assertTrue(any("target=/oracle/corpus" in item for item in mounts))
        env = [command[index + 1] for index, item in enumerate(command) if item == "--env"]
        self.assertTrue(any(item.startswith("HOME=/oracle/runtime/unit-test/") for item in env))
        self.assertTrue(any(item.startswith("XDG_CACHE_HOME=") for item in env))
        self.assertTrue(any(item.startswith("XDG_CONFIG_HOME=") for item in env))
        self.assertTrue(any(item.startswith("XDG_DATA_HOME=") for item in env))
        self.assertIn("RXLS_FONT_PACK_SHA256=" + "d" * 64, env)
        self.assertIn("RXLS_PRINT_MODE=single-page-sheets", env)

    def test_authored_print_mode_is_bound_into_container_and_output_contract(self) -> None:
        config = MODULE.RenderConfig(
            source=Path("source.xlsx"),
            font_pack=Path("fonts"),
            corpus=None,
            evidence_dir=Path("evidence"),
            run_id="authored-print",
            limits=MODULE.ResourceLimits(),
            print_mode="authored",
        )
        command = MODULE.build_create_command(
            "docker",
            "sha256:" + "a" * 64,
            config,
            source_mount=Path("/safe/source.xlsx"),
            font_mount=Path("/safe/fonts"),
            corpus_mount=Path("/safe/corpus"),
            source_bytes=7,
            source_sha256="b" * 64,
            extension=".xlsx",
            lock_sha256="c" * 64,
            font_pack_sha256="d" * 64,
        )
        env = [command[index + 1] for index, item in enumerate(command) if item == "--env"]
        self.assertIn("RXLS_PRINT_MODE=authored", env)

        source = b"source"
        pdf = b"%PDF-1.4\nfixture\n%%EOF\n"
        lock_sha = "1" * 64
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "oracle.pdf").write_bytes(pdf)
            (root / "oracle-manifest.json").write_bytes(
                MODULE.canonical_json_bytes(
                    output_manifest(
                        source,
                        ".xlsx",
                        lock_sha,
                        pdf,
                        single_page_sheets=False,
                    )
                )
            )
            MODULE.validate_output_evidence(
                root,
                source_sha256=sha256(source),
                source_bytes=len(source),
                extension=".xlsx",
                lock_sha256=lock_sha,
                font_pack_sha256="9" * 64,
                print_mode="authored",
            )
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "output_export_contract"
            ):
                MODULE.validate_output_evidence(
                    root,
                    source_sha256=sha256(source),
                    source_bytes=len(source),
                    extension=".xlsx",
                    lock_sha256=lock_sha,
                    font_pack_sha256="9" * 64,
                )

    def test_build_command_is_linux_amd64_and_passes_lock_identity(self) -> None:
        command = MODULE.build_build_command(
            "podman", "rxls-oracle:test", "d" * 64
        )
        self.assertEqual(command[:2], ["podman", "build"])
        self.assertIn("linux/amd64", command)
        self.assertIn("ORACLE_LOCK_SHA256=" + "d" * 64, command)
        self.assertIn("--pull=false", command)

    def test_podman_uses_its_portable_no_new_privileges_form(self) -> None:
        config = MODULE.RenderConfig(
            source=Path("source.xlsx"),
            font_pack=Path("fonts"),
            corpus=Path("corpus"),
            evidence_dir=Path("evidence"),
            run_id="podman-test",
            limits=MODULE.ResourceLimits(),
        )
        command = MODULE.build_create_command(
            "podman",
            "sha256:" + "a" * 64,
            config,
            source_mount=Path("/safe/source.xlsx"),
            font_mount=Path("/safe/fonts"),
            corpus_mount=Path("/safe/corpus"),
            source_bytes=7,
            source_sha256="b" * 64,
            extension=".xlsx",
            lock_sha256="c" * 64,
            font_pack_sha256="d" * 64,
        )
        pairs = list(zip(command, command[1:]))
        self.assertIn(("--security-opt", "no-new-privileges"), pairs)
        self.assertIn(("--ipc", "private"), pairs)

    def test_invalid_identifiers_and_limits_are_rejected(self) -> None:
        for run_id in ("", "UPPER", "../escape", "has space", "a" * 33):
            with self.subTest(run_id=run_id), self.assertRaises(
                MODULE.OracleContainerError
            ):
                MODULE.validate_run_id(run_id)
        for image in ("", "--privileged", "image\nnext", "has space"):
            with self.subTest(image=image), self.assertRaises(
                MODULE.OracleContainerError
            ):
                MODULE.validate_image_reference(image)
        with self.assertRaises(MODULE.OracleContainerError):
            MODULE.ResourceLimits(memory_mib=64).validate()
        with self.assertRaises(MODULE.OracleContainerError):
            MODULE.ResourceLimits(timeout_seconds=0).validate()

    def test_source_extension_size_font_layout_and_nonempty_output_are_checked(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            bad_source = root / "source.csv"
            bad_source.write_bytes(b"x")
            with self.assertRaisesRegex(MODULE.OracleContainerError, "source_extension"):
                MODULE.validate_source(bad_source, 100)

            source = root / "source.xlsx"
            source.write_bytes(b"x" * 101)
            with self.assertRaisesRegex(MODULE.OracleContainerError, "source_size"):
                MODULE.validate_source(source, 100)

            incomplete = root / "incomplete-fonts"
            incomplete.mkdir()
            with self.assertRaises(MODULE.OracleContainerError):
                MODULE.validate_font_pack(incomplete)

            font_pack = write_font_pack(root / "fonts")
            evidence = root / "evidence"
            evidence.mkdir()
            (evidence / "existing").write_text("do not overwrite")
            config = MODULE.RenderConfig(
                source=source,
                font_pack=font_pack,
                corpus=None,
                evidence_dir=evidence,
                run_id="checked",
                limits=MODULE.ResourceLimits(max_source_mib=1),
            )
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "evidence_not_empty"
            ):
                MODULE.validate_render_config(config)

    def test_font_pack_symlinks_are_rejected(self) -> None:
        if not hasattr(os, "symlink"):
            self.skipTest("symlinks unavailable")
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            pack = write_font_pack(root / "pack")
            try:
                (pack / "escape").symlink_to(pack / "fonts")
            except OSError:
                self.skipTest("symlinks unavailable")
            with self.assertRaisesRegex(MODULE.OracleContainerError, "font_pack_symlink"):
                MODULE.validate_font_pack(pack)

    def test_image_inspection_requires_architecture_and_exact_labels(self) -> None:
        lock_sha = "e" * 64
        runner = FakeRunner(lock_sha, label_mutation={"org.opencontainers.image.version": "wrong"})
        with self.assertRaisesRegex(MODULE.OracleContainerError, "image_label_mismatch"):
            MODULE.inspect_image(runner, "docker", "image", lock_sha)
        runner = FakeRunner(lock_sha)
        self.assertEqual(
            MODULE.inspect_image(
                runner,
                "docker",
                "image",
                lock_sha,
                "sha256:" + "a" * 64,
            ),
            "sha256:" + "a" * 64,
        )
        with self.assertRaisesRegex(MODULE.OracleContainerError, "image_id_mismatch"):
            MODULE.inspect_image(
                runner,
                "docker",
                "image",
                lock_sha,
                "sha256:" + "b" * 64,
            )

    def test_archive_extraction_rejects_traversal_links_duplicates_and_extra_files(self) -> None:
        source = b"source"
        pdf = b"%PDF-1.4\nfixture\n%%EOF\n"
        manifest = output_manifest(source, ".xlsx", "f" * 64, pdf)
        cases = [
            [("../escape", b"bad", "file")],
            [("link", b"/etc/passwd", "symlink")],
            [("oracle.pdf", b"duplicate", "file")],
            [("extra.txt", b"extra", "file")],
        ]
        for index, extra in enumerate(cases):
            with self.subTest(case=index), tempfile.TemporaryDirectory() as raw:
                root = Path(raw)
                archive = root / "evidence.tar"
                make_tar(archive, manifest, pdf, extra=extra)
                destination = root / "out"
                destination.mkdir()
                with self.assertRaises(MODULE.OracleContainerError):
                    MODULE.extract_evidence_archive(
                        archive, destination, maximum_bytes=1024 * 1024
                    )
                self.assertFalse((root / "escape").exists())

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            archive = root / "evidence.tar"
            make_tar(archive, manifest, pdf)
            destination = root / "out"
            destination.mkdir()
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "evidence_member_limit"
            ):
                MODULE.extract_evidence_archive(
                    archive, destination, maximum_bytes=10
                )

    def test_manifest_validation_rejects_absolute_and_mismatched_paths(self) -> None:
        source = b"source"
        pdf = b"%PDF-1.4\nfixture\n%%EOF\n"
        lock_sha = "1" * 64
        manifest = output_manifest(source, ".xlsx", lock_sha, pdf)
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "oracle.pdf").write_bytes(pdf)
            manifest["source"]["path"] = "/host/private/source.xlsx"
            (root / "oracle-manifest.json").write_bytes(
                MODULE.canonical_json_bytes(manifest)
            )
            with self.assertRaises(MODULE.OracleContainerError):
                MODULE.validate_output_evidence(
                    root,
                    source_sha256=sha256(source),
                    source_bytes=len(source),
                    extension=".xlsx",
                    lock_sha256=lock_sha,
                    font_pack_sha256="9" * 64,
                )
        with self.assertRaisesRegex(
            MODULE.OracleContainerError, "evidence_absolute_path"
        ):
            MODULE.reject_absolute_strings({"nested": ["file:///host/private"]})

    def test_host_path_scan_checks_binary_and_json_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            evidence = root / "evidence"
            evidence.mkdir()
            secret = root / "private" / "source.xlsx"
            (evidence / "artifact").write_bytes(b"prefix " + str(secret).encode())
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "evidence_host_path"
            ):
                MODULE.reject_host_paths(
                    evidence, [secret], maximum_bytes=1024 * 1024
                )

    def test_execute_render_is_bounded_verified_and_path_neutral(self) -> None:
        _, _, lock_sha = MODULE.load_lock()
        source_payload = b"fixture workbook"
        pdf = b"%PDF-1.4\nfixture\n%%EOF\n"
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "sensitive" / "source.xlsx"
            source.parent.mkdir()
            source.write_bytes(source_payload)
            font_pack = write_font_pack(root / "font-pack")
            font_pack_sha256 = json.loads(
                (font_pack / "manifest.json").read_text(encoding="utf-8")
            )["pack_sha256"]
            archive = root / "oracle.tar"
            make_tar(
                archive,
                output_manifest(
                    source_payload,
                    ".xlsx",
                    lock_sha,
                    pdf,
                    font_pack_sha256,
                ),
                pdf,
            )
            evidence = root / "evidence"
            config = MODULE.RenderConfig(
                source=source,
                font_pack=font_pack,
                corpus=None,
                evidence_dir=evidence,
                run_id="execute-test",
                limits=MODULE.ResourceLimits(
                    timeout_seconds=10,
                    memory_mib=512,
                    evidence_mib=16,
                    runtime_mib=64,
                    tmp_mib=64,
                ),
            )
            runner = FakeRunner(lock_sha, archive)
            result = MODULE.execute_render(
                config,
                "docker",
                "local/oracle:test",
                lock_sha,
                runner=runner,
            )

            self.assertEqual(result["schema"], MODULE.EXECUTION_SCHEMA)
            self.assertEqual(result["image"]["id"], "sha256:" + "a" * 64)
            self.assertEqual(result["isolation"]["network"], "none")
            self.assertEqual(
                result["isolation"]["evidence_mount"], "size_capped_tmpfs"
            )
            self.assertEqual((evidence / "oracle.pdf").read_bytes(), pdf)
            self.assertTrue((evidence / "execution.json").is_file())
            combined = b"".join(
                path.read_bytes() for path in evidence.iterdir() if path.is_file()
            )
            self.assertNotIn(str(root).encode(), combined)
            create = next(
                command for command in runner.commands if command[1] == "create"
            )
            self.assertEqual(create[-1], "sha256:" + "a" * 64)
            self.assertFalse(any(str(source) in argument for argument in create))
            self.assertEqual(runner.commands[-1][1:3], ["rm", "--force"])

    def test_timeout_cleans_container_and_preserves_empty_destination(self) -> None:
        _, _, lock_sha = MODULE.load_lock()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "source.xlsx"
            source.write_bytes(b"source")
            font_pack = write_font_pack(root / "font-pack")
            evidence = root / "evidence"
            evidence.mkdir()
            config = MODULE.RenderConfig(
                source=source,
                font_pack=font_pack,
                corpus=None,
                evidence_dir=evidence,
                run_id="timeout-test",
                limits=MODULE.ResourceLimits(
                    timeout_seconds=1,
                    memory_mib=512,
                    evidence_mib=16,
                    runtime_mib=64,
                    tmp_mib=64,
                ),
            )
            runner = FakeRunner(lock_sha, start_status="timeout")
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "container_start_timeout"
            ):
                MODULE.execute_render(
                    config,
                    "podman",
                    "local/oracle:test",
                    lock_sha,
                    runner=runner,
                )
            self.assertEqual(list(evidence.iterdir()), [])
            self.assertEqual(runner.commands[-1][1:3], ["rm", "--force"])

    def test_bounded_runner_enforces_output_and_wall_time(self) -> None:
        runner = MODULE.BoundedProcessRunner()
        excessive = runner.run(
            [sys.executable, "-c", "import sys; sys.stdout.write('x' * 100000)"],
            timeout_seconds=5,
            output_limit_bytes=1024,
        )
        self.assertEqual(excessive.status, "output_limit")
        self.assertLessEqual(len(excessive.stdout) + len(excessive.stderr), 1024)

        started = time.monotonic()
        timed_out = runner.run(
            [sys.executable, "-c", "import time; time.sleep(30)"],
            timeout_seconds=0.1,
            output_limit_bytes=1024,
        )
        self.assertEqual(timed_out.status, "timeout")
        self.assertLess(time.monotonic() - started, 3.0)

    def test_timeout_terminates_the_spawned_process_group(self) -> None:
        if os.name == "nt":
            self.skipTest("POSIX process groups are required")
        runner = MODULE.BoundedProcessRunner()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            ready = root / "ready"
            terminated = root / "terminated"
            child = root / "child.py"
            child.write_text(
                "import signal, sys, time\n"
                "from pathlib import Path\n"
                f"ready = Path({str(ready)!r})\n"
                f"terminated = Path({str(terminated)!r})\n"
                "def stop(*_):\n"
                "    terminated.write_text('yes')\n"
                "    raise SystemExit(0)\n"
                "signal.signal(signal.SIGTERM, stop)\n"
                "ready.write_text('yes')\n"
                "time.sleep(30)\n",
                encoding="utf-8",
            )
            parent_code = (
                "import subprocess, sys, time; "
                f"subprocess.Popen([sys.executable, {str(child)!r}]); "
                f"p={str(ready)!r}; "
                "exec('for _ in range(200):\\n"
                " import pathlib,time\\n"
                " if pathlib.Path(p).exists(): break\\n"
                " time.sleep(0.01)'); "
                "time.sleep(30)"
            )
            result = runner.run(
                [sys.executable, "-c", parent_code],
                timeout_seconds=1.0,
                output_limit_bytes=1024,
            )
            self.assertEqual(result.status, "timeout")
            deadline = time.monotonic() + 2.0
            while not terminated.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(ready.exists())
            self.assertEqual(terminated.read_text(), "yes")

    def test_bounded_runner_can_stream_stdout_to_a_file(self) -> None:
        runner = MODULE.BoundedProcessRunner()
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "stdout"
            result = runner.run(
                [sys.executable, "-c", "print('streamed')"],
                timeout_seconds=5,
                output_limit_bytes=1024,
                stdout_path=output,
            )
            self.assertEqual(result.status, "ok")
            self.assertEqual(result.stdout, b"")
            self.assertEqual(output.read_bytes(), b"streamed\n")

    def test_render_dry_run_is_engine_independent_and_side_effect_free(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "source.xlsx"
            source.write_bytes(b"source")
            font_pack = write_font_pack(root / "font-pack")
            evidence = root / "evidence"
            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "render",
                    "--engine",
                    "docker",
                    "--dry-run",
                    "--run-id",
                    "dry-run-test",
                    "--source",
                    str(source),
                    "--font-pack",
                    str(font_pack),
                    "--evidence-dir",
                    str(evidence),
                ],
                cwd=ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            document = json.loads(result.stdout)
            self.assertEqual(document["schema"], MODULE.PLAN_SCHEMA)
            create = document["commands"]["create"]
            self.assertIn("--read-only", create)
            self.assertIn("none", create)
            self.assertNotIn(str(root), json.dumps(document, sort_keys=True))
            self.assertIn("<source>", json.dumps(document, sort_keys=True))
            self.assertFalse(evidence.exists())

    def test_build_dry_run_does_not_claim_an_image_digest(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "build",
                "--engine",
                "podman",
                "--dry-run",
                "--image",
                "local/oracle:test",
            ],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        document = json.loads(result.stdout)
        self.assertFalse(document["image_verified"])
        self.assertNotIn("built_image_id", document)
        self.assertNotIn("image_digest", document)
        self.assertNotIn(str(ROOT), json.dumps(document, sort_keys=True))
        self.assertIn("<container-context>", json.dumps(document, sort_keys=True))

    def test_image_pin_requires_exact_current_bootstrap_build_evidence(self) -> None:
        lock, payload, contract = MODULE.load_lock()
        lock["built_image"]["expected_id"] = None
        evidence = {
            "build_contract_sha256": contract,
            "built_image_id": "sha256:" + "b" * 64,
            "expected_image_id": None,
            "image_identity_status": "bootstrap_capture_required",
            "lock_file_sha256": sha256(payload),
            "platform": "linux/amd64",
            "status": "ok",
        }
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            path = root / "build.json"
            path.write_bytes(MODULE.canonical_json_bytes(evidence))
            pinned = MODULE.pin_image_from_evidence(
                lock, payload, contract, path
            )
            self.assertEqual(
                pinned["built_image"]["expected_id"], "sha256:" + "b" * 64
            )
            for key in (
                "build_contract_sha256",
                "built_image_id",
                "lock_file_sha256",
                "image_identity_status",
            ):
                tampered = json.loads(json.dumps(evidence))
                tampered[key] = (
                    "runtime_verified_unpinned"
                    if key == "image_identity_status"
                    else "0" * 64
                )
                path.write_bytes(MODULE.canonical_json_bytes(tampered))
                with self.subTest(key=key):
                    with self.assertRaises(MODULE.OracleContainerError):
                        MODULE.pin_image_from_evidence(lock, payload, contract, path)

    def test_image_pin_cannot_be_rebootstrapped_after_pinning(self) -> None:
        lock, payload, contract = MODULE.load_lock()
        lock["built_image"]["expected_id"] = "sha256:" + "a" * 64
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "build.json"
            path.write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "image_lock_already_pinned"
            ):
                MODULE.pin_image_from_evidence(lock, payload, contract, path)

    def test_hosted_workflow_routes_a_four_format_campaign_through_the_adapter(self) -> None:
        workflow = (ROOT / ".github" / "workflows" / "render-oracle.yml").read_text(
            encoding="utf-8"
        )
        for required in (
            "generate-render-corpus.py --generate --profile pilot",
            "--manifest local/render-corpus-generated/pilot/manifest.json",
            "--max-files 40",
            "--require-renderer-binary-identity",
            "--require-font-pack",
            "--libreoffice-command \"$ADAPTER\"",
            "run-render-oracle-container.py --lock",
            "--image ${IMAGE_ID}",
            "--fail-on-incomparable",
            'assert report["summary"]["by_status"] == {"compared": 40}',
            "report_path.unlink()",
        ):
            self.assertIn(required, workflow)
        upload = workflow.split("Upload path-neutral aggregate identities only", 1)[1]
        self.assertNotIn("parity-report.json", upload)
        self.assertNotIn("oracle.pdf", upload)


if __name__ == "__main__":
    unittest.main()
