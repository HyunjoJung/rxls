#!/usr/bin/env python3
"""Capture and verify path-neutral hosted render-comparison tool identities."""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import subprocess
import sys
from typing import Any, Callable, Sequence


ROOT = Path(__file__).resolve().parent
DEFAULT_LOCK = ROOT / "render-oracle-host-tools-lock.json"
REQUIREMENTS = ROOT / "render-oracle-host-requirements.txt"
LOCK_SCHEMA = "rxls.render-oracle-host-tools-lock.v1"
EVIDENCE_SCHEMA = "rxls.render-oracle-host-tools-evidence.v1"
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
DEBIAN_PACKAGE_RE = re.compile(r"[a-z0-9][a-z0-9+.-]*(?::[a-z0-9]+)?\Z")
DEBIAN_VERSION_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9.+:~_-]*\Z")
REQUIREMENT_RE = re.compile(
    r"(?P<name>[A-Za-z0-9][A-Za-z0-9_.-]*)=="
    r"(?P<version>[A-Za-z0-9][A-Za-z0-9_.+!~-]*) "
    r"--hash=sha256:(?P<sha256>[0-9a-f]{64})\Z"
)
MAX_JSON_BYTES = 16 * 1024 * 1024
MAX_FILE_BYTES = 512 * 1024 * 1024
MAX_DISTRIBUTION_FILES = 50_000
MAX_LIBRARIES = 512
EXPECTED_LOCK_KEYS = {
    "cairo",
    "expected_identity",
    "platform",
    "poppler",
    "python",
    "schema",
}


class HostToolError(RuntimeError):
    """A stable fail-closed hosted-tool validation error."""


def canonical_json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def read_bounded(path: Path, limit: int, code: str) -> bytes:
    try:
        metadata = path.lstat()
        if path.is_symlink() or not path.is_file() or metadata.st_size > limit:
            raise HostToolError(code)
        return path.read_bytes()
    except OSError as error:
        raise HostToolError(code) from error


def file_fact(path: Path) -> dict[str, object]:
    payload = read_bounded(path, MAX_FILE_BYTES, "identity_file")
    return {"bytes": len(payload), "sha256": sha256_bytes(payload)}


def exact_keys(value: object, keys: set[str], code: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        raise HostToolError(code)
    return value


def safe_text(value: object, code: str, *, maximum: int = 256) -> str:
    if (
        not isinstance(value, str)
        or not value
        or len(value) > maximum
        or any(character < " " or character == "\x7f" for character in value)
        or "/" in value
        or "\\" in value
        or value.lower().startswith("file:")
    ):
        raise HostToolError(code)
    return value


def sha256_value(value: object, code: str) -> str:
    if not isinstance(value, str) or SHA256_RE.fullmatch(value) is None:
        raise HostToolError(code)
    return value


def positive_int(value: object, code: str, maximum: int = 2**40) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or not 0 < value <= maximum:
        raise HostToolError(code)
    return value


def canonical_name(value: str) -> str:
    return re.sub(r"[-_.]+", "-", value).lower()


def parse_requirements(payload: bytes) -> dict[str, tuple[str, str]]:
    try:
        text = payload.decode("utf-8")
    except UnicodeDecodeError as error:
        raise HostToolError("requirements_encoding") from error
    if not text.endswith("\n") or "\r" in text:
        raise HostToolError("requirements_newline")
    rows: dict[str, tuple[str, str]] = {}
    for line in text.splitlines():
        match = REQUIREMENT_RE.fullmatch(line)
        if match is None:
            raise HostToolError("requirements_line")
        name = canonical_name(match.group("name"))
        if name in rows:
            raise HostToolError("requirements_duplicate")
        rows[name] = (match.group("version"), match.group("sha256"))
    if not rows:
        raise HostToolError("requirements_empty")
    return rows


def validate_fact(value: object, code: str) -> dict[str, Any]:
    row = exact_keys(value, {"bytes", "sha256"}, code)
    positive_int(row["bytes"], code)
    sha256_value(row["sha256"], code)
    return row


def validate_package_fact(value: object, code: str) -> dict[str, Any]:
    row = exact_keys(
        value,
        {"bytes", "name", "package_name", "package_version", "sha256"},
        code,
    )
    positive_int(row["bytes"], code)
    safe_text(row["name"], code)
    package_name = safe_text(row["package_name"], code)
    package_version = safe_text(row["package_version"], code)
    if (
        DEBIAN_PACKAGE_RE.fullmatch(package_name) is None
        or DEBIAN_VERSION_RE.fullmatch(package_version) is None
    ):
        raise HostToolError(code)
    sha256_value(row["sha256"], code)
    return row


def validate_provider_fact(value: object, code: str) -> dict[str, Any]:
    row = exact_keys(
        value,
        {"bytes", "name", "provider", "provider_version", "sha256"},
        code,
    )
    positive_int(row["bytes"], code)
    safe_text(row["name"], code)
    provider = safe_text(row["provider"], code)
    provider_version = safe_text(row["provider_version"], code)
    if provider != "cpython" and DEBIAN_PACKAGE_RE.fullmatch(provider) is None:
        raise HostToolError(code)
    if DEBIAN_VERSION_RE.fullmatch(provider_version) is None:
        raise HostToolError(code)
    sha256_value(row["sha256"], code)
    return row


def validate_named_rows(
    value: object,
    code: str,
    validator: Callable[[object, str], dict[str, Any]],
) -> list[dict[str, Any]]:
    if not isinstance(value, list) or not value or len(value) > MAX_LIBRARIES:
        raise HostToolError(code)
    rows = [validator(row, code) for row in value]
    names = [safe_text(row.get("name"), code) for row in rows]
    if names != sorted(names) or len(names) != len(set(names)):
        raise HostToolError(code)
    return rows


def validate_platform(value: object, expected: dict[str, str]) -> dict[str, Any]:
    row = exact_keys(value, {"machine", "system"}, "identity_platform")
    if row != expected:
        raise HostToolError("identity_platform")
    return row


def validate_python_identity(
    value: object, python_lock: dict[str, Any]
) -> dict[str, Any]:
    row = exact_keys(
        value,
        {
            "distributions",
            "executable",
            "implementation",
            "native_libraries",
            "version",
        },
        "identity_python",
    )
    if (
        row["implementation"] != python_lock["implementation"]
        or row["version"] != python_lock["version"]
    ):
        raise HostToolError("identity_python_version")
    validate_fact(row["executable"], "identity_python_executable")
    distributions = row["distributions"]
    if not isinstance(distributions, list) or not distributions:
        raise HostToolError("identity_python_distributions")
    expected = python_lock["distributions"]
    if len(distributions) != len(expected):
        raise HostToolError("identity_python_distributions")
    names: list[str] = []
    for actual, locked in zip(distributions, expected):
        item = exact_keys(
            actual,
            {
                "installed_bytes",
                "installed_files",
                "installed_sha256",
                "name",
                "version",
                "wheel_bytes",
                "wheel_sha256",
            },
            "identity_python_distribution",
        )
        name = safe_text(item["name"], "identity_python_distribution")
        names.append(name)
        if name != locked["name"] or item["version"] != locked["version"]:
            raise HostToolError("identity_python_distribution")
        positive_int(item["installed_bytes"], "identity_python_distribution")
        positive_int(
            item["installed_files"],
            "identity_python_distribution",
            MAX_DISTRIBUTION_FILES,
        )
        sha256_value(item["installed_sha256"], "identity_python_distribution")
        if (
            item["wheel_bytes"] != locked["wheel"]["bytes"]
            or item["wheel_sha256"] != locked["wheel"]["sha256"]
        ):
            raise HostToolError("identity_python_wheel")
    if names != sorted(names) or len(names) != len(set(names)):
        raise HostToolError("identity_python_distributions")
    validate_named_rows(
        row["native_libraries"],
        "identity_python_libraries",
        validate_provider_fact,
    )
    return row


def validate_poppler_identity(
    value: object, poppler_lock: dict[str, Any]
) -> dict[str, Any]:
    row = exact_keys(
        value, {"executables", "native_libraries"}, "identity_poppler"
    )
    executables = row["executables"]
    if not isinstance(executables, list) or len(executables) != len(
        poppler_lock["executables"]
    ):
        raise HostToolError("identity_poppler_executables")
    names: list[str] = []
    for item in executables:
        executable = exact_keys(
            item,
            {
                "bytes",
                "name",
                "package_name",
                "package_version",
                "sha256",
                "version",
            },
            "identity_poppler_executable",
        )
        positive_int(executable["bytes"], "identity_poppler_executable")
        name = safe_text(executable["name"], "identity_poppler_executable")
        names.append(name)
        package_name = safe_text(
            executable["package_name"], "identity_poppler_executable"
        )
        package_version = safe_text(
            executable["package_version"], "identity_poppler_executable"
        )
        if (
            DEBIAN_PACKAGE_RE.fullmatch(package_name) is None
            or DEBIAN_VERSION_RE.fullmatch(package_version) is None
        ):
            raise HostToolError("identity_poppler_executable")
        sha256_value(executable["sha256"], "identity_poppler_executable")
        safe_text(executable["version"], "identity_poppler_executable")
    if names != poppler_lock["executables"]:
        raise HostToolError("identity_poppler_executables")
    validate_named_rows(
        row["native_libraries"],
        "identity_poppler_libraries",
        validate_package_fact,
    )
    return row


def validate_cairo_identity(value: object, cairo_lock: dict[str, Any]) -> dict[str, Any]:
    row = exact_keys(
        value, {"library", "native_libraries", "version"}, "identity_cairo"
    )
    library = validate_package_fact(row["library"], "identity_cairo_library")
    if library["name"] != cairo_lock["soname"]:
        raise HostToolError("identity_cairo_library")
    libraries = validate_named_rows(
        row["native_libraries"], "identity_cairo_libraries", validate_package_fact
    )
    if not any(
        {
            key: candidate[key]
            for key in ("bytes", "package_name", "package_version", "sha256")
        }
        == {
            key: library[key]
            for key in ("bytes", "package_name", "package_version", "sha256")
        }
        for candidate in libraries
    ):
        raise HostToolError("identity_cairo_library")
    safe_text(row["version"], "identity_cairo_version")
    return row


def validate_identity(value: object, lock: dict[str, Any]) -> dict[str, Any]:
    row = exact_keys(
        value, {"cairo", "platform", "poppler", "python"}, "identity_keys"
    )
    validate_platform(row["platform"], lock["platform"])
    validate_python_identity(row["python"], lock["python"])
    validate_poppler_identity(row["poppler"], lock["poppler"])
    validate_cairo_identity(row["cairo"], lock["cairo"])
    reject_path_strings(row)
    return row


def validate_lock(document: object, requirements_payload: bytes) -> dict[str, Any]:
    row = exact_keys(document, EXPECTED_LOCK_KEYS, "lock_keys")
    if row.get("schema") != LOCK_SCHEMA:
        raise HostToolError("lock_schema")
    platform_row = exact_keys(row["platform"], {"machine", "system"}, "lock_platform")
    if platform_row != {"machine": "x86_64", "system": "linux"}:
        raise HostToolError("lock_platform")
    if row["cairo"] != {"soname": "libcairo.so.2"}:
        raise HostToolError("lock_cairo")
    poppler = exact_keys(row["poppler"], {"executables"}, "lock_poppler")
    if poppler["executables"] != ["pdffonts", "pdfinfo", "pdftoppm", "pdftotext"]:
        raise HostToolError("lock_poppler")
    python = exact_keys(
        row["python"],
        {"distributions", "implementation", "requirements", "version"},
        "lock_python",
    )
    if python["implementation"] != "cpython" or python["version"] != "3.13.14":
        raise HostToolError("lock_python")
    requirements = exact_keys(
        python["requirements"], {"bytes", "sha256"}, "lock_requirements"
    )
    if (
        requirements["bytes"] != len(requirements_payload)
        or requirements["sha256"] != sha256_bytes(requirements_payload)
    ):
        raise HostToolError("lock_requirements_identity")
    parsed = parse_requirements(requirements_payload)
    distributions = python["distributions"]
    if not isinstance(distributions, list) or not distributions or len(distributions) > 64:
        raise HostToolError("lock_distributions")
    names: list[str] = []
    for item in distributions:
        distribution = exact_keys(
            item, {"name", "version", "wheel"}, "lock_distribution"
        )
        name = safe_text(distribution["name"], "lock_distribution")
        if canonical_name(name) != name:
            raise HostToolError("lock_distribution")
        names.append(name)
        version = safe_text(distribution["version"], "lock_distribution")
        wheel = exact_keys(
            distribution["wheel"],
            {"bytes", "filename", "sha256"},
            "lock_wheel",
        )
        positive_int(wheel["bytes"], "lock_wheel")
        filename = safe_text(wheel["filename"], "lock_wheel", maximum=512)
        if not filename.endswith(".whl") or Path(filename).name != filename:
            raise HostToolError("lock_wheel")
        digest = sha256_value(wheel["sha256"], "lock_wheel")
        if parsed.get(name) != (version, digest):
            raise HostToolError("lock_requirement_closure")
    if names != sorted(names) or len(names) != len(set(names)) or set(names) != set(parsed):
        raise HostToolError("lock_requirement_closure")
    expected = row["expected_identity"]
    if expected is not None:
        validate_identity(expected, row)
    return row


def load_lock(path: Path = DEFAULT_LOCK) -> tuple[dict[str, Any], bytes]:
    lock_payload = read_bounded(path, MAX_JSON_BYTES, "lock_unreadable")
    requirements_payload = read_bounded(
        REQUIREMENTS, MAX_JSON_BYTES, "requirements_unreadable"
    )
    try:
        document = json.loads(lock_payload)
    except json.JSONDecodeError as error:
        raise HostToolError("lock_unreadable") from error
    return validate_lock(document, requirements_payload), lock_payload


def run_text(command: Sequence[str], code: str) -> str:
    try:
        result = subprocess.run(
            list(command),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=15,
            env={**os.environ, "LANG": "C", "LC_ALL": "C"},
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise HostToolError(code) from error
    output = result.stdout + result.stderr
    if result.returncode != 0 or len(output) > 4 * 1024 * 1024:
        raise HostToolError(code)
    try:
        return output.decode("utf-8")
    except UnicodeDecodeError as error:
        raise HostToolError(code) from error


def resolve_executable(name: str) -> Path:
    located = shutil.which(name)
    if located is None:
        raise HostToolError("executable_missing")
    try:
        path = Path(located).resolve(strict=True)
    except OSError as error:
        raise HostToolError("executable_missing") from error
    if not path.is_file():
        raise HostToolError("executable_missing")
    return path


def package_identity(path: Path) -> tuple[str, str]:
    candidates = [str(path)]
    raw = str(path)
    if raw.startswith("/usr/"):
        candidates.append(raw[4:])
    elif raw.startswith(("/lib/", "/lib64/", "/bin/", "/sbin/")):
        candidates.append("/usr" + raw)
    package: str | None = None
    for candidate in candidates:
        try:
            output = run_text(["dpkg-query", "--search", candidate], "package_search")
        except HostToolError:
            continue
        first = next((line for line in output.splitlines() if ": " in line), "")
        if first:
            package = first.split(": ", 1)[0]
            break
    if package is None:
        raise HostToolError("package_owner_missing")
    output = run_text(
        [
            "dpkg-query",
            "--show",
            "--showformat=${binary:Package}\t${Version}",
            package,
        ],
        "package_version",
    )
    fields = output.strip().split("\t")
    if len(fields) != 2:
        raise HostToolError("package_version")
    return (
        safe_text(fields[0], "package_version"),
        safe_text(fields[1], "package_version"),
    )


def package_file_fact(path: Path) -> dict[str, object]:
    fact = file_fact(path)
    package_name, package_version = package_identity(path)
    return {
        **fact,
        "name": path.name,
        "package_name": package_name,
        "package_version": package_version,
    }


def ldd_paths(path: Path) -> list[Path]:
    output = run_text(["ldd", str(path)], "ldd_failed")
    if "not found" in output:
        raise HostToolError("ldd_missing")
    paths: set[Path] = set()
    for line in output.splitlines():
        match = re.search(r"(?:=>\s+)?(/[^\s(]+)\s+\(0x[0-9a-fA-F]+\)", line)
        if match is None:
            continue
        try:
            candidate = Path(match.group(1)).resolve(strict=True)
        except OSError as error:
            raise HostToolError("ldd_path") from error
        if not candidate.is_file():
            raise HostToolError("ldd_path")
        paths.add(candidate)
    if not paths or len(paths) > MAX_LIBRARIES:
        raise HostToolError("ldd_paths")
    return sorted(paths, key=lambda item: (item.name, str(item)))


def library_facts(paths: Sequence[Path]) -> list[dict[str, object]]:
    rows: dict[str, dict[str, object]] = {}
    for path in paths:
        fact = package_file_fact(path)
        name = str(fact["name"])
        previous = rows.get(name)
        if previous is not None and previous != fact:
            raise HostToolError("library_name_collision")
        rows[name] = fact
    return [rows[name] for name in sorted(rows)]


def python_library_facts(
    paths: Sequence[Path], python_version: str
) -> list[dict[str, object]]:
    rows: dict[str, dict[str, object]] = {}
    for path in paths:
        fact = file_fact(path)
        try:
            provider, provider_version = package_identity(path)
        except HostToolError as error:
            if str(error) != "package_owner_missing":
                raise
            provider = "cpython"
            provider_version = python_version
        row = {
            **fact,
            "name": path.name,
            "provider": provider,
            "provider_version": provider_version,
        }
        previous = rows.get(path.name)
        if previous is not None and previous != row:
            raise HostToolError("library_name_collision")
        rows[path.name] = row
    return [rows[name] for name in sorted(rows)]


def executable_identity(name: str) -> tuple[dict[str, object], Path]:
    path = resolve_executable(name)
    output = run_text([str(path), "-v"], "executable_version")
    version = next((line.strip() for line in output.splitlines() if line.strip()), "")
    package_name, package_version = package_identity(path)
    return (
        {
            **file_fact(path),
            "name": name,
            "package_name": package_name,
            "package_version": package_version,
            "version": safe_text(version, "executable_version"),
        },
        path,
    )


def resolve_cairo(soname: str) -> Path:
    output = run_text(["ldconfig", "-p"], "ldconfig")
    candidates: set[Path] = set()
    for line in output.splitlines():
        match = re.match(
            rf"\s*{re.escape(soname)}\s+\([^)]*x86-64[^)]*\)\s+=>\s+(\S+)\s*$",
            line,
        )
        if match is None:
            continue
        try:
            candidate = Path(match.group(1)).resolve(strict=True)
        except OSError as error:
            raise HostToolError("cairo_library") from error
        candidates.add(candidate)
    if len(candidates) != 1:
        raise HostToolError("cairo_library")
    path = next(iter(candidates))
    if path.name != soname and not Path(str(path)).name.startswith("libcairo.so."):
        raise HostToolError("cairo_library")
    return path


def cairo_version(path: Path) -> str:
    try:
        library = ctypes.CDLL(str(path))
        function = library.cairo_version_string
        function.restype = ctypes.c_char_p
        raw = function()
        version = raw.decode("ascii") if raw is not None else ""
    except (AttributeError, OSError, UnicodeDecodeError) as error:
        raise HostToolError("cairo_version") from error
    return safe_text(version, "cairo_version")


def distribution_identity(locked: dict[str, Any]) -> dict[str, object]:
    try:
        distribution = importlib.metadata.distribution(locked["name"])
    except importlib.metadata.PackageNotFoundError as error:
        raise HostToolError("distribution_missing") from error
    if distribution.version != locked["version"]:
        raise HostToolError("distribution_version")
    files = distribution.files
    if files is None or not files or len(files) > MAX_DISTRIBUTION_FILES:
        raise HostToolError("distribution_files")
    facts: list[dict[str, object]] = []
    total = 0
    for item in files:
        relative = PurePosixPath(str(item).replace("\\", "/"))
        if ".." in relative.parts or relative.suffix == ".pyc" or "__pycache__" in relative.parts:
            continue
        try:
            path = Path(distribution.locate_file(item)).resolve(strict=True)
        except OSError as error:
            raise HostToolError("distribution_file") from error
        if not path.is_file():
            raise HostToolError("distribution_file")
        fact = file_fact(path)
        total += int(fact["bytes"])
        if total > 2**40:
            raise HostToolError("distribution_size")
        facts.append({"name": relative.as_posix(), **fact})
    if not facts:
        raise HostToolError("distribution_files")
    facts.sort(key=lambda item: str(item["name"]))
    wheel = locked["wheel"]
    return {
        "installed_bytes": total,
        "installed_files": len(facts),
        "installed_sha256": sha256_bytes(canonical_json_bytes(facts)),
        "name": locked["name"],
        "version": locked["version"],
        "wheel_bytes": wheel["bytes"],
        "wheel_sha256": wheel["sha256"],
    }


def normalize_machine(value: str) -> str:
    return "x86_64" if value.lower() in {"amd64", "x86_64"} else value.lower()


def capture_identity(lock: dict[str, Any]) -> dict[str, object]:
    actual_platform = {
        "machine": normalize_machine(platform.machine()),
        "system": platform.system().lower(),
    }
    if actual_platform != lock["platform"]:
        raise HostToolError("capture_platform")
    if (
        sys.implementation.name != lock["python"]["implementation"]
        or platform.python_version() != lock["python"]["version"]
    ):
        raise HostToolError("capture_python_version")
    try:
        python_path = Path(sys.executable).resolve(strict=True)
    except OSError as error:
        raise HostToolError("capture_python") from error

    executable_rows: list[dict[str, object]] = []
    poppler_libraries: set[Path] = set()
    for name in lock["poppler"]["executables"]:
        identity, path = executable_identity(name)
        executable_rows.append(identity)
        poppler_libraries.update(ldd_paths(path))

    cairo_path = resolve_cairo(lock["cairo"]["soname"])
    cairo_paths = {cairo_path, *ldd_paths(cairo_path)}
    cairo_library = package_file_fact(cairo_path)
    cairo_library["name"] = lock["cairo"]["soname"]
    identity = {
        "cairo": {
            "library": cairo_library,
            "native_libraries": library_facts(sorted(cairo_paths)),
            "version": cairo_version(cairo_path),
        },
        "platform": actual_platform,
        "poppler": {
            "executables": executable_rows,
            "native_libraries": library_facts(sorted(poppler_libraries)),
        },
        "python": {
            "distributions": [
                distribution_identity(item)
                for item in lock["python"]["distributions"]
            ],
            "executable": file_fact(python_path),
            "implementation": sys.implementation.name,
            "native_libraries": python_library_facts(
                ldd_paths(python_path), platform.python_version()
            ),
            "version": platform.python_version(),
        },
    }
    return validate_identity(identity, lock)


def reject_path_strings(value: object) -> None:
    if isinstance(value, dict):
        for item in value.values():
            reject_path_strings(item)
    elif isinstance(value, list):
        for item in value:
            reject_path_strings(item)
    elif isinstance(value, str):
        safe_text(value, "pathful_evidence", maximum=4096)


def scoped_identity(identity: dict[str, Any], scope: str) -> dict[str, Any]:
    if scope == "all":
        return identity
    if scope == "poppler":
        return {"platform": identity["platform"], "poppler": identity["poppler"]}
    raise HostToolError("scope")


def apt_specs(lock: dict[str, Any], scope: str) -> list[str]:
    expected = lock["expected_identity"]
    if expected is None:
        raise HostToolError("host_identity_pin_required")
    sources: list[dict[str, Any]] = []
    sources.extend(expected["poppler"]["executables"])
    sources.extend(expected["poppler"]["native_libraries"])
    if scope == "all":
        sources.append(expected["cairo"]["library"])
        sources.extend(expected["cairo"]["native_libraries"])
    elif scope != "poppler":
        raise HostToolError("scope")
    packages: dict[str, str] = {}
    for row in sources:
        name = row["package_name"]
        version = row["package_version"]
        if (
            DEBIAN_PACKAGE_RE.fullmatch(name) is None
            or DEBIAN_VERSION_RE.fullmatch(version) is None
        ):
            raise HostToolError("apt_package")
        previous = packages.get(name)
        if previous is not None and previous != version:
            raise HostToolError("apt_package_conflict")
        packages[name] = version
    if not packages or len(packages) > MAX_LIBRARIES:
        raise HostToolError("apt_packages")
    return [f"{name}={packages[name]}" for name in sorted(packages)]


def write_evidence(path: Path, document: dict[str, Any]) -> None:
    reject_path_strings(document)
    payload = canonical_json_bytes(document)
    if len(payload) > MAX_JSON_BYTES:
        raise HostToolError("evidence_limit")
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.exists() and (path.is_symlink() or not path.is_file()):
            raise HostToolError("evidence_output")
        path.write_bytes(payload)
    except OSError as error:
        raise HostToolError("evidence_output") from error


def verify_host(
    lock_path: Path,
    output: Path,
    *,
    scope: str,
    bootstrap_identities: bool,
    capture: Callable[[dict[str, Any]], dict[str, Any]] = capture_identity,
) -> dict[str, Any]:
    lock, lock_payload = load_lock(lock_path)
    actual_full = capture(lock)
    actual = scoped_identity(actual_full, scope)
    expected_full = lock["expected_identity"]
    expected = (
        None if expected_full is None else scoped_identity(expected_full, scope)
    )
    actual_sha = sha256_bytes(canonical_json_bytes(actual))
    expected_sha = (
        None if expected is None else sha256_bytes(canonical_json_bytes(expected))
    )
    if expected is None:
        status = "bootstrap_capture_required"
    elif canonical_json_bytes(expected) == canonical_json_bytes(actual):
        status = "pinned_match"
    else:
        status = "mismatch"
    document = {
        "captured_identity_sha256": actual_sha,
        "expected_identity_sha256": expected_sha,
        "identity": actual,
        "identity_status": status,
        "lock_file_sha256": sha256_bytes(lock_payload),
        "schema": EVIDENCE_SCHEMA,
        "scope": scope,
    }
    write_evidence(output, document)
    if expected is None and not bootstrap_identities:
        raise HostToolError("host_identity_pin_required")
    if status == "mismatch":
        raise HostToolError("host_identity_mismatch")
    return document


def load_evidence(path: Path) -> dict[str, Any]:
    payload = read_bounded(path, MAX_JSON_BYTES, "evidence_unreadable")
    try:
        document = json.loads(payload)
    except json.JSONDecodeError as error:
        raise HostToolError("evidence_unreadable") from error
    return exact_keys(
        document,
        {
            "captured_identity_sha256",
            "expected_identity_sha256",
            "identity",
            "identity_status",
            "lock_file_sha256",
            "schema",
            "scope",
        },
        "evidence_keys",
    )


def pin_from_evidence(lock_path: Path, evidence_path: Path) -> dict[str, Any]:
    lock, lock_payload = load_lock(lock_path)
    if lock["expected_identity"] is not None:
        raise HostToolError("lock_already_pinned")
    evidence = load_evidence(evidence_path)
    if (
        evidence["schema"] != EVIDENCE_SCHEMA
        or evidence["scope"] != "all"
        or evidence["identity_status"] != "bootstrap_capture_required"
        or evidence["expected_identity_sha256"] is not None
        or evidence["lock_file_sha256"] != sha256_bytes(lock_payload)
    ):
        raise HostToolError("bootstrap_evidence")
    identity = validate_identity(evidence["identity"], lock)
    if evidence["captured_identity_sha256"] != sha256_bytes(
        canonical_json_bytes(identity)
    ):
        raise HostToolError("bootstrap_evidence_identity")
    pinned = json.loads(json.dumps(lock))
    pinned["expected_identity"] = identity
    validate_lock(pinned, read_bounded(REQUIREMENTS, MAX_JSON_BYTES, "requirements_unreadable"))
    return pinned


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    subparsers = parser.add_subparsers(dest="action", required=True)

    verify_lock = subparsers.add_parser("verify-lock")
    verify_lock.set_defaults(action="verify-lock")

    verify = subparsers.add_parser("verify")
    verify.add_argument("--output", type=Path, required=True)
    verify.add_argument("--scope", choices=("all", "poppler"), default="all")
    verify.add_argument("--bootstrap-identities", action="store_true")

    apt = subparsers.add_parser("apt-specs")
    apt.add_argument("--scope", choices=("all", "poppler"), default="all")

    pin = subparsers.add_parser("pin")
    pin.add_argument("--evidence", type=Path, required=True)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.action == "verify-lock":
            lock, payload = load_lock(args.lock)
            print(
                canonical_json_bytes(
                    {
                        "expected_identity_sha256": (
                            None
                            if lock["expected_identity"] is None
                            else sha256_bytes(
                                canonical_json_bytes(lock["expected_identity"])
                            )
                        ),
                        "lock_file_sha256": sha256_bytes(payload),
                        "schema": LOCK_SCHEMA,
                        "status": "ok",
                    }
                ).decode("utf-8"),
                end="",
            )
            return 0
        if args.action == "verify":
            document = verify_host(
                args.lock,
                args.output,
                scope=args.scope,
                bootstrap_identities=args.bootstrap_identities,
            )
            print(canonical_json_bytes(document).decode("utf-8"), end="")
            return 0
        if args.action == "apt-specs":
            lock, _ = load_lock(args.lock)
            for spec in apt_specs(lock, args.scope):
                print(spec)
            return 0
        pinned = pin_from_evidence(args.lock, args.evidence)
        print(canonical_json_bytes(pinned).decode("utf-8"), end="")
        return 0
    except HostToolError as error:
        print(str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
