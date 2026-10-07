#!/usr/bin/env python3
"""Tests for the hosted render-oracle tool identity lock."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
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


def restoration_fixture() -> tuple[dict, bytes]:
    payload = b"authenticated archive fixture"
    record = dict(MODULE.APT_RESTORATIONS[0])
    record.update(bytes=len(payload), sha256=hashlib.sha256(payload).hexdigest())
    return record, payload


def restoration_metadata(record: dict) -> str:
    return (
        f"Package: {record['name']}\nVersion: {record['version']}\n"
        f"Architecture: {record['architecture']}\n"
    )


class RestorationResponse(io.BytesIO):
    def __init__(self, payload: bytes, record: dict, headers: dict | None = None):
        super().__init__(payload)
        self.headers = headers or {}
        self.status = 200
        self.url = record["url"]
        self.read_sizes: list[int] = []
        self.bytes_read = 0

    def geturl(self):
        return self.url

    def read(self, size=-1):
        if size <= 0:
            raise AssertionError("restoration reads must have a positive bound")
        self.read_sizes.append(size)
        payload = super().read(size)
        self.bytes_read += len(payload)
        return payload

    def read1(self, size=-1):
        return self.read(size)


class RenderOracleHostToolsTests(unittest.TestCase):
    def test_installed_companion_specs_derive_exact_versions_from_locked_libraries(self) -> None:
        lock, _ = MODULE.load_lock()
        installed = {
            "bzip2", "libc6-i386", "libexpat1-dev", "libssl-dev", "p11-kit",
            "p11-kit-modules", "zlib1g-dev",
        }
        with mock.patch.object(MODULE, "installed_apt_companions", return_value=installed):
            self.assertEqual(MODULE.companion_apt_specs(lock), [
                "bzip2:amd64=1.0.8-5.1build0.1",
                "libc6-i386:amd64=2.39-0ubuntu8.8",
                "libexpat1-dev:amd64=2.6.1-2ubuntu0.4",
                "libssl-dev:amd64=3.0.13-0ubuntu3.12",
                "p11-kit-modules:amd64=0.25.3-4ubuntu2.1",
                "p11-kit:amd64=0.25.3-4ubuntu2.1",
                "zlib1g-dev:amd64=1:1.3.dfsg-3.1ubuntu2.1",
            ])

    def test_absent_companions_are_not_added_and_installed_query_is_bounded(self) -> None:
        lock, _ = MODULE.load_lock()
        output = (
            "bash\tamd64\tinstalled\n"
            "bzip2\tamd64\tconfig-files\n"
            "libc6-i386\tamd64\tnot-installed\n"
        )
        with mock.patch.object(MODULE, "run_text", return_value=output) as query:
            self.assertEqual(MODULE.companion_apt_specs(lock), [])
            self.assertEqual(query.call_count, 1)
            self.assertEqual(query.call_args.args, (
                ["dpkg-query", "--show", "--showformat=${Package}\t${Architecture}\t${db:Status-Status}\n"],
                "apt_companion_query",
            ))

    def test_companion_inventory_rejects_malformed_duplicate_foreign_or_partial_state(self) -> None:
        for output, code in (
            ("bzip2\tamd64\tinstalled", "apt_companion_query"),
            ("bzip2\tinstalled\n", "apt_companion_query"),
            ("bzip2:amd64\tamd64\tinstalled\n", "apt_companion_query"),
            ("bzip2\t$(id)\tinstalled\n", "apt_companion_query"),
            ("bzip2\tamd64\tinstalled\nbzip2\tamd64\tinstalled\n", "apt_companion_query"),
            ("bzip2\ti386\tinstalled\n", "apt_companion_architecture"),
            ("libc6-i386\tarm64\tinstalled\n", "apt_companion_architecture"),
            ("libssl-dev\tamd64\thalf-installed\n", "apt_companion_status"),
            ("p11-kit\tamd64\tunpacked\n", "apt_companion_status"),
            ("zlib1g-dev\tamd64\ttriggers-pending\n", "apt_companion_status"),
        ):
            with self.subTest(output=output):
                with mock.patch.object(MODULE, "run_text", return_value=output):
                    with self.assertRaisesRegex(MODULE.HostToolError, code):
                        MODULE.installed_apt_companions()
        with mock.patch.object(MODULE, "MAX_APT_INSTALLED_RECORDS", 2):
            with mock.patch.object(MODULE, "run_text", return_value="bash\tamd64\tinstalled\n" * 3):
                with self.assertRaisesRegex(MODULE.HostToolError, "apt_companion_query"):
                    MODULE.installed_apt_companions()

    def test_companion_inventory_query_failure_is_not_treated_as_absence(self) -> None:
        lock, _ = MODULE.load_lock()
        with mock.patch.object(MODULE, "run_text", side_effect=MODULE.HostToolError("apt_companion_query")):
            with self.assertRaisesRegex(MODULE.HostToolError, "apt_companion_query"):
                MODULE.companion_apt_specs(lock)
        result = subprocess.CompletedProcess([], 1, b"", b"database failure")
        with mock.patch.object(MODULE.subprocess, "run", return_value=result) as query:
            with self.assertRaisesRegex(MODULE.HostToolError, "apt_companion_query"):
                MODULE.installed_apt_companions()
            self.assertEqual(query.call_args.kwargs["timeout"], 15)
            self.assertFalse(query.call_args.kwargs.get("shell", False))

    def test_companion_sources_reject_missing_conflicting_foreign_or_unsafe_versions(self) -> None:
        original, _ = MODULE.load_lock()
        self.assertEqual(MODULE.APT_COMPANION_SOURCES, {
            "bzip2": "libbz2-1.0", "libc6-i386": "libc6",
            "libexpat1-dev": "libexpat1",
            "libssl-dev": "libssl3t64", "p11-kit": "libp11-kit0",
            "p11-kit-modules": "libp11-kit0", "zlib1g-dev": "zlib1g",
        })
        for companion, source in MODULE.APT_COMPANION_SOURCES.items():
            for mutation in ("missing", "conflict", "architecture", "unsafe"):
                lock = json.loads(json.dumps(original))
                matches = []
                for section in ("poppler", "cairo", "python"):
                    rows = lock["expected_identity"][section]["native_libraries"]
                    for row in rows:
                        key = "provider" if section == "python" else "package_name"
                        if row[key].split(":", 1)[0] == source:
                            matches.append((row, key, "provider_version" if section == "python" else "package_version"))
                    if mutation == "missing":
                        lock["expected_identity"][section]["native_libraries"] = [
                            row for row in rows
                            if row.get("package_name", row.get("provider", "")).split(":", 1)[0] != source
                        ]
                self.assertTrue(matches)
                row, name_key, version_key = matches[0]
                if mutation == "conflict":
                    # Add a contradictory row even for a source present only
                    # once, rather than accidentally creating a consistent pin.
                    conflict = dict(row)
                    conflict[version_key] = "different-version"
                    section = "python" if name_key == "provider" else "poppler"
                    lock["expected_identity"][section]["native_libraries"].append(conflict)
                elif mutation == "architecture":
                    row[name_key] = f"{source}:arm64"
                elif mutation == "unsafe":
                    row[version_key] = "$(id)"
                with self.subTest(companion=companion, mutation=mutation):
                    with mock.patch.object(MODULE, "installed_apt_companions", return_value={companion}):
                        with self.assertRaisesRegex(MODULE.HostToolError, "apt_companion_source_identity"):
                            MODULE.companion_apt_specs(lock)

    def test_companion_versions_are_derived_not_hardcoded(self) -> None:
        lock, _ = MODULE.load_lock()
        versions = {"zlib1g": "2:1.3.fixture-1", "libexpat1": "2.6.1-fixture.9"}
        for section in ("poppler", "cairo", "python"):
            for row in lock["expected_identity"][section]["native_libraries"]:
                name = row.get("package_name", row.get("provider", "")).split(":", 1)[0]
                if name in versions:
                    row["provider_version" if section == "python" else "package_version"] = versions[name]
        with mock.patch.object(
            MODULE, "installed_apt_companions", return_value={"zlib1g-dev", "libexpat1-dev"}
        ):
            self.assertEqual(MODULE.companion_apt_specs(lock), [
                "libexpat1-dev:amd64=2.6.1-fixture.9",
                "zlib1g-dev:amd64=2:1.3.fixture-1",
            ])

    def test_companions_are_only_queried_for_pinned_opt_in_and_only_installed_are_added(self) -> None:
        lock, _ = MODULE.load_lock()
        with mock.patch.object(MODULE, "installed_apt_companions") as query:
            for scope in ("all", "poppler", "bootstrap"):
                self.assertEqual(MODULE.apt_specs(lock, scope), MODULE.default_apt_specs(lock, scope))
            unpinned = json.loads(json.dumps(lock))
            unpinned["expected_identity"] = None
            self.assertEqual(
                MODULE.apt_specs(unpinned, "bootstrap", restoration_dir=Path("/not-used")),
                MODULE.default_apt_specs(unpinned, "bootstrap"),
            )
            query.assert_not_called()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            with (
                mock.patch.object(MODULE, "verify_apt_restoration", side_effect=lambda path, _: path),
                mock.patch.object(MODULE, "run_text", return_value="bzip2\tamd64\tinstalled\n") as query,
            ):
                for scope in ("all", "poppler", "bootstrap"):
                    query.reset_mock()
                    specs = MODULE.apt_specs(lock, scope, restoration_dir=root)
                    self.assertEqual(query.call_count, 1)
                    self.assertIn("bzip2:amd64=1.0.8-5.1build0.1", specs)
                    for absent in set(MODULE.APT_COMPANION_SOURCES) - {"bzip2"}:
                        self.assertFalse(any(spec.startswith(f"{absent}:amd64=") for spec in specs))

    def test_observed_expat_python_development_chain_preserves_only_runtime_companion(self) -> None:
        # Hosted pdf job 111745142542 planned these five removals. Only expat-dev
        # has an exact-version coupling to our locked runtime; Python versions
        # remain the runner's installed versions and must not become apt args.
        lock, _ = MODULE.load_lock()
        before = MODULE.canonical_json_bytes(lock)
        python_dev = {"libpython3-dev", "libpython3.12-dev", "python3-dev", "python3.12-dev"}
        installed = "".join(
            f"{name}\tamd64\tinstalled\n"
            for name in sorted(python_dev | {"libexpat1-dev"})
        )
        with tempfile.TemporaryDirectory() as raw:
            with (
                mock.patch.object(MODULE, "verify_apt_restoration", side_effect=lambda path, _: path),
                mock.patch.object(MODULE, "run_text", return_value=installed) as query,
            ):
                for scope in ("poppler", "all"):
                    with self.subTest(scope=scope):
                        query.reset_mock()
                        specs = MODULE.apt_specs(lock, scope, restoration_dir=Path(raw).resolve())
                        self.assertEqual(query.call_count, 1)
                        self.assertEqual(specs, sorted(set(specs)))
                        self.assertIn("libexpat1:amd64=2.6.1-2ubuntu0.4", specs)
                        self.assertIn("libexpat1-dev:amd64=2.6.1-2ubuntu0.4", specs)
                        self.assertEqual(sum(s.startswith("libexpat1-dev:amd64=") for s in specs), 1)
                        for name in python_dev:
                            self.assertFalse(any(s.startswith(f"{name}:amd64=") for s in specs))
        self.assertEqual(MODULE.canonical_json_bytes(lock), before)

    def test_expat_companion_absence_and_invalid_installed_state_fail_closed(self) -> None:
        lock, _ = MODULE.load_lock()
        for status in ("not-installed", "config-files"):
            with self.subTest(status=status):
                with mock.patch.object(MODULE, "run_text", return_value=f"libexpat1-dev\tamd64\t{status}\n"):
                    self.assertEqual(MODULE.companion_apt_specs(lock), [])
        for output, error in (
            ("libexpat1-dev\ti386\tinstalled\n", "apt_companion_architecture"),
            ("libexpat1-dev\tamd64\thalf-installed\n", "apt_companion_status"),
            ("libexpat1-dev\tamd64\tinstalled\n" * 2, "apt_companion_query"),
        ):
            with self.subTest(output=output):
                with mock.patch.object(MODULE, "run_text", return_value=output):
                    with self.assertRaisesRegex(MODULE.HostToolError, error):
                        MODULE.companion_apt_specs(lock)

    def test_restoration_records_are_the_four_authenticated_exact_archives(self) -> None:
        records = MODULE.APT_RESTORATIONS
        self.assertEqual(len(records), 4)
        self.assertEqual(
            {row["name"] for row in records},
            {"libgssapi-krb5-2", "libk5crypto3", "libkrb5-3", "libkrb5support0"},
        )
        self.assertEqual(sum(row["bytes"] for row in records), 606934)
        self.assertEqual(
            {row["name"]: (row["bytes"], row["sha256"]) for row in records},
            {
                "libgssapi-krb5-2": (142680, "6cd99ec16ae12eb465712f950e43eaf03a8d2a6ab24c00178df56470d5343b66"),
                "libk5crypto3": (81946, "48f689737191cfafaf3c158e9b07d6448f9e6217ad7abbaacc4f96dc95403fa2"),
                "libkrb5-3": (347620, "63ab8110daea359f55d8135d395de198257acb1f948500c561745addddfece4c"),
                "libkrb5support0": (34688, "cee1efc93d4ce4a97db756269824b5a2b90d2cb993cd76102432db60890819fc"),
            },
        )
        for row in records:
            self.assertEqual(row["version"], "1.20.1-6ubuntu2.8")
            self.assertEqual(row["architecture"], "amd64")
            self.assertRegex(row["sha256"], r"^[0-9a-f]{64}$")
            self.assertEqual(
                row["url"],
                "https://snapshot.ubuntu.com/ubuntu/20260909T000000Z/"
                f"pool/main/k/krb5/{row['name']}_{row['version']}_amd64.deb",
            )

    def test_restoration_opt_in_pins_one_consistent_exact_libc_family(self) -> None:
        lock, _ = MODULE.load_lock()
        before = MODULE.canonical_json_bytes(lock)
        version = next(
            row["version"] for row in lock["ubuntu_apt"]["bootstrap_packages"]
            if row["name"] == "libc6-dev:amd64"
        )
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            with (
                mock.patch.object(MODULE, "verify_apt_restoration", side_effect=lambda path, _: path),
                mock.patch.object(MODULE, "installed_apt_companions", return_value=set()),
            ):
                for scope in ("all", "poppler", "bootstrap"):
                    with self.subTest(scope=scope):
                        specs = MODULE.apt_specs(lock, scope, restoration_dir=root)
                        self.assertEqual(specs, sorted(set(specs)))
                        for name in ("libc6", "libc6-dev", "libc-dev-bin", "libc-bin"):
                            self.assertIn(f"{name}:amd64={version}", specs)
                            self.assertEqual(
                                len([s for s in specs if s.split("=", 1)[0].split(":", 1)[0] == name]), 1
                            )
                        for record in MODULE.APT_RESTORATIONS:
                            self.assertIn(str(root / record["url"].rsplit("/", 1)[1]), specs)
                            self.assertNotIn(f"{record['name']}:amd64={record['version']}", specs)
        self.assertEqual(MODULE.canonical_json_bytes(lock), before)

    def test_null_identity_does_not_restore_unproven_packages(self) -> None:
        lock, _ = MODULE.load_lock()
        lock["expected_identity"] = None
        with mock.patch.object(MODULE, "download_apt_restoration") as download:
            unsafe = Path("/unavailable/restorations")
            self.assertEqual(MODULE.fetch_apt_restorations(lock, unsafe), [])
            self.assertEqual(
                MODULE.apt_specs(lock, "bootstrap", restoration_dir=unsafe),
                MODULE.apt_specs(lock, "bootstrap"),
            )
            download.assert_not_called()

    def test_restoration_cli_has_explicit_opt_in_and_fetch_output(self) -> None:
        parser = MODULE.build_parser()
        args = parser.parse_args(["fetch-apt-restorations", "--output-dir", "restorations"])
        self.assertEqual(args.output_dir, Path("restorations"))
        args = parser.parse_args(["apt-specs", "--scope", "bootstrap", "--restoration-dir", "restorations"])
        self.assertEqual(args.restoration_dir, Path("restorations"))
        with mock.patch.object(MODULE, "fetch_apt_restorations", return_value=[Path("/safe/pkg.deb")]) as fetch:
            with mock.patch("sys.stdout", new_callable=io.StringIO) as output:
                self.assertEqual(MODULE.main(["fetch-apt-restorations", "--output-dir", "/safe"]), 0)
            self.assertEqual(output.getvalue(), "/safe/pkg.deb\n")
            self.assertEqual(fetch.call_args.args[1], Path("/safe"))

    def test_restoration_download_bounds_and_authenticates_before_inspection(self) -> None:
        record, payload = restoration_fixture()
        for content, code in (
            (payload, None),
            (payload[:-1], "apt_restoration_size"),
            (payload + b"trailing", "apt_restoration_size"),
            (b"X" * len(payload), "apt_restoration_sha256"),
        ):
            with self.subTest(code=code):
                response = RestorationResponse(content, record)
                opener = mock.Mock()
                opener.open.return_value = response
                with mock.patch.object(MODULE.urllib.request, "build_opener", return_value=opener):
                    with mock.patch.object(MODULE, "run_text") as inspect:
                        if code is None:
                            self.assertEqual(MODULE.download_apt_restoration(record), payload)
                        else:
                            with self.assertRaisesRegex(MODULE.HostToolError, code):
                                MODULE.download_apt_restoration(record)
                        inspect.assert_not_called()
                self.assertLessEqual(response.bytes_read, len(payload) + 1)
                self.assertTrue(all(0 < size <= len(payload) + 1 for size in response.read_sizes))
                self.assertEqual(opener.open.call_args.kwargs["timeout"], 30)
                self.assertEqual(opener.open.call_args.args[0].full_url, record["url"])

    def test_restoration_download_rejects_headers_redirects_errors_and_timeout(self) -> None:
        record, payload = restoration_fixture()
        for headers, status, url, code in (
            ({"Content-Length": str(len(payload) + 1)}, 200, record["url"], "apt_restoration_size"),
            ({"Content-Length": "invalid"}, 200, record["url"], "apt_restoration_size"),
            ({"Content-Encoding": "gzip"}, 200, record["url"], "apt_restoration_response"),
            ({}, 404, record["url"], "apt_restoration_response"),
            ({}, 200, "https://untrusted.invalid/pkg.deb", "apt_restoration_response"),
        ):
            with self.subTest(headers=headers, status=status, url=url):
                response = RestorationResponse(payload, record, headers)
                response.status, response.url = status, url
                opener = mock.Mock()
                opener.open.return_value = response
                with mock.patch.object(MODULE.urllib.request, "build_opener", return_value=opener):
                    with self.assertRaisesRegex(MODULE.HostToolError, code):
                        MODULE.download_apt_restoration(record)
                self.assertEqual(response.read_sizes, [])
        with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_redirect"):
            MODULE.NoRestorationRedirect().redirect_request(None, None, 302, "", {}, record["url"])
        opener = mock.Mock()
        opener.open.side_effect = TimeoutError("bounded timeout")
        with mock.patch.object(MODULE.urllib.request, "build_opener", return_value=opener):
            with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_download"):
                MODULE.download_apt_restoration(record)
        opener.open.side_effect = None
        opener.open.return_value = RestorationResponse(payload, record)
        with mock.patch.object(MODULE.urllib.request, "build_opener", return_value=opener):
            with mock.patch.object(MODULE.time, "monotonic", side_effect=[0, 60]):
                with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_timeout"):
                    MODULE.download_apt_restoration(record)

    def test_restoration_total_deadline_covers_open_and_slow_reads(self) -> None:
        record, payload = restoration_fixture()
        previous_handler = MODULE.signal.getsignal(MODULE.signal.SIGALRM)
        opener = mock.Mock()
        # Deliver the real alarm during open: the deadline must be installed
        # before DNS/TLS/header parsing, not merely around reading the body.
        opener.open.side_effect = lambda *args, **kwargs: MODULE.signal.raise_signal(MODULE.signal.SIGALRM)
        with mock.patch.object(MODULE.urllib.request, "build_opener", return_value=opener):
            with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_timeout"):
                MODULE.download_apt_restoration(record)
        self.assertEqual(MODULE.signal.getitimer(MODULE.signal.ITIMER_REAL), (0.0, 0.0))
        self.assertEqual(MODULE.signal.getsignal(MODULE.signal.SIGALRM), previous_handler)
        opener.open.side_effect = None
        opener.open.return_value = RestorationResponse(payload, record)
        with mock.patch.object(MODULE.urllib.request, "build_opener", return_value=opener):
            with mock.patch.object(MODULE.time, "monotonic", side_effect=[0, 1, 61]):
                with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_timeout"):
                    MODULE.download_apt_restoration(record)
        with mock.patch.object(MODULE.signal, "getitimer", return_value=(1.0, 0.0)):
            with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_timer"):
                MODULE.download_apt_restoration(record)

    def test_restoration_record_size_cap_precedes_network_and_process(self) -> None:
        record, _ = restoration_fixture()
        for size in (0, -1, True, MODULE.MAX_APT_RESTORATION_BYTES + 1):
            record["bytes"] = size
            with mock.patch.object(MODULE.urllib.request, "build_opener") as network:
                with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_size"):
                    MODULE.download_apt_restoration(record)
                network.assert_not_called()

    def test_restoration_metadata_requires_exact_three_fields(self) -> None:
        record, payload = restoration_fixture()
        good = restoration_metadata(record)
        malformed = (
            good.replace(record["name"], "wrong-package"),
            good.replace(record["version"], "wrong-version"),
            good.replace("amd64", "arm64"),
            good + "Package: duplicate\n",
            good + "Extra: value\n",
            good.replace("Package: ", ""),
            good.replace("Architecture: amd64\n", ""),
            "X" * 1025,
        )
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw).resolve() / "fixture.deb"
            path.write_bytes(payload)
            with mock.patch.object(MODULE, "run_text", return_value=good) as inspect:
                self.assertEqual(MODULE.verify_apt_restoration(path, record), path)
                self.assertEqual(inspect.call_args.args, (
                    ["dpkg-deb", "--field", str(path), "Package", "Version", "Architecture"],
                    "apt_restoration_metadata",
                ))
            for output in malformed:
                with self.subTest(output=output[:80]):
                    with mock.patch.object(MODULE, "run_text", return_value=output):
                        with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_metadata"):
                            MODULE.verify_apt_restoration(path, record)
            with mock.patch.object(MODULE, "run_text", side_effect=MODULE.HostToolError("apt_restoration_metadata")):
                with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_metadata"):
                    MODULE.verify_apt_restoration(path, record)

    def test_restoration_inspection_uses_bounded_existing_process_helper(self) -> None:
        record, payload = restoration_fixture()
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw).resolve() / "fixture.deb"
            path.write_bytes(payload)
            result = subprocess.CompletedProcess([], 0, restoration_metadata(record).encode(), b"")
            with mock.patch.object(MODULE.subprocess, "run", return_value=result) as run:
                MODULE.verify_apt_restoration(path, record)
                self.assertEqual(run.call_args.kwargs["timeout"], 15)
                self.assertFalse(run.call_args.kwargs.get("shell", False))
            for result in (
                subprocess.CompletedProcess([], 1, b"", b"failure"),
                subprocess.CompletedProcess([], 0, b"X" * (4 * 1024 * 1024 + 1), b""),
            ):
                with mock.patch.object(MODULE.subprocess, "run", return_value=result):
                    with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_metadata"):
                        MODULE.verify_apt_restoration(path, record)
            with mock.patch.object(MODULE.subprocess, "run", side_effect=subprocess.TimeoutExpired("dpkg-deb", 15)):
                with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_metadata"):
                    MODULE.verify_apt_restoration(path, record)

    def test_restoration_directory_rejects_unsafe_paths_without_writes(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            for name in ("with space", "line\nbreak", "quote'", 'quote"', "back\\slash", "tab\t", "control\x01"):
                with self.subTest(name=name):
                    with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_path"):
                        MODULE.restoration_directory(root / name, create=True)
                    self.assertFalse((root / name).exists())
            with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_path"):
                MODULE.restoration_directory(root / ".." / "escape", create=True)
            target = root / "real"
            target.mkdir()
            link = root / "symlink"
            link.symlink_to(target, target_is_directory=True)
            for path in (link, link / "child"):
                with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_path"):
                    MODULE.restoration_directory(path, create=True)
            self.assertFalse((target / "child").exists())
            occupied = root / "file"
            occupied.write_bytes(b"keep")
            with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_path"):
                MODULE.restoration_directory(occupied, create=True)
            self.assertEqual(occupied.read_bytes(), b"keep")
            target.chmod(0o777)
            with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_path"):
                MODULE.restoration_directory(target)

    def test_restoration_archive_rejects_tampering_and_unsafe_targets(self) -> None:
        record, payload = restoration_fixture()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            for contents, code in ((payload[:-1], "size"), (b"X" * len(payload), "sha256")):
                path = root / "fixture.deb"
                path.write_bytes(contents)
                with mock.patch.object(MODULE, "run_text") as inspect:
                    with self.assertRaisesRegex(MODULE.HostToolError, f"apt_restoration_{code}"):
                        MODULE.verify_apt_restoration(path, record)
                    inspect.assert_not_called()
            path.write_bytes(payload)
            link = root / "linked.deb"
            link.symlink_to(path)
            directory = root / "directory.deb"
            directory.mkdir()
            for unsafe in (link, directory, root / "missing.deb"):
                with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_path"):
                    MODULE.verify_apt_restoration(unsafe, record)
            hardlink = root / "hardlinked.deb"
            os.link(path, hardlink)
            with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_path"):
                MODULE.verify_apt_restoration(path, record)
            hardlink.unlink()
            path.chmod(0o666)
            with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_path"):
                MODULE.verify_apt_restoration(path, record)

    def test_restoration_rejects_archive_replacement_during_inspection(self) -> None:
        record, payload = restoration_fixture()
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw).resolve() / "fixture.deb"
            path.write_bytes(payload)
            def replace(*_):
                replacement = path.with_suffix(".new")
                replacement.write_bytes(payload)
                replacement.replace(path)
                return restoration_metadata(record)
            with mock.patch.object(MODULE, "run_text", side_effect=replace):
                with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_changed"):
                    MODULE.verify_apt_restoration(path, record)

    def test_restoration_fetch_publishes_verified_cache_and_rechecks_reuse(self) -> None:
        lock, _ = MODULE.load_lock()
        record, payload = restoration_fixture()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve() / "new"
            with mock.patch.object(MODULE, "APT_RESTORATIONS", (record,)):
                with mock.patch.object(MODULE, "download_apt_restoration", return_value=payload) as download:
                    with mock.patch.object(MODULE, "run_text", return_value=restoration_metadata(record)) as inspect:
                        paths = MODULE.fetch_apt_restorations(lock, root)
                        self.assertEqual(paths, [root / record["url"].rsplit("/", 1)[1]])
                        self.assertEqual(paths[0].read_bytes(), payload)
                        self.assertEqual(list(root.iterdir()), paths)
                        self.assertEqual(MODULE.fetch_apt_restorations(lock, root), paths)
                        self.assertEqual(download.call_count, 1)
                        self.assertEqual(inspect.call_count, 2)
                        paths[0].write_bytes(b"X" * len(payload))
                        with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_sha256"):
                            MODULE.fetch_apt_restorations(lock, root)
                        self.assertEqual(download.call_count, 1)
                        with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_sha256"):
                            MODULE.apt_specs(lock, "all", restoration_dir=root)

    def test_restoration_fetch_failure_never_publishes_unverified_archive(self) -> None:
        lock, _ = MODULE.load_lock()
        record, payload = restoration_fixture()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            with mock.patch.object(MODULE, "APT_RESTORATIONS", (record,)):
                for content, metadata, code in (
                    (payload + b"X", restoration_metadata(record), "apt_restoration_size"),
                    (payload, "Package: wrong\n", "apt_restoration_metadata"),
                ):
                    with self.subTest(code=code):
                        with mock.patch.object(MODULE, "download_apt_restoration", return_value=content):
                            with mock.patch.object(MODULE, "run_text", return_value=metadata):
                                with self.assertRaisesRegex(MODULE.HostToolError, code):
                                    MODULE.fetch_apt_restorations(lock, root)
                        self.assertEqual(list(root.iterdir()), [])

    def test_restoration_fetch_never_overwrites_an_existing_target(self) -> None:
        lock, _ = MODULE.load_lock()
        record, payload = restoration_fixture()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw).resolve()
            destination = root / record["url"].rsplit("/", 1)[1]
            with mock.patch.object(MODULE, "APT_RESTORATIONS", (record,)):
                with mock.patch.object(MODULE, "download_apt_restoration", return_value=payload):
                    def concurrent_writer(*_):
                        destination.write_bytes(b"concurrent contents")
                        return restoration_metadata(record)
                    with mock.patch.object(MODULE, "run_text", side_effect=concurrent_writer):
                        with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_path"):
                            MODULE.fetch_apt_restorations(lock, root)
                self.assertEqual(destination.read_bytes(), b"concurrent contents")
                self.assertEqual(list(root.iterdir()), [destination])
                destination.unlink()
                destination.symlink_to(root / "absent")
                with mock.patch.object(MODULE, "download_apt_restoration") as download:
                    with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_path"):
                        MODULE.fetch_apt_restorations(lock, root)
                    download.assert_not_called()

    def test_restoration_selection_is_exact_and_does_not_fetch_unrelated_versions(self) -> None:
        lock, _ = MODULE.load_lock()
        records = MODULE.relevant_apt_restorations(lock)
        self.assertEqual(records, list(MODULE.APT_RESTORATIONS))
        for row in lock["expected_identity"]["poppler"]["native_libraries"]:
            if row["package_name"].split(":")[0] == "libkrb5-3":
                row["package_version"] = "1.20.1-6ubuntu2.99"
        self.assertNotIn("libkrb5-3", [row["name"] for row in MODULE.relevant_apt_restorations(lock)])
        lock["expected_identity"] = fixture_identity(lock)
        with mock.patch.object(MODULE, "download_apt_restoration") as download:
            self.assertEqual(MODULE.fetch_apt_restorations(lock, Path("/not-created")), [])
            download.assert_not_called()

    def test_restoration_libc_consistency_includes_every_identity_section(self) -> None:
        original, _ = MODULE.load_lock()
        for section in ("poppler", "cairo", "python"):
            for mutation in ("version", "architecture"):
                lock = json.loads(json.dumps(original))
                key, version_key = ("provider", "provider_version") if section == "python" else ("package_name", "package_version")
                row = next(item for item in lock["expected_identity"][section]["native_libraries"] if item[key] == "libc6:amd64")
                row[version_key if mutation == "version" else key] = "2.39-wrong" if mutation == "version" else "libc6:arm64"
                with self.subTest(section=section, mutation=mutation):
                    with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_libc_identity"):
                        MODULE.apt_specs(lock, "all", restoration_dir=Path("/irrelevant"))
        lock = json.loads(json.dumps(original))
        next(row for row in lock["ubuntu_apt"]["bootstrap_packages"] if row["name"] == "libc6-dev:amd64")["version"] = "2.39-wrong"
        with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_libc_identity"):
            MODULE.exact_libc_specs(lock)
        lock["ubuntu_apt"]["bootstrap_packages"] = [
            row for row in lock["ubuntu_apt"]["bootstrap_packages"]
            if row["name"] != "libc6-dev:amd64"
        ]
        with self.assertRaisesRegex(MODULE.HostToolError, "apt_restoration_libc_identity"):
            MODULE.exact_libc_specs(lock)

    def test_restoration_cli_passes_directory_and_handles_validation_failure(self) -> None:
        with mock.patch.object(MODULE, "apt_specs", return_value=["/safe/pkg.deb"]) as specs:
            with mock.patch("sys.stdout", new_callable=io.StringIO) as output:
                self.assertEqual(MODULE.main(["apt-specs", "--scope", "poppler", "--restoration-dir", "/safe"]), 0)
            self.assertEqual(output.getvalue(), "/safe/pkg.deb\n")
            self.assertEqual(specs.call_args.args[1], "poppler")
            self.assertEqual(specs.call_args.kwargs, {"restoration_dir": Path("/safe")})
        with mock.patch.object(MODULE, "fetch_apt_restorations", side_effect=MODULE.HostToolError("apt_restoration_sha256")):
            with mock.patch("sys.stderr", new_callable=io.StringIO) as output:
                self.assertEqual(MODULE.main(["fetch-apt-restorations", "--output-dir", "/safe"]), 1)
            self.assertEqual(output.getvalue(), "apt_restoration_sha256\n")

    def test_checked_in_lock_has_exact_python_and_hashed_full_closure(self) -> None:
        lock, _ = MODULE.load_lock()
        self.assertEqual(lock["schema"], "rxls.render-oracle-host-tools-lock.v2")
        # The snapshot advances when the attested toolchain does, so assert its
        # shape — an immutable timestamped snapshot — rather than a literal that
        # has to be edited in lockstep with the lock it is meant to guard.
        self.assertIsNotNone(
            MODULE.UBUNTU_SNAPSHOT_RE.fullmatch(lock["ubuntu_apt"]["snapshot"])
        )
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
        # The shipped lock is unpinned while the oracle identity is being
        # re-established, and the bootstrap/identity cross-check only runs
        # against a pinned lock. Pin one explicitly so the rejection this test
        # exists to prove is actually exercised.
        lock["expected_identity"] = fixture_identity(lock)
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
        snapshot = lock["ubuntu_apt"]["snapshot"]
        self.assertEqual(
            MODULE.apt_sources(lock),
            f"""Types: deb
URIs: https://snapshot.ubuntu.com/ubuntu/{snapshot}
Suites: noble noble-updates noble-security
Components: main universe
Architectures: amd64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
""",
        )
        # Whatever the snapshot is, it must be an immutable pinned one.
        self.assertIsNotNone(MODULE.UBUNTU_SNAPSHOT_RE.fullmatch(snapshot))
        self.assertNotIn("latest", MODULE.apt_sources(lock))
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
        # The shipped lock carries the reviewed oracle identity.
        self.assertIsNotNone(lock["expected_identity"])
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
        # Re-bootstrapping against an existing attestation installs the whole
        # attested closure, so the captured identity is comparable by
        # construction rather than by exempting drifted libraries one at a time.
        pinned = json.loads(json.dumps(lock))
        pinned["expected_identity"] = fixture_identity(lock)
        attested = MODULE.apt_specs(pinned, "bootstrap")
        self.assertEqual(attested, sorted(set(attested)))
        self.assertTrue(set(attested) >= set(MODULE.apt_specs(pinned, "all")))
        self.assertIn("libcairo2:amd64=1.18.0-3build1", attested)
        self.assertIn("poppler-utils=24.02.0-1ubuntu9.9", attested)
        self.assertGreater(len(attested), 3, "the closure must exceed the top-level tools")
        lock = pinned
        # The provenance version stays recorded in the lock even though the
        # bootstrap install no longer pins it. Its value tracks the attested C
        # runtime rather than a literal, because the two are cross-checked and a
        # distribution security bump moves both together.
        recorded = [
            item["version"]
            for item in lock["ubuntu_apt"]["bootstrap_packages"]
            if item["name"] == "libc6-dev:amd64"
        ]
        self.assertEqual(len(recorded), 1)
        self.assertIsNotNone(MODULE.DEBIAN_VERSION_RE.fullmatch(recorded[0]))
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

    @unittest.skipIf(
        sys.platform == "win32",
        "ldd output and absolute-path semantics are Linux-specific",
    )
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
            try:
                link.symlink_to(target)
            except OSError as error:
                if sys.platform == "win32" and error.winerror == 1314:
                    self.skipTest("Windows symlink privilege is unavailable")
                raise
            with self.assertRaisesRegex(MODULE.HostToolError, "evidence_output"):
                MODULE.write_evidence(link, {"status": "fixture"})


if __name__ == "__main__":
    unittest.main()
