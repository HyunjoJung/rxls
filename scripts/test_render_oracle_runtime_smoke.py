#!/usr/bin/env python3
"""Focused tests for the locked render-oracle runtime smoke."""

from __future__ import annotations

from contextlib import contextmanager
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SMOKE_SCRIPT = ROOT / "scripts" / "smoke-render-oracle-runtime.py"
WRAPPER_TEST = ROOT / "scripts" / "test_render_oracle_container.py"
WORKFLOW = ROOT / ".github" / "workflows" / "render-oracle.yml"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


SMOKE = _load("render_oracle_runtime_smoke", SMOKE_SCRIPT)
HELPERS = _load("render_oracle_container_test_helpers", WRAPPER_TEST)
WRAPPER = HELPERS.MODULE


def _pilot_manifest(root: Path, selected_payload: bytes) -> Path:
    rows = []
    total_bytes = 0
    for format_name in SMOKE.FORMATS:
        for index in range(10):
            case_id = f"{format_name}-{index:04d}"
            payload = (
                selected_payload
                if case_id == "xlsx-0000"
                else b"x"
            )
            digest = (
                HELPERS.sha256(payload)
                if case_id == "xlsx-0000"
                else "0" * 64
            )
            row = {
                "byte_length": len(payload),
                "case_id": case_id,
                "features": [],
                "format": format_name,
                "generator": "rxls-synthetic-render-corpus",
                "generator_version": "1.5.0",
                "license": "MIT",
                "path": (
                    f"payload/{format_name}/{case_id}.{format_name}"
                ),
                "redistribution": "allowed",
                "render_redistributable": True,
                "rights_tier": "S",
                "seed": index,
                "sha256": digest,
                "source_redistributable": True,
            }
            rows.append(row)
            total_bytes += len(payload)
    selected = root / "payload" / "xlsx" / "xlsx-0000.xlsx"
    selected.parent.mkdir(parents=True)
    selected.write_bytes(selected_payload)
    manifest = {
        "case_count": 40,
        "feature_counts": {},
        "files": rows,
        "format_counts": {name: 10 for name in SMOKE.FORMATS},
        "format_feature_counts": {},
        "generator": "rxls-synthetic-render-corpus",
        "generator_version": "1.5.0",
        "license": "MIT",
        "profile": "pilot",
        "redistribution": "allowed",
        "render_redistributable": True,
        "rights_tier": "S",
        "schema_version": 1,
        "source_redistributable": True,
        "total_bytes": total_bytes,
    }
    path = root / "manifest.json"
    path.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    return path


class FixtureRunner:
    def __init__(
        self,
        lock_sha256: str,
        image_id: str,
        archive: Path,
        *,
        start_status: str = "ok",
        start_stderr: bytes = b"",
        diagnostic_logs: bytes | None = None,
        diagnostic_state: dict | None = None,
    ) -> None:
        self.lock_sha256 = lock_sha256
        self.image_id = image_id
        self.archive = archive
        self.start_status = start_status
        self.start_stderr = start_stderr
        self.diagnostic_logs = diagnostic_logs
        self.diagnostic_state = diagnostic_state
        self.commands: list[list[str]] = []
        self.start_stderr_limit: int | None = None

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
        normalized = list(command)
        self.commands.append(normalized)
        if normalized[1:3] == ["image", "inspect"]:
            return WRAPPER.CommandResult(
                "ok",
                0,
                HELPERS.image_inspect(
                    self.lock_sha256,
                    image_id=self.image_id,
                ),
            )
        if normalized[1] == "create":
            return WRAPPER.CommandResult("ok", 0, b"container-id\n")
        if normalized[1] == "start":
            self.start_stderr_limit = stderr_limit_bytes
            if self.start_status == "ok":
                assert stdout_path is not None
                Path(stdout_path).write_bytes(self.archive.read_bytes())
                return WRAPPER.CommandResult("ok", 0)
            return WRAPPER.CommandResult(
                self.start_status,
                70 if self.start_status == "nonzero" else None,
                stderr=self.start_stderr,
            )
        if normalized[1] == "logs":
            if self.diagnostic_logs is None:
                return WRAPPER.CommandResult("nonzero", 1)
            return WRAPPER.CommandResult(
                "ok",
                0,
                stderr=self.diagnostic_logs,
            )
        if normalized[1] == "inspect" and normalized[2] == "--format":
            if self.diagnostic_state is None:
                return WRAPPER.CommandResult("nonzero", 1)
            return WRAPPER.CommandResult(
                "ok",
                0,
                json.dumps(self.diagnostic_state).encode("utf-8") + b"\n",
            )
        if normalized[1] == "rm":
            return WRAPPER.CommandResult("ok", 0)
        raise AssertionError("unexpected fixture command")


class ExplodingRunner:
    def run(self, *args, **kwargs):
        raise RuntimeError("/private/source.xlsx must never be exposed")


@contextmanager
def _fixture(
    *,
    start_status: str = "ok",
    start_stderr: bytes = b"",
    diagnostic_logs: bytes | None = None,
    diagnostic_state: dict | None = None,
):
    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw)
        source_payload = b"generated pilot workbook"
        manifest = _pilot_manifest(root / "pilot", source_payload)
        font_pack = HELPERS.write_font_pack(root / "font-pack")
        font_pack_sha256 = json.loads(
            (font_pack / "manifest.json").read_text(encoding="utf-8")
        )["pack_sha256"]
        # The checked-in candidate lock is intentionally bootstrap-only.  The
        # runtime smoke itself exercises the post-bootstrap pinned path, so
        # stage a structurally identical temporary lock with synthetic,
        # authenticated-looking identities instead of weakening the runtime
        # contract or repinning the candidate lock.
        lock, _, _ = WRAPPER.load_lock()
        image_id = "sha256:" + "a" * 64
        manifest_digest = "sha256:" + "b" * 64
        lock["built_image"]["expected_id"] = image_id
        lock["built_image"]["expected_manifest_digest"] = manifest_digest
        lock["built_image"]["bootstrap_receipt"] = HELPERS.fake_bootstrap_receipt(
            b"{}\n", HELPERS.fake_source_identity(lock)
        )
        for row in lock["files"]:
            source = HELPERS.CONTAINER_DIR / row["path"]
            destination = root / row["path"]
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(source.read_bytes())
        lock_path = root / "locked-oracle-lock.json"
        lock_path.write_bytes(WRAPPER.canonical_json_bytes(lock))
        lock, lock_payload, lock_sha256 = WRAPPER.load_lock(lock_path)
        pdf = b"%PDF-1.4\nruntime smoke\n%%EOF\n"
        archive = root / "oracle.tar"
        HELPERS.make_tar(
            archive,
            HELPERS.output_manifest(
                source_payload,
                ".xlsx",
                lock_sha256,
                pdf,
                font_pack_sha256,
            ),
            pdf,
        )
        runner = FixtureRunner(
            lock_sha256,
            image_id,
            archive,
            start_status=start_status,
            start_stderr=start_stderr,
            diagnostic_logs=diagnostic_logs,
            diagnostic_state=diagnostic_state,
        )
        inputs = SMOKE.SmokeInputs(
            lock=lock_path,
            manifest=manifest,
            font_pack=font_pack,
            image=image_id,
        )
        yield inputs, runner, manifest_digest


def _argv(inputs) -> list[str]:
    return [
        "--lock",
        str(inputs.lock),
        "--manifest",
        str(inputs.manifest),
        "--font-pack",
        str(inputs.font_pack),
        "--image",
        inputs.image,
    ]


class RuntimeSmokeTests(unittest.TestCase):
    def _run(self, inputs, runner):
        stdout = io.StringIO()
        stderr = io.StringIO()
        status = SMOKE.run_cli(
            _argv(inputs),
            wrapper=WRAPPER,
            delegate=runner,
            enforce_repository_paths=False,
            stdout=stdout,
            stderr=stderr,
        )
        return status, stdout.getvalue(), stderr.getvalue()

    def test_success_reuses_the_locked_runtime_and_emits_no_evidence(self) -> None:
        with _fixture() as (inputs, runner, _):
            status, stdout, stderr = self._run(inputs, runner)

        self.assertEqual(status, 0)
        self.assertEqual(stdout, "oracle_status:ok\n")
        self.assertEqual(stderr, "")
        self.assertEqual(
            [
                "image_inspect",
                "create",
                "start",
                "remove",
            ],
            [
                "image_inspect"
                if command[1:3] == ["image", "inspect"]
                else "remove"
                if command[1] == "rm"
                else command[1]
                for command in runner.commands
            ],
        )
        self.assertEqual(
            runner.start_stderr_limit,
            SMOKE.MAX_CONTAINER_START_STDERR_BYTES,
        )
        create = runner.commands[1]
        self.assertEqual(create[-1], inputs.image)
        self.assertIn("--read-only", create)
        self.assertEqual(create[create.index("--network") + 1], "none")

    def test_exact_entrypoint_error_is_the_only_failure_output(self) -> None:
        with _fixture(
            start_status="nonzero",
            start_stderr=b"oracle_error:libreoffice_failed\n",
        ) as (inputs, runner, _):
            status, stdout, stderr = self._run(inputs, runner)

        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr, "oracle_error:libreoffice_failed\n")
        self.assertEqual(runner.commands[-1][1:3], ["rm", "--force"])

    def test_entrypoint_error_survives_bounded_engine_diagnostics(self) -> None:
        with _fixture(
            start_status="nonzero",
            start_stderr=(
                b"docker: bounded engine diagnostic\n"
                b"oracle_error:libreoffice_failed\n"
            ),
        ) as (inputs, runner, _):
            status, stdout, stderr = self._run(inputs, runner)

        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr, "oracle_error:libreoffice_failed\n")

    def test_entrypoint_error_is_recovered_from_bounded_container_logs(
        self,
    ) -> None:
        with _fixture(
            start_status="nonzero",
            start_stderr=b"docker: start failed\n",
            diagnostic_logs=b"oracle_error:libreoffice_failed\n",
        ) as (inputs, runner, _):
            status, stdout, stderr = self._run(inputs, runner)

        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr, "oracle_error:libreoffice_failed\n")
        self.assertEqual(runner.commands[-1][1:3], ["rm", "--force"])

    def test_container_state_is_reduced_to_a_path_neutral_code(self) -> None:
        with _fixture(
            start_status="nonzero",
            start_stderr=b"/private/runtime failed",
            diagnostic_state={
                "Error": "/private/runtime failed",
                "ExitCode": 137,
                "OOMKilled": True,
            },
        ) as (inputs, runner, _):
            status, stdout, stderr = self._run(inputs, runner)

        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr, "oracle_error:container_oom_killed\n")
        self.assertNotIn("private", stderr)

    def test_known_mount_failure_is_reduced_without_echoing_logs(self) -> None:
        with _fixture(
            start_status="nonzero",
            start_stderr=b"docker: start failed",
            diagnostic_logs=(
                b"mkdir: cannot create directory "
                b"'/oracle/runtime/runtime-smoke': Permission denied\n"
                b"/private/source.xlsx"
            ),
        ) as (inputs, runner, _):
            status, stdout, stderr = self._run(inputs, runner)

        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr, "oracle_error:runtime_mount_not_writable\n")
        self.assertNotIn("private", stderr)

    def test_entrypoint_phase_failures_are_reduced_without_echoing_logs(
        self,
    ) -> None:
        diagnostics = {
            (
                b"cp: cannot open "
                b"'/opt/rxls/profile/registrymodifications.xcu': error\n"
                b"/private/source.xlsx"
            ): "profile_setup_failed",
            (
                b"mkdir: cannot create directory "
                b"'/oracle/runtime/runtime-smoke': Invalid argument\n"
            ): "runtime_setup_failed",
            (
                b"find: '/oracle/evidence': Input/output error\n"
            ): "evidence_preflight_failed",
            (
                b"wc: /oracle/source/input.xlsx: Input/output error\n"
            ): "source_size_failed",
            (
                b"sha256sum: /oracle/source/input.xlsx: Input/output error\n"
            ): "source_hash_failed",
            (
                b"mv: cannot move '/oracle/evidence/input.pdf': "
                b"Invalid argument\n"
            ): "evidence_finalize_failed",
            (
                b"cannot create /oracle/evidence/oracle-manifest.json: "
                b"Invalid argument\n"
            ): "evidence_manifest_failed",
            (
                b"chmod: changing permissions of "
                b"'/oracle/evidence/oracle.pdf': Operation not permitted\n"
            ): "evidence_permissions_failed",
            (
                b"tar: oracle.pdf: Cannot stat: Input/output error\n"
            ): "evidence_archive_failed",
        }
        for diagnostic, expected in diagnostics.items():
            with self.subTest(expected=expected):
                with _fixture(
                    start_status="nonzero",
                    start_stderr=b"docker: start failed",
                    diagnostic_logs=diagnostic,
                ) as (inputs, runner, _):
                    status, stdout, stderr = self._run(inputs, runner)
                self.assertEqual(status, 2)
                self.assertEqual(stdout, "")
                self.assertEqual(stderr, f"oracle_error:{expected}\n")
                self.assertNotIn("private", stderr)
                self.assertNotIn("source.xlsx", stderr)

    def test_profile_copy_failure_is_classified_without_echoing_logs(
        self,
    ) -> None:
        diagnostics = {
            (
                b"cp: cannot stat "
                b"'/opt/rxls/profile/registrymodifications.xcu': "
                b"No such file or directory\n"
            ): "profile_path_missing",
            (
                b"cp: cannot create "
                b"'/profile/user/registrymodifications.xcu': "
                b"Operation not permitted\n"
            ): "profile_copy_not_writable",
            (
                b"cp: cannot create "
                b"'/profile/user/registrymodifications.xcu': "
                b"No space left on device\n"
            ): "profile_copy_no_space",
            (
                b"cp: cannot create "
                b"'/profile/user/registrymodifications.xcu': "
                b"Invalid argument\n"
            ): "profile_copy_invalid_argument",
            (
                b"cp: error reading "
                b"'/opt/rxls/profile/registrymodifications.xcu': "
                b"Input/output error\n"
            ): "profile_copy_io_error",
            (
                b"cp: unexpected "
                b"'/opt/rxls/profile/registrymodifications.xcu' failure\n"
            ): "profile_setup_failed",
        }
        for diagnostic, expected in diagnostics.items():
            with self.subTest(expected=expected):
                with _fixture(
                    start_status="nonzero",
                    start_stderr=b"docker: start failed",
                    diagnostic_logs=diagnostic,
                ) as (inputs, runner, _):
                    status, stdout, stderr = self._run(inputs, runner)
                self.assertEqual(status, 2)
                self.assertEqual(stdout, "")
                self.assertEqual(stderr, f"oracle_error:{expected}\n")
                self.assertNotIn("opt/rxls", stderr)

    def test_unknown_exit_code_is_retained_as_a_bounded_integer(self) -> None:
        with _fixture(
            start_status="nonzero",
            start_stderr=b"docker: start failed",
            diagnostic_state={
                "Error": "",
                "ExitCode": 17,
                "OOMKilled": False,
            },
        ) as (inputs, runner, _):
            status, stdout, stderr = self._run(inputs, runner)

        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr, "oracle_error:container_exit_17\n")

    def test_untrusted_start_stderr_collapses_to_the_typed_wrapper_code(
        self,
    ) -> None:
        diagnostics = (
            b"/private/source.xlsx failed",
            b" oracle_error:libreoffice_failed\n",
            b"oracle_error:UPPERCASE\n",
            b"oracle_error:" + b"a" * 65 + b"\n",
            (
                b"oracle_error:libreoffice_failed\n"
                b"oracle_error:pdf_missing\n"
            ),
            b"x" * (SMOKE.MAX_CONTAINER_START_STDERR_BYTES + 1),
        )
        for diagnostic in diagnostics:
            with self.subTest(diagnostic=diagnostic):
                with _fixture(
                    start_status="nonzero",
                    start_stderr=diagnostic,
                ) as (inputs, runner, _):
                    status, stdout, stderr = self._run(inputs, runner)
                self.assertEqual(status, 2)
                self.assertEqual(stdout, "")
                self.assertEqual(
                    stderr,
                    "oracle_error:container_start_nonzero\n",
                )
                self.assertNotIn("private", stderr)
                self.assertNotIn("source.xlsx", stderr)

    def test_non_nonzero_start_status_cannot_claim_entrypoint_code(self) -> None:
        with _fixture(
            start_status="timeout",
            start_stderr=b"oracle_error:libreoffice_failed\n",
        ) as (inputs, runner, _):
            status, stdout, stderr = self._run(inputs, runner)

        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr, "oracle_error:container_start_timeout\n")

    def test_unexpected_diagnostic_is_sanitized(self) -> None:
        with _fixture() as (inputs, _, _):
            status, stdout, stderr = self._run(inputs, ExplodingRunner())

        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr, "oracle_error:runtime_smoke_failed\n")
        self.assertNotIn("private", stderr)
        self.assertNotIn("source.xlsx", stderr)

    def test_invalid_arguments_do_not_echo_values(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        status = SMOKE.run_cli(
            ["--manifest", "/private/source.xlsx"],
            stdout=stdout,
            stderr=stderr,
        )

        self.assertEqual(status, 2)
        self.assertEqual(stdout.getvalue(), "")
        self.assertEqual(stderr.getvalue(), "oracle_error:invalid_arguments\n")

    def test_workflow_smoke_is_between_image_build_and_pilot_campaign(
        self,
    ) -> None:
        text = WORKFLOW.read_text(encoding="utf-8")
        build = text.index("- name: Build and inspect the locked oracle image")
        smoke = text.index("- name: Smoke the locked oracle runtime")
        campaign = text.index(
            "- name: Run the bounded four-format campaign "
            "through the container adapter"
        )

        self.assertLess(build, smoke)
        self.assertLess(smoke, campaign)
        self.assertIn(
            "if: ${{ env.RXLS_IDENTITY_BOOTSTRAP != '1' "
            "&& env.RXLS_ORACLE_CAMPAIGN == 'pilot' }}",
            text,
        )
        self.assertIn(
            "python3 scripts/smoke-render-oracle-runtime.py \\\n"
            "            --lock scripts/render-oracle-container/lock.json \\\n"
            "            --manifest "
            "local/render-corpus-generated/pilot/manifest.json \\\n"
            "            --font-pack local/render-fonts/pack \\\n"
            '            --image "$IMAGE_ID"',
            text,
        )


if __name__ == "__main__":
    unittest.main()
