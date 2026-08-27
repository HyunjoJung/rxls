#!/usr/bin/env python3
"""Tests for the isolated Linux LibreOffice render-oracle wrapper."""

from __future__ import annotations

from dataclasses import replace
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
from contextlib import redirect_stderr
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
import xml.etree.ElementTree as ET
import zipfile


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


FAKE_IMAGE_CONFIG = MODULE.canonical_json_bytes(
    {
        "architecture": "amd64",
        "config": {"Labels": MODULE.EXPECTED_IMAGE_LABELS},
        "created": MODULE.SOURCE_DATE_EPOCH_RFC3339,
        "os": "linux",
        "rootfs": {
            "diff_ids": [
                "sha256:" + "b" * 64,
                "sha256:" + "c" * 64,
            ],
            "type": "layers",
        },
    }
)
FAKE_CONFIG_ID = "sha256:" + sha256(FAKE_IMAGE_CONFIG)
FAKE_SOURCE_COMMIT = "1" * 40
FAKE_GITHUB_RUN_ID = 29_555_910_469
FAKE_GITHUB_RUN_ATTEMPT = 2
FAKE_GITHUB_JOB_ID = 87_807_789_483
FAKE_GITHUB_ARTIFACT_ID = 8_397_403_909


def fake_source_identity(lock: dict) -> object:
    return MODULE.SourceIdentity(
        commit=FAKE_SOURCE_COMMIT,
        wrapper_sha256=lock["wrapper"]["sha256"],
    )


def make_bootstrap_artifact_zip(
    evidence_payload: bytes,
    *,
    members: list[tuple[str, bytes, int | None]] | None = None,
) -> bytes:
    output = io.BytesIO()
    rows = members or [
        (MODULE.GITHUB_BOOTSTRAP_EVIDENCE_MEMBER, evidence_payload, None)
    ]
    with zipfile.ZipFile(
        output, mode="w", compression=zipfile.ZIP_DEFLATED
    ) as bundle:
        for name, payload, mode in rows:
            info = zipfile.ZipInfo(name)
            info.date_time = (1980, 1, 1, 0, 0, 0)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (
                ((0o100444 if mode is None else mode) & 0xFFFF) << 16
            )
            bundle.writestr(info, payload)
    return output.getvalue()


def fake_bootstrap_receipt(
    evidence_payload: bytes,
    source_identity: object,
    *,
    artifact_zip: bytes | None = None,
    run_id: int = FAKE_GITHUB_RUN_ID,
    run_attempt: int = FAKE_GITHUB_RUN_ATTEMPT,
    job_id: int = FAKE_GITHUB_JOB_ID,
    artifact_id: int = FAKE_GITHUB_ARTIFACT_ID,
) -> dict:
    archive = (
        make_bootstrap_artifact_zip(evidence_payload)
        if artifact_zip is None
        else artifact_zip
    )
    return {
        "artifact": {
            "digest": "sha256:" + sha256(archive),
            "id": artifact_id,
            "name": MODULE.bootstrap_artifact_name(
                source_identity.commit, run_id, run_attempt
            ),
            "size_in_bytes": len(archive),
        },
        "evidence": {
            "bytes": len(evidence_payload),
            "member": MODULE.GITHUB_BOOTSTRAP_EVIDENCE_MEMBER,
            "sha256": sha256(evidence_payload),
        },
        "job": {
            "conclusion": "failure",
            "id": job_id,
            "name": MODULE.GITHUB_BOOTSTRAP_JOB_NAME,
            "run_attempt": run_attempt,
            "run_id": run_id,
        },
        "repository": {
            "full_name": MODULE.GITHUB_REPOSITORY,
            "id": MODULE.GITHUB_REPOSITORY_ID,
        },
        "run": {
            "conclusion": "failure",
            "event": MODULE.GITHUB_BOOTSTRAP_EVENT,
            "head_sha": source_identity.commit,
            "id": run_id,
            "run_attempt": run_attempt,
            "workflow": MODULE.GITHUB_WORKFLOW_PATH,
        },
        "schema": MODULE.BOOTSTRAP_RECEIPT_SCHEMA,
    }


def make_docker_build_archive(
    path: Path,
    config: bytes = FAKE_IMAGE_CONFIG,
) -> None:
    config_name = f"{sha256(config)}.json"
    manifest = json.dumps(
        [
            {
                "Config": config_name,
                "Layers": [],
                "RepoTags": ["rxls-oracle:test"],
            }
        ],
        separators=(",", ":"),
    ).encode()
    with tarfile.open(path, mode="w") as bundle:
        for name, payload in (
            ("manifest.json", manifest),
            (config_name, config),
        ):
            info = tarfile.TarInfo(name)
            info.mtime = 0
            info.mode = 0o444
            info.size = len(payload)
            bundle.addfile(info, io.BytesIO(payload))


def git(root: Path, *args: str) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        ["git", "-C", str(root), *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    )


def make_clean_source_repository(
    root: Path,
) -> tuple[dict, Path, Path]:
    wrapper = root / MODULE.WRAPPER_RELATIVE_PATH
    lock_path = root / MODULE.LOCK_RELATIVE_PATH
    context_file = lock_path.parent / "Containerfile"
    wrapper.parent.mkdir(parents=True)
    lock_path.parent.mkdir(parents=True)
    wrapper_payload = b"#!/usr/bin/env python3\nprint('fixture')\n"
    wrapper.write_bytes(wrapper_payload)
    lock_path.write_bytes(b'{"fixture":"lock"}\n')
    context_file.write_bytes(b"FROM scratch\n")
    (root / ".gitignore").write_text("ignored/\n", encoding="utf-8")
    git(root, "init", "--quiet")
    git(root, "config", "user.name", "fixture")
    git(root, "config", "user.email", "fixture@example.invalid")
    git(root, "add", ".")
    git(root, "commit", "--quiet", "-m", "fixture")
    document = {
        "files": [
            {
                "bytes": context_file.stat().st_size,
                "path": "Containerfile",
                "sha256": sha256(context_file.read_bytes()),
            }
        ],
        "wrapper": {
            "bytes": len(wrapper_payload),
            "path": MODULE.WRAPPER_RELATIVE_PATH,
            "sha256": sha256(wrapper_payload),
        }
    }
    return document, wrapper, lock_path


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


def image_inspect(
    lock_sha256: str,
    *,
    mutate: dict[str, str] | None = None,
    image_id: str = FAKE_CONFIG_ID,
    architecture: str = "amd64",
    operating_system: str = "linux",
    created: str = MODULE.SOURCE_DATE_EPOCH_RFC3339,
    rootfs_type: str = "layers",
    diff_ids: tuple[str, ...] = (
        "sha256:" + "b" * 64,
        "sha256:" + "c" * 64,
    ),
) -> bytes:
    labels = {
        **MODULE.EXPECTED_IMAGE_LABELS,
        "org.rxls.render-oracle.lock-sha256": lock_sha256,
    }
    labels.update(mutate or {})
    return json.dumps(
        [
            {
                "Architecture": architecture,
                "Config": {"Labels": labels},
                "Created": created,
                "Id": image_id,
                "Os": operating_system,
                "RootFS": {
                    "Layers": list(diff_ids),
                    "Type": rootfs_type,
                },
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
        start_stderr: bytes = b"",
        load_status: str = "ok",
        label_mutation: dict[str, str] | None = None,
        image_ids: tuple[str, ...] = (FAKE_CONFIG_ID,),
        metadata_ids: tuple[str, ...] | None = None,
        config_payloads: tuple[bytes, ...] = (FAKE_IMAGE_CONFIG,),
        manifest_digests: tuple[str, ...] = ("sha256:" + "d" * 64,),
        metadata_descriptors: tuple[object | None, ...] | None = None,
        architecture: str = "amd64",
        operating_system: str = "linux",
        created: str = MODULE.SOURCE_DATE_EPOCH_RFC3339,
        rootfs_type: str = "layers",
        diff_id_sets: tuple[tuple[str, ...], ...] = (
            (
                "sha256:" + "b" * 64,
                "sha256:" + "c" * 64,
            ),
        ),
        build_result: object | None = None,
        buildx_version_output: bytes | None = None,
        builder_description: bytes | None = None,
    ) -> None:
        self.lock_sha256 = lock_sha256
        self.archive = archive
        self.start_status = start_status
        self.start_stderr = start_stderr
        self.load_status = load_status
        self.label_mutation = label_mutation
        self.image_ids = image_ids
        self.config_payloads = config_payloads
        self.metadata_ids = (
            tuple(
                "sha256:" + sha256(payload)
                for payload in config_payloads
            )
            if metadata_ids is None
            else metadata_ids
        )
        self.manifest_digests = manifest_digests
        self.metadata_descriptors = metadata_descriptors
        self.architecture = architecture
        self.operating_system = operating_system
        self.created = created
        self.rootfs_type = rootfs_type
        self.diff_id_sets = diff_id_sets
        self.build_result = build_result
        self.buildx_version_output = buildx_version_output or (
            f"github.com/docker/buildx {MODULE.BUILDX_VERSION} "
            f"{MODULE.BUILDX_COMMIT}\n"
        ).encode()
        self.builder_description = builder_description or (
            f"BuildKit version: {MODULE.BUILDKIT_VERSION}\n"
            "Labels:\n"
            " org.mobyproject.buildkit.worker.snapshotter: "
            f"{MODULE.BUILDKIT_SNAPSHOTTER}\n"
        ).encode()
        self.commands: list[list[str]] = []
        self.build_count = 0

    @staticmethod
    def _at(values, index):
        return values[min(index, len(values) - 1)]

    def run(
        self,
        command,
        *,
        timeout_seconds,
        output_limit_bytes,
        stdout_path=None,
        stdout_limit_bytes=None,
        stderr_limit_bytes=None,
    ):
        command = list(command)
        self.commands.append(command)
        if command[1:3] == ["image", "inspect"]:
            index = max(0, self.build_count - 1)
            return MODULE.CommandResult(
                "ok",
                0,
                image_inspect(
                    self.lock_sha256,
                    mutate=self.label_mutation,
                    image_id=self._at(self.image_ids, index),
                    architecture=self.architecture,
                    operating_system=self.operating_system,
                    created=self.created,
                    rootfs_type=self.rootfs_type,
                    diff_ids=self._at(self.diff_id_sets, index),
                ),
            )
        if command[1:3] == ["buildx", "version"]:
            return MODULE.CommandResult("ok", 0, self.buildx_version_output)
        if command[1:3] == ["buildx", "create"]:
            return MODULE.CommandResult("ok", 0, b"builder\n")
        if command[1:3] == ["buildx", "inspect"]:
            return MODULE.CommandResult("ok", 0, self.builder_description)
        if command[1:3] == ["buildx", "build"]:
            if self.build_result is not None:
                return self.build_result
            assert stdout_path is not None
            metadata_path = Path(command[command.index("--metadata-file") + 1])
            manifest_digest = self._at(
                self.manifest_digests, self.build_count
            )
            config_digest = self._at(self.metadata_ids, self.build_count)
            metadata = {
                "containerimage.config.digest": config_digest,
                "containerimage.digest": manifest_digest,
            }
            descriptor = (
                {
                    "annotations": {
                        "org.opencontainers.image.created": (
                            MODULE.SOURCE_DATE_EPOCH_RFC3339
                        ),
                    },
                    "digest": manifest_digest,
                    "mediaType": (
                        "application/vnd.docker.distribution."
                        "manifest.v2+json"
                    ),
                    "platform": {
                        "architecture": "amd64",
                        "os": "linux",
                    },
                    "size": 1234,
                }
                if self.metadata_descriptors is None
                else self._at(self.metadata_descriptors, self.build_count)
            )
            if descriptor is not None:
                metadata["containerimage.descriptor"] = descriptor
            metadata_path.write_bytes(
                MODULE.canonical_json_bytes(metadata)
            )
            make_docker_build_archive(
                Path(stdout_path),
                self._at(self.config_payloads, self.build_count),
            )
            self.build_count += 1
            return MODULE.CommandResult("ok", 0)
        if command[1:3] == ["image", "load"]:
            return MODULE.CommandResult(
                self.load_status,
                0 if self.load_status == "ok" else 1,
                b"Loaded image\n" if self.load_status == "ok" else b"",
            )
        if command[1:3] == ["buildx", "rm"]:
            return MODULE.CommandResult("ok", 0)
        if command[1] == "create":
            return MODULE.CommandResult("ok", 0, b"container-id\n")
        if command[1] == "start":
            if self.start_status == "ok":
                assert stdout_path is not None and self.archive is not None
                Path(stdout_path).write_bytes(self.archive.read_bytes())
                return MODULE.CommandResult("ok", 0)
            return MODULE.CommandResult(
                self.start_status,
                None,
                stderr=self.start_stderr,
            )
        if command[1] == "rm":
            return MODULE.CommandResult("ok", 0)
        raise AssertionError(f"unexpected command: {command!r}")


def hosted_bootstrap_api_fixture(
    source_identity: object,
    evidence_payload: bytes,
    *,
    artifact_zip: bytes | None = None,
) -> tuple[dict[str, dict], bytes]:
    archive = (
        make_bootstrap_artifact_zip(evidence_payload)
        if artifact_zip is None
        else artifact_zip
    )
    run_id = FAKE_GITHUB_RUN_ID
    run_attempt = FAKE_GITHUB_RUN_ATTEMPT
    job_id = FAKE_GITHUB_JOB_ID
    artifact_id = FAKE_GITHUB_ARTIFACT_ID
    responses = {
        f"/repos/{MODULE.GITHUB_REPOSITORY}/actions/runs/{run_id}": {
            "conclusion": "failure",
            "event": MODULE.GITHUB_BOOTSTRAP_EVENT,
            "head_repository": {
                "full_name": MODULE.GITHUB_REPOSITORY,
                "id": MODULE.GITHUB_REPOSITORY_ID,
            },
            "head_sha": source_identity.commit,
            "id": run_id,
            "path": MODULE.GITHUB_WORKFLOW_PATH,
            "repository": {
                "full_name": MODULE.GITHUB_REPOSITORY,
                "id": MODULE.GITHUB_REPOSITORY_ID,
            },
            "run_attempt": run_attempt,
            "status": "completed",
        },
        f"/repos/{MODULE.GITHUB_REPOSITORY}/actions/jobs/{job_id}": {
            "conclusion": "failure",
            "head_sha": source_identity.commit,
            "id": job_id,
            "name": MODULE.GITHUB_BOOTSTRAP_JOB_NAME,
            "run_attempt": run_attempt,
            "run_id": run_id,
            "status": "completed",
            "steps": [
                {
                    "conclusion": "failure",
                    "name": MODULE.GITHUB_BOOTSTRAP_BUILD_STEP,
                    "status": "completed",
                },
                {
                    "conclusion": "success",
                    "name": MODULE.GITHUB_BOOTSTRAP_UPLOAD_STEP,
                    "status": "completed",
                },
            ],
            "workflow_name": MODULE.GITHUB_WORKFLOW_NAME,
        },
        (
            f"/repos/{MODULE.GITHUB_REPOSITORY}/actions/artifacts/"
            f"{artifact_id}"
        ): {
            "digest": "sha256:" + sha256(archive),
            "expired": False,
            "id": artifact_id,
            "name": MODULE.bootstrap_artifact_name(
                source_identity.commit, run_id, run_attempt
            ),
            "size_in_bytes": len(archive),
            "workflow_run": {
                "head_repository_id": MODULE.GITHUB_REPOSITORY_ID,
                "head_sha": source_identity.commit,
                "id": run_id,
                "repository_id": MODULE.GITHUB_REPOSITORY_ID,
            },
        },
    }
    return responses, archive


class FakeGithubRunner:
    def __init__(
        self,
        responses: dict[str, dict],
        artifact_zip: bytes,
        *,
        download_result: object | None = None,
    ) -> None:
        self.responses = responses
        self.artifact_zip = artifact_zip
        self.download_result = download_result
        self.commands: list[list[str]] = []

    def run(
        self,
        command,
        *,
        timeout_seconds,
        output_limit_bytes,
        stdout_path=None,
        stdout_limit_bytes=None,
        stderr_limit_bytes=None,
    ):
        command = list(command)
        self.commands.append(command)
        self.asserted_command(command)
        endpoint = command[-1]
        if endpoint.endswith("/zip"):
            if self.download_result is not None:
                return self.download_result
            assert stdout_path is not None
            Path(stdout_path).write_bytes(self.artifact_zip)
            return MODULE.CommandResult("ok", 0)
        assert stdout_path is None
        if endpoint not in self.responses:
            raise AssertionError(f"unexpected endpoint: {endpoint}")
        return MODULE.CommandResult(
            "ok",
            0,
            MODULE.canonical_json_bytes(self.responses[endpoint]),
        )

    @staticmethod
    def asserted_command(command: list[str]) -> None:
        assert command[:4] == [
            "gh",
            "api",
            "--hostname",
            "github.com",
        ]
        assert "--method" in command
        assert command[command.index("--method") + 1] == "GET"
        assert "X-GitHub-Api-Version: 2022-11-28" in command


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

    def test_clean_source_identity_binds_commit_wrapper_and_tree(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            document, wrapper, lock_path = make_clean_source_repository(
                root
            )
            ignored = root / "ignored" / "cache"
            ignored.parent.mkdir()
            ignored.write_text("ignored", encoding="utf-8")
            identity = MODULE.require_clean_source(
                document,
                root=root,
                wrapper_path=wrapper,
                lock_path=lock_path,
            )
            self.assertRegex(identity.commit, r"^[0-9a-f]{40}$")
            self.assertEqual(
                identity.wrapper_sha256,
                document["wrapper"]["sha256"],
            )

        mutations = {
            "unstaged": lambda root, wrapper, lock: wrapper.write_text(
                "changed\n", encoding="utf-8"
            ),
            "staged": lambda root, wrapper, lock: (
                lock.write_text("changed\n", encoding="utf-8"),
                git(root, "add", MODULE.LOCK_RELATIVE_PATH),
            ),
            "untracked": lambda root, wrapper, lock: (
                root / "unexpected.txt"
            ).write_text("unexpected", encoding="utf-8"),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name), tempfile.TemporaryDirectory() as raw:
                root = Path(raw)
                document, wrapper, lock_path = (
                    make_clean_source_repository(root)
                )
                mutate(root, wrapper, lock_path)
                with self.assertRaisesRegex(
                    MODULE.OracleContainerError, "source_tree_dirty"
                ):
                    MODULE.require_clean_source(
                        document,
                        root=root,
                        wrapper_path=wrapper,
                        lock_path=lock_path,
                    )

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            document, wrapper, lock_path = make_clean_source_repository(
                root
            )
            document["wrapper"]["sha256"] = "0" * 64
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "wrapper_hash"
            ):
                MODULE.require_clean_source(
                    document,
                    root=root,
                    wrapper_path=wrapper,
                    lock_path=lock_path,
                )

    def test_build_and_pin_require_the_canonical_lock_path(self) -> None:
        MODULE.require_canonical_build_lock(MODULE.DEFAULT_LOCK)
        with tempfile.TemporaryDirectory() as raw:
            alternate = Path(raw) / "lock.json"
            alternate.write_bytes(MODULE.DEFAULT_LOCK.read_bytes())
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "canonical_build_lock"
            ):
                MODULE.require_canonical_build_lock(alternate)

    def test_lock_and_build_evidence_reads_reject_special_files_early(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            lock_link = root / "lock.json"
            try:
                lock_link.symlink_to(MODULE.DEFAULT_LOCK)
            except OSError:
                self.skipTest("symlinks unavailable")
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "lock_type"
            ):
                MODULE.load_lock(lock_link)

            metadata_link = root / "metadata.json"
            metadata_link.symlink_to(MODULE.DEFAULT_LOCK)
            with self.assertRaisesRegex(
                MODULE.OracleContainerError,
                "build_metadata_symlink",
            ):
                MODULE.read_build_metadata(metadata_link)

            if hasattr(os, "mkfifo"):
                fifo = root / "evidence.fifo"
                os.mkfifo(fifo)
                lock, _, _ = MODULE.load_lock()
                lock["built_image"]["expected_id"] = None
                lock["built_image"]["expected_manifest_digest"] = None
                lock["built_image"]["bootstrap_receipt"] = None
                payload = MODULE.canonical_json_bytes(lock)
                contract = MODULE.build_contract_sha256(lock)
                with self.assertRaisesRegex(
                    MODULE.OracleContainerError,
                    "bootstrap_build_type",
                ):
                    MODULE.pin_image_from_evidence(
                        lock,
                        payload,
                        contract,
                        fifo,
                        fake_source_identity(lock),
                        fake_bootstrap_receipt(
                            b"{}\n", fake_source_identity(lock)
                        ),
                    )

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
        if lock["built_image"]["expected_id"] is None:
            self.assertEqual(accepted.returncode, 0, accepted.stderr.decode())
            self.assertEqual(
                json.loads(accepted.stdout)["expected_image_id"],
                None,
            )
            self.assertEqual(
                json.loads(accepted.stdout)["expected_manifest_digest"],
                None,
            )
        else:
            self.assertEqual(accepted.returncode, 2)
            self.assertIn(
                b"bootstrap_identities_after_pin", accepted.stderr
            )

    def test_bootstrap_flag_is_rejected_for_pinned_verify_and_build(
        self,
    ) -> None:
        lock, _, _ = MODULE.load_lock()
        pinned = json.loads(json.dumps(lock))
        pinned["built_image"]["expected_id"] = "sha256:" + "a" * 64
        pinned["built_image"]["expected_manifest_digest"] = (
            "sha256:" + "b" * 64
        )
        pinned["built_image"]["bootstrap_receipt"] = (
            fake_bootstrap_receipt(
                b"{}\n", fake_source_identity(lock)
            )
        )
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            for row in pinned["files"]:
                source = CONTAINER_DIR / row["path"]
                destination = root / row["path"]
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(source.read_bytes())
            lock_path = root / "lock.json"
            lock_path.write_bytes(MODULE.canonical_json_bytes(pinned))
            commands = (
                [
                    sys.executable,
                    str(SCRIPT),
                    "--lock",
                    str(lock_path),
                    "verify-lock",
                    "--bootstrap-identities",
                ],
                [
                    sys.executable,
                    str(SCRIPT),
                    "--lock",
                    str(lock_path),
                    "build",
                    "--engine",
                    "docker",
                    "--bootstrap-identities",
                    "--dry-run",
                ],
            )
            for command in commands:
                result = subprocess.run(
                    command,
                    cwd=ROOT,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    check=False,
                )
                with self.subTest(action=command[4]):
                    self.assertEqual(result.returncode, 2)
                    self.assertIn(
                        b"bootstrap_identities_after_pin", result.stderr
                    )

    def test_optional_image_pin_does_not_change_the_build_contract(self) -> None:
        document, _, digest = MODULE.load_lock()
        pinned = json.loads(json.dumps(document))
        pinned["built_image"]["expected_id"] = "sha256:" + "a" * 64
        pinned["built_image"]["expected_manifest_digest"] = (
            "sha256:" + "b" * 64
        )
        pinned["built_image"]["bootstrap_receipt"] = (
            fake_bootstrap_receipt(
                b"{}\n", fake_source_identity(document)
            )
        )
        MODULE.validate_lock(pinned)
        self.assertEqual(MODULE.build_contract_sha256(pinned), digest)
        changed_receipt = json.loads(json.dumps(pinned))
        changed_receipt["built_image"]["bootstrap_receipt"]["artifact"][
            "id"
        ] += 1
        self.assertEqual(
            MODULE.build_contract_sha256(changed_receipt), digest
        )
        changed_wrapper = json.loads(json.dumps(document))
        changed_wrapper["wrapper"]["sha256"] = "0" * 64
        self.assertNotEqual(
            MODULE.build_contract_sha256(changed_wrapper),
            digest,
        )
        pinned["built_image"]["expected_id"] = "not-an-image-id"
        with self.assertRaisesRegex(
            MODULE.OracleContainerError, "lock_built_image_id"
        ):
            MODULE.validate_lock(pinned)
        mismatched_pair = json.loads(json.dumps(document))
        mismatched_pair["built_image"]["expected_id"] = "sha256:" + "a" * 64
        mismatched_pair["built_image"]["expected_manifest_digest"] = None
        mismatched_pair["built_image"]["bootstrap_receipt"] = None
        with self.assertRaisesRegex(
            MODULE.OracleContainerError, "lock_built_image_pin_pair"
        ):
            MODULE.validate_lock(mismatched_pair)
        receipt_without_pins = json.loads(json.dumps(document))
        receipt_without_pins["built_image"]["expected_id"] = None
        receipt_without_pins["built_image"]["expected_manifest_digest"] = None
        receipt_without_pins["built_image"]["bootstrap_receipt"] = (
            fake_bootstrap_receipt(
                b"{}\n", fake_source_identity(document)
            )
        )
        with self.assertRaisesRegex(
            MODULE.OracleContainerError,
            "lock_bootstrap_receipt_pair",
        ):
            MODULE.validate_lock(receipt_without_pins)
        wrong_source = json.loads(json.dumps(pinned))
        wrong_source["built_image"]["expected_id"] = (
            "sha256:" + "a" * 64
        )
        wrong_source["built_image"]["bootstrap_receipt"]["run"][
            "head_sha"
        ] = "0" * 40
        with self.assertRaisesRegex(
            MODULE.OracleContainerError,
            "bootstrap_receipt_artifact",
        ):
            MODULE.validate_lock(wrong_source)

    def test_v3_lock_pins_the_canonical_buildx_buildkit_contract(self) -> None:
        document, _, _ = MODULE.load_lock()
        self.assertEqual(
            document["builder"],
            {
                "buildkit": {
                    "commit": MODULE.BUILDKIT_COMMIT,
                    "compatibility": {
                        "explicit": False,
                        "source": MODULE.BUILDKIT_COMPATIBILITY_SOURCE,
                        "version": (
                            MODULE.BUILDKIT_DEFAULT_COMPATIBILITY_VERSION
                        ),
                    },
                    "image": MODULE.BUILDKIT_IMAGE,
                    "index_sha256": MODULE.BUILDKIT_INDEX_SHA256,
                    "linux_amd64_manifest_sha256": (
                        MODULE.BUILDKIT_AMD64_MANIFEST_SHA256
                    ),
                    "version": "v0.31.2",
                },
                "buildx": {
                    "commit": MODULE.BUILDX_COMMIT,
                    "setup_action": MODULE.BUILDX_SETUP_ACTION,
                    "version": "v0.35.0",
                },
                "driver": "docker-container",
                "driver_options": {
                    "provenance_add_gha": False,
                },
                "exporter": {
                    "archive_max_bytes": MODULE.MAX_BUILD_ARCHIVE_BYTES,
                    "destination": "stdout",
                    "media_type": MODULE.DOCKER_V2_MANIFEST_MEDIA_TYPE,
                    "oci_mediatypes": False,
                    "provenance": False,
                    "rewrite_timestamp": True,
                    "sbom": False,
                    "tar": True,
                    "type": "docker",
                },
                "platform": "linux/amd64",
                "reproducibility_builds": 2,
                "snapshotter": MODULE.BUILDKIT_SNAPSHOTTER,
            },
        )
        for path, value in (
            (("builder", "buildx", "version"), "v0.35.1"),
            (("builder", "buildkit", "version"), "v0.31.1"),
            (
                ("builder", "buildkit", "compatibility", "explicit"),
                True,
            ),
            (
                ("builder", "driver_options", "provenance_add_gha"),
                True,
            ),
            (("builder", "snapshotter"), "overlayfs"),
            (("builder", "exporter", "rewrite_timestamp"), False),
            (("builder", "reproducibility_builds"), 1),
        ):
            mutated = json.loads(json.dumps(document))
            target = mutated
            for key in path[:-1]:
                target = target[key]
            target[path[-1]] = value
            with self.subTest(path=path), self.assertRaisesRegex(
                MODULE.OracleContainerError, "lock_builder"
            ):
                MODULE.validate_lock(mutated)

    def test_containerfile_has_exact_architecture_artifact_and_snapshot_pins(self) -> None:
        lock, _, _ = MODULE.load_lock()
        containerfile = (CONTAINER_DIR / "Containerfile").read_text()
        base = lock["base_image"]
        artifact = lock["libreoffice"]["artifact"]
        self.assertEqual(
            (artifact["url"], artifact["fallback_url"]),
            MODULE.LIBREOFFICE_ARTIFACT_URLS,
        )
        for key in ("url", "fallback_url"):
            mutated = json.loads(json.dumps(lock))
            mutated["libreoffice"]["artifact"][key] += "?unreviewed=1"
            with self.subTest(key=key), self.assertRaisesRegex(
                MODULE.OracleContainerError,
                "lock_artifact_url",
            ):
                MODULE.validate_lock(mutated)
        self.assertIn(
            f"FROM --platform=linux/amd64 {base['reference']}",
            containerfile,
        )
        self.assertIn(artifact["url"], containerfile)
        self.assertIn(artifact["fallback_url"], containerfile)
        self.assertIn(str(artifact["bytes"]), containerfile)
        self.assertIn(artifact["sha256"], containerfile)
        self.assertEqual(containerfile.count("--http1.1"), 1)
        self.assertEqual(containerfile.count("--retry 4"), 1)
        self.assertEqual(containerfile.count("--retry-all-errors"), 1)
        self.assertEqual(containerfile.count("--retry-delay 2"), 1)
        self.assertEqual(containerfile.count("--connect-timeout 30"), 1)
        self.assertIn(
            'for download_url in "${primary_url}" "${fallback_url}"',
            containerfile,
        )
        self.assertIn("test \"${downloaded}\" = '1'", containerfile)
        self.assertIn(lock["debian_snapshot"]["timestamp"], containerfile)
        for dependency in (
            "libcairo2=1.16.0-7",
            "libcups2=2.4.2-3+deb12u9",
            "libdbus-1-3=1.14.10-1~deb12u1",
            "libglib2.0-0=2.74.6-2+deb12u9",
            "libnss3=2:3.87.1-1+deb12u2",
            "libx11-xcb1=2:1.8.4-2+deb12u2",
            "libxinerama1=2:1.1.4-3",
        ):
            self.assertIn(dependency, containerfile)
        self.assertIn("if ! ldd", containerfile)
        self.assertIn("/opt/libreoffice26.2/program/oosplash", containerfile)
        self.assertIn("/opt/libreoffice26.2/program/soffice.bin", containerfile)
        self.assertIn("grep --fixed-strings '=> not found'", containerfile)
        self.assertIn("grep_status=0", containerfile)
        self.assertIn("|| grep_status=$?", containerfile)
        self.assertIn("1) ;;", containerfile)
        self.assertIn('LC_ALL=C sort --unique "${missing_output}"', containerfile)
        self.assertIn(
            "LibreOffice runtime dependency closure is incomplete",
            containerfile,
        )
        self.assertIn(
            "LibreOffice runtime dependency closure check failed",
            containerfile,
        )
        self.assertIn(
            "LibreOffice runtime dependency scan failed",
            containerfile,
        )
        for suffix in (
            "*.ttf",
            "*.otf",
            "*.ttc",
            "*.otc",
            "*.pfa",
            "*.pfb",
            "*.afm",
            "*.pcf",
            "*.pcf.gz",
            "*.bdf",
            "*.woff",
            "*.woff2",
        ):
            self.assertEqual(containerfile.count(f"-iname '{suffix}'"), 2)
        self.assertIn("find /usr /opt -type f", containerfile)
        self.assertIn(
            "LibreOffice image font closure is not empty",
            containerfile,
        )
        self.assertIn(
            f"ARG SOURCE_DATE_EPOCH={lock['built_image']['source_date_epoch']}",
            containerfile,
        )
        self.assertIn("/var/log/dpkg.log", containerfile)
        self.assertIn("/var/log/apt/*", containerfile)
        self.assertIn("/var/cache/ldconfig/aux-cache", containerfile)
        self.assertIn("/var/cache/fontconfig/*", containerfile)
        self.assertIn("/var/lib/dpkg/status-old", containerfile)
        self.assertNotRegex(containerfile, r"^FROM\s+[^\n]+:(?:latest|bookworm-slim)\s*$")

    def test_image_build_command_locks_every_reproducibility_input(self) -> None:
        lock, _, contract = MODULE.load_lock()
        metadata = Path("/safe/metadata.json")
        command = MODULE.build_build_command(
            "docker",
            "local/oracle:test",
            contract,
            builder_name="rxls-unit-builder",
            metadata_file=metadata,
        )
        pairs = list(zip(command, command[1:]))
        self.assertEqual(command[:3], ["docker", "buildx", "build"])
        self.assertIn(("--builder", "rxls-unit-builder"), pairs)
        self.assertIn(("--platform", "linux/amd64"), pairs)
        self.assertIn("--pull=false", command)
        self.assertIn("--no-cache", command)
        self.assertIn("--provenance=false", command)
        self.assertIn("--sbom=false", command)
        self.assertNotIn("BUILDKIT_MULTI_PLATFORM=1", command)
        self.assertIn("ORACLE_LOCK_SHA256=" + contract, command)
        self.assertIn(
            f"SOURCE_DATE_EPOCH={lock['built_image']['source_date_epoch']}",
            command,
        )
        self.assertIn(
            (
                "type=docker,dest=-,tar=true,rewrite-timestamp=true,"
                "oci-mediatypes=false"
            ),
            command,
        )
        self.assertFalse(
            any("compatibility-version" in token for token in command)
        )
        self.assertIn(("--metadata-file", str(metadata)), pairs)

    def test_bounded_docker_archive_binds_the_exact_config_blob(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            archive = root / "image.tar"
            make_docker_build_archive(archive)
            MODULE.validate_build_archive(archive)
            MODULE.verify_docker_archive_config(
                archive, FAKE_CONFIG_ID
            )
            with self.assertRaisesRegex(
                MODULE.OracleContainerError,
                "build_archive_config_digest",
            ):
                MODULE.verify_docker_archive_config(
                    archive, "sha256:" + "0" * 64
                )

            duplicate = root / "duplicate.tar"
            manifest = b'[{"Config":"config.json"}]'
            with tarfile.open(duplicate, mode="w") as bundle:
                for name, payload in (
                    ("manifest.json", manifest),
                    ("manifest.json", manifest),
                    ("config.json", FAKE_IMAGE_CONFIG),
                ):
                    info = tarfile.TarInfo(name)
                    info.size = len(payload)
                    bundle.addfile(info, io.BytesIO(payload))
            with self.assertRaisesRegex(
                MODULE.OracleContainerError,
                "build_archive_duplicate",
            ):
                MODULE.verify_docker_archive_config(
                    duplicate, FAKE_CONFIG_ID
                )

            empty = root / "empty.tar"
            empty.write_bytes(b"")
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "build_archive_limit"
            ):
                MODULE.validate_build_archive(empty)

            oversized = root / "oversized.tar"
            with oversized.open("wb") as stream:
                stream.truncate(MODULE.MAX_BUILD_ARCHIVE_BYTES + 1)
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "build_archive_limit"
            ):
                MODULE.validate_build_archive(oversized)

    def test_explicit_archive_load_failure_fails_closed_and_cleans_builder(
        self,
    ) -> None:
        runner = FakeRunner("d" * 64, load_status="nonzero")
        with redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "image_load_nonzero"
            ):
                MODULE.execute_build(
                    "docker",
                    "rxls-oracle:test",
                    "d" * 64,
                    runner=runner,
                )
        self.assertEqual(runner.commands[-1][1:3], ["buildx", "rm"])
        self.assertEqual(
            len(
                [
                    command
                    for command in runner.commands
                    if command[1:3] == ["image", "inspect"]
                ]
            ),
            0,
        )

    def test_failed_image_build_emits_bounded_path_neutral_diagnostics(self) -> None:
        stderr = (
            f"\x1b[31mstep at {MODULE.ROOT}/private\r\n"
            "curl: (1) Protocol http disabled\rtrailer\n"
        ).encode()
        runner = FakeRunner(
            "d" * 64,
            build_result=MODULE.CommandResult(
                "nonzero",
                42,
                b"stdout detail\n",
                stderr,
            ),
        )

        diagnostic = io.StringIO()
        with redirect_stderr(diagnostic), self.assertRaisesRegex(
            MODULE.OracleContainerError, "image_build_nonzero"
        ):
            MODULE.execute_build(
                "docker",
                "local/oracle:test",
                "d" * 64,
                runner=runner,
            )

        rendered = diagnostic.getvalue()
        self.assertIn("status=nonzero returncode=42", rendered)
        self.assertIn(f"stderr_sha256={sha256(stderr)}", rendered)
        self.assertIn("<repo>/private\ncurl: (1) Protocol http disabled\ntrailer", rendered)
        self.assertNotIn(str(MODULE.ROOT), rendered)
        self.assertNotIn("\x1b", rendered)
        self.assertEqual(runner.commands[-1][1:3], ["buildx", "rm"])

    def test_container_and_host_profiles_split_active_content_policy(self) -> None:
        container_profile = CONTAINER_DIR / "profile" / "registrymodifications.xcu"
        host_profile = ROOT / "scripts" / "render-oracle-host-profile.xcu"
        container_profile_sha256 = sha256(container_profile.read_bytes())
        host_profile_sha256 = sha256(host_profile.read_bytes())
        self.assertNotEqual(container_profile_sha256, host_profile_sha256)

        container_lock = json.loads((CONTAINER_DIR / "lock.json").read_text())
        profile_rows = [
            row
            for row in container_lock["files"]
            if row["path"] == "profile/registrymodifications.xcu"
        ]
        self.assertEqual(len(profile_rows), 1)
        self.assertEqual(profile_rows[0]["sha256"], container_profile_sha256)

        host_lock = json.loads(
            (ROOT / "scripts" / "render-oracle-lock.json").read_text()
        )
        self.assertGreater(len(host_lock["profiles"]), 0)
        for profile in host_lock["profiles"]:
            self.assertEqual(
                profile["configuration"]["profile_sha256"], host_profile_sha256
            )

        def profile_settings(path: Path) -> dict[tuple[str, str], str]:
            root = ET.parse(path).getroot()
            oor = "{http://openoffice.org/2001/registry}"
            settings: dict[tuple[str, str], str] = {}
            for item in root.findall("item"):
                item_path = item.attrib[f"{oor}path"]
                for prop in item.findall("prop"):
                    name = prop.attrib[f"{oor}name"]
                    value = prop.findtext("value")
                    self.assertIsNotNone(value)
                    key = (item_path, name)
                    self.assertNotIn(key, settings)
                    settings[key] = value
            return settings

        scripting = "/org.openoffice.Office.Common/Security/Scripting"
        container_settings = profile_settings(container_profile)
        host_settings = profile_settings(host_profile)
        self.assertEqual(
            container_settings[(scripting, "DisableActiveContent")], "false"
        )
        self.assertEqual(host_settings[(scripting, "DisableActiveContent")], "true")
        for settings in (container_settings, host_settings):
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
                "1",
            )

        entrypoint = (CONTAINER_DIR / "oracle-entrypoint.sh").read_text()
        containerfile = (CONTAINER_DIR / "Containerfile").read_text()
        self.assertIn("SinglePageSheets", entrypoint)
        self.assertIn("UserInstallation=file://", entrypoint)
        self.assertIn(
            'test -r "${profile_seed}" || fail profile_source_unreadable',
            entrypoint,
        )
        self.assertIn(
            'test -w "${profile}/user" || fail profile_target_not_writable',
            entrypoint,
        )
        self.assertIn(
            'cat "${profile_seed}" > "${profile_config}" '
            "|| fail profile_copy_failed",
            entrypoint,
        )
        self.assertIn(
            'chmod 0600 "${profile_config}" '
            "|| fail profile_permissions_failed",
            entrypoint,
        )
        self.assertIn("|| fail evidence_archive_failed", entrypoint)
        self.assertNotIn("curl ", entrypoint)
        self.assertIn(
            "find /oracle/fonts/fonts",
            entrypoint,
        )
        self.assertIn(
            "fc-list --format='%{file}\\n'",
            entrypoint,
        )
        self.assertIn(
            'cmp -s "${expected_fonts}" "${active_fonts}"',
            entrypoint,
        )
        self.assertIn("font_runtime_closure_mismatch", entrypoint)
        self.assertIn(
            "chmod 0555 /opt/rxls /opt/rxls/profile",
            containerfile,
        )
        self.assertIn(
            "RUN test -f /opt/rxls/profile/registrymodifications.xcu",
            containerfile,
        )

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

    def test_canonical_build_is_docker_only_but_podman_runtime_remains_valid(
        self,
    ) -> None:
        for operation in (
            lambda: MODULE.buildx_create_command("podman", "rxls-builder"),
            lambda: MODULE.build_build_command(
                "podman", "rxls-oracle:test", "d" * 64
            ),
            lambda: MODULE.execute_build(
                "podman",
                "rxls-oracle:test",
                "d" * 64,
                runner=FakeRunner("d" * 64),
            ),
        ):
            with self.subTest(operation=operation), self.assertRaisesRegex(
                MODULE.OracleContainerError, "build_engine_docker_required"
            ):
                operation()

        rejected = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "build",
                "--engine",
                "podman",
                "--dry-run",
            ],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(rejected.returncode, 2)
        self.assertIn(b"invalid choice", rejected.stderr)

    def test_pinned_buildx_builder_lifecycle_proves_two_isolated_builds(
        self,
    ) -> None:
        lock_sha = "d" * 64
        image_id = FAKE_CONFIG_ID
        runner = FakeRunner(lock_sha, image_ids=(image_id, image_id))
        result = MODULE.execute_build(
            "docker",
            "rxls-oracle:test",
            lock_sha,
            expected_image_id=image_id,
            runner=runner,
        )

        self.assertEqual(result.image_id, image_id)
        self.assertEqual(len(result.identities), 2)
        self.assertEqual(result.identities[0], result.identities[1])
        self.assertEqual(
            result.identities[0].manifest_digest,
            "sha256:" + "d" * 64,
        )
        self.assertEqual(
            result.identities[0].descriptor_digest,
            result.identities[0].manifest_digest,
        )
        self.assertEqual(
            result.identities[0].descriptor_media_type,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        self.assertEqual(result.identities[0].descriptor_size, 1234)
        self.assertEqual(
            result.identities[0].descriptor_platform,
            (("architecture", "amd64"), ("os", "linux")),
        )
        self.assertEqual(
            result.identities[0].descriptor_annotations,
            (
                (
                    "org.opencontainers.image.created",
                    MODULE.SOURCE_DATE_EPOCH_RFC3339,
                ),
            ),
        )
        self.assertEqual(
            result.evidence(),
            {
                "build_count": 2,
                "buildkit_compatibility": {
                    "explicit": False,
                    "source": MODULE.BUILDKIT_COMPATIBILITY_SOURCE,
                    "version": (
                        MODULE.BUILDKIT_DEFAULT_COMPATIBILITY_VERSION
                    ),
                },
                "buildkit_commit": MODULE.BUILDKIT_COMMIT,
                "buildkit_image": MODULE.BUILDKIT_IMAGE,
                "buildkit_version": "v0.31.2",
                "buildx_commit": MODULE.BUILDX_COMMIT,
                "buildx_version": "v0.35.0",
                "config_ids": [image_id, image_id],
                "descriptor_digests": [
                    result.identities[0].descriptor_digest,
                    result.identities[0].descriptor_digest,
                ],
                "descriptor_media_types": [
                    result.identities[0].descriptor_media_type,
                    result.identities[0].descriptor_media_type,
                ],
                "descriptor_sizes": [
                    result.identities[0].descriptor_size,
                    result.identities[0].descriptor_size,
                ],
                "driver": "docker-container",
                "export_archive_max_bytes": MODULE.MAX_BUILD_ARCHIVE_BYTES,
                "export_destination": "stdout",
                "export_media_type": MODULE.DOCKER_V2_MANIFEST_MEDIA_TYPE,
                "export_tar": True,
                "identities": [
                    result.identities[0].evidence_row(),
                    result.identities[0].evidence_row(),
                ],
                "identity_sha256": [
                    result.identities[0].identity_sha256,
                    result.identities[0].identity_sha256,
                ],
                "manifest_digests": [
                    result.identities[0].manifest_digest,
                    result.identities[0].manifest_digest,
                ],
                "no_cache": True,
                "provenance": False,
                "rewrite_timestamp": True,
                "rootfs_diff_ids_sha256": [
                    result.identities[0].diff_ids_sha256,
                    result.identities[0].diff_ids_sha256,
                ],
                "sbom": False,
                "snapshotter": MODULE.BUILDKIT_SNAPSHOTTER,
                "source_date_epoch": MODULE.SOURCE_DATE_EPOCH,
                "status": "matched",
            },
        )

        version_commands = [
            command
            for command in runner.commands
            if command[1:3] == ["buildx", "version"]
        ]
        creates = [
            command
            for command in runner.commands
            if command[1:3] == ["buildx", "create"]
        ]
        inspections = [
            command
            for command in runner.commands
            if command[1:3] == ["buildx", "inspect"]
        ]
        builds = [
            command
            for command in runner.commands
            if command[1:3] == ["buildx", "build"]
        ]
        image_inspections = [
            command
            for command in runner.commands
            if command[1:3] == ["image", "inspect"]
        ]
        image_loads = [
            command
            for command in runner.commands
            if command[1:3] == ["image", "load"]
        ]
        cleanups = [
            command
            for command in runner.commands
            if command[1:3] == ["buildx", "rm"]
        ]
        self.assertEqual(len(version_commands), 1)
        self.assertEqual(
            (
                len(creates),
                len(inspections),
                len(builds),
                len(image_loads),
                len(image_inspections),
            ),
            (2, 2, 2, 2, 2),
        )
        self.assertEqual(len(cleanups), 2)
        lifecycle = [
            command[1:3]
            for command in runner.commands
            if command[1:3]
            in (
                ["buildx", "build"],
                ["image", "load"],
                ["image", "inspect"],
                ["buildx", "rm"],
            )
        ]
        self.assertEqual(
            lifecycle,
            [
                ["buildx", "build"],
                ["image", "load"],
                ["image", "inspect"],
                ["buildx", "rm"],
            ]
            * 2,
        )

        builder_names = [command[command.index("--name") + 1] for command in creates]
        self.assertEqual(len(set(builder_names)), 2)
        for index, builder_name in enumerate(builder_names):
            create = creates[index]
            self.assertIn(("--driver", "docker-container"), zip(create, create[1:]))
            self.assertIn(
                ("--driver-opt", f"image={MODULE.BUILDKIT_IMAGE}"),
                zip(create, create[1:]),
            )
            self.assertIn(
                ("--driver-opt", "provenance-add-gha=false"),
                zip(create, create[1:]),
            )
            self.assertIn(
                (
                    "--buildkitd-flags",
                    f"--oci-worker-snapshotter={MODULE.BUILDKIT_SNAPSHOTTER}",
                ),
                zip(create, create[1:]),
            )
            self.assertIn("--bootstrap", create)
            self.assertEqual(
                inspections[index],
                [
                    "docker",
                    "buildx",
                    "inspect",
                    "--builder",
                    builder_name,
                    "--bootstrap",
                ],
            )
            self.assertEqual(
                builds[index][builds[index].index("--builder") + 1],
                builder_name,
            )
            self.assertIn("--no-cache", builds[index])
            self.assertEqual(
                cleanups[index],
                [
                    "docker",
                    "buildx",
                    "rm",
                    "--force",
                    builder_name,
                ],
            )

    def test_buildx_and_buildkit_runtime_identity_must_match_the_lock(self) -> None:
        lock_sha = "d" * 64
        client_cases = (
            b"github.com/docker/buildx v0.34.0 "
            + MODULE.BUILDX_COMMIT.encode(),
            b"github.com/docker/buildx v0.35.0 wrong-commit",
        )
        for output in client_cases:
            runner = FakeRunner(lock_sha, buildx_version_output=output)
            with self.subTest(output=output), self.assertRaisesRegex(
                MODULE.OracleContainerError, "buildx_client_identity"
            ):
                MODULE.verify_buildx_client(runner, "docker")

        descriptions = (
            b"BuildKit version: v0.31.1\n"
            b"org.mobyproject.buildkit.worker.snapshotter: native\n",
            b"BuildKit version: v0.31.2\n"
            b"org.mobyproject.buildkit.worker.snapshotter: overlayfs\n",
        )
        for description, error in zip(
            descriptions, ("buildkit_version", "buildkit_snapshotter")
        ):
            with self.subTest(description=description), self.assertRaisesRegex(
                MODULE.OracleContainerError, error
            ):
                MODULE.verify_buildx_builder_description(
                    MODULE.CommandResult("ok", 0, description)
                )

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

    def test_image_inspection_requires_exact_config_platform_epoch_and_rootfs(
        self,
    ) -> None:
        lock_sha = "e" * 64
        runner = FakeRunner(
            lock_sha,
            label_mutation={"org.opencontainers.image.version": "wrong"},
        )
        with self.assertRaisesRegex(MODULE.OracleContainerError, "image_label_mismatch"):
            MODULE.inspect_image(runner, "docker", "image", lock_sha)

        runner = FakeRunner(lock_sha)
        identity = MODULE.inspect_image_identity(
            runner,
            "docker",
            "image",
            lock_sha,
            FAKE_CONFIG_ID,
        )
        self.assertEqual(identity.image_id, FAKE_CONFIG_ID)
        self.assertEqual(identity.platform, "linux/amd64")
        self.assertEqual(identity.created, MODULE.SOURCE_DATE_EPOCH_RFC3339)
        self.assertEqual(
            identity.diff_ids,
            (
                "sha256:" + "b" * 64,
                "sha256:" + "c" * 64,
            ),
        )

        invalid_cases = (
            (
                {"architecture": "arm64"},
                "image_architecture",
            ),
            (
                {"operating_system": "windows"},
                "image_operating_system",
            ),
            (
                {"created": "2026-07-13T00:00:01Z"},
                "image_created",
            ),
            (
                {"rootfs_type": "other"},
                "image_rootfs",
            ),
            (
                {"diff_id_sets": ((),)},
                "image_rootfs",
            ),
            (
                {"diff_id_sets": (("not-a-diff-id",),)},
                "image_rootfs",
            ),
        )
        for runner_options, error in invalid_cases:
            with self.subTest(options=runner_options), self.assertRaisesRegex(
                MODULE.OracleContainerError, error
            ):
                MODULE.inspect_image_identity(
                    FakeRunner(lock_sha, **runner_options),
                    "docker",
                    "image",
                    lock_sha,
                )

        diagnostic = io.StringIO()
        with redirect_stderr(diagnostic), self.assertRaisesRegex(
            MODULE.OracleContainerError, "image_id_mismatch"
        ):
            MODULE.inspect_image(
                FakeRunner(lock_sha),
                "docker",
                "image",
                lock_sha,
                "sha256:" + "b" * 64,
            )
        rendered = diagnostic.getvalue()
        self.assertIn("render_oracle_image_identity_diagnostic ", rendered)
        self.assertIn(
            '"reason":"expected_loaded_store_id_mismatch"', rendered
        )
        self.assertIn('"expected_image_id":"sha256:' + "b" * 64 + '"', rendered)
        self.assertIn('"rootfs_diff_ids":2', rendered)
        self.assertNotIn(str(ROOT), rendered)

        manifest_digest = "sha256:" + "d" * 64
        containerd_identity = MODULE.inspect_image_identity(
            FakeRunner(
                lock_sha,
                image_ids=(manifest_digest,),
                manifest_digests=(manifest_digest,),
            ),
            "docker",
            "image",
            lock_sha,
            FAKE_CONFIG_ID,
            manifest_digest,
        )
        self.assertEqual(containerd_identity.image_id, FAKE_CONFIG_ID)

    def test_build_metadata_requires_manifest_identity_and_valid_descriptor(
        self,
    ) -> None:
        config_digest = "sha256:" + "a" * 64
        manifest_digest = "sha256:" + "d" * 64
        valid = {
            "containerimage.config.digest": config_digest,
            "containerimage.descriptor": {
                "annotations": {
                    "org.opencontainers.image.created": (
                        MODULE.SOURCE_DATE_EPOCH_RFC3339
                    ),
                },
                "digest": manifest_digest,
                "mediaType": (
                    "application/vnd.docker.distribution."
                    "manifest.v2+json"
                ),
                "platform": {
                    "architecture": "amd64",
                    "os": "linux",
                },
                "size": 1234,
            },
            "containerimage.digest": manifest_digest,
        }
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "metadata.json"
            path.write_bytes(MODULE.canonical_json_bytes(valid))
            identity = MODULE.read_build_metadata(path)
            self.assertEqual(identity.config_digest, config_digest)
            self.assertEqual(identity.manifest_digest, manifest_digest)
            self.assertEqual(identity.descriptor_digest, manifest_digest)
            self.assertEqual(
                identity.descriptor_media_type,
                "application/vnd.docker.distribution.manifest.v2+json",
            )
            self.assertEqual(identity.descriptor_size, 1234)
            self.assertEqual(
                identity.descriptor_platform,
                (("architecture", "amd64"), ("os", "linux")),
            )
            self.assertEqual(
                identity.descriptor_annotations,
                (
                    (
                        "org.opencontainers.image.created",
                        MODULE.SOURCE_DATE_EPOCH_RFC3339,
                    ),
                ),
            )
            equivalent_created = json.loads(json.dumps(valid))
            equivalent_created["containerimage.descriptor"]["annotations"][
                "org.opencontainers.image.created"
            ] = "2026-07-13T09:00:00+09:00"
            path.write_bytes(
                MODULE.canonical_json_bytes(equivalent_created)
            )
            self.assertEqual(
                MODULE.read_build_metadata(path).descriptor_annotations,
                identity.descriptor_annotations,
            )
            without_platform = json.loads(json.dumps(valid))
            without_platform["containerimage.descriptor"].pop("platform")
            path.write_bytes(MODULE.canonical_json_bytes(without_platform))
            self.assertIsNone(
                MODULE.read_build_metadata(path).descriptor_platform
            )

            without_descriptor = dict(valid)
            without_descriptor.pop("containerimage.descriptor")
            path.write_bytes(
                MODULE.canonical_json_bytes(without_descriptor)
            )
            with self.assertRaisesRegex(
                MODULE.OracleContainerError,
                "build_metadata_descriptor",
            ):
                MODULE.read_build_metadata(path)

            invalid_cases = (
                ([], "build_metadata_unreadable"),
                (
                    {
                        key: value
                        for key, value in valid.items()
                        if key != "containerimage.config.digest"
                    },
                    "build_metadata_config_digest",
                ),
                (
                    {**valid, "containerimage.config.digest": "sha256:bad"},
                    "build_metadata_config_digest",
                ),
                (
                    {
                        key: value
                        for key, value in valid.items()
                        if key != "containerimage.digest"
                    },
                    "build_metadata_manifest_digest",
                ),
                (
                    {**valid, "containerimage.digest": "sha256:bad"},
                    "build_metadata_manifest_digest",
                ),
                (
                    {**valid, "containerimage.descriptor": None},
                    "build_metadata_descriptor",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            key: value
                            for key, value in valid[
                                "containerimage.descriptor"
                            ].items()
                            if key != "annotations"
                        },
                    },
                    "build_metadata_descriptor",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "annotations": [],
                        },
                    },
                    "build_metadata_descriptor_annotations",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "annotations": {
                                "config.digest": config_digest,
                                "org.opencontainers.image.created": (
                                    MODULE.SOURCE_DATE_EPOCH_RFC3339
                                ),
                            },
                        },
                    },
                    "build_metadata_descriptor_annotations",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "annotations": {
                                "org.opencontainers.image.created": 0,
                            },
                        },
                    },
                    "build_metadata_descriptor_annotations",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "annotations": {
                                "org.opencontainers.image.created": (
                                    "2026-07-13T00:00:01Z"
                                ),
                            },
                        },
                    },
                    "build_metadata_descriptor_created",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "platform": {
                                "architecture": "arm64",
                                "os": "linux",
                            },
                        },
                    },
                    "build_metadata_descriptor_platform",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "digest": "sha256:bad",
                        },
                    },
                    "build_metadata_descriptor_digest",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "digest": "sha256:" + "e" * 64,
                        },
                    },
                    "build_metadata_descriptor_digest_mismatch",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "mediaType": (
                                "application/vnd.oci.image.index.v1+json"
                            ),
                        },
                    },
                    "build_metadata_descriptor_media_type",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "mediaType": (
                                "application/vnd.oci.image.manifest.v1+json"
                            ),
                        },
                    },
                    "build_metadata_descriptor_media_type",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "mediaType": {"value": "not-a-string"},
                        },
                    },
                    "build_metadata_descriptor_media_type",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "size": True,
                        },
                    },
                    "build_metadata_descriptor_size",
                ),
                (
                    {
                        **valid,
                        "containerimage.descriptor": {
                            **valid["containerimage.descriptor"],
                            "size": 0,
                        },
                    },
                    "build_metadata_descriptor_size",
                ),
            )
            for document, error in invalid_cases:
                with self.subTest(error=error):
                    path.write_bytes(
                        MODULE.canonical_json_bytes(document)
                    )
                    with self.assertRaisesRegex(
                        MODULE.OracleContainerError, error
                    ):
                        MODULE.read_build_metadata(path)

    def test_build_metadata_digest_must_equal_the_loaded_config_digest(self) -> None:
        lock_sha = "d" * 64
        runner = FakeRunner(
            lock_sha,
            metadata_ids=("sha256:" + "b" * 64,),
        )
        with self.assertRaisesRegex(
            MODULE.OracleContainerError, "build_archive_config_digest"
        ):
            MODULE.execute_build(
                "docker",
                "rxls-oracle:test",
                lock_sha,
                runner=runner,
            )
        self.assertEqual(runner.commands[-1][1:3], ["buildx", "rm"])

    def test_two_isolated_build_identity_mismatch_is_bounded_and_diagnostic(
        self,
    ) -> None:
        lock_sha = "d" * 64
        runner = FakeRunner(
            lock_sha,
            image_ids=(FAKE_CONFIG_ID,) * 2,
            diff_id_sets=(
                (
                    "sha256:" + "b" * 64,
                    "sha256:" + "c" * 64,
                ),
                (
                    "sha256:" + "b" * 64,
                    "sha256:" + "e" * 64,
                ),
            ),
        )
        diagnostic = io.StringIO()
        with redirect_stderr(diagnostic), self.assertRaisesRegex(
            MODULE.OracleContainerError, "image_reproducibility_mismatch"
        ):
            MODULE.execute_build(
                "docker",
                "rxls-oracle:test",
                lock_sha,
                runner=runner,
            )
        rendered = diagnostic.getvalue()
        self.assertIn('"reason":"isolated_build_identity_mismatch"', rendered)
        self.assertIn('"image_id":"' + FAKE_CONFIG_ID + '"', rendered)
        self.assertLessEqual(
            len(rendered.encode("utf-8")),
            MODULE.MAX_BUILD_DIAGNOSTIC_BYTES,
        )
        self.assertNotIn(str(ROOT), rendered)
        self.assertEqual(
            len(
                [
                    command
                    for command in runner.commands
                    if command[1:3] == ["buildx", "rm"]
                ]
            ),
            2,
        )

    def test_two_isolated_builds_reject_different_manifest_digests(self) -> None:
        lock_sha = "d" * 64
        first_manifest = "sha256:" + "1" * 64
        second_manifest = "sha256:" + "2" * 64
        runner = FakeRunner(
            lock_sha,
            image_ids=(FAKE_CONFIG_ID,) * 2,
            manifest_digests=(first_manifest, second_manifest),
        )
        diagnostic = io.StringIO()
        with redirect_stderr(diagnostic), self.assertRaisesRegex(
            MODULE.OracleContainerError, "image_reproducibility_mismatch"
        ):
            MODULE.execute_build(
                "docker",
                "rxls-oracle:test",
                lock_sha,
                runner=runner,
            )
        rendered = diagnostic.getvalue()
        self.assertIn('"reason":"isolated_build_identity_mismatch"', rendered)
        self.assertIn('"manifest_digest":"' + first_manifest + '"', rendered)
        self.assertIn('"manifest_digest":"' + second_manifest + '"', rendered)
        self.assertEqual(
            len(
                [
                    command
                    for command in runner.commands
                    if command[1:3] == ["buildx", "rm"]
                ]
            ),
            2,
        )

    def test_two_isolated_builds_reject_different_descriptor_identity(
        self,
    ) -> None:
        lock_sha = "d" * 64
        manifest_digest = "sha256:" + "1" * 64
        docker_media_type = (
            "application/vnd.docker.distribution.manifest.v2+json"
        )
        annotations = {
            "org.opencontainers.image.created": (
                MODULE.SOURCE_DATE_EPOCH_RFC3339
            ),
        }
        runner = FakeRunner(
            lock_sha,
            image_ids=(FAKE_CONFIG_ID,) * 2,
            manifest_digests=(manifest_digest,) * 2,
            metadata_descriptors=(
                {
                    "annotations": annotations,
                    "digest": manifest_digest,
                    "mediaType": docker_media_type,
                    "size": 1234,
                },
                {
                    "annotations": annotations,
                    "digest": manifest_digest,
                    "mediaType": docker_media_type,
                    "size": 1235,
                },
            ),
        )
        diagnostic = io.StringIO()
        with redirect_stderr(diagnostic), self.assertRaisesRegex(
            MODULE.OracleContainerError, "image_reproducibility_mismatch"
        ):
            MODULE.execute_build(
                "docker",
                "rxls-oracle:test",
                lock_sha,
                runner=runner,
            )
        rendered = diagnostic.getvalue()
        self.assertIn(
            '"descriptor_media_type":"' + docker_media_type + '"',
            rendered,
        )
        self.assertIn('"descriptor_size":1234', rendered)
        self.assertIn('"descriptor_size":1235', rendered)

    def test_pinned_manifest_digest_must_match_the_reproducible_build(
        self,
    ) -> None:
        lock_sha = "d" * 64
        observed_manifest = "sha256:" + "1" * 64
        expected_manifest = "sha256:" + "2" * 64
        runner = FakeRunner(
            lock_sha,
            image_ids=(FAKE_CONFIG_ID,) * 2,
            manifest_digests=(observed_manifest,) * 2,
        )
        diagnostic = io.StringIO()
        with redirect_stderr(diagnostic), self.assertRaisesRegex(
            MODULE.OracleContainerError,
            "image_manifest_digest_mismatch",
        ):
            MODULE.execute_build(
                "docker",
                "rxls-oracle:test",
                lock_sha,
                expected_image_id=FAKE_CONFIG_ID,
                expected_manifest_digest=expected_manifest,
                runner=runner,
            )
        rendered = diagnostic.getvalue()
        self.assertIn(
            '"reason":"expected_manifest_digest_mismatch"',
            rendered,
        )
        self.assertIn(
            '"expected_manifest_digest":"' + expected_manifest + '"',
            rendered,
        )
        self.assertIn(
            '"manifest_digest":"' + observed_manifest + '"',
            rendered,
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
            expected_manifest_digest = "sha256:" + "d" * 64
            result = MODULE.execute_render(
                config,
                "docker",
                "local/oracle:test",
                lock_sha,
                expected_image_id=FAKE_CONFIG_ID,
                expected_manifest_digest=expected_manifest_digest,
                runner=runner,
            )

            self.assertEqual(result["schema"], MODULE.EXECUTION_SCHEMA)
            self.assertEqual(result["image"]["id"], FAKE_CONFIG_ID)
            self.assertEqual(
                result["image"]["manifest_digest"],
                expected_manifest_digest,
            )
            self.assertEqual(
                result["image"]["expected_manifest_digest"],
                expected_manifest_digest,
            )
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
            self.assertEqual(create[-1], FAKE_CONFIG_ID)
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

    def test_execute_render_propagates_only_reviewed_font_closure_errors(
        self,
    ) -> None:
        _, _, lock_sha = MODULE.load_lock()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "source.xlsx"
            source.write_bytes(b"source")
            font_pack = write_font_pack(root / "font-pack")
            config = MODULE.RenderConfig(
                source=source,
                font_pack=font_pack,
                corpus=None,
                evidence_dir=root / "evidence",
                run_id="closure-test",
                limits=MODULE.ResourceLimits(
                    timeout_seconds=1,
                    memory_mib=512,
                    evidence_mib=16,
                    runtime_mib=64,
                    tmp_mib=64,
                ),
            )
            for code in sorted(MODULE.REVIEWED_ENTRYPOINT_ERROR_CODES):
                runner = FakeRunner(
                    lock_sha,
                    start_status="nonzero",
                    start_stderr=f"oracle_error:{code}\n".encode("ascii"),
                )
                with self.subTest(code=code), self.assertRaisesRegex(
                    MODULE.OracleContainerError,
                    rf"^{code}$",
                ):
                    MODULE.execute_render(
                        config,
                        "docker",
                        "local/oracle:test",
                        lock_sha,
                        runner=runner,
                    )

            for name, stderr in (
                ("unreviewed", b"oracle_error:private_path_suffix\n"),
                (
                    "extra_line",
                    b"diagnostic\noracle_error:font_runtime_closure_mismatch\n",
                ),
                ("oversized", b"x" * 257),
            ):
                runner = FakeRunner(
                    lock_sha,
                    start_status="nonzero",
                    start_stderr=stderr,
                )
                with self.subTest(name=name), self.assertRaisesRegex(
                    MODULE.OracleContainerError,
                    "^container_start_nonzero$",
                ):
                    MODULE.execute_render(
                        config,
                        "docker",
                        "local/oracle:test",
                        lock_sha,
                        runner=runner,
                    )

    def test_bounded_runner_enforces_output_and_wall_time(self) -> None:
        runner = MODULE.BoundedProcessRunner()
        excessive = runner.run(
            [sys.executable, "-c", "import sys; sys.stdout.write('x' * 100000)"],
            timeout_seconds=5,
            output_limit_bytes=1024,
        )
        self.assertEqual(excessive.status, "output_limit")
        self.assertLessEqual(len(excessive.stdout) + len(excessive.stderr), 1024)

        with tempfile.TemporaryDirectory() as raw:
            archive = Path(raw) / "stdout.bin"
            separate = runner.run(
                [
                    sys.executable,
                    "-c",
                    (
                        "import sys; "
                        "sys.stdout.buffer.write(b'a' * 100); "
                        "sys.stderr.buffer.write(b'b' * 5000)"
                    ),
                ],
                timeout_seconds=5,
                output_limit_bytes=10_000,
                stdout_path=archive,
                stdout_limit_bytes=200,
                stderr_limit_bytes=1000,
            )
            self.assertEqual(separate.status, "output_limit")
            self.assertLessEqual(archive.stat().st_size, 200)
            self.assertLessEqual(len(separate.stderr), 1000)

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
            self.assertEqual(output.read_bytes(), f"streamed{os.linesep}".encode())

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
                "docker",
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
        self.assertEqual(
            document["commands"]["buildx_client_version"],
            ["docker", "buildx", "version"],
        )
        isolated = document["commands"]["isolated_builds"]
        self.assertEqual(len(isolated), 2)
        self.assertNotEqual(
            isolated[0]["create"][isolated[0]["create"].index("--name") + 1],
            isolated[1]["create"][isolated[1]["create"].index("--name") + 1],
        )
        for build in isolated:
            self.assertEqual(build["create"][:3], ["docker", "buildx", "create"])
            self.assertEqual(build["build"][:3], ["docker", "buildx", "build"])
            self.assertIn("--no-cache", build["build"])
            self.assertIn("--provenance=false", build["build"])
            self.assertIn("--sbom=false", build["build"])
            self.assertNotIn("BUILDKIT_MULTI_PLATFORM=1", build["build"])
            self.assertIn(
                (
                    "type=docker,dest=-,tar=true,rewrite-timestamp=true,"
                    "oci-mediatypes=false"
                ),
                build["build"],
            )
            self.assertFalse(
                any(
                    "compatibility-version" in token
                    for token in build["build"]
                )
            )
            self.assertIn(
                ("--driver-opt", "provenance-add-gha=false"),
                zip(build["create"], build["create"][1:]),
            )

    def test_hosted_bootstrap_receipt_authenticates_exact_live_metadata_and_zip(
        self,
    ) -> None:
        lock, _, _ = MODULE.load_lock()
        source_identity = fake_source_identity(lock)
        evidence_payload = b'{"hosted":"bootstrap-evidence"}\n'

        def fetch(
            root: Path,
            responses: dict[str, dict],
            archive: bytes,
            *,
            local_payload: bytes = evidence_payload,
            download_result: object | None = None,
        ) -> dict:
            evidence_path = root / "render-oracle-image-build.json"
            evidence_path.write_bytes(local_payload)
            return MODULE.fetch_hosted_bootstrap_receipt(
                evidence_path,
                source_identity,
                run_id=FAKE_GITHUB_RUN_ID,
                run_attempt=FAKE_GITHUB_RUN_ATTEMPT,
                job_id=FAKE_GITHUB_JOB_ID,
                artifact_id=FAKE_GITHUB_ARTIFACT_ID,
                runner=FakeGithubRunner(
                    responses,
                    archive,
                    download_result=download_result,
                ),
            )

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            responses, archive = hosted_bootstrap_api_fixture(
                source_identity, evidence_payload
            )
            runner = FakeGithubRunner(responses, archive)
            path = root / "render-oracle-image-build.json"
            path.write_bytes(evidence_payload)
            receipt = MODULE.fetch_hosted_bootstrap_receipt(
                path,
                source_identity,
                run_id=FAKE_GITHUB_RUN_ID,
                run_attempt=FAKE_GITHUB_RUN_ATTEMPT,
                job_id=FAKE_GITHUB_JOB_ID,
                artifact_id=FAKE_GITHUB_ARTIFACT_ID,
                runner=runner,
            )
            self.assertEqual(
                receipt,
                fake_bootstrap_receipt(
                    evidence_payload,
                    source_identity,
                    artifact_zip=archive,
                ),
            )
            self.assertEqual(len(runner.commands), 4)
            self.assertTrue(runner.commands[-1][-1].endswith("/zip"))
            self.assertNotIn(
                str(root), json.dumps(receipt, sort_keys=True)
            )
            MODULE.validate_bootstrap_receipt(
                receipt,
                source_commit=source_identity.commit,
                evidence_payload=evidence_payload,
            )

            run_endpoint = (
                f"/repos/{MODULE.GITHUB_REPOSITORY}/actions/runs/"
                f"{FAKE_GITHUB_RUN_ID}"
            )
            job_endpoint = (
                f"/repos/{MODULE.GITHUB_REPOSITORY}/actions/jobs/"
                f"{FAKE_GITHUB_JOB_ID}"
            )
            artifact_endpoint = (
                f"/repos/{MODULE.GITHUB_REPOSITORY}/actions/artifacts/"
                f"{FAKE_GITHUB_ARTIFACT_ID}"
            )
            metadata_mutations = (
                (
                    "wrong_repository",
                    run_endpoint,
                    ("repository", "id"),
                    1,
                    "bootstrap_receipt_run",
                ),
                (
                    "wrong_run_id",
                    run_endpoint,
                    ("id",),
                    FAKE_GITHUB_RUN_ID + 1,
                    "bootstrap_receipt_run",
                ),
                (
                    "wrong_run_attempt",
                    run_endpoint,
                    ("run_attempt",),
                    FAKE_GITHUB_RUN_ATTEMPT + 1,
                    "bootstrap_receipt_run",
                ),
                (
                    "wrong_head",
                    run_endpoint,
                    ("head_sha",),
                    "0" * 40,
                    "bootstrap_receipt_run",
                ),
                (
                    "wrong_workflow",
                    run_endpoint,
                    ("path",),
                    ".github/workflows/ci.yml",
                    "bootstrap_receipt_run",
                ),
                (
                    "wrong_event",
                    run_endpoint,
                    ("event",),
                    "workflow_dispatch",
                    "bootstrap_receipt_run",
                ),
                (
                    "wrong_job_id",
                    job_endpoint,
                    ("id",),
                    FAKE_GITHUB_JOB_ID + 1,
                    "bootstrap_receipt_job",
                ),
                (
                    "wrong_job_name",
                    job_endpoint,
                    ("name",),
                    "oracle image",
                    "bootstrap_receipt_job",
                ),
                (
                    "job_not_failed",
                    job_endpoint,
                    ("conclusion",),
                    "success",
                    "bootstrap_receipt_job",
                ),
                (
                    "wrong_artifact_id",
                    artifact_endpoint,
                    ("id",),
                    FAKE_GITHUB_ARTIFACT_ID + 1,
                    "bootstrap_receipt_artifact",
                ),
                (
                    "wrong_artifact_name",
                    artifact_endpoint,
                    ("name",),
                    "render-oracle-image-wrong",
                    "bootstrap_receipt_artifact",
                ),
                (
                    "wrong_artifact_digest",
                    artifact_endpoint,
                    ("digest",),
                    "sha256:" + "0" * 64,
                    "bootstrap_receipt_download",
                ),
                (
                    "wrong_artifact_size",
                    artifact_endpoint,
                    ("size_in_bytes",),
                    len(archive) + 1,
                    "bootstrap_receipt_download",
                ),
                (
                    "wrong_artifact_source",
                    artifact_endpoint,
                    ("workflow_run", "head_sha"),
                    "0" * 40,
                    "bootstrap_receipt_artifact",
                ),
            )
            for name, endpoint, keys, value, error in metadata_mutations:
                mutated = json.loads(json.dumps(responses))
                target = mutated[endpoint]
                for key in keys[:-1]:
                    target = target[key]
                target[keys[-1]] = value
                with self.subTest(metadata=name), self.assertRaisesRegex(
                    MODULE.OracleContainerError, error
                ):
                    fetch(root, mutated, archive)

            malformed_archives = {
                "not_zip": b"not a zip archive",
                "multiple_members": make_bootstrap_artifact_zip(
                    evidence_payload,
                    members=[
                        (
                            MODULE.GITHUB_BOOTSTRAP_EVIDENCE_MEMBER,
                            evidence_payload,
                            None,
                        ),
                        ("extra.json", b"{}\n", None),
                    ],
                ),
                "wrong_member": make_bootstrap_artifact_zip(
                    evidence_payload,
                    members=[
                        (
                            "../render-oracle-image-build.json",
                            evidence_payload,
                            None,
                        )
                    ],
                ),
                "symlink_member": make_bootstrap_artifact_zip(
                    evidence_payload,
                    members=[
                        (
                            MODULE.GITHUB_BOOTSTRAP_EVIDENCE_MEMBER,
                            b"/host/evidence",
                            0o120777,
                        )
                    ],
                ),
            }
            for name, malformed_archive in malformed_archives.items():
                malformed_responses, _ = hosted_bootstrap_api_fixture(
                    source_identity,
                    evidence_payload,
                    artifact_zip=malformed_archive,
                )
                with self.subTest(zip=name), self.assertRaisesRegex(
                    MODULE.OracleContainerError, "bootstrap_receipt_zip"
                ):
                    fetch(root, malformed_responses, malformed_archive)

            other_evidence = b'{"hosted":"different"}\n'
            other_responses, other_archive = (
                hosted_bootstrap_api_fixture(
                    source_identity, other_evidence
                )
            )
            with self.assertRaisesRegex(
                MODULE.OracleContainerError,
                "bootstrap_receipt_evidence",
            ):
                fetch(root, other_responses, other_archive)

            with self.assertRaisesRegex(
                MODULE.OracleContainerError,
                "bootstrap_receipt_download",
            ):
                fetch(
                    root,
                    responses,
                    archive,
                    download_result=MODULE.CommandResult(
                        "output_limit", None
                    ),
                )

    def test_image_pin_requires_exact_current_bootstrap_build_evidence(self) -> None:
        lock, payload, contract = MODULE.load_lock()
        lock["built_image"]["expected_id"] = None
        lock["built_image"]["expected_manifest_digest"] = None
        lock["built_image"]["bootstrap_receipt"] = None
        source_identity = fake_source_identity(lock)
        image_id = "sha256:" + "b" * 64
        manifest_digest = "sha256:" + "e" * 64
        descriptor_media_type = (
            "application/vnd.docker.distribution.manifest.v2+json"
        )
        identity = MODULE.ImageIdentity(
            image_id=image_id,
            platform="linux/amd64",
            created=MODULE.SOURCE_DATE_EPOCH_RFC3339,
            diff_ids=(
                "sha256:" + "c" * 64,
                "sha256:" + "d" * 64,
            ),
            labels=tuple(
                sorted(
                    {
                        **MODULE.EXPECTED_IMAGE_LABELS,
                        "org.rxls.render-oracle.lock-sha256": contract,
                    }.items()
                )
            ),
            manifest_digest=manifest_digest,
            descriptor_digest=manifest_digest,
            descriptor_media_type=descriptor_media_type,
            descriptor_size=1234,
            descriptor_annotations=(
                (
                    "org.opencontainers.image.created",
                    MODULE.SOURCE_DATE_EPOCH_RFC3339,
                ),
            ),
            descriptor_platform=(
                ("architecture", "amd64"),
                ("os", "linux"),
            ),
        )
        identity_row = identity.evidence_row()
        different_identity_row = replace(
            identity, descriptor_size=1235
        ).evidence_row()
        evidence = {
            "build_contract_sha256": contract,
            "built_image_id": image_id,
            "built_manifest_digest": manifest_digest,
            "expected_image_id": None,
            "expected_manifest_digest": None,
            "image_identity_status": "bootstrap_capture_required",
            "lock_file_sha256": sha256(payload),
            "platform": "linux/amd64",
            "reproducibility": MODULE.ReproducibleBuild(
                (identity, identity)
            ).evidence(),
            "schema": MODULE.BUILD_EVIDENCE_SCHEMA,
            "source_commit": source_identity.commit,
            "status": "ok",
            "wrapper_sha256": source_identity.wrapper_sha256,
        }
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            path = root / "build.json"
            path.write_bytes(MODULE.canonical_json_bytes(evidence))

            def pin_current() -> dict:
                return MODULE.pin_image_from_evidence(
                    lock,
                    payload,
                    contract,
                    path,
                    source_identity,
                    fake_bootstrap_receipt(
                        path.read_bytes(), source_identity
                    ),
                )

            pinned = pin_current()
            self.assertEqual(
                pinned["built_image"]["expected_id"], image_id
            )
            self.assertEqual(
                pinned["built_image"]["expected_manifest_digest"],
                manifest_digest,
            )
            self.assertEqual(
                pinned["built_image"]["bootstrap_receipt"],
                fake_bootstrap_receipt(
                    MODULE.canonical_json_bytes(evidence),
                    source_identity,
                ),
            )
            wrong_receipt = fake_bootstrap_receipt(
                MODULE.canonical_json_bytes(evidence),
                source_identity,
            )
            wrong_receipt["run"]["head_sha"] = "0" * 40
            with self.assertRaisesRegex(
                MODULE.OracleContainerError,
                "bootstrap_receipt_source",
            ):
                MODULE.pin_image_from_evidence(
                    lock,
                    payload,
                    contract,
                    path,
                    source_identity,
                    wrong_receipt,
                )
            output_lock = root / "lock.pinned.json"
            output_bytes, output_sha256 = MODULE.write_pinned_lock(
                pinned,
                output_lock,
                expected_output=output_lock,
            )
            self.assertEqual(output_bytes, output_lock.stat().st_size)
            self.assertEqual(
                output_sha256, sha256(output_lock.read_bytes())
            )
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "pinned_lock_write"
            ):
                MODULE.write_pinned_lock(
                    pinned,
                    output_lock,
                    expected_output=output_lock,
                )
            self.assertEqual(
                output_lock.read_bytes(),
                MODULE.canonical_json_bytes(pinned),
            )
            identity_rows = evidence["reproducibility"]["identities"]
            self.assertEqual(identity_rows, [identity_row, identity_row])
            self.assertEqual(
                identity_rows[0]["rootfs_diff_ids"],
                list(identity.diff_ids),
            )
            self.assertEqual(
                identity_rows[0]["descriptor"]["annotations"],
                {
                    "org.opencontainers.image.created": (
                        MODULE.SOURCE_DATE_EPOCH_RFC3339
                    ),
                },
            )
            for key in (
                "build_contract_sha256",
                "built_image_id",
                "built_manifest_digest",
                "lock_file_sha256",
                "image_identity_status",
                "source_commit",
                "wrapper_sha256",
            ):
                tampered = json.loads(json.dumps(evidence))
                tampered[key] = (
                    "runtime_verified_unpinned"
                    if key == "image_identity_status"
                    else (
                        "0" * 40
                        if key == "source_commit"
                        else "0" * 64
                    )
                )
                path.write_bytes(MODULE.canonical_json_bytes(tampered))
                with self.subTest(key=key):
                    with self.assertRaises(MODULE.OracleContainerError):
                        pin_current()

            reproducibility_mutations = (
                ("build_count", 1),
                ("buildkit_version", "v0.31.1"),
                ("buildx_version", "v0.34.0"),
                (
                    "buildkit_compatibility",
                    {
                        "explicit": True,
                        "source": MODULE.BUILDKIT_COMPATIBILITY_SOURCE,
                        "version": (
                            MODULE.BUILDKIT_DEFAULT_COMPATIBILITY_VERSION
                        ),
                    },
                ),
                ("driver", "docker"),
                ("export_archive_max_bytes", 1),
                ("export_destination", "daemon"),
                ("export_media_type", "application/vnd.oci.image.manifest.v1+json"),
                ("export_tar", False),
                ("no_cache", False),
                ("provenance", True),
                ("rewrite_timestamp", False),
                ("sbom", True),
                ("snapshotter", "overlayfs"),
                ("status", "unmatched"),
                ("config_ids", [image_id, "sha256:" + "e" * 64]),
                (
                    "manifest_digests",
                    [manifest_digest, "sha256:" + "f" * 64],
                ),
                (
                    "descriptor_digests",
                    [manifest_digest, "sha256:" + "f" * 64],
                ),
                (
                    "descriptor_media_types",
                    [
                        descriptor_media_type,
                        "application/vnd.oci.image.manifest.v1+json",
                    ],
                ),
                ("descriptor_sizes", [1234, 1235]),
                (
                    "identity_sha256",
                    [identity.identity_sha256, "e" * 64],
                ),
                (
                    "rootfs_diff_ids_sha256",
                    [identity.diff_ids_sha256, "e" * 64],
                ),
            )
            for key, value in reproducibility_mutations:
                tampered = json.loads(json.dumps(evidence))
                tampered["reproducibility"][key] = value
                path.write_bytes(MODULE.canonical_json_bytes(tampered))
                with self.subTest(reproducibility=key), self.assertRaisesRegex(
                    MODULE.OracleContainerError,
                    "bootstrap_build_reproducibility",
                ):
                    pin_current()

            malformed_vectors = (
                ("manifest_digests", [{"digest": manifest_digest}] * 2),
                ("descriptor_digests", [{"digest": manifest_digest}] * 2),
                ("descriptor_media_types", [{"mediaType": "bad"}] * 2),
                ("descriptor_sizes", [True, True]),
            )
            for key, value in malformed_vectors:
                tampered = json.loads(json.dumps(evidence))
                tampered["reproducibility"][key] = value
                path.write_bytes(MODULE.canonical_json_bytes(tampered))
                with self.subTest(malformed=key), self.assertRaisesRegex(
                    MODULE.OracleContainerError,
                    "bootstrap_build_reproducibility",
                ):
                    pin_current()

            for key, value in (
                ("identity_sha256", ["e" * 64] * 2),
                ("rootfs_diff_ids_sha256", ["e" * 64] * 2),
            ):
                tampered = json.loads(json.dumps(evidence))
                tampered["reproducibility"][key] = value
                path.write_bytes(MODULE.canonical_json_bytes(tampered))
                with self.subTest(
                    forged_repeated_hash=key
                ), self.assertRaisesRegex(
                        MODULE.OracleContainerError,
                        "bootstrap_build_reproducibility",
                    ):
                    pin_current()

            def mutated_rows(
                *keys: str, value: object
            ) -> list[object]:
                rows = json.loads(json.dumps([identity_row, identity_row]))
                for row in rows:
                    target = row
                    for key in keys[:-1]:
                        target = target[key]
                    target[keys[-1]] = value
                return rows

            adversarial_rows = (
                ("singleton", [identity_row]),
                ("non_mapping", [[], []]),
                (
                    "two_distinct_authenticated_rows",
                    [identity_row, different_identity_row],
                ),
                (
                    "unhashable_rootfs",
                    mutated_rows(
                        "rootfs_diff_ids",
                        value=[{"digest": "sha256:" + "c" * 64}],
                    ),
                ),
                (
                    "forged_identity_hash",
                    mutated_rows("identity_sha256", value="e" * 64),
                ),
                (
                    "forged_rootfs_hash",
                    mutated_rows(
                        "rootfs_diff_ids_sha256", value="e" * 64
                    ),
                ),
                (
                    "wrong_config",
                    mutated_rows(
                        "config_id", value="sha256:" + "f" * 64
                    ),
                ),
                (
                    "wrong_label",
                    mutated_rows(
                        "labels",
                        "org.rxls.render-oracle.lock-sha256",
                        value="f" * 64,
                    ),
                ),
                (
                    "oci_manifest",
                    mutated_rows(
                        "descriptor",
                        "mediaType",
                        value="application/vnd.oci.image.manifest.v1+json",
                    ),
                ),
                (
                    "nondeterministic_annotation",
                    mutated_rows(
                        "descriptor",
                        "annotations",
                        value={
                            "config.digest": image_id,
                            "org.opencontainers.image.created": (
                                MODULE.SOURCE_DATE_EPOCH_RFC3339
                            ),
                        },
                    ),
                ),
                (
                    "wrong_created_annotation",
                    mutated_rows(
                        "descriptor",
                        "annotations",
                        value={
                            "org.opencontainers.image.created": (
                                "2026-07-13T00:00:01Z"
                            ),
                        },
                    ),
                ),
                (
                    "wrong_descriptor_platform",
                    mutated_rows(
                        "descriptor",
                        "platform",
                        value={"architecture": "arm64", "os": "linux"},
                    ),
                ),
            )
            for name, rows in adversarial_rows:
                tampered = json.loads(json.dumps(evidence))
                tampered["reproducibility"]["identities"] = rows
                path.write_bytes(MODULE.canonical_json_bytes(tampered))
                with self.subTest(
                    adversarial_rows=name
                ), self.assertRaisesRegex(
                        MODULE.OracleContainerError,
                        "bootstrap_build_reproducibility",
                    ):
                    pin_current()

    def test_image_pin_cannot_be_rebootstrapped_after_pinning(self) -> None:
        lock, payload, contract = MODULE.load_lock()
        lock["built_image"]["expected_id"] = "sha256:" + "a" * 64
        lock["built_image"]["expected_manifest_digest"] = (
            "sha256:" + "b" * 64
        )
        source_identity = fake_source_identity(lock)
        lock["built_image"]["bootstrap_receipt"] = (
            fake_bootstrap_receipt(b"{}\n", source_identity)
        )
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "build.json"
            path.write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(
                MODULE.OracleContainerError, "image_lock_already_pinned"
            ):
                MODULE.pin_image_from_evidence(
                    lock,
                    payload,
                    contract,
                    path,
                    source_identity,
                    fake_bootstrap_receipt(b"{}\n", source_identity),
                )

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
