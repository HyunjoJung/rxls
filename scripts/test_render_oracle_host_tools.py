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


def package_fact(
    name: str,
    package_name: str = "fixture-package",
    package_version: str = "1.2.3-1ubuntu1",
) -> dict[str, object]:
    return {
        "bytes": 17,
        "name": name,
        "package_name": package_name,
        "package_version": package_version,
        "sha256": digest(name),
    }


def fixture_identity(lock: dict) -> dict:
    bootstrap = {
        row["name"]: row["version"]
        for row in lock["ubuntu_apt"]["bootstrap_packages"]
    }
    cairo_library = package_fact(
        "libcairo.so.2",
        "libcairo2:amd64",
        bootstrap["libcairo2:amd64"],
    )
    libc_library = package_fact(
        "libc.so.6",
        "libc6:amd64",
        bootstrap["libc6-dev:amd64"],
    )
    cairo_libraries = [libc_library, cairo_library]
    cairo_libraries.sort(key=lambda row: row["name"])
    poppler_libraries = [
        libc_library,
        package_fact("libpoppler.so.1"),
    ]
    poppler_libraries.sort(key=lambda row: row["name"])
    executables = []
    for name in lock["poppler"]["executables"]:
        executables.append(
            {
                "bytes": 31,
                "name": name,
                "package_name": "poppler-utils",
                "package_version": bootstrap["poppler-utils"],
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
            "library": cairo_library,
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
        self.assertEqual(lock["schema"], "rxls.render-oracle-host-tools-lock.v2")
        self.assertEqual(lock["ubuntu_apt"]["snapshot"], "20260718T000000Z")
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

    def test_lock_rejects_mutable_or_mismatched_ubuntu_acquisition(self) -> None:
        lock, _ = MODULE.load_lock()
        requirements = MODULE.REQUIREMENTS.read_bytes()
        mutations = []
        for snapshot in ("latest", "20261340T250000Z"):
            candidate = json.loads(json.dumps(lock))
            candidate["ubuntu_apt"]["snapshot"] = snapshot
            mutations.append(candidate)
        candidate = json.loads(json.dumps(lock))
        candidate["ubuntu_apt"]["components"].append("multiverse")
        mutations.append(candidate)
        candidate = json.loads(json.dumps(lock))
        candidate["ubuntu_apt"]["bootstrap_packages"][0]["version"] = "mutable"
        mutations.append(candidate)
        candidate = json.loads(json.dumps(lock))
        candidate["ubuntu_apt"]["bootstrap_packages"][0]["version"] = (
            "2.39-0ubuntu8.6"
        )
        mutations.append(candidate)
        for candidate in mutations:
            with self.subTest(candidate=candidate["ubuntu_apt"]):
                with self.assertRaises(MODULE.HostToolError):
                    MODULE.validate_lock(candidate, requirements)

    def test_apt_sources_are_exact_snapshot_only(self) -> None:
        lock, _ = MODULE.load_lock()
        self.assertEqual(
            MODULE.apt_sources(lock),
            """Types: deb
URIs: https://snapshot.ubuntu.com/ubuntu/20260718T000000Z
Suites: noble noble-updates noble-security
Components: main universe
Architectures: amd64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
""",
        )
        self.assertNotIn("archive.ubuntu.com", MODULE.apt_sources(lock))
        self.assertNotIn("security.ubuntu.com", MODULE.apt_sources(lock))

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

    def test_identity_mismatch_names_the_drifted_entry_without_digests(self) -> None:
        # A bare error code cannot be acted on: it does not distinguish an
        # incidental distribution bump from a real change to a tool that decides
        # output. The report must name the entry and both package versions, and
        # must never echo a file digest into the log.
        lock, _ = MODULE.load_lock()
        expected = fixture_identity(lock)
        actual = json.loads(json.dumps(expected))
        moved = next(
            row
            for row in actual["poppler"]["native_libraries"]
            if row["package_name"] not in MODULE.IDENTITY_PROVENANCE_ONLY_PACKAGES
        )
        original_version = moved["package_version"]
        moved["package_version"] = "9.9.9-9ubuntu9"
        moved["sha256"] = digest("drifted")
        lines = MODULE.identity_mismatch_report(
            MODULE.identity_for_comparison(expected),
            MODULE.identity_for_comparison(actual),
        )
        self.assertTrue(lines)
        joined = "\n".join(lines)
        self.assertIn(moved["name"], joined)
        self.assertIn(original_version, joined)
        self.assertIn("9.9.9-9ubuntu9", joined)
        self.assertNotIn(moved["sha256"], joined)
        self.assertNotIn(digest("drifted"), joined)

        # A same-version content change is reported distinctly, because that is
        # the case that must never be waved through as a distribution bump.
        same = json.loads(json.dumps(expected))
        target = next(
            row
            for row in same["poppler"]["native_libraries"]
            if row["package_name"] not in MODULE.IDENTITY_PROVENANCE_ONLY_PACKAGES
        )
        target["sha256"] = digest("tampered")
        report = "\n".join(
            MODULE.identity_mismatch_report(
                MODULE.identity_for_comparison(expected),
                MODULE.identity_for_comparison(same),
            )
        )
        self.assertIn("content changed at the same package version", report)
        self.assertNotIn(digest("tampered"), report)

        # A library excluded from the identity requirement produces no report.
        exempt = json.loads(json.dumps(expected))
        for row in exempt["poppler"]["native_libraries"]:
            if row["package_name"] in MODULE.IDENTITY_PROVENANCE_ONLY_PACKAGES:
                row["sha256"] = digest("libc-moved")
        self.assertEqual(
            MODULE.identity_mismatch_report(
                MODULE.identity_for_comparison(expected),
                MODULE.identity_for_comparison(exempt),
            ),
            [],
        )

    def test_provenance_exemption_covers_the_provider_spelling(self) -> None:
        # Packaged rows name their source in `package_name`; the Python section
        # uses `provider`. Honouring only one spelling leaves the C runtime
        # compared under the other, which is exactly how a glibc security bump
        # kept failing the hosted bootstrap after the exemption was added.
        exempt = sorted(MODULE.IDENTITY_PROVENANCE_ONLY_PACKAGES)[0]

        def section(version: str, sha: str) -> dict:
            return {
                "packaged": {
                    "native_libraries": [
                        {
                            "bytes": 1,
                            "name": "libc.so.6",
                            "package_name": exempt,
                            "package_version": version,
                            "sha256": sha,
                        },
                        {
                            "bytes": 2,
                            "name": "libfreetype.so.6",
                            "package_name": "libfreetype6:amd64",
                            "package_version": "2.13.2-1",
                            "sha256": digest("freetype"),
                        },
                    ]
                },
                "provided": {
                    "native_libraries": [
                        {
                            "bytes": 3,
                            "name": "libm.so.6",
                            "provider": exempt,
                            "provider_version": version,
                            "sha256": sha,
                        }
                    ]
                },
            }

        before = section("2.39-0ubuntu8.7", digest("glibc-8.7"))
        after = section("2.39-0ubuntu8.8", digest("glibc-8.8"))
        self.assertEqual(
            MODULE.identity_mismatch_report(
                MODULE.identity_for_comparison(before),
                MODULE.identity_for_comparison(after),
            ),
            [],
            "the C runtime must be exempt under both spellings",
        )

        # A library that is not exempt still fails closed under either spelling.
        moved = json.loads(json.dumps(after))
        moved["packaged"]["native_libraries"][1]["package_version"] = "2.13.3-1"
        self.assertTrue(
            MODULE.identity_mismatch_report(
                MODULE.identity_for_comparison(before),
                MODULE.identity_for_comparison(moved),
            )
        )

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
        # The bootstrap scope does not pin libc6, so pinning libc6-dev would
        # fail against any runner image carrying a newer libc6 through the dev
        # package's exact `libc6 (= version)` dependency. libc6-dev affects
        # neither rendering nor measurement, so it resolves freely while every
        # package that does affect the oracle stays exactly pinned.
        # Re-bootstrapping against an existing attestation installs the whole
        # attested closure, so the captured identity is comparable by
        # construction rather than by exempting drifted libraries one at a time.
        attested = MODULE.apt_specs(lock, "bootstrap")
        self.assertEqual(attested, sorted(set(attested)))
        for spec in (
            "libcairo2:amd64=1.18.0-3build1",
            "poppler-utils=24.02.0-1ubuntu9.9",
            "libssl3t64:amd64=3.0.13-0ubuntu3.11",
            "libkrb5-3:amd64=1.20.1-6ubuntu2.6",
        ):
            self.assertIn(spec, attested)
        self.assertEqual(set(attested), set(attested) | set(MODULE.apt_specs(lock, "all")))
        # A first bootstrap has nothing attested, so only the snapshot-pinned
        # top-level tools can be named.
        unpinned = json.loads(json.dumps(lock))
        unpinned["expected_identity"] = None
        self.assertEqual(
            MODULE.apt_specs(unpinned, "bootstrap"),
            [
                "libc6-dev:amd64",
                "libcairo2:amd64=1.18.0-3build1",
                "poppler-utils=24.02.0-1ubuntu9.9",
            ],
        )
        # The provenance version stays recorded in the lock even though the
        # bootstrap install no longer pins it.
        self.assertEqual(
            [
                item["version"]
                for item in lock["ubuntu_apt"]["bootstrap_packages"]
                if item["name"] == "libc6-dev:amd64"
            ],
            ["2.39-0ubuntu8.7"],
        )
        # Only libc6-dev is exempt; nothing else may lose its pin.
        self.assertEqual(
            MODULE.BOOTSTRAP_UNPINNED_PACKAGES, frozenset({"libc6-dev:amd64"})
        )
        # The libc6 family is requested by name only: libc6-dev and
        # libc-dev-bin each depend on an exact libc6 version, so pinning any of
        # them downgrades the runner's C runtime, and none of the three is part
        # of the identity requirement.
        poppler_specs = MODULE.apt_specs(lock, "poppler")
        for package in sorted(MODULE.LIBC_FAMILY_UNPINNED_PACKAGES):
            self.assertIn(package, poppler_specs)
        self.assertFalse(
            [
                spec
                for spec in poppler_specs
                if spec.split("=", 1)[0] in MODULE.LIBC_FAMILY_UNPINNED_PACKAGES
                and "=" in spec
            ],
            "no libc family package may carry a version pin",
        )
        # Everything else keeps an exact pin.
        self.assertTrue(
            all(
                "=" in spec
                for spec in poppler_specs
                if spec not in MODULE.LIBC_FAMILY_UNPINNED_PACKAGES
            )
        )
        self.assertNotIn(
            "libcairo2:amd64=1.18.0-3build1",
            MODULE.apt_specs(lock, "poppler"),
        )
        lock["expected_identity"] = None
        with self.assertRaisesRegex(
            MODULE.HostToolError, "host_identity_pin_required"
        ):
            MODULE.apt_specs(lock, "all")
        lock["expected_identity"] = fixture_identity(lock)
        specs = MODULE.apt_specs(lock, "all")
        self.assertEqual(specs, sorted(specs))
        self.assertIn("fixture-package=1.2.3-1ubuntu1", specs)
        self.assertIn("poppler-utils=24.02.0-1ubuntu9.9", specs)
        # Every spec is either an exactly pinned `name=version` or one of the
        # C runtime family requested by bare name.
        for row in specs:
            if row in MODULE.LIBC_FAMILY_UNPINNED_PACKAGES:
                continue
            name, separator, version = row.partition("=")
            self.assertEqual(separator, "=", row)
            self.assertIsNotNone(MODULE.DEBIAN_PACKAGE_RE.fullmatch(name), row)
            self.assertIsNotNone(MODULE.DEBIAN_VERSION_RE.fullmatch(version), row)

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
