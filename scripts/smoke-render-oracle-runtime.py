#!/usr/bin/env python3
"""Run one authenticated generated pilot fixture through the locked oracle."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
from dataclasses import dataclass
import importlib.util
import json
from pathlib import Path
import re
import stat
import sys
import tempfile
from typing import Any, Sequence, TextIO


ROOT = Path(__file__).resolve().parents[1]
LOCKED_WRAPPER = ROOT / "scripts" / "run-render-oracle-container.py"
LOCK_PATH = ROOT / "scripts" / "render-oracle-container" / "lock.json"
PILOT_MANIFEST = (
    ROOT / "local" / "render-corpus-generated" / "pilot" / "manifest.json"
)
FONT_PACK = ROOT / "local" / "render-fonts" / "pack"
RUN_ID = "runtime-smoke"
MAX_MANIFEST_BYTES = 1024 * 1024
MAX_CONTAINER_START_STDERR_BYTES = 1024 * 1024
MAX_CONTAINER_STATE_BYTES = 64 * 1024
ERROR_CODE_RE = re.compile(r"[a-z][a-z0-9_]{0,63}\Z")
ENTRYPOINT_ERROR_LINE_RE = re.compile(
    rb"(?:\A|\n)oracle_error:([a-z][a-z0-9_]{0,63})(?=\n|\Z)"
)
FORMATS = ("ods", "xls", "xlsb", "xlsx")
EXPECTED_PHASES = ("image_inspect", "create", "start", "remove")


class RuntimeSmokeError(RuntimeError):
    """A stable, path-neutral runtime-smoke contract failed."""

    def __init__(self, code: str) -> None:
        super().__init__(
            code
            if ERROR_CODE_RE.fullmatch(code)
            else "runtime_smoke_failed"
        )


@dataclass(frozen=True)
class SmokeInputs:
    lock: Path
    manifest: Path
    font_pack: Path
    image: str


class DiscardText:
    """Discard wrapper diagnostics without accumulating their contents."""

    encoding = "utf-8"

    def write(self, value: str) -> int:
        return len(value)

    def flush(self) -> None:
        pass


def _load_locked_wrapper() -> Any:
    name = "rxls_locked_render_oracle_runtime_smoke"
    spec = importlib.util.spec_from_file_location(name, LOCKED_WRAPPER)
    if spec is None or spec.loader is None:
        raise RuntimeSmokeError("wrapper_import_failed")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def _parse_cli(argv: Sequence[str]) -> SmokeInputs:
    required = {"--font-pack", "--image", "--lock", "--manifest"}
    values: dict[str, str] = {}
    for index in range(0, len(argv), 2):
        if (
            index + 1 >= len(argv)
            or argv[index] not in required
            or argv[index] in values
        ):
            raise RuntimeSmokeError("invalid_arguments")
        values[argv[index]] = argv[index + 1]
    if set(values) != required:
        raise RuntimeSmokeError("invalid_arguments")
    return SmokeInputs(
        Path(values["--lock"]),
        Path(values["--manifest"]),
        Path(values["--font-pack"]),
        values["--image"],
    )


def _select_pilot_fixture(path: Path, wrapper: Any) -> tuple[Path, dict[str, Any]]:
    try:
        metadata = path.lstat()
        if (
            not stat.S_ISREG(metadata.st_mode)
            or path.is_symlink()
            or not 0 < metadata.st_size <= MAX_MANIFEST_BYTES
        ):
            raise RuntimeSmokeError("pilot_manifest_invalid")
        with path.open("rb") as source:
            payload = source.read(MAX_MANIFEST_BYTES + 1)
        if len(payload) != metadata.st_size:
            raise RuntimeSmokeError("pilot_manifest_invalid")
        document = json.loads(payload)
    except RuntimeSmokeError:
        raise
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeSmokeError("pilot_manifest_invalid") from error
    expected = {
        "case_count": 40,
        "format_counts": {
            "ods": 10,
            "xls": 10,
            "xlsb": 10,
            "xlsx": 10,
        },
        "generator": "rxls-synthetic-render-corpus",
        "generator_version": "1.3.0",
        "license": "MIT",
        "profile": "pilot",
        "redistribution": "allowed",
        "render_redistributable": True,
        "rights_tier": "S",
        "schema_version": 1,
        "source_redistributable": True,
    }
    if (
        not isinstance(document, dict)
        or any(document.get(key) != value for key, value in expected.items())
        or not isinstance(document.get("files"), list)
        or len(document["files"]) != 40
    ):
        raise RuntimeSmokeError("pilot_manifest_invalid")
    matches = [
        row
        for row in document["files"]
        if isinstance(row, dict)
        and row.get("format") == "xlsx"
        and row.get("case_id") == "xlsx-0000"
    ]
    if len(matches) != 1:
        raise RuntimeSmokeError("pilot_fixture_invalid")
    selected = matches[0]
    owned = {
        "generator": "rxls-synthetic-render-corpus",
        "generator_version": "1.3.0",
        "license": "MIT",
        "redistribution": "allowed",
        "render_redistributable": True,
        "rights_tier": "S",
        "source_redistributable": True,
    }
    byte_length = selected.get("byte_length")
    digest = selected.get("sha256")
    try:
        relative = wrapper.safe_relative(selected.get("path"))
    except Exception as error:
        raise RuntimeSmokeError("pilot_fixture_invalid") from error
    if (
        any(selected.get(key) != value for key, value in owned.items())
        or relative != "payload/xlsx/xlsx-0000.xlsx"
        or isinstance(byte_length, bool)
        or not isinstance(byte_length, int)
        or not 0 < byte_length <= 64 * 1024 * 1024
        or not isinstance(digest, str)
        or wrapper.SHA256_RE.fullmatch(digest) is None
    ):
        raise RuntimeSmokeError("pilot_fixture_invalid")
    source = path.parent / relative
    try:
        source_metadata = source.lstat()
        root = path.parent.resolve(strict=False)
        resolved = source.resolve(strict=True)
        if (
            not stat.S_ISREG(source_metadata.st_mode)
            or source.is_symlink()
            or root not in resolved.parents
            or source_metadata.st_size != byte_length
            or wrapper.sha256_file(source, 64 * 1024 * 1024) != digest
        ):
            raise RuntimeSmokeError("pilot_fixture_invalid")
    except RuntimeSmokeError:
        raise
    except OSError as error:
        raise RuntimeSmokeError("pilot_fixture_invalid") from error
    return source, selected


def _entrypoint_code(stderr: object) -> str | None:
    if (
        not isinstance(stderr, bytes)
        or len(stderr) > MAX_CONTAINER_START_STDERR_BYTES
    ):
        return None
    matches = ENTRYPOINT_ERROR_LINE_RE.findall(stderr)
    if len(matches) != 1:
        return None
    return matches[0].decode("ascii")


def _container_log_code(payload: object) -> str | None:
    if (
        not isinstance(payload, bytes)
        or len(payload) > MAX_CONTAINER_START_STDERR_BYTES
    ):
        return None
    lowered = payload.lower()
    not_writable = (
        b"permission denied" in lowered
        or b"read-only file system" in lowered
    )
    if b"/oracle/runtime" in lowered and not_writable:
        return "runtime_mount_not_writable"
    if b"/oracle/evidence" in lowered and not_writable:
        return "evidence_mount_not_writable"
    if b"/tmp" in lowered and not_writable:
        return "temporary_mount_not_writable"
    if (
        b"file size limit exceeded" in lowered
        or b"error setting limit" in lowered
    ):
        return "fsize_limit_failed"
    if b"/oracle/runtime" in lowered and b"no space left on device" in lowered:
        return "runtime_mount_full"
    if b"/oracle/evidence" in lowered and b"no space left on device" in lowered:
        return "evidence_mount_full"
    return None


def _container_state_code(payload: object) -> str | None:
    if not isinstance(payload, bytes) or len(payload) > MAX_CONTAINER_STATE_BYTES:
        return None
    try:
        state = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError):
        return None
    if not isinstance(state, dict):
        return None
    exit_code = state.get("ExitCode")
    oom_killed = state.get("OOMKilled")
    runtime_error = state.get("Error")
    if not isinstance(oom_killed, bool):
        return None
    if isinstance(exit_code, bool) or not isinstance(exit_code, int):
        return None
    if not isinstance(runtime_error, str):
        return None
    if oom_killed:
        return "container_oom_killed"
    if exit_code == 70:
        return "entrypoint_failed"
    if exit_code == 126:
        return "entrypoint_not_executable"
    if exit_code == 127:
        return "entrypoint_not_found"
    if runtime_error:
        return "container_runtime_start_failed"
    if 0 < exit_code <= 255:
        return f"container_exit_{exit_code}"
    return None


class RecordingRunner:
    """Record phase names only and enforce the locked runtime call sequence."""

    def __init__(self, wrapper: Any, delegate: Any, image: str) -> None:
        self.wrapper = wrapper
        self.delegate = delegate
        self.image = image
        self.phases: list[str] = []
        self.entrypoint_error: str | None = None

    def _diagnose_start_failure(self, name: str) -> str | None:
        try:
            logs = self.delegate.run(
                ["docker", "logs", name],
                timeout_seconds=10.0,
                output_limit_bytes=MAX_CONTAINER_START_STDERR_BYTES,
                stdout_limit_bytes=MAX_CONTAINER_START_STDERR_BYTES,
                stderr_limit_bytes=MAX_CONTAINER_START_STDERR_BYTES,
            )
            if isinstance(logs, self.wrapper.CommandResult):
                diagnostics = logs.stdout + b"\n" + logs.stderr
                code = _entrypoint_code(diagnostics)
                if code is not None:
                    return code
                code = _container_log_code(diagnostics)
                if code is not None:
                    return code
            state = self.delegate.run(
                [
                    "docker",
                    "inspect",
                    "--format",
                    "{{json .State}}",
                    name,
                ],
                timeout_seconds=10.0,
                output_limit_bytes=MAX_CONTAINER_STATE_BYTES,
                stdout_limit_bytes=MAX_CONTAINER_STATE_BYTES,
                stderr_limit_bytes=MAX_CONTAINER_STATE_BYTES,
            )
            if isinstance(state, self.wrapper.CommandResult):
                return _container_state_code(state.stdout)
        except Exception:
            return None
        return None

    def _phase(self, command: list[str]) -> str:
        name = f"rxls-lo-{RUN_ID}"
        if command == ["docker", "image", "inspect", self.image]:
            return "image_inspect"
        if (
            command[:2] == ["docker", "create"]
            and command[-1:] == [self.image]
            and "--name" in command
            and command[command.index("--name") + 1] == name
        ):
            return "create"
        if command == ["docker", "start", "--attach", name]:
            return "start"
        if command == ["docker", "rm", "--force", name]:
            return "remove"
        raise RuntimeSmokeError("runtime_command_sequence")

    def run(
        self,
        command: Sequence[str],
        *,
        timeout_seconds: float,
        output_limit_bytes: int,
        stdout_path: Path | None = None,
        stdout_limit_bytes: int | None = None,
        stderr_limit_bytes: int | None = None,
    ) -> Any:
        try:
            phase = self._phase(list(command))
        except (RuntimeSmokeError, ValueError, IndexError) as error:
            raise RuntimeSmokeError("runtime_command_sequence") from error
        if (
            len(self.phases) >= len(EXPECTED_PHASES)
            or phase != EXPECTED_PHASES[len(self.phases)]
        ):
            raise RuntimeSmokeError("runtime_command_sequence")
        self.phases.append(phase)
        result = self.delegate.run(
            list(command),
            timeout_seconds=timeout_seconds,
            output_limit_bytes=output_limit_bytes,
            stdout_path=stdout_path,
            stdout_limit_bytes=stdout_limit_bytes,
            stderr_limit_bytes=(
                MAX_CONTAINER_START_STDERR_BYTES
                if phase == "start"
                else stderr_limit_bytes
            ),
        )
        if not isinstance(result, self.wrapper.CommandResult):
            raise RuntimeSmokeError("runtime_result_invalid")
        if phase == "start" and result.status == "nonzero":
            self.entrypoint_error = _entrypoint_code(result.stderr)
            if self.entrypoint_error is None:
                self.entrypoint_error = self._diagnose_start_failure(
                    f"rxls-lo-{RUN_ID}"
                )
        if phase == "remove" and result.status != "ok":
            raise RuntimeSmokeError("container_cleanup_failed")
        return result


def _validate_success(
    wrapper: Any,
    execution: object,
    evidence: Path,
    selected: dict[str, Any],
    font_pack_sha256: str,
    lock_sha256: str,
    lock_file_sha256: str,
    image_id: str,
    manifest_digest: str,
    phases: Sequence[str],
) -> None:
    expected_source = {
        "bytes": selected["byte_length"],
        "path": "source/input.xlsx",
        "sha256": selected["sha256"],
    }
    if (
        not isinstance(execution, dict)
        or execution.get("schema") != wrapper.EXECUTION_SCHEMA
        or execution.get("runtime") != "docker"
        or execution.get("source") != expected_source
        or execution.get("font_pack_sha256") != font_pack_sha256
        or execution.get("lock_file_sha256") != lock_file_sha256
        or execution.get("image", {}).get("id") != image_id
        or execution.get("image", {}).get("expected_id") != image_id
        or execution.get("image", {}).get("manifest_digest")
        != manifest_digest
        or execution.get("isolation", {}).get("network") != "none"
        or execution.get("isolation", {}).get("root_filesystem")
        != "read_only"
        or tuple(phases) != EXPECTED_PHASES
    ):
        raise RuntimeSmokeError("runtime_evidence_invalid")
    try:
        if sorted(item.name for item in evidence.iterdir()) != [
            "execution.json",
            "oracle-manifest.json",
            "oracle.pdf",
        ]:
            raise RuntimeSmokeError("runtime_evidence_invalid")
        if json.loads((evidence / "execution.json").read_bytes()) != execution:
            raise RuntimeSmokeError("runtime_evidence_invalid")
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeSmokeError("runtime_evidence_invalid") from error
    wrapper.validate_output_evidence(
        evidence,
        source_sha256=selected["sha256"],
        source_bytes=selected["byte_length"],
        extension=".xlsx",
        lock_sha256=lock_sha256,
        font_pack_sha256=font_pack_sha256,
        print_mode="single-page-sheets",
    )


def execute_smoke(
    inputs: SmokeInputs,
    *,
    wrapper: Any | None = None,
    delegate: Any | None = None,
    enforce_repository_paths: bool = True,
) -> None:
    wrapper = wrapper or _load_locked_wrapper()
    recorder: RecordingRunner | None = None
    try:
        if enforce_repository_paths and any(
            supplied.resolve(strict=False) != expected.resolve(strict=False)
            for supplied, expected in (
                (inputs.lock, LOCK_PATH),
                (inputs.manifest, PILOT_MANIFEST),
                (inputs.font_pack, FONT_PACK),
            )
        ):
            raise RuntimeSmokeError("invalid_repository_input")
        lock, lock_payload, lock_sha256 = wrapper.load_lock(inputs.lock)
        built_image = lock.get("built_image", {})
        image_id = built_image.get("expected_id")
        manifest_digest = built_image.get("expected_manifest_digest")
        if (
            not isinstance(image_id, str)
            or wrapper.IMAGE_ID_RE.fullmatch(image_id) is None
            or not isinstance(manifest_digest, str)
            or wrapper.IMAGE_ID_RE.fullmatch(manifest_digest) is None
            or inputs.image != image_id
            or wrapper.validate_image_reference(inputs.image) != image_id
        ):
            raise RuntimeSmokeError("image_pin_mismatch")
        source, selected = _select_pilot_fixture(inputs.manifest, wrapper)
        lock_file_sha256 = wrapper.sha256_bytes(lock_payload)
        with tempfile.TemporaryDirectory(
            prefix="rxls-oracle-runtime-smoke-"
        ) as raw:
            evidence = Path(raw) / "evidence"
            config = wrapper.RenderConfig(
                source,
                inputs.font_pack,
                None,
                evidence,
                RUN_ID,
                wrapper.ResourceLimits(),
            )
            (
                validated_source,
                source_bytes,
                source_sha256,
                extension,
                font_pack,
                _,
            ) = wrapper.validate_render_config(config)
            preview = wrapper.build_create_command(
                "docker",
                image_id,
                config,
                source_mount=validated_source,
                font_mount=font_pack.root,
                corpus_mount=font_pack.root,
                source_bytes=source_bytes,
                source_sha256=source_sha256,
                extension=extension,
                lock_sha256=lock_sha256,
                font_pack_sha256=font_pack.pack_sha256,
            )
            if preview[:2] != ["docker", "create"] or preview[-1] != image_id:
                raise RuntimeSmokeError("runtime_create_contract")
            recorder = RecordingRunner(
                wrapper,
                delegate or wrapper.BoundedProcessRunner(),
                image_id,
            )
            execution = wrapper.execute_render(
                config,
                "docker",
                image_id,
                lock_sha256,
                expected_image_id=image_id,
                expected_manifest_digest=manifest_digest,
                lock_file_sha256=lock_file_sha256,
                runner=recorder,
            )
            _validate_success(
                wrapper,
                execution,
                evidence,
                selected,
                font_pack.pack_sha256,
                lock_sha256,
                lock_file_sha256,
                image_id,
                manifest_digest,
                recorder.phases,
            )
    except RuntimeSmokeError:
        raise
    except wrapper.OracleContainerError as error:
        code = str(error)
        if (
            code.startswith("container_start_")
            and recorder is not None
            and recorder.entrypoint_error is not None
        ):
            code = recorder.entrypoint_error
        raise RuntimeSmokeError(code) from None


def run_cli(
    argv: Sequence[str],
    *,
    wrapper: Any | None = None,
    delegate: Any | None = None,
    enforce_repository_paths: bool = True,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    output = stdout or sys.stdout
    errors = stderr or sys.stderr
    try:
        inputs = _parse_cli(argv)
        discard = DiscardText()
        with redirect_stdout(discard), redirect_stderr(discard):
            execute_smoke(
                inputs,
                wrapper=wrapper,
                delegate=delegate,
                enforce_repository_paths=enforce_repository_paths,
            )
    except RuntimeSmokeError as error:
        errors.write(f"oracle_error:{error}\n")
        return 2
    except Exception:
        errors.write("oracle_error:runtime_smoke_failed\n")
        return 2
    output.write("oracle_status:ok\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(run_cli(sys.argv[1:]))
