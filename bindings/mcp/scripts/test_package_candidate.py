#!/usr/bin/env python3
"""Regression tests for the non-publishing MCP package overlay."""

from __future__ import annotations

import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location("package_candidate", Path(__file__).with_name("package_candidate.py"))
assert spec and spec.loader
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)

CORE = '[package]\nname="rxls"\nversion="0.1.4"\n'
MCP = ('[package]\nname="rxls-mcp"\nversion="0.1.0"\nrust-version="1.88"\npublish=false\n'
       '[dependencies.rxls]\nversion="0.1.4"\npath="../.."\n')
NORMALIZED = MCP.replace('path="../.."\n', '')
LOCK = ('version=4\n[[package]]\nname="rxls"\nversion="0.1.4"\n'
        '[[package]]\nname="rxls-mcp"\nversion="0.1.0"\n')


def archive(path: Path, files: dict[str, bytes]) -> None:
    with tarfile.open(path, "w:gz") as result:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            result.addfile(info, io.BytesIO(data))


class PackageCandidateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "bindings/mcp/src").mkdir(parents=True)
        (self.root / "Cargo.toml").write_bytes(CORE.encode())
        self.mcp = self.root / "bindings/mcp"
        (self.mcp / "Cargo.toml").write_bytes(MCP.encode())
        (self.mcp / "Cargo.lock").write_bytes(LOCK.encode())
        (self.mcp / "src/main.rs").write_bytes(b"fn main() {}\n")
        self.path = self.root / "rxls-mcp-0.1.0.crate"

    def files(self, manifest: str = NORMALIZED) -> dict[str, bytes]:
        return {"rxls-mcp-0.1.0/Cargo.toml": manifest.encode(),
                "rxls-mcp-0.1.0/Cargo.toml.orig": MCP.encode(),
                "rxls-mcp-0.1.0/Cargo.lock": LOCK.encode(),
                "rxls-mcp-0.1.0/src/main.rs": b"fn main() {}\n"}

    def test_independent_versions_and_exact_local_lock(self) -> None:
        self.assertEqual(helper.identities(self.root), ("0.1.4", "0.1.0"))

    def test_source_identity_and_locked_graph_drift_are_rejected(self) -> None:
        for original, changed in ((MCP, MCP.replace('0.1.4', '0.1.3')),
                                  (MCP, MCP.replace('publish=false', 'publish=true')),
                                  (MCP, MCP.replace('1.88', '1.85'))):
            (self.mcp / "Cargo.toml").write_bytes(changed.encode())
            with self.assertRaises(ValueError):
                helper.identities(self.root)
        (self.mcp / "Cargo.toml").write_bytes(MCP.encode())
        (self.mcp / "Cargo.lock").write_bytes(LOCK.replace('0.1.4', '0.1.3').encode())
        with self.assertRaises(ValueError):
            helper.identities(self.root)

    def test_verified_archive_inventory_binds_every_file(self) -> None:
        archive(self.path, self.files())
        report = helper.verify_archive(self.path, self.mcp, "rxls-mcp", "0.1.0")
        self.assertEqual(len(report["files"]), 4)
        self.assertEqual(report["sha256"], helper.sha256(self.path.read_bytes()))

    def test_normalized_dependency_publication_and_msrv_drift_are_rejected(self) -> None:
        for changed in (NORMALIZED.replace('0.1.4', '0.1.3'),
                        NORMALIZED.replace('publish=false', 'publish=true'),
                        NORMALIZED.replace('1.88', '1.85'), MCP,
                        NORMALIZED + 'registry="local"\n'):
            archive(self.path, self.files(changed))
            with self.assertRaises(ValueError):
                helper.verify_archive(self.path, self.mcp, "rxls-mcp", "0.1.0")

    def test_original_manifest_and_archived_source_drift_are_rejected(self) -> None:
        for path in ("Cargo.toml.orig", "src/main.rs"):
            files = self.files()
            files[f"rxls-mcp-0.1.0/{path}"] += b"\n"
            archive(self.path, files)
            with self.assertRaises(ValueError):
                helper.verify_archive(self.path, self.mcp, "rxls-mcp", "0.1.0")

    def test_missing_lock_and_unexpected_archive_root_are_rejected(self) -> None:
        files = self.files()
        del files["rxls-mcp-0.1.0/Cargo.lock"]
        archive(self.path, files)
        with self.assertRaises(ValueError):
            helper.verify_archive(self.path, self.mcp, "rxls-mcp", "0.1.0")
        archive(self.path, {"rxls-mcp-0.9.0/src/main.rs": b""})
        with self.assertRaises(ValueError):
            helper.verify_archive(self.path, self.mcp, "rxls-mcp", "0.1.0")

    def test_archive_path_links_duplicates_and_byte_budget_are_bounded(self) -> None:
        for name in ("../escape", "/absolute", "safe\\escape", "safe/./escape", "safe//escape", "C:/escape"):
            archive(self.path, {name: b""})
            with tarfile.open(self.path) as result, self.assertRaises(ValueError):
                list(helper.safe_members(result))
        with tarfile.open(self.path, "w:gz") as result:
            link = tarfile.TarInfo("link")
            link.type = tarfile.SYMTYPE
            link.linkname = "../../escape"
            result.addfile(link)
        with tarfile.open(self.path) as result, self.assertRaises(ValueError):
            list(helper.safe_members(result))
        archive(self.path, {"large": b"aa"})
        with mock.patch.object(helper, "MAX_BYTES", 1), tarfile.open(self.path) as result, self.assertRaises(ValueError):
            list(helper.safe_members(result))
        with tarfile.open(self.path, "w:gz") as result:
            result.addfile(tarfile.TarInfo("duplicate"))
            result.addfile(tarfile.TarInfo("duplicate"))
        with tarfile.open(self.path) as result, self.assertRaises(ValueError):
            list(helper.safe_members(result))

    def test_source_revision_rejects_short_wrong_and_dirty_sources(self) -> None:
        with self.assertRaises(ValueError):
            helper.source_revision(self.root, "a" * 7)
        with mock.patch.object(helper, "run", return_value="b" * 40), self.assertRaises(ValueError):
            helper.source_revision(self.root, "a" * 40)

    def test_mcp_lock_is_bound_to_paired_core_registry_archive(self) -> None:
        checksum = "a" * 64
        canonical = 'registry+https://github.com/rust-lang/crates.io-index'
        lock = LOCK.replace('version="0.1.4"\n',
                            f'version="0.1.4"\nsource="{canonical}"\nchecksum="{checksum}"\n')
        files = self.files()
        files["rxls-mcp-0.1.0/Cargo.lock"] = lock.encode()
        archive(self.path, files)
        helper.verify_packaged_lock(self.path, LOCK.encode(), "0.1.4", checksum, mcp=True)
        for version, digest in (("0.1.3", checksum), ("0.1.4", "b" * 64)):
            with self.assertRaises(ValueError):
                helper.verify_packaged_lock(self.path, LOCK.encode(), version, digest, mcp=True)
        files["rxls-mcp-0.1.0/Cargo.lock"] = lock.replace(canonical, "registry+file:///fake").encode()
        archive(self.path, files)
        with self.assertRaises(ValueError):
            helper.verify_packaged_lock(self.path, LOCK.encode(), "0.1.4", checksum, mcp=True)

    def test_registry_version_and_checksum_drift_are_rejected(self) -> None:
        registry = ('[[package]]\nname="serde"\nversion="1.0.228"\n'
                    'source="registry+https://github.com/rust-lang/crates.io-index"\n'
                    f'checksum="{"a" * 64}"\n')
        files = self.files()
        files["rxls-mcp-0.1.0/Cargo.lock"] = (LOCK + registry.replace("1.0.228", "1.0.227")).encode()
        archive(self.path, files)
        with self.assertRaises(ValueError):
            helper.verify_packaged_lock(self.path, (LOCK + registry).encode(), "0.1.4", "", mcp=False)

    def test_git_archive_omission_or_substitution_is_rejected(self) -> None:
        data = b"original\n"
        blob = helper.hashlib.sha1(b"blob 9\0" + data).hexdigest()
        listing = f"100644 blob {blob} 9\ttracked.txt\0".encode()
        destination = self.root / "staged"
        for files in ({}, {"tracked.txt": b"substituted\n"}):
            def write_archive(arguments, **kwargs):
                with tarfile.open(arguments[4], "w:") as result:
                    for name, content in files.items():
                        info = tarfile.TarInfo(name)
                        info.size = len(content)
                        result.addfile(info, io.BytesIO(content))
            with mock.patch.object(helper.subprocess, "check_output", return_value=listing), \
                    mock.patch.object(helper.subprocess, "run", side_effect=write_archive), \
                    self.assertRaises(ValueError):
                helper.snapshot(self.root, "a" * 40, destination)

    def test_commands_have_finite_timeouts(self) -> None:
        with mock.patch.object(helper.subprocess, "run") as execution:
            execution.return_value.stdout = "ok"
            self.assertEqual(helper.run(["tool"], cwd=self.root), "ok")
            self.assertEqual(execution.call_args.kwargs["timeout"], helper.COMMAND_TIMEOUT)
        with mock.patch.object(helper, "run", side_effect=["a" * 40, " M Cargo.toml"]), self.assertRaises(ValueError):
            helper.source_revision(self.root, "a" * 40)


if __name__ == "__main__":
    unittest.main()
