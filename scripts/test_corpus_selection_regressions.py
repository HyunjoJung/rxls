"""Regression tests for deterministic corpus selection, without oracle packages."""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

from public_corpus_manifest import corpus_files, manifest_files


class CorpusSelectionRegressionTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def write_file(self, name: str) -> Path:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"selection fixture; not a workbook")
        return path

    def write_manifest(self, entries: list[dict], *, bare: bool = False) -> Path:
        path = self.root / "manifest.json"
        path.write_text(
            json.dumps(entries if bare else {"files": entries}), encoding="utf-8"
        )
        return path

    def test_flat_selection_matches_manifest_for_mixed_case_extensions(self) -> None:
        names = ["a.XLS", "b.XlSx", "c.XLSM", "d.XlsB", "e.ODS"]
        paths = [self.write_file(name) for name in names]
        manifest = self.write_manifest(
            [{"local_path": str(path), "status": "ready"} for path in paths]
        )
        extensions = {".xls", ".xlsx", ".xlsm", ".xlsb", ".ods"}
        expected = sorted(str(path) for path in paths)
        self.assertEqual(manifest_files(manifest, extensions), expected)
        self.assertEqual(corpus_files(self.root, extensions), expected)

    def test_dotless_extension_aliases_do_not_duplicate_flat_files(self) -> None:
        workbook = self.write_file("sample.xls")
        self.assertEqual(
            corpus_files(self.root, ["xls", ".xls", ".XLS"]), [str(workbook)]
        )

    def test_manifest_accepts_the_same_dotless_extensions_as_flat_selection(self) -> None:
        workbook = self.write_file("sample.xlsx")
        manifest = self.write_manifest([{"local_path": str(workbook)}])
        expected = [str(workbook)]
        self.assertEqual(corpus_files(self.root, ["XLSX"]), expected)
        self.assertEqual(manifest_files(manifest, ["XLSX"]), expected)

    def test_flat_selection_treats_root_brackets_literally(self) -> None:
        selected = self.write_file("corpus[1]/sample.xls")
        self.write_file("corpus1/wrong.xls")
        self.assertEqual(corpus_files(selected.parent, {".xls"}), [str(selected)])

    def test_flat_selection_excludes_directories_before_applying_limit(self) -> None:
        (self.root / "a.xls").mkdir()
        workbook = self.write_file("b.xls")
        self.assertEqual(corpus_files(self.root, {".xls"}, limit=1), [str(workbook)])

    def test_manifest_selection_excludes_directory_entries(self) -> None:
        directory = self.root / "a.xls"
        directory.mkdir()
        workbook = self.write_file("b.xls")
        manifest = self.write_manifest(
            [{"local_path": str(path), "status": "ready"} for path in [directory, workbook]]
        )
        self.assertEqual(manifest_files(manifest, {".xls"}, limit=1), [str(workbook)])

    def test_manifest_relative_file_is_not_shadowed_by_checkout_directory(self) -> None:
        workbook = self.write_file("payload/sample.xls")
        checkout = self.root / "checkout"
        (checkout / "payload/sample.xls").mkdir(parents=True)
        manifest = self.write_manifest([{"local_path": "payload/sample.xls"}])
        previous = os.getcwd()
        try:
            os.chdir(checkout)
            self.assertEqual(manifest_files(manifest, {".xls"}), [str(workbook)])
        finally:
            os.chdir(previous)

    def test_flat_selection_remains_nonrecursive_and_excludes_hidden_files(self) -> None:
        workbook = self.write_file("visible.xls")
        for name in [".hidden.xls", "nested/other.xls", "ignored.txt"]:
            self.write_file(name)
        self.assertEqual(corpus_files(self.root, {".xls"}), [str(workbook)])

    def test_flat_selection_sorts_before_applying_limit(self) -> None:
        for name in ["c.xls", "a.xls", "b.xls"]:
            self.write_file(name)
        self.assertEqual(
            corpus_files(self.root, {".xls"}, limit=2),
            [str(self.root / name) for name in ["a.xls", "b.xls"]],
        )
        self.assertEqual(corpus_files(self.root, {".xls"}, limit=0), [])

    def test_missing_or_empty_flat_corpus_preserves_empty_selection(self) -> None:
        self.assertEqual(corpus_files(self.root, {".xls"}), [])
        self.assertEqual(corpus_files(self.root / "missing", {".xls"}), [])
        self.write_file("sample.xls")
        self.assertEqual(corpus_files(self.root, []), [])

    def test_manifest_preserves_status_eligibility_and_deduplication(self) -> None:
        workbook = self.write_file("selected.xls")
        excluded = self.write_file("excluded.xls")
        entries = [
            {"local_path": str(workbook), "status": status}
            for status in ["cached", "downloaded", "duplicate", "ready"]
        ]
        entries += [
            {"local_path": str(excluded), "eligible": False},
            {"local_path": str(excluded), "render_selected": False},
            {"local_path": str(excluded), "status": "failed"},
            {"local_path": str(self.root / "missing.xls"), "status": "ready"},
        ]
        manifest = self.write_manifest(entries, bare=True)
        self.assertEqual(manifest_files(manifest, {".xls"}), [str(workbook)])
        self.assertEqual(manifest_files(manifest, {".xls"}, limit=0), [])

    def test_manifest_keeps_checkout_relative_file_precedence(self) -> None:
        self.write_file("payload/sample.xls")
        workbook = self.write_file("checkout/payload/sample.xls")
        manifest = self.write_manifest([{"local_path": "payload/sample.xls"}])
        previous = os.getcwd()
        try:
            os.chdir(self.root / "checkout")
            self.assertEqual(manifest_files(manifest, {".xls"}), [str(workbook)])
        finally:
            os.chdir(previous)

    @unittest.skipUnless(hasattr(os, "symlink"), "symlinks are unavailable")
    def test_flat_selection_keeps_file_links_but_excludes_broken_and_directory_links(self) -> None:
        workbook = self.write_file("real.xls")
        directory = self.root / "directory"
        directory.mkdir()
        link = self.root / "linked.xls"
        try:
            link.symlink_to(workbook)
            (self.root / "broken.xls").symlink_to(self.root / "missing.xls")
            (self.root / "directory.xls").symlink_to(directory, target_is_directory=True)
        except (OSError, NotImplementedError) as error:
            self.skipTest(f"cannot create symlinks: {error}")
        self.assertEqual(
            corpus_files(self.root, {".xls"}), sorted([str(workbook), str(link)])
        )


if __name__ == "__main__":
    unittest.main()
