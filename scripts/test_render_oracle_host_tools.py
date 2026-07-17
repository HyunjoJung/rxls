#!/usr/bin/env python3
"""Tests for the hosted render-oracle tool identity lock."""

from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "render-oracle-host-tools.py"


def load_module():
    spec = importlib.util.spec_from_file_location("render_oracle_host_tools", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def digest(label: str) -> str:
    return hashlib.sha256(label.encode()).hexdigest()


def package_fact(name: str) -> dict[str, object]:
    return {
        "bytes": 17,
        "name": name,
        "package_name": "fixture-package",
        "package_version": "1.2.3-1ubuntu1",
        "sha256": digest(name),
    }


def fixture_identity(lock: dict) -> dict:
    cairo_libraries = [package_fact("libc.so.6"), package_fact("libcairo.so.2")]
    cairo_libraries.sort(key=lambda row: row["name"])
    poppler_libraries = [package_fact("libc.so.6"), package_fact("libpoppler.so.1")]
    poppler_libraries.sort(key=lambda row: row["name"])
    executables = []
    for name in lock["poppler"]["executables"]:
        executables.append(
            {
                "bytes": 31,
                "name": name,
                "package_name": "poppler-utils",
                "package_version": "24.02.0-1ubuntu9",
                "sha256": digest(name),
                "version": f"{name} version 24.02.0",
            }
        )
    distributions = []
    for row in lock["python"]["distributions"]:
        distributions.append(
            {
                "installed_bytes": 101,
                "installed_files": 3,
                "installed_sha256": digest(row["name"]),
                "name": row["name"],
                "version": row["version"],
                "wheel_bytes": row["wheel"]["bytes"],
                "wheel_sha256": row["wheel"]["sha256"],
            }
        )
    return {
        "cairo": {
            "library": package_fact("libcairo.so.2"),
            "native_libraries": cairo_libraries,
            "version": "1.18.4",
        },
        "platform": {"machine": "x86_64", "system": "linux"},
        "poppler": {
            "executables": executables,
            "native_libraries": poppler_libraries,
        },
        "python": {
            "distributions": distributions,
            "executable": {"bytes": 4096, "sha256": digest("python")},
            "implementation": "cpython",
            "native_libraries": [
                {
                    "bytes": 99,
                    "name": "libpython3.13.so.1.0",
                    "provider": "cpython",
                    "provider_version": "3.13.14",
                    "sha256": digest("libpython3.13.so.1.0"),
                }
            ],
            "version": "3.13.14",
        },
    }


class RenderOracleHostToolsTests(unittest.TestCase):
    def test_checked_in_lock_has_exact_python_and_hashed_full_closure(self) -> None:
        lock, _ = MODULE.load_lock()
        self.assertEqual(lock["python"]["version"], "3.13.14")
        self.assertEqual(lock["python"]["implementation"], "cpython")
        if lock["expected_identity"] is not None:
            MODULE.validate_identity(lock["expected_identity"], lock)
        names = [row["name"] for row in lock["python"]["distributions"]]
        self.assertEqual(
            names,
            [
                "cairocffi",
                "cairosvg",
                "cffi",
                "cssselect2",
                "defusedxml",
                "numpy",
                "pillow",
                "pycparser",
                "tinycss2",
                "webencodings",
            ],
        )
        for row in lock["python"]["distributions"]:
            self.assertRegex(row["wheel"]["sha256"], r"^[0-9a-f]{64}$")
            self.assertGreater(row["wheel"]["bytes"], 0)

    def test_requirements_reject_unhashed_extra_and_duplicate_rows(self) -> None:
        valid = MODULE.REQUIREMENTS.read_bytes()
        for mutation in (
            valid + b"unlocked==1.0\n",
            valid + valid.splitlines(keepends=True)[0],
            valid.replace(b" --hash=sha256:", b" ", 1),
            valid.replace(b"\n", b"\r\n", 1),
        ):
            with self.subTest(mutation=mutation[-100:]):
                with self.assertRaises(MODULE.HostToolError):
                    MODULE.parse_requirements(mutation)

    def test_lock_rejects_requirement_and_wheel_tampering(self) -> None:
        lock, _ = MODULE.load_lock()
        requirements = MODULE.REQUIREMENTS.read_bytes()
        for mutate in ("requirements", "wheel", "distribution"):
            candidate = json.loads(json.dumps(lock))
            if mutate == "requirements":
                candidate["python"]["requirements"]["sha256"] = "0" * 64
            elif mutate == "wheel":
                candidate["python"]["distributions"][0]["wheel"]["sha256"] = "0" * 64
            else:
                candidate["python"]["distributions"].pop()
            with self.subTest(mutate=mutate):
                with self.assertRaises(MODULE.HostToolError):
                    MODULE.validate_lock(candidate, requirements)

    def test_identity_rejects_paths_reordering_and_library_collisions(self) -> None:
        lock, _ = MODULE.load_lock()
        identity = fixture_identity(lock)
        MODULE.validate_identity(identity, lock)
        mutations = []
        pathful = json.loads(json.dumps(identity))
        pathful["cairo"]["library"]["package_version"] = "/tmp/leak"
        mutations.append(pathful)
        reordered = json.loads(json.dumps(identity))
        reordered["poppler"]["native_libraries"].reverse()
        mutations.append(reordered)
        duplicate = json.loads(json.dumps(identity))
        duplicate["cairo"]["native_libraries"].append(
            duplicate["cairo"]["native_libraries"][0]
        )
        mutations.append(duplicate)
        for candidate in mutations:
            with self.subTest(candidate=candidate):
                with self.assertRaises(MODULE.HostToolError):
                    MODULE.validate_identity(candidate, lock)

    def test_bootstrap_writes_path_neutral_evidence_then_pin_is_exact(self) -> None:
        lock, _ = MODULE.load_lock()
        lock["expected_identity"] = None
        identity = fixture_identity(lock)
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            lock_path = root / "lock.json"
            evidence_path = root / "evidence.json"
            lock_path.write_bytes(MODULE.canonical_json_bytes(lock))
            capture = lambda _, __: json.loads(json.dumps(identity))

            with self.assertRaisesRegex(
                MODULE.HostToolError, "host_identity_pin_required"
            ):
                MODULE.verify_host(
                    lock_path,
                    evidence_path,
                    scope="all",
                    bootstrap_identities=False,
                    capture=capture,
                )
            evidence = json.loads(evidence_path.read_bytes())
            self.assertEqual(evidence["identity_status"], "bootstrap_capture_required")
            self.assertNotIn(str(root), json.dumps(evidence, sort_keys=True))

            MODULE.verify_host(
                lock_path,
                evidence_path,
                scope="all",
                bootstrap_identities=True,
                capture=capture,
            )
            pinned = MODULE.pin_from_evidence(lock_path, evidence_path)
            self.assertEqual(pinned["expected_identity"], identity)

    def test_pinned_mismatch_fails_even_in_bootstrap_mode_and_uploads_actual(self) -> None:
        lock, _ = MODULE.load_lock()
        identity = fixture_identity(lock)
        lock["expected_identity"] = identity
        mismatch = json.loads(json.dumps(identity))
        mismatch["python"]["executable"]["sha256"] = digest("different")
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            lock_path = root / "lock.json"
            evidence_path = root / "evidence.json"
            lock_path.write_bytes(MODULE.canonical_json_bytes(lock))
            with self.assertRaisesRegex(
                MODULE.HostToolError, "host_identity_mismatch"
            ):
                MODULE.verify_host(
                    lock_path,
                    evidence_path,
                    scope="all",
                    bootstrap_identities=True,
                    capture=lambda _, __: mismatch,
                )
            evidence = json.loads(evidence_path.read_bytes())
            self.assertEqual(evidence["identity_status"], "mismatch")
            self.assertEqual(
                evidence["captured_identity_sha256"],
                MODULE.sha256_bytes(MODULE.canonical_json_bytes(mismatch)),
            )

    def test_poppler_capture_never_probes_python_or_cairo(self) -> None:
        lock, _ = MODULE.load_lock()
        identity = fixture_identity(lock)
        executable_by_name = {
            row["name"]: row for row in identity["poppler"]["executables"]
        }
        executable_paths = {
            name: Path(f"/fixture/{name}") for name in executable_by_name
        }

        def poppler_executable(name: str):
            return executable_by_name[name], executable_paths[name]

        with (
            mock.patch.object(MODULE.platform, "machine", return_value="x86_64"),
            mock.patch.object(MODULE.platform, "system", return_value="Linux"),
            mock.patch.object(
                MODULE.platform,
                "python_version",
                side_effect=AssertionError("Python identity was probed"),
            ),
            mock.patch.object(
                MODULE.importlib.metadata,
                "distribution",
                side_effect=AssertionError("Python distributions were probed"),
            ),
            mock.patch.object(
                MODULE,
                "resolve_cairo",
                side_effect=AssertionError("Cairo was probed"),
            ),
            mock.patch.object(
                MODULE, "executable_identity", side_effect=poppler_executable
            ),
            mock.patch.object(MODULE, "ldd_paths", return_value=[Path("/fixture/lib")]),
            mock.patch.object(
                MODULE,
                "library_facts",
                return_value=identity["poppler"]["native_libraries"],
            ),
        ):
            captured = MODULE.capture_identity(lock, "poppler")

        self.assertEqual(captured, MODULE.scoped_identity(identity, "poppler"))

    def test_poppler_scope_is_still_pinned(self) -> None:
        lock, _ = MODULE.load_lock()
        identity = fixture_identity(lock)
        lock["expected_identity"] = identity
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            lock_path = root / "lock.json"
            evidence_path = root / "poppler.json"
            lock_path.write_bytes(MODULE.canonical_json_bytes(lock))
            evidence = MODULE.verify_host(
                lock_path,
                evidence_path,
                scope="poppler",
                bootstrap_identities=False,
                capture=lambda _, scope: MODULE.scoped_identity(identity, scope),
            )
            self.assertEqual(evidence["identity_status"], "pinned_match")
            self.assertEqual(set(evidence["identity"]), {"platform", "poppler"})

    def test_apt_specs_are_sorted_exact_versions_and_require_a_pin(self) -> None:
        lock, _ = MODULE.load_lock()
        lock["expected_identity"] = None
        with self.assertRaisesRegex(
            MODULE.HostToolError, "host_identity_pin_required"
        ):
            MODULE.apt_specs(lock, "all")
        lock["expected_identity"] = fixture_identity(lock)
        specs = MODULE.apt_specs(lock, "all")
        self.assertEqual(specs, sorted(specs))
        self.assertIn("fixture-package=1.2.3-1ubuntu1", specs)
        self.assertIn("poppler-utils=24.02.0-1ubuntu9", specs)
        self.assertTrue(
            all(
                MODULE.DEBIAN_VERSION_RE.fullmatch(row.split("=", 1)[1])
                for row in specs
            )
        )

    def test_apt_specs_reject_conflicting_or_shell_like_package_values(self) -> None:
        lock, _ = MODULE.load_lock()
        lock["expected_identity"] = fixture_identity(lock)
        conflict = lock["expected_identity"]["poppler"]["native_libraries"][0]
        conflict["package_name"] = "poppler-utils"
        conflict["package_version"] = "different"
        with self.assertRaisesRegex(MODULE.HostToolError, "apt_package_conflict"):
            MODULE.apt_specs(lock, "poppler")
        conflict["package_version"] = "$(id)"
        with self.assertRaisesRegex(MODULE.HostToolError, "apt_package"):
            MODULE.apt_specs(lock, "poppler")

    def test_pin_rejects_stale_or_tampered_bootstrap_evidence(self) -> None:
        lock, _ = MODULE.load_lock()
        lock["expected_identity"] = None
        identity = fixture_identity(lock)
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            lock_path = root / "lock.json"
            evidence_path = root / "evidence.json"
            lock_path.write_bytes(MODULE.canonical_json_bytes(lock))
            MODULE.verify_host(
                lock_path,
                evidence_path,
                scope="all",
                bootstrap_identities=True,
                capture=lambda _, __: identity,
            )
            for key in ("lock_file_sha256", "captured_identity_sha256"):
                evidence = json.loads(evidence_path.read_bytes())
                evidence[key] = "0" * 64
                tampered = root / f"{key}.json"
                tampered.write_bytes(MODULE.canonical_json_bytes(evidence))
                with self.subTest(key=key):
                    with self.assertRaises(MODULE.HostToolError):
                        MODULE.pin_from_evidence(lock_path, tampered)

    def test_ldd_parser_rejects_missing_and_only_accepts_existing_absolute_files(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            library = Path(raw) / "libfixture.so.1"
            library.write_bytes(b"library")
            output = f"\tlibfixture.so.1 => {library} (0x1234)\n"
            with mock.patch.object(MODULE, "run_text", return_value=output):
                self.assertEqual(MODULE.ldd_paths(Path("fixture")), [library.resolve()])
            with mock.patch.object(
                MODULE,
                "run_text",
                return_value="libfixture.so.1 => not found\n",
            ):
                with self.assertRaisesRegex(MODULE.HostToolError, "ldd_missing"):
                    MODULE.ldd_paths(Path("fixture"))

    def test_evidence_output_symlink_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            target = root / "target"
            target.write_text("fixture", encoding="utf-8")
            link = root / "evidence.json"
            link.symlink_to(target)
            with self.assertRaisesRegex(MODULE.HostToolError, "evidence_output"):
                MODULE.write_evidence(link, {"status": "fixture"})


if __name__ == "__main__":
    unittest.main()
